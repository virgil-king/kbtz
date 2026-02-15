use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

pub struct Session {
    pub master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub passthrough: Arc<Mutex<Passthrough>>,
    pub status: SessionStatus,
    pub task_name: String,
    pub session_id: String,
    /// Set when we've sent `/exit` and are waiting for the process to stop.
    pub stopping_since: Option<Instant>,
}

/// Shared state between the reader thread and the main thread.
///
/// Holds a virtual terminal emulator (`vt100::Parser`) that receives
/// every byte the child writes. When `active` is true the reader
/// thread also forwards those bytes to stdout.  On zoom-in the main
/// thread renders the VTE screen to stdout then sets `active`.
pub struct Passthrough {
    active: bool,
    vte: vt100::Parser,
    bytes_processed: usize,
    last_output: Option<Instant>,
}

impl Passthrough {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self {
            active: false,
            vte: vt100::Parser::new(rows, cols, 0),
            bytes_processed: 0,
            last_output: None,
        }
    }

    /// Switch to passthrough mode.  Render the VTE's current screen
    /// state to stdout so the user sees the child's UI immediately,
    /// then set `active` so the reader thread starts forwarding live
    /// output.  Both happen under the same Mutex guard.
    fn start(&mut self) {
        let screen = self.vte.screen();
        let contents = screen.contents_formatted();
        let plain = screen.contents();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let hide = screen.hide_cursor();
        let alt = screen.alternate_screen();
        let (rows, cols) = screen.size();

        // Debug dump
        if let Ok(mut f) = std::fs::File::create("/tmp/kbtz-vte-debug.txt") {
            let _ = write!(f, "VTE size: {}x{}\n", rows, cols);
            let _ = write!(f, "alternate_screen: {}\n", alt);
            let _ = write!(f, "cursor: ({}, {})\n", cursor_row, cursor_col);
            let _ = write!(f, "hide_cursor: {}\n", hide);
            let _ = write!(f, "contents_formatted len: {}\n", contents.len());
            let _ = write!(f, "plain text len: {}\n", plain.len());
            let _ = write!(f, "bytes_processed: {}\n", self.bytes_processed);
            let _ = write!(f, "--- plain text ---\n{}\n", plain);
            // Hex dump of first 512 bytes of contents_formatted
            let _ = write!(f, "--- contents_formatted hex (first 512) ---\n");
            for (i, chunk) in contents[..contents.len().min(512)].chunks(32).enumerate() {
                let hex: String = chunk.iter().map(|b| format!("{:02x} ", b)).collect();
                let ascii: String = chunk
                    .iter()
                    .map(|b| if b.is_ascii_graphic() || *b == b' ' { *b as char } else { '.' })
                    .collect();
                let _ = write!(f, "{:04x}: {}  {}\n", i * 32, hex, ascii);
            }
        }

        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        // Reproduce the screen contents (cells + attributes).
        let _ = out.write_all(&contents);
        // Restore cursor position.
        let _ = write!(out, "\x1b[{};{}H", cursor_row + 1, cursor_col + 1);
        // Restore cursor visibility.
        if hide {
            let _ = out.write_all(b"\x1b[?25l");
        } else {
            let _ = out.write_all(b"\x1b[?25h");
        }
        let _ = out.flush();

        self.active = true;
    }

    fn stop(&mut self) {
        self.active = false;
    }

    fn process(&mut self, data: &[u8]) {
        self.bytes_processed += data.len();
        self.last_output = Some(Instant::now());
        self.vte.process(data);
    }

    fn set_size(&mut self, rows: u16, cols: u16) {
        self.vte.screen_mut().set_size(rows, cols);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Starting,
    Active,
    Idle,
    NeedsInput,
}

impl SessionStatus {
    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "active" => Self::Active,
            "idle" => Self::Idle,
            "needs_input" => Self::NeedsInput,
            _ => Self::Starting,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Idle => "idle",
            Self::NeedsInput => "needs input",
        }
    }

    pub fn indicator(&self) -> &'static str {
        match self {
            Self::Starting => "\u{23f3}",   // ⏳
            Self::Active => "\u{1f7e2}",    // 🟢
            Self::Idle => "\u{1f7e1}",      // 🟡
            Self::NeedsInput => "\u{1f514}", // 🔔
        }
    }
}

impl Session {
    pub fn spawn(
        command: &str,
        args: &[&str],
        task_name: &str,
        session_id: &str,
        rows: u16,
        cols: u16,
        env_vars: &[(&str, &str)],
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pty_rows = rows.saturating_sub(1); // leave room for status bar
        let pty_size = PtySize {
            rows: pty_rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(pty_size)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        cmd.cwd(std::env::current_dir().context("failed to get current directory")?);
        for (k, v) in env_vars {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        drop(pair.slave);

        let passthrough = Arc::new(Mutex::new(Passthrough::new(pty_rows, cols)));
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let pt = Arc::clone(&passthrough);
        std::thread::spawn(move || reader_thread(reader, pt));

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(Session {
            master: pair.master,
            writer,
            child,
            passthrough,
            status: SessionStatus::Starting,
            task_name: task_name.to_string(),
            session_id: session_id.to_string(),
            stopping_since: None,
        })
    }

    /// Enable passthrough, rendering the VTE screen to stdout first.
    pub fn start_passthrough(&self) {
        self.passthrough.lock().unwrap().start();
    }

    pub fn stop_passthrough(&self) {
        self.passthrough.lock().unwrap().stop();
    }

    pub fn write_input(&mut self, buf: &[u8]) -> Result<()> {
        self.writer.write_all(buf).context("write to PTY")?;
        self.writer.flush().context("flush PTY")?;
        Ok(())
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Send SIGTERM to ask the child process to shut down cleanly.
    /// Sets `stopping_since` so the lifecycle tick can force-kill after a timeout.
    pub fn request_exit(&mut self) {
        if self.stopping_since.is_some() {
            return; // already requested
        }
        if let Some(pid) = self.child.process_id() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        self.stopping_since = Some(Instant::now());
    }

    /// Force-kill the process immediately (SIGKILL).
    pub fn force_kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let pty_rows = rows.saturating_sub(1);
        self.passthrough.lock().unwrap().set_size(pty_rows, cols);
        self.master
            .resize(PtySize {
                rows: pty_rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("resize PTY: {e}"))
    }
}

fn reader_thread(mut reader: Box<dyn Read + Send>, passthrough: Arc<Mutex<Passthrough>>) {
    let mut buf = [0u8; 4096];
    let stdout = std::io::stdout();
    let mut total_bytes: usize = 0;
    let mut read_count: usize = 0;
    let mut last_alt_screen = false;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/kbtz-reader-debug.log");
    let mut log_file = log.ok();

    macro_rules! dbg_log {
        ($($arg:tt)*) => {
            if let Some(ref mut f) = log_file {
                let _ = writeln!(f, $($arg)*);
                let _ = f.flush();
            }
        };
    }

    dbg_log!("=== reader thread started ===");

    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => {
                dbg_log!("reader exiting after {read_count} reads, {total_bytes} total bytes");
                break;
            }
            Ok(n) => {
                read_count += 1;
                total_bytes += n;

                let mut pt = passthrough.lock().unwrap();
                pt.process(&buf[..n]);

                // Log first 10 reads with raw byte snippets
                if read_count <= 10 {
                    let snippet: String = buf[..n.min(120)]
                        .iter()
                        .map(|b| {
                            if b.is_ascii_graphic() || *b == b' ' {
                                *b as char
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    let alt = pt.vte.screen().alternate_screen();
                    dbg_log!(
                        "read #{read_count}: {n} bytes (total {total_bytes}), alt_screen={alt}, snippet=[{snippet}]"
                    );
                }

                // Log alternate screen transitions
                let alt = pt.vte.screen().alternate_screen();
                if alt != last_alt_screen {
                    dbg_log!(
                        "ALT SCREEN CHANGED: {last_alt_screen} -> {alt} at read #{read_count}, {total_bytes} total bytes"
                    );
                    // Dump screen contents at transition
                    let plain = pt.vte.screen().contents();
                    let lines: Vec<&str> = plain.lines().take(5).collect();
                    dbg_log!("  screen first 5 lines: {:?}", lines);
                    last_alt_screen = alt;
                }

                if pt.active {
                    let mut out = stdout.lock();
                    let _ = out.write_all(&buf[..n]);
                    let _ = out.flush();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_from_str_known_values() {
        assert_eq!(SessionStatus::from_str("active"), SessionStatus::Active);
        assert_eq!(SessionStatus::from_str("idle"), SessionStatus::Idle);
        assert_eq!(SessionStatus::from_str("needs_input"), SessionStatus::NeedsInput);
    }

    #[test]
    fn session_status_from_str_trims_whitespace() {
        assert_eq!(SessionStatus::from_str("active\n"), SessionStatus::Active);
        assert_eq!(SessionStatus::from_str("  idle  "), SessionStatus::Idle);
    }

    #[test]
    fn session_status_from_str_unknown_is_starting() {
        assert_eq!(SessionStatus::from_str(""), SessionStatus::Starting);
        assert_eq!(SessionStatus::from_str("unknown"), SessionStatus::Starting);
    }

    #[test]
    fn session_status_label_roundtrips() {
        // from_str(label()) should return the same variant (except Starting)
        assert_eq!(SessionStatus::from_str(SessionStatus::Active.label()), SessionStatus::Active);
        assert_eq!(SessionStatus::from_str(SessionStatus::Idle.label()), SessionStatus::Idle);
    }

    #[test]
    fn session_status_indicators_not_empty() {
        let variants = [
            SessionStatus::Starting,
            SessionStatus::Active,
            SessionStatus::Idle,
            SessionStatus::NeedsInput,
        ];
        for v in &variants {
            assert!(!v.indicator().is_empty(), "{:?} has empty indicator", v);
            assert!(!v.label().is_empty(), "{:?} has empty label", v);
        }
    }
}
