use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use kbtz_workspace::protocol::{self, Message};
use kbtz_workspace::{build_restore_sequence, resize_both_screens, SCROLLBACK_ROWS};

/// Non-blocking client connection with message buffering.
///
/// Uses non-blocking reads with an internal buffer to avoid false disconnects.
/// Blocking reads with timeouts are unsuitable here because `poll()` can return
/// spurious readiness (e.g. after macOS sleep/wake cycles), and a subsequent
/// blocking read that times out would be misinterpreted as a client disconnect,
/// dropping the socket and breaking session persistence.
///
/// Partial messages are accumulated across poll iterations, and only true
/// EOF (read returning 0) or real I/O errors cause a disconnect.
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
            return Some(Err(())); // invalid frame
        }
        if self.buf.len() < 4 + length {
            return None; // incomplete frame, need more data
        }
        let frame = self.buf[4..4 + length].to_vec();
        self.buf.drain(..4 + length);
        match protocol::decode(&frame) {
            Ok(msg) => Some(Ok(msg)),
            Err(_) => Some(Err(())),
        }
    }

    /// Write a complete message on the non-blocking socket, polling for
    /// writability on WouldBlock.  Times out after 5 seconds to avoid
    /// indefinite stalls if the workspace stops reading.
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
        "usage: kbtz-shepherd <socket-path> <state-file> <rows> <cols> \
         <task> <agent-type> <session-id> <command> [args...]"
    );
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 9 {
        usage();
    }

    let socket_path = PathBuf::from(&args[1]);
    let state_file = PathBuf::from(&args[2]);
    let rows: u16 = args[3].parse().unwrap_or_else(|_| {
        eprintln!("kbtz-shepherd: invalid rows: {}", args[3]);
        std::process::exit(1);
    });
    let cols: u16 = args[4].parse().unwrap_or_else(|_| {
        eprintln!("kbtz-shepherd: invalid cols: {}", args[4]);
        std::process::exit(1);
    });
    let task = args[5].clone();
    let agent_type = args[6].clone();
    let session_id = args[7].clone();
    let command = &args[8];
    let command_args: Vec<&str> = args[9..].iter().map(|s| s.as_str()).collect();

    kbtz::debug_log::log(&format!(
        "shepherd: starting pid={} socket={} command={command} args={command_args:?} rows={rows} cols={cols}",
        std::process::id(),
        socket_path.display(),
    ));
    if let Err(e) = run(
        &socket_path,
        &state_file,
        rows,
        cols,
        &task,
        &agent_type,
        &session_id,
        command,
        &command_args,
    ) {
        kbtz::debug_log::log(&format!(
            "shepherd: run() failed pid={}: {e:#}",
            std::process::id()
        ));
        cleanup(&socket_path, &state_file);
        std::process::exit(1);
    }
}

fn cleanup(socket_path: &Path, state_file: &Path) {
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(state_file);
}

#[allow(clippy::too_many_arguments)]
fn run(
    socket_path: &Path,
    state_file: &Path,
    rows: u16,
    cols: u16,
    task: &str,
    agent_type: &str,
    session_id: &str,
    command: &str,
    command_args: &[&str],
) -> anyhow::Result<()> {
    // 1. Detach from parent session.
    unsafe {
        if libc::setsid() == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                kbtz::debug_log::log(&format!(
                    "shepherd({}): setsid: EPERM — still in parent session, \
                     child may receive SIGHUP on workspace exit",
                    std::process::id()
                ));
            } else {
                anyhow::bail!("setsid failed: {err:?}");
            }
        }
    }

    // Redirect stdin/stdout/stderr to /dev/null.
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

    // 3. Create PTY and spawn child.
    let pty_system = native_pty_system();
    let pty_size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system
        .openpty(pty_size)
        .map_err(|e| anyhow::anyhow!("openpty: {e}"))?;

    let mut cmd = CommandBuilder::new(command);
    cmd.args(command_args);
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow::anyhow!("spawn: {e}"))?;
    drop(pair.slave);

    let child_pid = child.process_id();
    kbtz::debug_log::log(&format!("shepherd: child spawned, child_pid={child_pid:?}"));

    // 4. Write atomic state file with all session metadata.
    let state = kbtz_workspace::ShepherdState {
        shepherd_pid: std::process::id(),
        child_pid,
        socket_path: socket_path.to_string_lossy().to_string(),
        task: task.to_string(),
        agent_type: agent_type.to_string(),
        session_id: session_id.to_string(),
    };
    state
        .write_atomic(state_file)
        .map_err(|e| anyhow::anyhow!("failed to write state file: {e}"))?;

    let mut pty_writer = pair
        .master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("take_writer: {e}"))?;

    let mut pty_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("try_clone_reader: {e}"))?;

    // Get the PTY master fd for poll. We poll this fd for readability,
    // then read from the cloned reader (which shares the same underlying
    // file description). We keep the fd blocking so that writes to the
    // PTY via the writer don't get partial-write issues from O_NONBLOCK.
    let pty_master_fd = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| anyhow::anyhow!("cannot get PTY master fd"))?;

    // 5. Create Unix socket listener.
    let stale_socket = socket_path.exists();
    if stale_socket {
        kbtz::debug_log::log(&format!(
            "shepherd: removing stale socket at {}",
            socket_path.display()
        ));
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();
    kbtz::debug_log::log(&format!(
        "shepherd: socket created at {} (stale_existed={stale_socket})",
        socket_path.display()
    ));

    // 6. VTE parser with scrollback — this is the authoritative scrollback
    // store, like tmux's server-side pane history.  No raw byte buffer.
    let mut vte = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);

    let mut client: Option<ClientConn> = None;
    let mut shutdown_requested = false;
    let mut read_buf = [0u8; 8192];

    // 7. Main loop.
    loop {
        // Check SIGTERM.
        if SIGTERM_RECEIVED.load(Ordering::SeqCst) && !shutdown_requested {
            shutdown_requested = true;
            forward_sigterm(child_pid);
        }

        // Check if child has exited.
        match child.try_wait() {
            Ok(Some(status)) => {
                kbtz::debug_log::log(&format!(
                    "shepherd: child exited in main loop, exit_code={:?} pid={}",
                    status.exit_code(),
                    std::process::id()
                ));
                // Child exited. Set PTY reader non-blocking so drain_pty
                // can't hang waiting for data that will never arrive.
                unsafe {
                    let flags = libc::fcntl(pty_master_fd, libc::F_GETFL);
                    libc::fcntl(pty_master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                drain_pty(&mut pty_reader, &mut vte, &mut client);
                cleanup(socket_path, state_file);
                // Exit with the child's exit code so the workspace can
                // detect failed sessions via waitpid on the shepherd.
                std::process::exit(status.exit_code() as i32);
            }
            Ok(None) => {} // still running
            Err(e) => {
                kbtz::debug_log::log(&format!(
                    "shepherd: try_wait error pid={}: {e}",
                    std::process::id()
                ));
                // Error checking child status -- treat as exited.
                cleanup(socket_path, state_file);
                return Ok(());
            }
        }

        // Build pollfd array.
        let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(3);

        // Index 0: PTY master
        pollfds.push(libc::pollfd {
            fd: pty_master_fd,
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
            // Unexpected poll error.
            cleanup(socket_path, state_file);
            return Err(err.into());
        }

        // Handle PTY read.
        if pollfds[0].revents & libc::POLLIN != 0 {
            match pty_reader.read(&mut read_buf) {
                Ok(0) => {
                    // PTY EOF -- child closed.
                }
                Ok(n) => {
                    let data = &read_buf[..n];
                    vte.process(data);

                    // CSI 3 J (Erase Saved Lines) — the vt100 crate
                    // doesn't implement this, so clear scrollback
                    // manually to stay consistent with the workspace.
                    if data.windows(4).any(|w| w == b"\x1b[3J") {
                        clear_scrollback(&mut vte);
                    }

                    if let Some(ref mut cc) = client {
                        if cc
                            .write_message(&Message::PtyOutput(data.to_vec()))
                            .is_err()
                        {
                            client = None;
                        }
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    // Read error on PTY (likely child exited).
                }
            }
        }

        // Handle new connection on listener.
        if pollfds[1].revents & libc::POLLIN != 0 {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    kbtz::debug_log::log(&format!(
                        "shepherd: accepted connection pid={}",
                        std::process::id()
                    ));
                    // Close existing client if any.
                    client = None;

                    // The handshake uses blocking I/O (read_exact,
                    // write_all).  On BSD/macOS, accepted sockets inherit
                    // O_NONBLOCK from the listener — explicitly switch to
                    // blocking mode.  After the handshake, ClientConn::new
                    // switches back to non-blocking for the main loop.
                    let mut handshake_stream = stream;
                    let _ = handshake_stream.set_nonblocking(false);
                    let _ =
                        handshake_stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

                    match protocol::read_message(&mut handshake_stream) {
                        Ok(Some(Message::Resize {
                            rows: new_rows,
                            cols: new_cols,
                        })) => {
                            kbtz::debug_log::log(&format!(
                                "shepherd: received Resize({new_rows}x{new_cols}) pid={}",
                                std::process::id()
                            ));
                            // Resize VTE and PTY to match the workspace's terminal.
                            let _ = pair.master.resize(PtySize {
                                rows: new_rows,
                                cols: new_cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                            resize_both_screens(&mut vte, new_rows, new_cols);

                            // Check if the child has already exited before
                            // completing the handshake. Without this, a child
                            // that crashes during init would still get a
                            // successful handshake, causing the workspace to
                            // persist a session UUID for a dead conversation.
                            //
                            // Note: try_wait() reaps the exit status. We store
                            // it so the main loop's exit handler can use it
                            // instead of calling try_wait() again.
                            if let Ok(Some(status)) = child.try_wait() {
                                kbtz::debug_log::log(&format!(
                                    "shepherd: child dead during handshake, exit_code={:?} pid={}",
                                    status.exit_code(),
                                    std::process::id()
                                ));
                                // Child is dead — drop the connection
                                // without sending InitialState, then
                                // clean up and exit.
                                drop(handshake_stream);
                                unsafe {
                                    let flags = libc::fcntl(pty_master_fd, libc::F_GETFL);
                                    libc::fcntl(
                                        pty_master_fd,
                                        libc::F_SETFL,
                                        flags | libc::O_NONBLOCK,
                                    );
                                }
                                drain_pty(&mut pty_reader, &mut vte, &mut client);
                                cleanup(socket_path, state_file);
                                std::process::exit(status.exit_code() as i32);
                            }

                            let restore = build_restore_sequence(&mut vte);
                            kbtz::debug_log::log(&format!(
                                "shepherd: sending InitialState ({} bytes) pid={}",
                                restore.len(),
                                std::process::id()
                            ));
                            if let Err(e) = protocol::write_message(
                                &mut handshake_stream,
                                &Message::InitialState(restore),
                            ) {
                                kbtz::debug_log::log(&format!(
                                    "shepherd: failed to send InitialState pid={}: {e:#}",
                                    std::process::id()
                                ));
                                // Failed to send; drop connection.
                            } else {
                                // Handshake complete — switch to non-blocking.
                                if let Ok(cc) = ClientConn::new(handshake_stream) {
                                    kbtz::debug_log::log(&format!(
                                        "shepherd: handshake complete pid={}",
                                        std::process::id()
                                    ));
                                    client = Some(cc);
                                }
                            }
                        }
                        Ok(Some(other)) => {
                            kbtz::debug_log::log(&format!(
                                "shepherd: expected Resize, got {:?} pid={}",
                                std::mem::discriminant(&other),
                                std::process::id()
                            ));
                        }
                        Ok(None) => {
                            kbtz::debug_log::log(&format!(
                                "shepherd: client disconnected before sending Resize pid={}",
                                std::process::id()
                            ));
                        }
                        Err(e) => {
                            kbtz::debug_log::log(&format!(
                                "shepherd: error reading handshake: {e:#} pid={}",
                                std::process::id()
                            ));
                        }
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        }

        // Handle client read (non-blocking with buffering).
        if let Some(idx) = client_poll_idx {
            if pollfds[idx].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let mut disconnect = false;

                if let Some(ref mut cc) = client {
                    let alive = cc.fill_buf();

                    // Process all complete messages in the buffer.
                    loop {
                        match cc.try_parse() {
                            Some(Ok(msg)) => match msg {
                                Message::PtyInput(data) => {
                                    let _ = pty_writer.write_all(&data);
                                    let _ = pty_writer.flush();
                                }
                                Message::Resize {
                                    rows: new_rows,
                                    cols: new_cols,
                                } => {
                                    let _ = pair.master.resize(PtySize {
                                        rows: new_rows,
                                        cols: new_cols,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    });
                                    resize_both_screens(&mut vte, new_rows, new_cols);
                                }
                                Message::Shutdown => {
                                    shutdown_requested = true;
                                    forward_sigterm(child_pid);
                                }
                                _ => {}
                            },
                            Some(Err(())) => {
                                // Corrupt frame — disconnect.
                                disconnect = true;
                                break;
                            }
                            None => break, // no complete message yet
                        }
                    }

                    if !alive && !disconnect {
                        // EOF or real I/O error — client is gone.
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

/// Clear scrollback by replacing the VTE with a fresh one that has only
/// the visible screen state.  Mirrors `Passthrough::clear_scrollback()`
/// in the workspace — both must handle CSI 3 J identically so the
/// shepherd's authoritative scrollback matches the workspace's view.
fn clear_scrollback(vte: &mut vt100::Parser) {
    let (rows, cols) = vte.screen().size();
    let was_alt = vte.screen().alternate_screen();

    let mut alt_state = None;
    if was_alt {
        alt_state = Some(vte.screen().state_formatted());
        vte.process(b"\x1b[?47l");
    }
    let main_state = vte.screen().state_formatted();
    if was_alt {
        vte.process(b"\x1b[?47h");
    }

    let mut fresh = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
    fresh.process(&main_state);
    if let Some(alt) = alt_state {
        fresh.process(b"\x1b[?47h");
        fresh.process(&alt);
    }

    *vte = fresh;
}

fn forward_sigterm(child_pid: Option<u32>) {
    if let Some(pid) = child_pid {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── try_parse tests ──────────────────────────────────────────────

    /// Helper: create a ClientConn with a dummy socket (only used for
    /// try_parse which operates on the internal buffer, not the socket).
    fn conn_with_buf(buf: Vec<u8>) -> ClientConn {
        let (s, _) = UnixStream::pair().unwrap();
        ClientConn { stream: s, buf }
    }

    #[test]
    fn try_parse_empty_buffer() {
        let mut cc = conn_with_buf(vec![]);
        assert!(cc.try_parse().is_none());
    }

    #[test]
    fn try_parse_incomplete_length_prefix() {
        let mut cc = conn_with_buf(vec![0, 0]);
        assert!(cc.try_parse().is_none());
        // Buffer is not consumed.
        assert_eq!(cc.buf.len(), 2);
    }

    #[test]
    fn try_parse_zero_length_frame_is_error() {
        let mut cc = conn_with_buf(vec![0, 0, 0, 0]);
        assert!(matches!(cc.try_parse(), Some(Err(()))));
    }

    #[test]
    fn try_parse_incomplete_body() {
        // Length says 10 bytes, but only 2 bytes of body present.
        let mut cc = conn_with_buf(vec![0, 0, 0, 10, 0x01, 0x02]);
        assert!(cc.try_parse().is_none());
        // Buffer not consumed — waiting for more data.
        assert_eq!(cc.buf.len(), 6);
    }

    #[test]
    fn try_parse_complete_message() {
        let msg = Message::PtyInput(b"hello".to_vec());
        let encoded = protocol::encode(&msg);
        let mut cc = conn_with_buf(encoded);
        let parsed = cc.try_parse().unwrap().unwrap();
        assert_eq!(parsed, msg);
        assert!(cc.buf.is_empty());
    }

    #[test]
    fn try_parse_two_messages_back_to_back() {
        let msg1 = Message::PtyInput(b"one".to_vec());
        let msg2 = Message::Resize { rows: 24, cols: 80 };
        let mut buf = protocol::encode(&msg1);
        buf.extend_from_slice(&protocol::encode(&msg2));
        let mut cc = conn_with_buf(buf);

        let parsed1 = cc.try_parse().unwrap().unwrap();
        assert_eq!(parsed1, msg1);

        let parsed2 = cc.try_parse().unwrap().unwrap();
        assert_eq!(parsed2, msg2);

        assert!(cc.try_parse().is_none());
        assert!(cc.buf.is_empty());
    }

    #[test]
    fn try_parse_message_plus_partial() {
        let msg = Message::Shutdown;
        let mut buf = protocol::encode(&msg);
        // Append a partial second message (just the length prefix).
        buf.extend_from_slice(&[0, 0, 0, 5]);
        let mut cc = conn_with_buf(buf);

        let parsed = cc.try_parse().unwrap().unwrap();
        assert_eq!(parsed, Message::Shutdown);

        // Second parse returns None — incomplete.
        assert!(cc.try_parse().is_none());
        assert_eq!(cc.buf.len(), 4);
    }

    // ── fill_buf + try_parse integration tests ───────────────────────

    #[test]
    fn fill_buf_reads_and_parses() {
        let (client, mut server) = UnixStream::pair().unwrap();

        // Write a complete message to the server side.
        let msg = Message::PtyInput(b"test data".to_vec());
        protocol::write_message(&mut server, &msg).unwrap();

        let mut cc = ClientConn::new(client).unwrap();
        let alive = cc.fill_buf();
        assert!(alive);

        let parsed = cc.try_parse().unwrap().unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn fill_buf_eof_when_sender_closes() {
        let (client, server) = UnixStream::pair().unwrap();
        drop(server); // close the write side

        let mut cc = ClientConn::new(client).unwrap();
        let alive = cc.fill_buf();
        assert!(!alive);
    }

    #[test]
    fn fill_buf_no_data_returns_alive() {
        let (client, _server) = UnixStream::pair().unwrap();
        let mut cc = ClientConn::new(client).unwrap();
        // No data written — non-blocking read should return WouldBlock.
        let alive = cc.fill_buf();
        assert!(alive);
        assert!(cc.try_parse().is_none());
    }

    #[test]
    fn write_message_roundtrip() {
        let (client, server) = UnixStream::pair().unwrap();
        let mut cc = ClientConn::new(client).unwrap();

        let msg = Message::PtyOutput(b"output".to_vec());
        cc.write_message(&msg).unwrap();

        let mut reader = std::io::BufReader::new(server);
        let received = protocol::read_message(&mut reader).unwrap().unwrap();
        assert_eq!(received, msg);
    }

    #[test]
    fn write_message_to_closed_peer_fails() {
        let (client, server) = UnixStream::pair().unwrap();
        let mut cc = ClientConn::new(client).unwrap();
        drop(server);

        let msg = Message::PtyOutput(b"data".to_vec());
        assert!(cc.write_message(&msg).is_err());
    }

    #[test]
    fn partial_message_across_two_fills() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let msg = Message::PtyInput(b"split".to_vec());
        let encoded = protocol::encode(&msg);

        // Write only the first 3 bytes (partial length prefix).
        server.write_all(&encoded[..3]).unwrap();
        server.flush().unwrap();

        let mut cc = ClientConn::new(client).unwrap();
        let alive = cc.fill_buf();
        assert!(alive);
        assert!(cc.try_parse().is_none()); // not enough data yet

        // Write the rest.
        server.write_all(&encoded[3..]).unwrap();
        server.flush().unwrap();

        // Small delay to let the kernel deliver the data.
        std::thread::sleep(std::time::Duration::from_millis(10));

        let alive = cc.fill_buf();
        assert!(alive);
        let parsed = cc.try_parse().unwrap().unwrap();
        assert_eq!(parsed, msg);
    }
}

fn drain_pty(
    reader: &mut Box<dyn Read + Send>,
    vte: &mut vt100::Parser,
    client: &mut Option<ClientConn>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let data = &buf[..n];
                vte.process(data);

                if let Some(ref mut cc) = client {
                    if cc
                        .write_message(&Message::PtyOutput(data.to_vec()))
                        .is_err()
                    {
                        *client = None;
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}
