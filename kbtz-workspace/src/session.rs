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
    fn enter_scroll_mode(&self) -> Result<usize>;
    fn exit_scroll_mode(&self) -> Result<()>;
    fn render_scrollback(&self, offset: usize, cols: u16) -> Result<usize>;
    fn scrollback_available(&self) -> Result<usize>;
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

/// Max scrollback rows retained per session for the scroll-back viewer.
const SCROLLBACK_ROWS: usize = 10_000;

/// Max raw output we buffer per session for scrollback replay.
const OUTPUT_BUFFER_MAX: usize = 16 * 1024 * 1024;

/// Shared state between the reader thread and the main thread.
///
/// Holds a virtual terminal emulator (`vt100::Parser`) that receives
/// every byte the child writes.  When `active` is true the reader
/// thread also forwards those bytes to stdout.
///
/// A bounded raw output buffer is kept so that scroll mode can
/// reconstruct the full terminal history (including content that
/// predates an alternate-screen switch) by replaying it into a
/// temporary VTE.
pub struct Passthrough {
    active: bool,
    vte: vt100::Parser,
    /// Bounded buffer of raw child output for scrollback reconstruction.
    output_buffer: Vec<u8>,
    /// Temporary VTE used during scroll mode, built from `output_buffer`.
    scroll_vte: Option<vt100::Parser>,
}

impl Passthrough {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self {
            active: false,
            vte: vt100::Parser::new(rows, cols, 0),
            output_buffer: Vec::new(),
            scroll_vte: None,
        }
    }

    /// Switch to passthrough mode.  Render the VTE's current screen
    /// state and set `active` for live forwarding.  Both happen under
    /// the same Mutex guard so no child output is lost.
    fn start(&mut self) {
        debug_assert!(!self.active, "start() called while already active");

        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = out.write_all(&self.vte.screen().state_formatted());
        // Enable SGR mouse button reporting so scroll wheel events
        // arrive on stdin.  Mode 1000 = button events only (no
        // motion), mode 1006 = SGR encoding.  stop() disables all
        // mouse modes, and state_formatted() only restores visual
        // state, not input modes.
        let _ = out.write_all(b"\x1b[?1000h\x1b[?1006h");
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
                "\x1b[?1004l", // disable focus event reporting
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

    /// Enter scroll mode: build a temporary VTE from the output buffer,
    /// stop forwarding live output, disable mouse tracking for native
    /// text selection, and return the number of scrollback rows available.
    fn enter_scroll_mode(&mut self) -> usize {
        let screen = self.vte.screen();
        let (rows, cols) = screen.size();
        let mut scroll_vte = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        scroll_vte.process(&self.output_buffer);

        // If the child is on the alternate screen, switch the temp
        // VTE back to the main screen to access its scrollback.
        if scroll_vte.screen().alternate_screen() {
            scroll_vte.process(b"\x1b[?1049l");
        }

        let total = Self::scrollback_of(&mut scroll_vte);

        self.active = false;
        self.scroll_vte = Some(scroll_vte);

        // Enter alternate screen and disable mouse tracking.
        // The alternate screen gives us a clean canvas for the frozen
        // viewport and — critically — causes the terminal to convert
        // scroll wheel events into arrow key sequences.  Disabling
        // mouse tracking allows native text selection.
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = out.write_all(b"\x1b[?1049h\x1b[?1000l\x1b[?1006l");
        let _ = out.flush();

        total
    }

    /// Exit scroll mode: discard the temporary VTE, leave alternate
    /// screen, re-render the live screen, re-enable mouse tracking,
    /// and resume live forwarding.
    fn exit_scroll_mode(&mut self) {
        self.scroll_vte = None;

        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        // Leave the alternate screen we entered for scroll mode.
        let _ = out.write_all(b"\x1b[?1049l");
        // Restore the child's screen state and re-enable mouse tracking.
        if self.vte.screen().alternate_screen() {
            let _ = out.write_all(b"\x1b[?1049h");
        }
        let _ = out.write_all(&self.vte.screen().state_formatted());
        let _ = out.write_all(b"\x1b[?1000h\x1b[?1006h");
        let _ = out.flush();

        self.active = true;
    }

    /// Set the scrollback offset and write the viewport to `out`.
    /// Returns the clamped offset actually applied.
    fn render_scrollback(&mut self, out: &mut impl Write, offset: usize, cols: u16) -> usize {
        let svte = match self.scroll_vte.as_mut() {
            Some(v) => v,
            None => return 0,
        };
        let max = Self::scrollback_of(svte);
        let clamped = offset.min(max);
        svte.screen_mut().set_scrollback(clamped);

        for (i, row_bytes) in svte.screen().rows_formatted(0, cols).enumerate() {
            // Reset attributes before clearing so \x1b[K doesn't inherit
            // stale SGR state (e.g. reverse video) from the previous row.
            // rows_formatted() emits each row independently starting from
            // default attrs, so resetting here keeps the terminal in sync.
            let _ = write!(out, "\x1b[0m\x1b[{};1H\x1b[K", i + 1);
            let _ = out.write_all(&row_bytes);
        }
        let _ = write!(out, "\x1b[0m");
        let _ = out.flush();

        clamped
    }

    /// Total scrollback rows available (not counting the visible screen).
    fn scrollback_available(&mut self) -> usize {
        match self.scroll_vte.as_mut() {
            Some(svte) => Self::scrollback_of(svte),
            None => 0,
        }
    }

    /// Probe a VTE for its total scrollback depth.
    fn scrollback_of(vte: &mut vt100::Parser) -> usize {
        let saved = vte.screen().scrollback();
        vte.screen_mut().set_scrollback(usize::MAX);
        let total = vte.screen().scrollback();
        vte.screen_mut().set_scrollback(saved);
        total
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

    fn write_input(&mut self, buf: &[u8]) -> Result<()> {
        if let Err(e) = self.writer.write_all(buf) {
            // EIO means the child exited and the slave PTY side closed.
            // Discard the write — the session will be reaped on the next tick.
            if e.raw_os_error() == Some(libc::EIO) {
                return Ok(());
            }
            return Err(e).context("write to PTY");
        }
        if let Err(e) = self.writer.flush() {
            if e.raw_os_error() == Some(libc::EIO) {
                return Ok(());
            }
            return Err(e).context("flush PTY");
        }
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
    fn scroll_mode_has_scrollback() {
        let mut pt = Passthrough::new(4, 80);
        // Write enough lines to push some into scrollback.
        for i in 0..10 {
            pt.process(format!("line {i}\n").as_bytes());
        }
        pt.enter_scroll_mode();
        assert!(
            pt.scrollback_available() > 0,
            "expected scrollback rows, got 0"
        );
    }

    #[test]
    fn scroll_mode_renders_viewport() {
        let mut pt = Passthrough::new(4, 80);
        for i in 0..20 {
            pt.process(format!("line {i}\n").as_bytes());
        }
        pt.enter_scroll_mode();
        let total = pt.scrollback_available();
        assert!(total > 0);

        let mut buf = Vec::new();
        let applied = pt.render_scrollback(&mut buf, total, 80);
        assert_eq!(applied, total);
        assert!(!buf.is_empty());
    }

    #[test]
    fn scroll_mode_clamps_offset() {
        let mut pt = Passthrough::new(4, 80);
        pt.process(b"hello\n");
        pt.enter_scroll_mode();
        let total = pt.scrollback_available();
        let mut buf = Vec::new();
        let applied = pt.render_scrollback(&mut buf, total + 100, 80);
        assert_eq!(applied, total);
    }

    #[test]
    fn scroll_mode_sees_pre_altscreen_content() {
        let mut pt = Passthrough::new(4, 80);
        // Write content on the main screen.
        for i in 0..10 {
            pt.process(format!("main line {i}\n").as_bytes());
        }
        // Switch to alternate screen (like Claude Code does).
        pt.process(b"\x1b[?1049h");
        pt.process(b"alt screen content");

        // Enter scroll mode — should see main screen scrollback.
        pt.enter_scroll_mode();
        assert!(
            pt.scrollback_available() > 0,
            "expected main screen scrollback to be visible through alt screen"
        );
    }

    #[test]
    fn output_buffer_trims_with_escape_cancel_prefix() {
        let mut pt = Passthrough::new(24, 80);
        let chunk = vec![b'x'; OUTPUT_BUFFER_MAX + 1];
        pt.process(&chunk);
        assert!(pt.output_buffer.len() <= OUTPUT_BUFFER_MAX / 2 + 10);
        assert_eq!(&pt.output_buffer[..3], b"\x18\x1b\\");
    }

    #[test]
    fn exit_scroll_mode_discards_scroll_vte() {
        let mut pt = Passthrough::new(4, 80);
        for i in 0..10 {
            pt.process(format!("line {i}\n").as_bytes());
        }
        pt.enter_scroll_mode();
        assert!(pt.scroll_vte.is_some());

        pt.exit_scroll_mode();
        assert!(pt.scroll_vte.is_none());
        assert!(pt.active);
    }

    #[test]
    fn scroll_mode_without_altscreen_has_scrollback() {
        // When child stays on main screen, scrollback should work.
        let mut pt = Passthrough::new(4, 80);
        for i in 0..20 {
            pt.process(format!("line {i}\n").as_bytes());
        }
        pt.enter_scroll_mode();
        let total = pt.scrollback_available();
        assert!(total > 0, "expected scrollback on main screen");

        // Render at the top of scrollback.
        let mut buf = Vec::new();
        let applied = pt.render_scrollback(&mut buf, total, 80);
        assert_eq!(applied, total);
        let rendered = String::from_utf8_lossy(&buf);
        assert!(
            rendered.contains("line 0"),
            "expected earliest line in scrollback, got: {rendered}"
        );
    }

    #[test]
    fn scroll_mode_altscreen_sees_main_screen_lines() {
        let mut pt = Passthrough::new(4, 80);
        // Write identifiable content on main screen.
        pt.process(b"MARKER_MAIN_SCREEN\n");
        for _ in 0..10 {
            pt.process(b"filler\n");
        }
        // Switch to alt screen.
        pt.process(b"\x1b[?1049h");
        pt.process(b"alt content");

        pt.enter_scroll_mode();
        let total = pt.scrollback_available();
        assert!(total > 0);

        // Render at the top of scrollback — should contain main screen content.
        let mut buf = Vec::new();
        let applied = pt.render_scrollback(&mut buf, total, 80);
        assert_eq!(applied, total);
        let rendered = String::from_utf8_lossy(&buf);
        assert!(
            rendered.contains("MARKER_MAIN_SCREEN"),
            "expected main screen content in scrollback, got: {rendered}"
        );
    }

    #[test]
    fn render_scrollback_resets_attrs_before_each_row() {
        let mut pt = Passthrough::new(4, 80);
        // Write a line with reverse video, then enough lines to create
        // scrollback.  When rendering, the row with reverse video should
        // NOT leak its attributes into the \x1b[K of the following row.
        pt.process(b"\x1b[7mreversed\x1b[0m\n");
        for i in 0..10 {
            pt.process(format!("normal {i}\n").as_bytes());
        }
        pt.enter_scroll_mode();
        let total = pt.scrollback_available();
        assert!(total > 0);

        let mut buf = Vec::new();
        pt.render_scrollback(&mut buf, total, 80);
        let rendered = String::from_utf8_lossy(&buf);

        // Every \x1b[K (erase-in-line) must be preceded by \x1b[0m (reset)
        // so the erase uses the default background, not stale attributes.
        // The reset appears before the cursor-positioning sequence, so we
        // check that \x1b[0m occurs between consecutive \x1b[K sequences.
        let el_positions: Vec<usize> = rendered.match_indices("\x1b[K").map(|(p, _)| p).collect();
        assert!(!el_positions.is_empty(), "expected at least one \\x1b[K");
        let mut prev_end = 0;
        for &pos in &el_positions {
            let segment = &rendered[prev_end..pos];
            assert!(
                segment.contains("\x1b[0m"),
                "no \\x1b[0m reset before \\x1b[K at byte {pos}; segment: {segment:?}"
            );
            prev_end = pos + 3; // skip past "\x1b[K"
        }
    }

    #[test]
    fn scrollback_available_zero_without_scroll_mode() {
        let mut pt = Passthrough::new(4, 80);
        for i in 0..10 {
            pt.process(format!("line {i}\n").as_bytes());
        }
        // Without entering scroll mode, scrollback_available returns 0
        // because no scroll_vte exists.
        assert_eq!(pt.scrollback_available(), 0);
    }

    #[test]
    fn scroll_mode_reenter_rebuilds_scroll_vte() {
        let mut pt = Passthrough::new(4, 80);
        for i in 0..10 {
            pt.process(format!("line {i}\n").as_bytes());
        }

        pt.enter_scroll_mode();
        let total1 = pt.scrollback_available();
        pt.exit_scroll_mode();

        // Add more content.
        for i in 10..20 {
            pt.process(format!("line {i}\n").as_bytes());
        }

        // Re-enter: scroll_vte should be rebuilt with new content.
        pt.enter_scroll_mode();
        let total2 = pt.scrollback_available();
        assert!(
            total2 > total1,
            "expected more scrollback after adding content: {total2} <= {total1}"
        );
    }
}
