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
    status: SessionStatus,
    task_name: String,
    session_id: String,
    shepherd_pid: u32,
    child_pid: Option<u32>,
    stopping_since: Option<Instant>,
    /// Set to false by the reader thread when it exits.  Mirrors the
    /// `reader_alive` flag on `Session` — a dead reader with a live
    /// shepherd means the socket connection broke and the session is
    /// frozen (no output forwarding, input silently dropped).
    reader_alive: Arc<AtomicBool>,
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

        let child_pid: Option<u32> = std::fs::read_to_string(pid_path.with_extension("child-pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok());

        let stream = UnixStream::connect(socket_path).with_context(|| {
            format!("failed to connect to shepherd at {}", socket_path.display())
        })?;
        let read_stream = stream
            .try_clone()
            .context("failed to clone Unix stream for reader")?;
        let write_stream = stream;

        let pty_rows = rows.saturating_sub(1);

        // Handshake: version byte → Resize → InitialState.
        // The version byte lets the shepherd reject incompatible clients.
        let writer = Mutex::new(BufWriter::new(write_stream));
        {
            let mut w = writer
                .lock()
                .expect("writer lock poisoned during construction");
            protocol::write_version(&mut *w)
                .context("failed to send protocol version to shepherd")?;
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
        // The InitialState also carries the shepherd's session_id so we can
        // verify we connected to the right process (prevents PID-reuse races).
        let first_msg = protocol::read_message(&mut reader)
            .context("failed to read initial message from shepherd")?;
        let initial_data = match first_msg {
            Some(Message::InitialState {
                session_id: shepherd_sid,
                data,
            }) => {
                if shepherd_sid != session_id {
                    bail!(
                        "session_id mismatch: expected {:?}, shepherd reported {:?} \
                         (PID was likely reused by a different session)",
                        session_id,
                        shepherd_sid,
                    );
                }
                data
            }
            Some(other) => bail!(
                "expected InitialState from shepherd, got {:?}",
                std::mem::discriminant(&other)
            ),
            None => bail!("shepherd closed connection before sending InitialState"),
        };

        // Process directly — the restore sequence is structured data at
        // our terminal size, so no temp VTE or filtering needed.
        let mut pt = Passthrough::new(pty_rows, cols);
        pt.process(&initial_data);
        let passthrough = Arc::new(Mutex::new(pt));

        // Spawn reader thread
        let pt_clone = Arc::clone(&passthrough);
        let reader_alive = Arc::new(AtomicBool::new(true));
        let ra = Arc::clone(&reader_alive);
        let reader_sid = session_id.to_string();
        std::thread::spawn(move || shepherd_reader_thread(reader, pt_clone, ra, reader_sid));

        Ok(ShepherdSession {
            socket_path: socket_path.to_path_buf(),
            writer,
            passthrough,
            status: SessionStatus::Starting,
            task_name: task_name.to_string(),
            session_id: session_id.to_string(),
            shepherd_pid,
            child_pid,
            stopping_since: None,
            reader_alive,
        })
    }
}

// Note: EINTR is handled internally by `read_exact` (which `protocol::read_message`
// uses), so unlike the PTY reader thread we don't need explicit EINTR retry here.
fn shepherd_reader_thread(
    mut reader: BufReader<UnixStream>,
    passthrough: Arc<Mutex<Passthrough>>,
    alive_flag: Arc<AtomicBool>,
    session_id: String,
) {
    let stdout = std::io::stdout();

    let exit_reason;
    loop {
        match protocol::read_message(&mut reader) {
            Ok(Some(Message::PtyOutput(data))) => {
                let Ok(mut pt) = passthrough.lock() else {
                    exit_reason = "mutex poisoned";
                    break;
                };
                pt.process(&data);

                if pt.active {
                    let mut out = stdout.lock();
                    let _ = out.write_all(&data);
                    let _ = out.flush();
                }
            }
            Ok(Some(_)) => {} // Ignore unexpected messages
            Ok(None) => {
                exit_reason = "EOF";
                break;
            }
            Err(_) => {
                exit_reason = "error";
                break;
            }
        }
    }

    alive_flag.store(false, Ordering::Release);
    kbtz::debug_log::log(&format!(
        "shepherd_reader_thread({session_id}): exited ({exit_reason})"
    ));
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
        process_alive && self.socket_path.exists()
    }

    fn mark_stopping(&mut self) {
        if self.stopping_since.is_none() {
            self.stopping_since = Some(Instant::now());
        }
    }

    fn force_kill(&mut self) {
        // Kill the child's process group before the shepherd so the agent
        // and its descendants don't become orphans.
        if let Some(pid) = self.child_pid {
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        unsafe {
            libc::kill(self.shepherd_pid as libc::pid_t, libc::SIGKILL);
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
        let mut pt = self
            .passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?;
        Ok(kbtz_workspace::with_sync_stdout(|out| {
            pt.render_scrollback(out, offset, cols)
        }))
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

    fn terminal_sync_bytes(&self) -> Result<Vec<u8>> {
        Ok(self
            .passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .terminal_sync_bytes())
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let pty_rows = rows.saturating_sub(1);
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

    fn reader_alive(&self) -> bool {
        self.reader_alive.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::os::unix::process::CommandExt;

    /// Helper: create a ShepherdSession with one end of a UnixStream pair,
    /// simulating the size-first handshake.
    fn make_test_session(socket_path: &Path) -> (ShepherdSession, BufReader<UnixStream>) {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();

        // Simulate the shepherd side: it will receive a Resize first,
        // then send InitialState.  For tests we just send InitialState
        // with simple content (the test helper doesn't need a real VTE).
        let mut server_writer = BufWriter::new(server_stream.try_clone().unwrap());
        protocol::write_message(
            &mut server_writer,
            &Message::InitialState {
                session_id: "test-session".to_string(),
                data: b"hello".to_vec(),
            },
        )
        .unwrap();

        let read_stream = client_stream.try_clone().unwrap();
        let write_stream = client_stream;

        let mut reader = BufReader::new(read_stream);

        // Read InitialState (in real code the client sends Resize first,
        // but in this test helper we skip that since there's no real shepherd).
        let first_msg = protocol::read_message(&mut reader).unwrap().unwrap();
        let initial_data = match first_msg {
            Message::InitialState { data, .. } => data,
            other => panic!("expected InitialState, got {:?}", other),
        };

        let mut pt = Passthrough::new(23, 80);
        pt.process(&initial_data);
        let passthrough = Arc::new(Mutex::new(pt));

        let pt_clone = Arc::clone(&passthrough);
        let reader_alive = Arc::new(AtomicBool::new(true));
        let ra = Arc::clone(&reader_alive);
        std::thread::spawn(move || {
            shepherd_reader_thread(reader, pt_clone, ra, "test-session".to_string())
        });

        let session = ShepherdSession {
            socket_path: socket_path.to_path_buf(),
            writer: Mutex::new(BufWriter::new(write_stream)),
            passthrough,
            status: SessionStatus::Starting,
            task_name: "test-task".to_string(),
            session_id: "test-session".to_string(),
            shepherd_pid: std::process::id(),
            child_pid: None,
            stopping_since: None,
            reader_alive,
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

        let session = ShepherdSession {
            socket_path: socket_path.clone(),
            writer: Mutex::new(BufWriter::new(UnixStream::pair().unwrap().0)),
            passthrough: Arc::new(Mutex::new(Passthrough::new(24, 80))),
            status: SessionStatus::Starting,
            task_name: "test".to_string(),
            session_id: "test-id".to_string(),
            shepherd_pid: std::process::id(),
            child_pid: None,
            stopping_since: None,
            reader_alive: Arc::new(AtomicBool::new(true)),
        };

        // is_alive takes &mut self
        let mut session = session;
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

        let (session, mut server_reader) = make_test_session(&socket_path);

        session.resize(25, 80).unwrap();

        let msg = protocol::read_message(&mut server_reader).unwrap().unwrap();
        assert_eq!(msg, Message::Resize { rows: 24, cols: 80 });
    }

    #[test]
    fn write_input_ok_after_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&socket_path, "").unwrap();

        let (mut session, server_reader) = make_test_session(&socket_path);

        // Drop the server — subsequent writes hit a broken pipe.
        drop(server_reader);
        std::thread::sleep(std::time::Duration::from_millis(50));

        // write_input should swallow the broken pipe and return Ok.
        assert!(session.write_input(b"hello").is_ok());
    }

    #[test]
    fn resize_ok_after_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&socket_path, "").unwrap();

        let (session, server_reader) = make_test_session(&socket_path);

        drop(server_reader);
        std::thread::sleep(std::time::Duration::from_millis(50));

        // resize should swallow the broken pipe and return Ok.
        assert!(session.resize(25, 80).is_ok());
    }

    #[test]
    fn reader_alive_false_after_server_closes() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&socket_path, "").unwrap();

        let (session, server_reader) = make_test_session(&socket_path);
        assert!(session.reader_alive(), "reader should be alive initially");

        // Drop the server side — the reader thread sees EOF and exits.
        drop(server_reader);
        // Give the reader thread time to detect EOF and set the flag.
        std::thread::sleep(std::time::Duration::from_millis(100));

        assert!(
            !session.reader_alive(),
            "reader should be dead after server closes"
        );
    }

    /// Spawn a `sleep` process in its own process group so tests can
    /// safely kill it (or its group) without affecting the test runner.
    fn spawn_isolated_sleep() -> std::process::Child {
        unsafe {
            std::process::Command::new("sleep")
                .arg("999")
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()
                .unwrap()
        }
    }

    #[test]
    fn connect_rejects_session_id_mismatch() {
        // Test the validation logic directly: simulate a shepherd sending
        // InitialState with a different session_id than expected.
        let (client_stream, server_stream) = UnixStream::pair().unwrap();

        // Shepherd side: send InitialState with wrong session_id.
        let handle = std::thread::spawn(move || {
            let mut server_read = BufReader::new(server_stream.try_clone().unwrap());
            let mut server_write = BufWriter::new(server_stream);
            // Read the Resize the client sends first.
            let _ = protocol::read_message(&mut server_read).unwrap();
            protocol::write_message(
                &mut server_write,
                &Message::InitialState {
                    session_id: "ws/999".to_string(),
                    data: b"restore".to_vec(),
                },
            )
            .unwrap();
        });

        // Client side: replicate the connect() handshake logic inline.
        let read_stream = client_stream.try_clone().unwrap();
        let write_stream = client_stream;
        let writer = Mutex::new(BufWriter::new(write_stream));
        {
            let mut w = writer.lock().unwrap();
            protocol::write_message(&mut *w, &Message::Resize { rows: 23, cols: 80 }).unwrap();
        }

        let mut reader = BufReader::new(read_stream);
        let first_msg = protocol::read_message(&mut reader).unwrap();
        let expected_session_id = "ws/1";

        let is_mismatch = match first_msg {
            Some(Message::InitialState {
                session_id: shepherd_sid,
                ..
            }) => shepherd_sid != expected_session_id,
            _ => panic!("expected InitialState message"),
        };

        handle.join().unwrap();
        assert!(is_mismatch, "should detect session_id mismatch");
    }

    #[test]
    fn force_kill_kills_child_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&socket_path, "").unwrap();

        let mut child = spawn_isolated_sleep();
        let child_pid = child.id();
        let mut shepherd = spawn_isolated_sleep();
        let shepherd_pid = shepherd.id();

        let mut session = ShepherdSession {
            socket_path,
            writer: Mutex::new(BufWriter::new(UnixStream::pair().unwrap().0)),
            passthrough: Arc::new(Mutex::new(Passthrough::new(24, 80))),
            status: SessionStatus::Starting,
            task_name: "test".to_string(),
            session_id: "test-id".to_string(),
            shepherd_pid,
            child_pid: Some(child_pid),
            stopping_since: None,
            reader_alive: Arc::new(AtomicBool::new(true)),
        };

        session.force_kill();

        // try_wait reaps the zombie and confirms the process exited.
        let child_exited = child.wait().unwrap().code().is_none(); // killed by signal → no code
        let shepherd_exited = shepherd.wait().unwrap().code().is_none();
        assert!(child_exited, "child should have been killed by signal");
        assert!(
            shepherd_exited,
            "shepherd should have been killed by signal"
        );
    }

    #[test]
    fn force_kill_without_child_pid_only_kills_shepherd() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&socket_path, "").unwrap();

        let mut shepherd = spawn_isolated_sleep();
        let shepherd_pid = shepherd.id();

        let mut session = ShepherdSession {
            socket_path,
            writer: Mutex::new(BufWriter::new(UnixStream::pair().unwrap().0)),
            passthrough: Arc::new(Mutex::new(Passthrough::new(24, 80))),
            status: SessionStatus::Starting,
            task_name: "test".to_string(),
            session_id: "test-id".to_string(),
            shepherd_pid,
            child_pid: None,
            stopping_since: None,
            reader_alive: Arc::new(AtomicBool::new(true)),
        };

        session.force_kill();

        let shepherd_exited = shepherd.wait().unwrap().code().is_none();
        assert!(
            shepherd_exited,
            "shepherd should have been killed by signal"
        );
    }
}
