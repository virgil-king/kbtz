use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::session::{SessionHandle, SessionStatus};
use kbtz_workspace::protocol::{self, Message};

pub struct ShepherdSession {
    socket_path: PathBuf,
    writer: Mutex<BufWriter<UnixStream>>,
    forwarding: Arc<AtomicBool>,
    status: SessionStatus,
    task_name: String,
    session_id: String,
    shepherd_pid: u32,
    stopping_since: Option<Instant>,
}

impl ShepherdSession {
    pub fn connect(
        socket_path: &Path,
        pid_path: &Path,
        task_name: &str,
        session_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let pid_str = std::fs::read_to_string(pid_path)
            .with_context(|| format!("failed to read shepherd PID from {}", pid_path.display()))?;
        let shepherd_pid: u32 = pid_str
            .trim()
            .parse()
            .with_context(|| format!("invalid PID in {}: {:?}", pid_path.display(), pid_str))?;

        let stream = UnixStream::connect(socket_path).with_context(|| {
            format!("failed to connect to shepherd at {}", socket_path.display())
        })?;
        let read_stream = stream
            .try_clone()
            .context("failed to clone Unix stream for reader")?;
        let write_stream = stream;

        let pty_rows = rows.saturating_sub(1);

        // Size-first handshake: send Resize before reading InitialState
        // so the shepherd builds the restore sequence at our terminal size.
        let writer = Mutex::new(BufWriter::new(write_stream));
        {
            let mut w = writer
                .lock()
                .expect("writer lock poisoned during construction");
            protocol::write_message(
                &mut *w,
                &Message::Resize {
                    rows: pty_rows,
                    cols,
                },
            )
            .context("failed to send initial resize to shepherd")?;
        }

        let mut reader = BufReader::new(read_stream);

        // Read InitialState — shepherd builds this from structured VTE
        // data (scrollback rows + state_formatted), not raw byte replay.
        let first_msg = protocol::read_message(&mut reader)
            .context("failed to read initial message from shepherd")?;
        let initial_data = match first_msg {
            Some(Message::InitialState(data)) => data,
            Some(other) => bail!(
                "expected InitialState from shepherd, got {:?}",
                std::mem::discriminant(&other)
            ),
            None => bail!("shepherd closed connection before sending InitialState"),
        };

        // The initial_data is a restore sequence from the shepherd's VTE.
        // We don't process it through a local VTE — it will be replayed
        // raw to the terminal when forwarding starts.  Store it so the
        // first start_forwarding can replay it.
        let restore_data = Arc::new(Mutex::new(Some(initial_data)));

        let forwarding = Arc::new(AtomicBool::new(false));

        // Spawn reader thread
        let fwd_clone = Arc::clone(&forwarding);
        let restore_clone = Arc::clone(&restore_data);
        std::thread::spawn(move || shepherd_reader_thread(reader, fwd_clone, restore_clone));

        Ok(ShepherdSession {
            socket_path: socket_path.to_path_buf(),
            writer,
            forwarding,
            status: SessionStatus::Starting,
            task_name: task_name.to_string(),
            session_id: session_id.to_string(),
            shepherd_pid,
            stopping_since: None,
        })
    }
}

fn shepherd_reader_thread(
    mut reader: BufReader<UnixStream>,
    forwarding: Arc<AtomicBool>,
    restore_data: Arc<Mutex<Option<Vec<u8>>>>,
) {
    let stdout = std::io::stdout();

    // Check if there's restore data to replay on first forwarding activation.
    let mut pending_restore = true;

    loop {
        match protocol::read_message(&mut reader) {
            Ok(Some(Message::PtyOutput(data))) => {
                if forwarding.load(Ordering::Relaxed) {
                    let mut out = stdout.lock();

                    // Replay restore data before the first forwarded chunk.
                    if pending_restore {
                        pending_restore = false;
                        if let Ok(mut guard) = restore_data.lock() {
                            if let Some(restore) = guard.take() {
                                let _ = out.write_all(&restore);
                            }
                        }
                    }

                    let _ = out.write_all(&data);
                    let _ = out.flush();
                }
            }
            Ok(Some(_)) => {}           // Ignore unexpected messages
            Ok(None) | Err(_) => break, // EOF or error
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::BrokenPipe)
}

impl SessionHandle for ShepherdSession {
    fn task_name(&self) -> &str {
        &self.task_name
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn status(&self) -> &SessionStatus {
        &self.status
    }

    fn set_status(&mut self, status: SessionStatus) {
        self.status = status;
    }

    fn stopping_since(&self) -> Option<Instant> {
        self.stopping_since
    }

    fn is_alive(&mut self) -> bool {
        let ret = unsafe { libc::kill(self.shepherd_pid as libc::pid_t, 0) };
        let process_alive = ret == 0
            || (ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM));
        process_alive && self.socket_path.exists()
    }

    fn mark_stopping(&mut self) {
        if self.stopping_since.is_none() {
            self.stopping_since = Some(Instant::now());
        }
    }

    fn force_kill(&mut self) {
        unsafe {
            libc::kill(self.shepherd_pid as i32, libc::SIGKILL);
        }
    }

    fn start_forwarding(&self) {
        self.forwarding.store(true, Ordering::SeqCst);
    }

    fn stop_forwarding(&self) {
        self.forwarding.store(false, Ordering::SeqCst);
        crate::session::reset_terminal_modes();
    }

    fn write_input(&mut self, buf: &[u8]) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("writer mutex poisoned"))?;
        if let Err(e) = protocol::write_message(&mut *writer, &Message::PtyInput(buf.to_vec())) {
            if is_broken_pipe(&e) {
                return Ok(());
            }
            return Err(e).context("write input to shepherd");
        }
        Ok(())
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let pty_rows = rows.saturating_sub(1);
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("writer mutex poisoned"))?;
        if let Err(e) = protocol::write_message(
            &mut *writer,
            &Message::Resize {
                rows: pty_rows,
                cols,
            },
        ) {
            if is_broken_pipe(&e) {
                return Ok(());
            }
            return Err(e).context("send resize to shepherd");
        }
        Ok(())
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.shepherd_pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    /// Helper: create a ShepherdSession with one end of a UnixStream pair,
    /// simulating the size-first handshake.
    fn make_test_session(socket_path: &Path) -> (ShepherdSession, BufReader<UnixStream>) {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();

        // Simulate the shepherd side: send InitialState with simple content.
        let mut server_writer = BufWriter::new(server_stream.try_clone().unwrap());
        protocol::write_message(
            &mut server_writer,
            &Message::InitialState(b"hello".to_vec()),
        )
        .unwrap();

        let read_stream = client_stream.try_clone().unwrap();
        let write_stream = client_stream;

        let mut reader = BufReader::new(read_stream);

        let first_msg = protocol::read_message(&mut reader).unwrap().unwrap();
        let initial_data = match first_msg {
            Message::InitialState(data) => data,
            other => panic!("expected InitialState, got {:?}", other),
        };

        let restore_data = Arc::new(Mutex::new(Some(initial_data)));
        let forwarding = Arc::new(AtomicBool::new(false));

        let fwd_clone = Arc::clone(&forwarding);
        let restore_clone = Arc::clone(&restore_data);
        std::thread::spawn(move || shepherd_reader_thread(reader, fwd_clone, restore_clone));

        let session = ShepherdSession {
            socket_path: socket_path.to_path_buf(),
            writer: Mutex::new(BufWriter::new(write_stream)),
            forwarding,
            status: SessionStatus::Starting,
            task_name: "test-task".to_string(),
            session_id: "test-session".to_string(),
            shepherd_pid: std::process::id(),
            stopping_since: None,
        };

        let server_reader = BufReader::new(server_stream);
        (session, server_reader)
    }

    #[test]
    fn test_is_alive_with_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        std::fs::write(&socket_path, "").unwrap();

        let mut session = ShepherdSession {
            socket_path: socket_path.clone(),
            writer: Mutex::new(BufWriter::new(UnixStream::pair().unwrap().0)),
            forwarding: Arc::new(AtomicBool::new(false)),
            status: SessionStatus::Starting,
            task_name: "test".to_string(),
            session_id: "test-id".to_string(),
            shepherd_pid: std::process::id(),
            stopping_since: None,
        };

        assert!(session.is_alive(), "socket file exists, should be alive");

        std::fs::remove_file(&socket_path).unwrap();
        assert!(
            !session.is_alive(),
            "socket file removed, should not be alive"
        );
    }

    #[test]
    fn test_write_input_sends_pty_input() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&socket_path, "").unwrap();

        let (mut session, mut server_reader) = make_test_session(&socket_path);

        session.write_input(b"hello").unwrap();

        let msg = protocol::read_message(&mut server_reader).unwrap().unwrap();
        assert_eq!(msg, Message::PtyInput(b"hello".to_vec()));
    }

    #[test]
    fn test_resize_sends_resize_message() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&socket_path, "").unwrap();

        let (session, mut server_reader) = make_test_session(&socket_path);

        session.resize(25, 80).unwrap();

        let msg = protocol::read_message(&mut server_reader).unwrap().unwrap();
        assert_eq!(msg, Message::Resize { rows: 24, cols: 80 });
    }
}
