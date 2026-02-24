use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::session::{Passthrough, SessionHandle, SessionStatus};
use kbtz_workspace::protocol::{self, Message};

pub struct ShepherdSession {
    socket_path: PathBuf,
    writer: Mutex<BufWriter<UnixStream>>,
    passthrough: Arc<Mutex<Passthrough>>,
    reader_alive: Arc<AtomicBool>,
    status: SessionStatus,
    task_name: String,
    session_id: String,
    shepherd_pid: u32,
    stopping_since: Option<Instant>,
    last_rows: u16,
    last_cols: u16,
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

        let mut reader = BufReader::new(read_stream);

        // Read the first message — must be InitialState
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

        let pty_rows = rows.saturating_sub(1);
        let mut pt = Passthrough::new(pty_rows, cols);
        pt.process(&initial_data);
        let passthrough = Arc::new(Mutex::new(pt));

        // Spawn reader thread
        let reader_alive = Arc::new(AtomicBool::new(true));
        let pt_clone = Arc::clone(&passthrough);
        let alive_clone = Arc::clone(&reader_alive);
        std::thread::spawn(move || {
            shepherd_reader_thread(reader, pt_clone);
            alive_clone.store(false, Ordering::SeqCst);
        });

        // Send initial Resize to tell the shepherd our current terminal size
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

        Ok(ShepherdSession {
            socket_path: socket_path.to_path_buf(),
            writer,
            passthrough,
            reader_alive,
            status: SessionStatus::Starting,
            task_name: task_name.to_string(),
            session_id: session_id.to_string(),
            shepherd_pid,
            stopping_since: None,
            last_rows: pty_rows,
            last_cols: cols,
        })
    }
}

/// Establish a new socket connection to the shepherd, read the InitialState
/// handshake, and spawn a fresh reader thread.  On success the writer and
/// reader_alive flag are replaced so the session resumes normal operation.
fn reconnect_to_shepherd(
    socket_path: &Path,
    writer: &Mutex<BufWriter<UnixStream>>,
    passthrough: &Arc<Mutex<Passthrough>>,
    reader_alive: &Arc<AtomicBool>,
    rows: u16,
    cols: u16,
) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .with_context(|| format!("reconnect: connect to {}", socket_path.display()))?;
    let read_stream = stream
        .try_clone()
        .context("reconnect: clone stream for reader")?;
    let write_stream = stream;

    let mut reader = BufReader::new(read_stream);

    // Read and discard the InitialState — our passthrough already has the
    // accumulated terminal state.  Any output produced during the brief
    // disconnect is in the shepherd's buffer but will flow through the new
    // reader thread from this point forward.
    let first_msg =
        protocol::read_message(&mut reader).context("reconnect: read InitialState")?;
    match first_msg {
        Some(Message::InitialState(_)) => {}
        Some(other) => bail!(
            "reconnect: expected InitialState, got {:?}",
            std::mem::discriminant(&other)
        ),
        None => bail!("reconnect: shepherd closed connection before sending InitialState"),
    }

    // Spawn new reader thread.
    reader_alive.store(true, Ordering::SeqCst);
    let pt_clone = Arc::clone(passthrough);
    let alive_clone = Arc::clone(reader_alive);
    std::thread::spawn(move || {
        shepherd_reader_thread(reader, pt_clone);
        alive_clone.store(false, Ordering::SeqCst);
    });

    // Replace the writer.
    {
        let mut w = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("writer mutex poisoned during reconnect"))?;
        *w = BufWriter::new(write_stream);

        // Tell the shepherd our current terminal size.
        protocol::write_message(&mut *w, &Message::Resize { rows, cols })
            .context("reconnect: send resize")?;
    }

    Ok(())
}

fn shepherd_reader_thread(mut reader: BufReader<UnixStream>, passthrough: Arc<Mutex<Passthrough>>) {
    loop {
        match protocol::read_message(&mut reader) {
            Ok(Some(Message::PtyOutput(data))) => {
                let Ok(mut pt) = passthrough.lock() else {
                    break;
                };
                pt.process(&data);
                if pt.active {
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
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
        // Check process liveness first: if the shepherd was SIGKILLed its
        // cleanup code never ran and the socket file is left behind.
        // EPERM means the process exists but we can't signal it — treat as alive.
        let ret = unsafe { libc::kill(self.shepherd_pid as libc::pid_t, 0) };
        let process_alive = ret == 0
            || (ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM));
        if !process_alive || !self.socket_path.exists() {
            return false;
        }

        // The shepherd process is alive, but the reader thread may have died
        // (e.g. socket disrupted during macOS sleep/wake).  Try to reconnect
        // so the session self-heals instead of appearing alive but frozen.
        if !self.reader_alive.load(Ordering::SeqCst) {
            if reconnect_to_shepherd(
                &self.socket_path,
                &self.writer,
                &self.passthrough,
                &self.reader_alive,
                self.last_rows,
                self.last_cols,
            )
            .is_err()
            {
                // Reconnection failed — shepherd may be shutting down.
                // Report dead so lifecycle can reap the session.
                return false;
            }
        }

        true
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

    fn start_passthrough(&self) -> Result<()> {
        self.passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .start();
        Ok(())
    }

    fn stop_passthrough(&self) -> Result<()> {
        self.passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .stop();
        Ok(())
    }

    fn enter_scroll_mode(&self) -> Result<usize> {
        Ok(self
            .passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .enter_scroll_mode())
    }

    fn exit_scroll_mode(&self) -> Result<()> {
        self.passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .exit_scroll_mode();
        Ok(())
    }

    fn render_scrollback(&self, offset: usize, cols: u16) -> Result<usize> {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        Ok(self
            .passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .render_scrollback(&mut out, offset, cols))
    }

    fn scrollback_available(&self) -> Result<usize> {
        Ok(self
            .passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .scrollback_available())
    }

    fn has_mouse_tracking(&self) -> bool {
        self.passthrough
            .lock()
            .map(|pt| pt.has_mouse_tracking())
            .unwrap_or(false)
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

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let pty_rows = rows.saturating_sub(1);
        self.last_rows = pty_rows;
        self.last_cols = cols;
        self.passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .set_size(pty_rows, cols);

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
    /// after sending the required InitialState handshake on the other end.
    fn make_test_session(socket_path: &Path) -> (ShepherdSession, BufReader<UnixStream>) {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();

        // The server side sends InitialState, then the client connects.
        // But connect() expects to connect to a path. We need to build
        // the ShepherdSession manually for testing since we can't use a
        // real filesystem socket with UnixStream::pair().
        let mut server_writer = BufWriter::new(server_stream.try_clone().unwrap());
        protocol::write_message(
            &mut server_writer,
            &Message::InitialState(b"hello".to_vec()),
        )
        .unwrap();

        let read_stream = client_stream.try_clone().unwrap();
        let write_stream = client_stream;

        let mut reader = BufReader::new(read_stream);

        // Read the InitialState
        let first_msg = protocol::read_message(&mut reader).unwrap().unwrap();
        let initial_data = match first_msg {
            Message::InitialState(data) => data,
            other => panic!("expected InitialState, got {:?}", other),
        };

        let mut pt = Passthrough::new(23, 80);
        pt.process(&initial_data);
        let passthrough = Arc::new(Mutex::new(pt));

        let reader_alive = Arc::new(AtomicBool::new(true));
        let pt_clone = Arc::clone(&passthrough);
        let alive_clone = Arc::clone(&reader_alive);
        std::thread::spawn(move || {
            shepherd_reader_thread(reader, pt_clone);
            alive_clone.store(false, Ordering::SeqCst);
        });

        let session = ShepherdSession {
            socket_path: socket_path.to_path_buf(),
            writer: Mutex::new(BufWriter::new(write_stream)),
            passthrough,
            reader_alive,
            status: SessionStatus::Starting,
            task_name: "test-task".to_string(),
            session_id: "test-session".to_string(),
            shepherd_pid: std::process::id(),
            stopping_since: None,
            last_rows: 23,
            last_cols: 80,
        };

        let server_reader = BufReader::new(server_stream);
        (session, server_reader)
    }

    #[test]
    fn test_is_alive_with_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        // Create a file at the socket path
        std::fs::write(&socket_path, "").unwrap();

        let mut session = ShepherdSession {
            socket_path: socket_path.clone(),
            writer: Mutex::new(BufWriter::new(UnixStream::pair().unwrap().0)),
            passthrough: Arc::new(Mutex::new(Passthrough::new(24, 80))),
            reader_alive: Arc::new(AtomicBool::new(true)),
            status: SessionStatus::Starting,
            task_name: "test".to_string(),
            session_id: "test-id".to_string(),
            shepherd_pid: std::process::id(),
            stopping_since: None,
            last_rows: 23,
            last_cols: 80,
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

        // First message from session construction is the Resize that the reader
        // thread might have consumed — but in our test helper we don't send a
        // Resize during construction. We just write input directly.
        session.write_input(b"hello").unwrap();

        let msg = protocol::read_message(&mut server_reader).unwrap().unwrap();
        assert_eq!(msg, Message::PtyInput(b"hello".to_vec()));
    }

    #[test]
    fn test_resize_sends_resize_message() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&socket_path, "").unwrap();

        let (mut session, mut server_reader) = make_test_session(&socket_path);

        session.resize(25, 80).unwrap();

        let msg = protocol::read_message(&mut server_reader).unwrap().unwrap();
        assert_eq!(msg, Message::Resize { rows: 24, cols: 80 });
    }

    #[test]
    fn test_is_alive_reconnects_dead_reader() {
        // Set up a real Unix socket listener to simulate a shepherd.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        // Accept connections on a background thread and always send InitialState.
        let accept_thread = std::thread::spawn(move || {
            let mut connections = Vec::new();
            // Accept up to 2 connections (initial + reconnect).
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                protocol::write_message(
                    &mut stream,
                    &Message::InitialState(b"state".to_vec()),
                )
                .unwrap();
                connections.push(stream);
            }
            connections
        });

        // Connect the initial session.
        let stream = UnixStream::connect(&socket_path).unwrap();
        let read_stream = stream.try_clone().unwrap();
        let write_stream = stream;

        let mut reader = BufReader::new(read_stream);
        let first_msg = protocol::read_message(&mut reader).unwrap().unwrap();
        let initial_data = match first_msg {
            Message::InitialState(data) => data,
            other => panic!("expected InitialState, got {:?}", other),
        };

        let mut pt = Passthrough::new(23, 80);
        pt.process(&initial_data);
        let passthrough = Arc::new(Mutex::new(pt));

        let reader_alive = Arc::new(AtomicBool::new(true));
        let pt_clone = Arc::clone(&passthrough);
        let alive_clone = Arc::clone(&reader_alive);
        std::thread::spawn(move || {
            shepherd_reader_thread(reader, pt_clone);
            alive_clone.store(false, Ordering::SeqCst);
        });

        let mut session = ShepherdSession {
            socket_path: socket_path.clone(),
            writer: Mutex::new(BufWriter::new(write_stream)),
            passthrough,
            reader_alive,
            status: SessionStatus::Starting,
            task_name: "test-task".to_string(),
            session_id: "test-session".to_string(),
            shepherd_pid: std::process::id(),
            stopping_since: None,
            last_rows: 23,
            last_cols: 80,
        };

        // Session should be alive initially.
        assert!(session.is_alive());

        // Simulate reader thread death (as if the socket got disrupted).
        session.reader_alive.store(false, Ordering::SeqCst);

        // is_alive should detect the dead reader and reconnect.
        assert!(
            session.is_alive(),
            "is_alive should reconnect when reader is dead but shepherd is running"
        );
        assert!(
            session.reader_alive.load(Ordering::SeqCst),
            "reader_alive should be true after reconnect"
        );

        // Clean up.
        drop(session);
        let _ = accept_thread.join();
    }
}
