use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

pub trait SessionHandle: Send {
    fn task_name(&self) -> &str;
    fn session_id(&self) -> &str;
    fn status(&self) -> &SessionStatus;
    fn set_status(&mut self, status: SessionStatus);
    fn stopping_since(&self) -> Option<Instant>;
    fn is_alive(&mut self) -> bool;
    fn mark_stopping(&mut self);
    fn force_kill(&mut self);
    fn start_passthrough(&self) -> Result<()>;
    fn stop_passthrough(&self) -> Result<()>;
    fn write_input(&mut self, buf: &[u8]) -> Result<()>;
    fn resize(&self, rows: u16, cols: u16) -> Result<()>;
    fn process_id(&self) -> Option<u32>;
}

pub trait SessionSpawner: Send {
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        command: &str,
        args: &[&str],
        task_name: &str,
        session_id: &str,
        rows: u16,
        cols: u16,
        env_vars: &[(&str, &str)],
    ) -> Result<Box<dyn SessionHandle>>;
}

pub struct PtySpawner;

impl SessionSpawner for PtySpawner {
    fn spawn(
        &self,
        command: &str,
        args: &[&str],
        task_name: &str,
        session_id: &str,
        rows: u16,
        cols: u16,
        env_vars: &[(&str, &str)],
    ) -> Result<Box<dyn SessionHandle>> {
        Session::spawn(command, args, task_name, session_id, rows, cols, env_vars)
            .map(|s| Box::new(s) as Box<dyn SessionHandle>)
    }
}

pub struct Session {
    pub master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub passthrough: Arc<Mutex<Passthrough>>,
    pub status: SessionStatus,
    pub task_name: String,
    pub session_id: String,
    /// Set when exit has been requested and we are waiting for the process to stop.
    pub stopping_since: Option<Instant>,
}

/// Max raw output we buffer per session for scrollback replay.
const OUTPUT_BUFFER_MAX: usize = 16 * 1024 * 1024;

/// Shared state between the reader thread and the main thread.
///
/// Holds a virtual terminal emulator (`vt100::Parser`) that receives
/// every byte the child writes. When `active` is true the reader
/// thread also forwards those bytes to stdout.  On zoom-in the main
/// thread replays the raw output buffer to recreate terminal
/// scrollback, then sets `active` for live forwarding.
pub struct Passthrough {
    active: bool,
    vte: vt100::Parser,
    /// Bounded buffer of raw child output for scrollback replay.
    output_buffer: Vec<u8>,
}

impl Passthrough {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self {
            active: false,
            vte: vt100::Parser::new(rows, cols, 0),
            output_buffer: Vec::new(),
        }
    }

    /// Switch to passthrough mode.  Replay the raw output buffer to
    /// recreate terminal scrollback, then fix up the visible screen
    /// with the VTE's current state and set `active` for live
    /// forwarding.  Both happen under the same Mutex guard so no
    /// child output is lost.
    fn start(&mut self) {
        debug_assert!(!self.active, "start() called while already active");

        let stdout = std::io::stdout();
        let mut out = stdout.lock();

        // Replay raw output to recreate terminal scrollback, stripping
        // escape sequences that would trigger terminal responses and
        // appear as garbage input in the child session.
        crate::scrollback::replay(&mut out, &self.output_buffer);

        // Fix up the visible screen: state_formatted() clears the
        // screen (without touching scrollback), redraws cell contents,
        // positions the cursor, and restores input modes.  This
        // corrects any display issues from buffer trimming or
        // resize-induced layout drift.
        let _ = out.write_all(&self.vte.screen().state_formatted());

        let _ = out.flush();

        self.active = true;
    }

    fn stop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;

        // Reset input modes so they don't leak into other UI modes
        // (tree view, etc.).
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = out.write_all(
            concat!(
                "\x1b[?1000l", // disable mouse tracking modes
                "\x1b[?1002l",
                "\x1b[?1003l",
                "\x1b[?1006l", // disable SGR mouse encoding
                "\x1b[?2004l", // disable bracketed paste
                "\x1b[?1l",    // normal cursor keys
                "\x1b>",       // normal keypad
                "\x1b[?25h",   // show cursor
            )
            .as_bytes(),
        );
        let _ = out.flush();
    }

    fn process(&mut self, data: &[u8]) {
        self.vte.process(data);
        self.output_buffer.extend_from_slice(data);
        if self.output_buffer.len() > OUTPUT_BUFFER_MAX {
            let keep_from = self.output_buffer.len() - OUTPUT_BUFFER_MAX / 2;
            self.output_buffer.drain(..keep_from);
            // Terminate any escape sequence that was cut mid-stream.
            // CAN (0x18) aborts CSI sequences; ST (\x1b\\) ends
            // OSC/DCS sequences.
            self.output_buffer
                .splice(0..0, b"\x18\x1b\\".iter().copied());
        }
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
            "needs_input" | "needs input" => Self::NeedsInput,
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
            Self::Starting => "\u{23f3}",    // ⏳
            Self::Active => "\u{1f7e2}",     // 🟢
            Self::Idle => "\u{1f7e1}",       // 🟡
            Self::NeedsInput => "\u{1f514}", // 🔔
        }
    }
}

impl SessionHandle for Session {
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
        matches!(self.child.try_wait(), Ok(None))
    }

    fn mark_stopping(&mut self) {
        if self.stopping_since.is_none() {
            self.stopping_since = Some(Instant::now());
        }
    }

    fn force_kill(&mut self) {
        let _ = self.child.kill();
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

    fn write_input(&mut self, buf: &[u8]) -> Result<()> {
        self.writer.write_all(buf).context("write to PTY")?;
        self.writer.flush().context("flush PTY")?;
        Ok(())
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let pty_rows = rows.saturating_sub(1);
        self.passthrough
            .lock()
            .map_err(|_| anyhow::anyhow!("passthrough mutex poisoned"))?
            .set_size(pty_rows, cols);
        self.master
            .resize(PtySize {
                rows: pty_rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("resize PTY: {e}"))
    }

    fn process_id(&self) -> Option<u32> {
        self.child.process_id()
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
}

fn reader_thread(mut reader: Box<dyn Read + Send>, passthrough: Arc<Mutex<Passthrough>>) {
    let mut buf = [0u8; 4096];
    let stdout = std::io::stdout();

    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let Ok(mut pt) = passthrough.lock() else {
                    break;
                };
                pt.process(&buf[..n]);

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
        assert_eq!(
            SessionStatus::from_str("needs_input"),
            SessionStatus::NeedsInput
        );
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
        assert_eq!(
            SessionStatus::from_str(SessionStatus::Active.label()),
            SessionStatus::Active
        );
        assert_eq!(
            SessionStatus::from_str(SessionStatus::Idle.label()),
            SessionStatus::Idle
        );
        assert_eq!(
            SessionStatus::from_str(SessionStatus::NeedsInput.label()),
            SessionStatus::NeedsInput
        );
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

    #[test]
    fn passthrough_accumulates_output_buffer() {
        let mut pt = Passthrough::new(24, 80);
        pt.process(b"hello ");
        pt.process(b"world");
        assert_eq!(&pt.output_buffer, b"hello world");
    }

    #[test]
    fn passthrough_trims_output_buffer_with_escape_cancel_prefix() {
        let mut pt = Passthrough::new(24, 80);
        // Fill just past OUTPUT_BUFFER_MAX to trigger trim.
        let chunk = vec![b'x'; OUTPUT_BUFFER_MAX + 1];
        pt.process(&chunk);
        // After trim, buffer should contain CAN+ST prefix plus ~half of max.
        assert!(pt.output_buffer.len() <= OUTPUT_BUFFER_MAX / 2 + 10);
        // CAN (0x18) followed by ST (ESC + backslash) at the start.
        assert_eq!(&pt.output_buffer[..3], b"\x18\x1b\\");
    }

    #[test]
    fn passthrough_vte_state_survives_trim() {
        let mut pt = Passthrough::new(24, 80);
        // Write enough to trigger trim, then write identifiable text.
        let filler = vec![b'\n'; OUTPUT_BUFFER_MAX + 1];
        pt.process(&filler);
        pt.process(b"\x1b[1;1Htest");
        // VTE should reflect the text regardless of buffer trim.
        let screen = pt.vte.screen();
        let contents = screen.contents();
        assert!(
            contents.starts_with("test"),
            "expected 'test' at top of screen, got: {contents:?}"
        );
    }
}
