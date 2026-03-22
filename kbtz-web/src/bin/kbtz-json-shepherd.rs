use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use kbtz_web::protocol::{self, AgentEvent, Message, ShepherdState};

/// Non-blocking client connection with message buffering.
struct ClientConn {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl ClientConn {
    fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_read_timeout(None)?;
        Ok(Self {
            stream,
            buf: Vec::new(),
        })
    }

    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.stream.as_raw_fd()
    }

    /// Read available data from the socket into the internal buffer.
    /// Returns `false` on EOF or real error (client gone), `true` otherwise.
    fn fill_buf(&mut self) -> bool {
        let mut tmp = [0u8; 8192];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return false,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
    }

    /// Try to parse one complete message from the buffer.
    fn try_parse(&mut self) -> Option<Result<Message, ()>> {
        if self.buf.len() < 4 {
            return None;
        }
        let length =
            u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if length == 0 {
            return Some(Err(()));
        }
        if self.buf.len() < 4 + length {
            return None;
        }
        let frame = self.buf[4..4 + length].to_vec();
        self.buf.drain(..4 + length);
        match protocol::decode(&frame) {
            Ok(msg) => Some(Ok(msg)),
            Err(_) => Some(Err(())),
        }
    }

    /// Write a complete message, polling for writability on WouldBlock.
    /// Times out after 5 seconds.
    fn write_message(&mut self, msg: &Message) -> anyhow::Result<()> {
        let data = protocol::encode(msg);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut written = 0;
        while written < data.len() {
            match self.stream.write(&data[written..]) {
                Ok(n) => written += n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!("write timed out");
                    }
                    let mut pfd = libc::pollfd {
                        fd: self.stream.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut pfd, 1, 100) };
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::SeqCst);
}

fn usage() -> ! {
    eprintln!(
        "usage: kbtz-json-shepherd <socket-path> <state-file> <session-id> \
         <event-cap> [--ready-fd N] <command> [args...]"
    );
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        usage();
    }

    let socket_path = PathBuf::from(&args[1]);
    let state_path = PathBuf::from(&args[2]);
    let session_id = args[3].clone();
    let event_cap: usize = args[4].parse().unwrap_or_else(|_| {
        eprintln!("kbtz-json-shepherd: invalid event-cap: {}", args[4]);
        std::process::exit(1);
    });

    // Parse optional --ready-fd N before the command
    let mut idx = 5;
    let mut ready_fd: Option<i32> = None;
    if idx < args.len() && args[idx] == "--ready-fd" {
        idx += 1;
        if idx >= args.len() {
            usage();
        }
        ready_fd = Some(args[idx].parse().unwrap_or_else(|_| {
            eprintln!(
                "kbtz-json-shepherd: invalid --ready-fd value: {}",
                args[idx]
            );
            std::process::exit(1);
        }));
        idx += 1;
    }

    if idx >= args.len() {
        usage();
    }
    let command = &args[idx];
    let command_args: Vec<&str> = args[idx + 1..].iter().map(|s| s.as_str()).collect();

    kbtz::debug_log::log(&format!(
        "json-shepherd: starting pid={} socket={} session={session_id} cap={event_cap} \
         command={command} args={command_args:?}",
        std::process::id(),
        socket_path.display(),
    ));

    if let Err(e) = run(
        &socket_path,
        &state_path,
        &session_id,
        event_cap,
        ready_fd,
        command,
        &command_args,
    ) {
        kbtz::debug_log::log(&format!(
            "json-shepherd: run() failed pid={}: {e:#}",
            std::process::id()
        ));
        cleanup(&socket_path, &state_path);
        std::process::exit(1);
    }
}

fn cleanup(socket_path: &Path, state_path: &Path) {
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(state_path);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn run(
    socket_path: &Path,
    state_path: &Path,
    session_id: &str,
    event_cap: usize,
    ready_fd: Option<i32>,
    command: &str,
    command_args: &[&str],
) -> anyhow::Result<()> {
    // 1. Detach from parent session.
    unsafe {
        if libc::setsid() == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                kbtz::debug_log::log(&format!(
                    "json-shepherd({}): setsid: EPERM — still in parent session",
                    std::process::id()
                ));
            } else {
                anyhow::bail!("setsid failed: {err:?}");
            }
        }
    }

    // Redirect stdin/stdout/stderr to /dev/null (shepherd is a daemon).
    let dev_null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let null_fd = dev_null.as_raw_fd();
    unsafe {
        libc::dup2(null_fd, libc::STDIN_FILENO);
        libc::dup2(null_fd, libc::STDOUT_FILENO);
        libc::dup2(null_fd, libc::STDERR_FILENO);
    }
    drop(dev_null);

    // 2. Install SIGTERM handler.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigterm_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }

    // 3. Spawn agent child with piped stdin/stdout, stderr to /dev/null.
    let mut child = Command::new(command)
        .args(command_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {command}: {e}"))?;

    let child_pid = child.id();
    kbtz::debug_log::log(&format!(
        "json-shepherd: child spawned, child_pid={child_pid}"
    ));

    let child_stdin = child.stdin.take().expect("stdin was piped");
    let child_stdout = child.stdout.take().expect("stdout was piped");

    // Set stdout to non-blocking for poll-based reading.
    let stdout_fd = child_stdout.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(stdout_fd, libc::F_GETFL);
        libc::fcntl(stdout_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // 4. Create Unix socket listener.
    if socket_path.exists() {
        kbtz::debug_log::log(&format!(
            "json-shepherd: removing stale socket at {}",
            socket_path.display()
        ));
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)
        .map_err(|e| anyhow::anyhow!("bind {}: {e}", socket_path.display()))?;
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();

    // 5. Write initial state file.
    let mut state = ShepherdState {
        shepherd_pid: std::process::id(),
        child_pid: Some(child_pid),
        session_id: session_id.to_string(),
        event_count: 0,
        last_seq: 0,
    };
    state.write_state_file(state_path)?;

    // 6. Signal readiness via pipe fd.
    if let Some(fd) = ready_fd {
        let mut pipe = unsafe { std::fs::File::from_raw_fd(fd) };
        let _ = pipe.write_all(&[0x01]);
        drop(pipe); // close the write end
        kbtz::debug_log::log("json-shepherd: signaled readiness on pipe");
    }

    // 7. Event ring buffer.
    let mut events: VecDeque<AgentEvent> = VecDeque::new();
    let mut next_seq: u64 = 1;

    // BufReader for line-based reading from child stdout.
    let mut stdout_reader = BufReader::new(child_stdout);
    let mut line_buf = String::new();

    let mut child_stdin = child_stdin;
    let mut client: Option<ClientConn> = None;
    let mut shutdown_requested = false;

    // 8. Main loop.
    loop {
        // Check SIGTERM.
        if SIGTERM_RECEIVED.load(Ordering::SeqCst) && !shutdown_requested {
            shutdown_requested = true;
            forward_sigterm(child_pid);
        }

        // Check if child has exited.
        if let Some(exit_code) = check_child_exit(&mut child) {
            // Drain remaining stdout.
            drain_stdout(
                &mut stdout_reader,
                &mut line_buf,
                &mut events,
                &mut next_seq,
                event_cap,
                &mut client,
            );
            update_state(&mut state, &events, next_seq, state_path);
            cleanup(socket_path, state_path);
            kbtz::debug_log::log(&format!(
                "json-shepherd: child exited, code={exit_code} pid={}",
                std::process::id()
            ));
            std::process::exit(exit_code);
        }

        // Build pollfd array.
        let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(3);

        // Index 0: child stdout
        pollfds.push(libc::pollfd {
            fd: stdout_fd,
            events: libc::POLLIN,
            revents: 0,
        });

        // Index 1: listener socket
        pollfds.push(libc::pollfd {
            fd: listener_fd,
            events: libc::POLLIN,
            revents: 0,
        });

        // Index 2 (optional): client socket
        let client_poll_idx = if let Some(ref cc) = client {
            pollfds.push(libc::pollfd {
                fd: cc.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            Some(pollfds.len() - 1)
        } else {
            None
        };

        let nready =
            unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 100) };
        if nready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            cleanup(socket_path, state_path);
            return Err(err.into());
        }

        // Handle child stdout (JSON lines).
        if pollfds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            read_stdout_lines(
                &mut stdout_reader,
                &mut line_buf,
                &mut events,
                &mut next_seq,
                event_cap,
                &mut client,
            );
            update_state(&mut state, &events, next_seq, state_path);
        }

        // Handle new connection on listener.
        if pollfds[1].revents & libc::POLLIN != 0 {
            if let Ok((stream, _addr)) = listener.accept() {
                kbtz::debug_log::log(&format!(
                    "json-shepherd: accepted connection pid={}",
                    std::process::id()
                ));
                client = None;

                // Switch to blocking for handshake.
                let mut handshake_stream = stream;
                let _ = handshake_stream.set_nonblocking(false);
                let _ = handshake_stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

                // Send event history as EventBatch.
                let batch = Message::EventBatch {
                    events: events.iter().cloned().collect(),
                };
                if let Err(e) = protocol::write_message(&mut handshake_stream, &batch) {
                    kbtz::debug_log::log(&format!(
                        "json-shepherd: failed to send EventBatch: {e:#}"
                    ));
                } else if let Ok(cc) = ClientConn::new(handshake_stream) {
                    kbtz::debug_log::log(&format!(
                        "json-shepherd: handshake complete, sent {} events",
                        events.len()
                    ));
                    client = Some(cc);
                }
            }
        }

        // Handle client read.
        if let Some(idx) = client_poll_idx {
            if pollfds[idx].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let mut disconnect = false;

                if let Some(ref mut cc) = client {
                    let alive = cc.fill_buf();

                    loop {
                        match cc.try_parse() {
                            Some(Ok(msg)) => match msg {
                                Message::Input { data } => {
                                    let _ = child_stdin.write_all(data.as_bytes());
                                    let _ = child_stdin.flush();
                                }
                                Message::Shutdown => {
                                    shutdown_requested = true;
                                    forward_sigterm(child_pid);
                                }
                                _ => {}
                            },
                            Some(Err(())) => {
                                disconnect = true;
                                break;
                            }
                            None => break,
                        }
                    }

                    if !alive && !disconnect {
                        disconnect = true;
                    }
                }

                if disconnect {
                    client = None;
                }
            }
        }
    }
}

/// Process a single JSON line into an event, send to client, and add to
/// ring buffer. Shared by both `read_stdout_lines` and `drain_stdout`.
fn process_line(
    line: &str,
    events: &mut VecDeque<AgentEvent>,
    next_seq: &mut u64,
    cap: usize,
    client: &mut Option<ClientConn>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    // Parse as JSON; if it's not valid JSON, wrap as a string.
    let data: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|_| serde_json::Value::String(trimmed.to_string()));

    let event = AgentEvent {
        seq: *next_seq,
        timestamp: now_secs(),
        data,
    };
    *next_seq += 1;

    if let Some(ref mut cc) = client {
        let msg = Message::Event {
            event: event.clone(),
        };
        if cc.write_message(&msg).is_err() {
            *client = None;
        }
    }

    events.push_back(event);
    if cap > 0 && events.len() > cap {
        events.pop_front();
    }
}

/// Read available JSON lines from the child's stdout (non-blocking).
fn read_stdout_lines(
    reader: &mut BufReader<std::process::ChildStdout>,
    line_buf: &mut String,
    events: &mut VecDeque<AgentEvent>,
    next_seq: &mut u64,
    cap: usize,
    client: &mut Option<ClientConn>,
) {
    loop {
        line_buf.clear();
        match reader.read_line(line_buf) {
            Ok(0) => break,
            Ok(_) => process_line(line_buf, events, next_seq, cap, client),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Drain remaining stdout after child exit (blocking read until EOF).
fn drain_stdout(
    reader: &mut BufReader<std::process::ChildStdout>,
    line_buf: &mut String,
    events: &mut VecDeque<AgentEvent>,
    next_seq: &mut u64,
    cap: usize,
    client: &mut Option<ClientConn>,
) {
    let fd = reader.get_ref().as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }
    loop {
        line_buf.clear();
        match reader.read_line(line_buf) {
            Ok(0) => break,
            Ok(_) => process_line(line_buf, events, next_seq, cap, client),
            Err(_) => break,
        }
    }
}

fn update_state(
    state: &mut ShepherdState,
    events: &VecDeque<AgentEvent>,
    next_seq: u64,
    state_path: &Path,
) {
    state.event_count = events.len() as u64;
    state.last_seq = next_seq.saturating_sub(1);
    if let Err(e) = state.write_state_file(state_path) {
        kbtz::debug_log::log(&format!("json-shepherd: failed to write state: {e:#}"));
    }
}

fn check_child_exit(child: &mut Child) -> Option<i32> {
    match child.try_wait() {
        Ok(Some(status)) => Some(status.code().unwrap_or(1)),
        Ok(None) => None,
        Err(e) => {
            kbtz::debug_log::log(&format!(
                "json-shepherd: try_wait error pid={}: {e}",
                std::process::id()
            ));
            Some(1)
        }
    }
}

fn forward_sigterm(child_pid: u32) {
    unsafe {
        libc::kill(child_pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_conn_try_parse_empty() {
        let (s, _) = UnixStream::pair().unwrap();
        let mut cc = ClientConn {
            stream: s,
            buf: vec![],
        };
        assert!(cc.try_parse().is_none());
    }

    #[test]
    fn client_conn_try_parse_complete() {
        let msg = Message::Input {
            data: "hello".into(),
        };
        let encoded = protocol::encode(&msg);
        let (s, _) = UnixStream::pair().unwrap();
        let mut cc = ClientConn {
            stream: s,
            buf: encoded,
        };
        let parsed = cc.try_parse().unwrap().unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn client_conn_write_roundtrip() {
        let (client, server) = UnixStream::pair().unwrap();
        let mut cc = ClientConn::new(client).unwrap();

        let msg = Message::Event {
            event: AgentEvent {
                seq: 1,
                timestamp: 100,
                data: serde_json::json!({"type": "test"}),
            },
        };
        cc.write_message(&msg).unwrap();

        let mut reader = io::BufReader::new(server);
        let received = protocol::read_message(&mut reader).unwrap().unwrap();
        assert_eq!(received, msg);
    }

    #[test]
    fn zero_length_frame_is_error() {
        let (s, _) = UnixStream::pair().unwrap();
        let mut cc = ClientConn {
            stream: s,
            buf: vec![0, 0, 0, 0],
        };
        assert!(matches!(cc.try_parse(), Some(Err(()))));
    }
}
