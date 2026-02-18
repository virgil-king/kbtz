use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use kbtz_workspace::protocol::{self, Message};

/// Max raw output we buffer for scrollback replay (same as Passthrough in session.rs).
const OUTPUT_BUFFER_MAX: usize = 16 * 1024 * 1024;

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

    // 6. VTE parser + output buffer.
    let mut vte = vt100::Parser::new(rows, cols, 0);
    let mut output_buffer: Vec<u8> = Vec::new();

    let mut client: Option<UnixStream> = None;
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
                drain_pty(&mut pty_reader, &mut vte, &mut output_buffer, &mut client);
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
        let client_poll_idx = if let Some(ref cs) = client {
            pollfds.push(libc::pollfd {
                fd: cs.as_raw_fd(),
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
                    append_output_buffer(&mut output_buffer, data);

                    if let Some(ref mut cs) = client {
                        if protocol::write_message(cs, &Message::PtyOutput(data.to_vec())).is_err()
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

                    // Send initial state to the new client.
                    let mut new_client = stream;
                    if protocol::write_message(
                        &mut new_client,
                        &Message::InitialState(output_buffer.clone()),
                    )
                    .is_err()
                    {
                        // Failed to send initial state; drop connection.
                    } else {
                        // Set a read timeout so a misbehaving client sending a
                        // partial frame can't stall the main loop indefinitely.
                        let _ =
                            new_client.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                        client = Some(new_client);
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        }

        // Handle client read.
        if let Some(idx) = client_poll_idx {
            if pollfds[idx].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let mut disconnect = false;

                if let Some(ref mut cs) = client {
                    match protocol::read_message(cs) {
                        Ok(Some(msg)) => match msg {
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
                                vte.screen_mut().set_size(new_rows, new_cols);
                            }
                            Message::Shutdown => {
                                shutdown_requested = true;
                                forward_sigterm(child_pid);
                            }
                            _ => {} // Ignore unexpected messages from client.
                        },
                        Ok(None) => {
                            // Clean EOF -- client disconnected.
                            disconnect = true;
                        }
                        Err(_) => {
                            // Read error -- client disconnected.
                            disconnect = true;
                        }
                    }
                }

                if disconnect {
                    client = None;
                }
            }
        }
    }
}

fn append_output_buffer(buffer: &mut Vec<u8>, data: &[u8]) {
    buffer.extend_from_slice(data);
    if buffer.len() > OUTPUT_BUFFER_MAX {
        let keep_from = buffer.len() - OUTPUT_BUFFER_MAX / 2;
        buffer.drain(..keep_from);
        // Terminate any escape sequence that was cut mid-stream.
        // CAN (0x18) aborts CSI sequences; ST (\x1b\\) ends OSC/DCS sequences.
        buffer.splice(0..0, b"\x18\x1b\\".iter().copied());
    }
}

fn forward_sigterm(child_pid: Option<u32>) {
    if let Some(pid) = child_pid {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

fn drain_pty(
    reader: &mut Box<dyn Read + Send>,
    vte: &mut vt100::Parser,
    output_buffer: &mut Vec<u8>,
    client: &mut Option<UnixStream>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let data = &buf[..n];
                vte.process(data);
                append_output_buffer(output_buffer, data);

                if let Some(ref mut cs) = client {
                    if protocol::write_message(cs, &Message::PtyOutput(data.to_vec())).is_err() {
                        *client = None;
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}
