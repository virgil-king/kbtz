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
/// The previous implementation used blocking `read_exact()` with a 5-second
/// timeout, which caused false disconnects on macOS: spurious `poll()` wakeups
/// (e.g. after sleep/wake) triggered a blocking read that timed out, and the
/// timeout error was treated as a client disconnect. This dropped the socket
/// and caused the workspace's reader thread to see EOF, breaking session
/// persistence.
///
/// This implementation uses non-blocking reads with an internal buffer.
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

    fn write_message(&mut self, msg: &Message) -> anyhow::Result<()> {
        protocol::write_message(&mut self.stream, msg)
    }
}

static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::SeqCst);
}

fn usage() -> ! {
    eprintln!("usage: kbtz-shepherd <socket-path> <pid-file> <rows> <cols> <command> [args...]");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        usage();
    }

    let socket_path = PathBuf::from(&args[1]);
    let pid_file = PathBuf::from(&args[2]);
    let rows: u16 = args[3].parse().unwrap_or_else(|_| {
        eprintln!("kbtz-shepherd: invalid rows: {}", args[3]);
        std::process::exit(1);
    });
    let cols: u16 = args[4].parse().unwrap_or_else(|_| {
        eprintln!("kbtz-shepherd: invalid cols: {}", args[4]);
        std::process::exit(1);
    });
    let command = &args[5];
    let command_args: Vec<&str> = args[6..].iter().map(|s| s.as_str()).collect();

    if let Err(e) = run(&socket_path, &pid_file, rows, cols, command, &command_args) {
        eprintln!("kbtz-shepherd: {e:#}");
        cleanup(&socket_path, &pid_file);
        std::process::exit(1);
    }
}

fn cleanup(socket_path: &Path, pid_file: &Path) {
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(pid_file);
}

fn run(
    socket_path: &Path,
    pid_file: &Path,
    rows: u16,
    cols: u16,
    command: &str,
    command_args: &[&str],
) -> anyhow::Result<()> {
    // 1. Detach from parent session.
    unsafe {
        if libc::setsid() == -1 {
            anyhow::bail!("setsid failed: {:?}", io::Error::last_os_error());
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

    // 2. Write PID file.
    std::fs::write(pid_file, format!("{}", std::process::id()))?;

    // 3. Install SIGTERM handler.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigterm_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }

    // 4. Create PTY and spawn child.
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
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();

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
            Ok(Some(_status)) => {
                // Child exited. Set PTY reader non-blocking so drain_pty
                // can't hang waiting for data that will never arrive.
                unsafe {
                    let flags = libc::fcntl(pty_master_fd, libc::F_GETFL);
                    libc::fcntl(pty_master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                drain_pty(&mut pty_reader, &mut vte, &mut client);
                cleanup(socket_path, pid_file);
                return Ok(());
            }
            Ok(None) => {} // still running
            Err(_) => {
                // Error checking child status -- treat as exited.
                cleanup(socket_path, pid_file);
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
            cleanup(socket_path, pid_file);
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
                    // Close existing client if any.
                    client = None;

                    // Use a blocking read timeout for the handshake only.
                    // The handshake is a single Resize→InitialState exchange
                    // that must complete before entering the non-blocking
                    // main loop. 5 seconds is generous for a local socket.
                    let mut handshake_stream = stream;
                    let _ =
                        handshake_stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

                    match protocol::read_message(&mut handshake_stream) {
                        Ok(Some(Message::Resize {
                            rows: new_rows,
                            cols: new_cols,
                        })) => {
                            // Resize VTE and PTY to match the workspace's terminal.
                            let _ = pair.master.resize(PtySize {
                                rows: new_rows,
                                cols: new_cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                            resize_both_screens(&mut vte, new_rows, new_cols);

                            let restore = build_restore_sequence(&mut vte);
                            if protocol::write_message(
                                &mut handshake_stream,
                                &Message::InitialState(restore),
                            )
                            .is_err()
                            {
                                // Failed to send; drop connection.
                            } else {
                                // Handshake complete — switch to non-blocking.
                                if let Ok(cc) = ClientConn::new(handshake_stream) {
                                    client = Some(cc);
                                }
                            }
                        }
                        _ => {
                            // Client didn't send Resize first — drop it.
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
