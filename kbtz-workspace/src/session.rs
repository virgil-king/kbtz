use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use crate::shepherd_session::ShepherdSession;

pub trait SessionHandle: Send {
    fn task_name(&self) -> &str;
    fn session_id(&self) -> &str;
    fn status(&self) -> &SessionStatus;
    fn set_status(&mut self, status: SessionStatus);
    fn stopping_since(&self) -> Option<Instant>;
    fn is_alive(&mut self) -> bool;
    fn mark_stopping(&mut self);
    fn force_kill(&mut self);
    /// Start passthrough: render the VTE's current screen to stdout
    /// using explicit cursor positioning, restore input modes, and
    /// enable raw byte forwarding from the reader thread.
    fn start_passthrough(&self) -> Result<()>;
    /// Stop passthrough: disable raw forwarding and reset input modes
    /// so they don't leak into other UI modes.
    fn stop_passthrough(&self) -> Result<()>;
    fn enter_scroll_mode(&self) -> Result<usize>;
    fn exit_scroll_mode(&self) -> Result<()>;
    fn render_scrollback(&self, offset: usize, cols: u16) -> Result<usize>;
    fn scrollback_available(&self) -> Result<usize>;
    fn has_mouse_tracking(&self) -> bool;
    fn write_input(&mut self, buf: &[u8]) -> Result<()>;
    fn resize(&self, rows: u16, cols: u16) -> Result<()>;
    /// Return escape sequences that sync the real terminal to the VTE's
    /// current SGR attributes, cursor position, and cursor visibility.
    ///
    /// During passthrough mode the reader thread forwards raw child output
    /// to stdout.  If we write directly to the terminal between those raw
    /// writes (e.g. to draw a status bar), we leave the terminal in
    /// whatever SGR/cursor state our drawing code ended with.  The next
    /// raw write from the child then inherits that state, causing visible
    /// artifacts — most commonly reverse-video "selection" on text that
    /// should be unstyled.
    ///
    /// Writing these bytes after every non-forwarding write restores the
    /// terminal to the state the child's VTE expects, closing the leak.
    /// Prefer [`write_and_sync`] in main.rs which wraps this automatically.
    fn terminal_sync_bytes(&self) -> Result<Vec<u8>>;
    fn process_id(&self) -> Option<u32>;
    /// Returns true if the reader thread is still running.  A dead reader
    /// with a live child means the session is frozen (no output forwarding).
    fn reader_alive(&self) -> bool;
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
        cwd: &std::path::Path,
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
        cwd: &std::path::Path,
    ) -> Result<Box<dyn SessionHandle>> {
        Session::spawn(
            command, args, task_name, session_id, rows, cols, env_vars, cwd,
        )
        .map(|s| Box::new(s) as Box<dyn SessionHandle>)
    }
}

pub struct ShepherdSpawner {
    pub status_dir: PathBuf,
}

impl SessionSpawner for ShepherdSpawner {
    fn spawn(
        &self,
        command: &str,
        args: &[&str],
        task_name: &str,
        session_id: &str,
        rows: u16,
        cols: u16,
        env_vars: &[(&str, &str)],
        cwd: &std::path::Path,
    ) -> Result<Box<dyn SessionHandle>> {
        let filename = session_id.replace('/', "-");
        let socket_path = self.status_dir.join(format!("{filename}.sock"));
        let pid_path = self.status_dir.join(format!("{filename}.pid"));

        // Find kbtz-shepherd binary next to the current executable
        let self_exe = std::env::current_exe().context("failed to get current executable path")?;
        let shepherd_bin = self_exe.with_file_name("kbtz-shepherd");
        if !shepherd_bin.exists() {
            bail!(
                "kbtz-shepherd binary not found at {}",
                shepherd_bin.display()
            );
        }

        // Build shepherd command: kbtz-shepherd <socket> <pid> <rows> <cols> <command> [args...]
        let mut cmd = std::process::Command::new(&shepherd_bin);
        cmd.arg(&socket_path)
            .arg(&pid_path)
            .arg(rows.to_string())
            .arg(cols.to_string())
            .arg(command)
            .args(args);
        cmd.current_dir(cwd);
        for (k, v) in env_vars {
            cmd.env(k, v);
        }
        // Detach stdio.  All other FDs (SQLite, sockets, inotify) are
        // already opened with O_CLOEXEC / SOCK_CLOEXEC / IN_CLOEXEC by
        // their respective libraries, so no extra cleanup is needed.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        cmd.spawn().with_context(|| {
            format!(
                "failed to spawn kbtz-shepherd at {}",
                shepherd_bin.display()
            )
        })?;

        // Wait for socket to appear (shepherd needs a moment to start)
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket_path.exists() {
            if Instant::now() >= deadline {
                bail!(
                    "shepherd did not create socket at {} within 5 seconds",
                    socket_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Connect to the shepherd
        ShepherdSession::connect(&socket_path, &pid_path, task_name, session_id, rows, cols)
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
    /// Set to false by the reader thread when it exits.  Allows the main
    /// thread to detect a dead reader (e.g. due to a premature EOF on the
    /// PTY) while the child process is still running.
    pub reader_alive: Arc<AtomicBool>,
}

use kbtz_workspace::SCROLLBACK_ROWS;

/// Shared state between the reader thread and the main thread.
///
/// Holds a virtual terminal emulator (`vt100::Parser`) that receives
/// every byte the child writes.  When `active` is true the reader
/// thread also forwards those raw bytes to stdout — the child controls
/// its own rendering (including alt screen, cursor positioning, etc).
///
/// The VTE is kept up-to-date at all times for scroll mode, which
/// accesses the main screen's scrollback by temporarily toggling
/// DECRST/DECSET 47.
pub struct Passthrough {
    pub(crate) active: bool,
    vte: vt100::Parser,
    /// Cloned snapshot of the main screen, captured on scroll mode entry.
    scroll_screen: Option<vt100::Screen>,
}

impl Passthrough {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self {
            active: false,
            vte: vt100::Parser::new(rows, cols, SCROLLBACK_ROWS),
            scroll_screen: None,
        }
    }

    /// Switch to passthrough mode.  Render the VTE's current screen
    /// state using explicit cursor positioning (no `\r\n` that could
    /// cause scrolling within a scroll region), restore input modes,
    /// and set `active` for live raw byte forwarding.
    pub(crate) fn start(&mut self) {
        debug_assert!(!self.active, "start() called while already active");

        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let _ = out.write_all(b"\x1b[?2026h"); // begin synchronized update
        self.render_screen_positioned(&mut out);
        let _ = out.write_all(b"\x1b[?2026l"); // end synchronized update
        let _ = out.flush();

        self.active = true;
    }

    /// Stop passthrough: disable raw forwarding and reset input modes.
    pub(crate) fn stop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;

        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = out.write_all(
            concat!(
                "\x1b[m",      // reset all SGR attributes (colors, reverse, bold, etc.)
                "\x1b[?1000l", // disable mouse tracking modes
                "\x1b[?1002l",
                "\x1b[?1003l",
                "\x1b[?1006l", // disable SGR mouse encoding
                "\x1b[?1004l", // disable focus event reporting
                "\x1b[?2004l", // disable bracketed paste
                "\x1b[?1l",    // normal cursor keys
                "\x1b>",       // normal keypad
                "\x1b[?25h",   // show cursor
                "\x1b[<u",     // pop kitty keyboard protocol (if pushed by child)
            )
            .as_bytes(),
        );
        let _ = out.flush();
    }

    /// Render the VTE screen to `out` using explicit cursor positioning
    /// per row (CSI row;1 H + CSI K + row content).  This never causes
    /// terminal scrolling, unlike `state_formatted()` / `state_diff()`
    /// which use sequential `\r\n` between rows.
    fn render_screen_positioned(&self, out: &mut impl Write) {
        let screen = self.vte.screen();
        let (_rows, cols) = screen.size();
        for (i, row_bytes) in screen.rows_formatted(0, cols).enumerate() {
            // Reset SGR before erasing so \x1b[K doesn't inherit stale
            // attributes (e.g. reverse video) from the previous row.
            let _ = write!(out, "\x1b[0m\x1b[{};1H\x1b[K", i + 1);
            let _ = out.write_all(&row_bytes);
        }
        // Sync terminal state (SGR attributes, cursor position, cursor
        // visibility) to match the VTE — the VTE is the source of truth.
        let _ = out.write_all(&screen.attributes_formatted());
        let _ = out.write_all(&screen.cursor_state_formatted());
        let _ = out.write_all(&screen.input_mode_formatted());
        let _ = out.write_all(b"\x1b[?1000h\x1b[?1006h");
    }

    pub(crate) fn process(&mut self, data: &[u8]) {
        self.vte.process(data);

        // CSI 3 J (Erase Saved Lines) — clear the scrollback buffer.
        // The vt100 crate doesn't implement this, so we handle it by
        // creating a fresh parser and replaying the visible screen state.
        // This matches tmux's screen_write_clearhistory().
        if Self::contains_csi_3j(data) {
            self.clear_scrollback();
        }
    }

    /// Check if a byte slice contains the CSI 3 J sequence (\x1b[3J).
    fn contains_csi_3j(data: &[u8]) -> bool {
        data.windows(4).any(|w| w == b"\x1b[3J")
    }

    /// Clear scrollback by creating a fresh VTE and replaying the
    /// visible screen state.
    fn clear_scrollback(&mut self) {
        let (rows, cols) = self.vte.screen().size();
        let was_alt = self.vte.screen().alternate_screen();

        // Capture current screen state(s).
        let mut alt_state = None;

        if was_alt {
            // Save alt screen state, switch to main to capture it too.
            alt_state = Some(self.vte.screen().state_formatted());
            self.vte.process(b"\x1b[?47l");
        }
        let main_state = self.vte.screen().state_formatted();
        if was_alt {
            self.vte.process(b"\x1b[?47h");
        }

        // Create a fresh VTE with the same dimensions and scrollback capacity.
        let mut fresh = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        fresh.process(&main_state);
        if let Some(alt) = alt_state {
            fresh.process(b"\x1b[?47h");
            fresh.process(&alt);
        }

        self.vte = fresh;
    }

    pub(crate) fn set_size(&mut self, rows: u16, cols: u16) {
        kbtz_workspace::resize_both_screens(&mut self.vte, rows, cols);
    }

    /// Enter scroll mode: snapshot the main screen (with scrollback)
    /// from the live VTE and return the number of scrollback rows
    /// available.
    ///
    /// If the child is on the alternate screen, we temporarily toggle
    /// DECRST 47 / DECSET 47 to expose the main grid, clone it, and
    /// switch back.  DECSET 47 does not clear the alternate grid, so
    /// the child's display is preserved.
    pub(crate) fn enter_scroll_mode(&mut self) -> usize {
        self.active = false;

        let was_alt = self.vte.screen().alternate_screen();
        if was_alt {
            self.vte.process(b"\x1b[?47l"); // expose main grid
        }
        let mut snapshot = self.vte.screen().clone();
        if was_alt {
            self.vte.process(b"\x1b[?47h"); // restore alt grid
        }

        // Probe scrollback depth.
        snapshot.set_scrollback(usize::MAX);
        let total = snapshot.scrollback();
        snapshot.set_scrollback(0);

        self.scroll_screen = Some(snapshot);
        total
    }

    /// Exit scroll mode: discard the snapshot, re-render the live
    /// screen, and resume raw forwarding.
    pub(crate) fn exit_scroll_mode(&mut self) {
        self.scroll_screen = None;

        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let _ = out.write_all(b"\x1b[?2026h"); // begin synchronized update
        self.render_screen_positioned(&mut out);
        let _ = out.write_all(b"\x1b[?2026l"); // end synchronized update
        let _ = out.flush();

        self.active = true;
    }

    /// Whether the child has requested any mouse tracking mode.
    pub(crate) fn has_mouse_tracking(&self) -> bool {
        !matches!(
            self.vte.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        )
    }

    /// Return escape sequences that sync the real terminal to the VTE's
    /// current state.
    ///
    /// The returned bytes contain, in order:
    /// 1. `attributes_formatted()` — SGR reset (`\x1b[0m`) followed by the
    ///    VTE's current text attributes (colors, bold, reverse, etc.)
    /// 2. `cursor_state_formatted()` — cursor position (`\x1b[row;colH`)
    ///    and visibility (`\x1b[?25h` or `\x1b[?25l`)
    ///
    /// See [`SessionHandle::terminal_sync_bytes`] for the rationale.
    pub(crate) fn terminal_sync_bytes(&self) -> Vec<u8> {
        let screen = self.vte.screen();
        let mut bytes = screen.attributes_formatted();
        bytes.extend_from_slice(&screen.cursor_state_formatted());
        bytes
    }

    /// Set the scrollback offset and write the viewport to `out`.
    /// Returns the clamped offset actually applied.
    pub(crate) fn render_scrollback(
        &mut self,
        out: &mut impl Write,
        offset: usize,
        cols: u16,
    ) -> usize {
        let screen = match self.scroll_screen.as_mut() {
            Some(s) => s,
            None => return 0,
        };
        let max = Self::scrollback_depth(screen);
        let clamped = offset.min(max);
        screen.set_scrollback(clamped);

        let _ = out.write_all(b"\x1b[?2026h"); // begin synchronized update
        for (i, row_bytes) in screen.rows_formatted(0, cols).enumerate() {
            let _ = write!(out, "\x1b[0m\x1b[{};1H\x1b[K", i + 1);
            let _ = out.write_all(&row_bytes);
        }
        let _ = write!(out, "\x1b[0m");
        let _ = out.write_all(b"\x1b[?2026l"); // end synchronized update
        let _ = out.flush();

        clamped
    }

    /// Total scrollback rows available (not counting the visible screen).
    pub(crate) fn scrollback_available(&mut self) -> usize {
        match self.scroll_screen.as_mut() {
            Some(s) => Self::scrollback_depth(s),
            None => 0,
        }
    }

    /// Probe a Screen for its total scrollback depth.
    fn scrollback_depth(screen: &mut vt100::Screen) -> usize {
        let saved = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let total = screen.scrollback();
        screen.set_scrollback(saved);
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
        kbtz::ui::session_indicator(match self {
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Idle => "idle",
            Self::NeedsInput => "needs_input",
        })
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
        let mut out = std::io::BufWriter::new(stdout.lock());
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

    fn reader_alive(&self) -> bool {
        self.reader_alive.load(Ordering::Acquire)
    }
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        command: &str,
        args: &[&str],
        task_name: &str,
        session_id: &str,
        rows: u16,
        cols: u16,
        env_vars: &[(&str, &str)],
        cwd: &std::path::Path,
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
        cmd.cwd(cwd);
        for (k, v) in env_vars {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        drop(pair.slave);

        let passthrough = Arc::new(Mutex::new(Passthrough::new(pty_rows, cols)));
        let reader_alive = Arc::new(AtomicBool::new(true));
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let pt = Arc::clone(&passthrough);
        let ra = Arc::clone(&reader_alive);
        let reader_sid = session_id.to_string();
        std::thread::spawn(move || reader_thread(reader, pt, ra, reader_sid));

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
            reader_alive,
        })
    }
}

fn reader_thread(
    mut reader: Box<dyn Read + Send>,
    passthrough: Arc<Mutex<Passthrough>>,
    alive_flag: Arc<AtomicBool>,
    session_id: String,
) {
    let mut buf = [0u8; 4096];
    let stdout = std::io::stdout();

    let exit_reason;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                exit_reason = "EOF";
                break;
            }
            Err(e) => {
                // EINTR means a signal was delivered (SIGCHLD, SIGWINCH,
                // etc.) during the blocking read.  This is common on macOS
                // where these signals are not automatically restarted.
                // Retry instead of exiting.
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                // EIO from the PTY layer is mapped to Ok(0) by
                // portable_pty, but other error paths might surface it
                // directly.  Treat as EOF.
                if e.raw_os_error() == Some(libc::EIO) {
                    exit_reason = "EIO";
                    break;
                }
                exit_reason = "error";
                kbtz::debug_log::log(&format!(
                    "reader_thread({session_id}): exiting on error: {e}"
                ));
                break;
            }
            Ok(n) => {
                let Ok(mut pt) = passthrough.lock() else {
                    exit_reason = "mutex poisoned";
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

    alive_flag.store(false, Ordering::Release);
    kbtz::debug_log::log(&format!(
        "reader_thread({session_id}): exited ({exit_reason})"
    ));
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
    fn exit_scroll_mode_discards_snapshot() {
        let mut pt = Passthrough::new(4, 80);
        for i in 0..10 {
            pt.process(format!("line {i}\n").as_bytes());
        }
        pt.enter_scroll_mode();
        assert!(pt.scroll_screen.is_some());

        pt.exit_scroll_mode();
        assert!(pt.scroll_screen.is_none());
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
        // because no scroll_screen exists.
        assert_eq!(pt.scrollback_available(), 0);
    }

    #[test]
    fn scroll_mode_reenter_rebuilds_snapshot() {
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

        // Re-enter: snapshot should capture new content.
        pt.enter_scroll_mode();
        let total2 = pt.scrollback_available();
        assert!(
            total2 > total1,
            "expected more scrollback after adding content: {total2} <= {total1}"
        );
    }

    #[test]
    fn has_mouse_tracking_default_false() {
        let pt = Passthrough::new(24, 80);
        assert!(!pt.has_mouse_tracking());
    }

    #[test]
    fn has_mouse_tracking_after_mode_1000() {
        let mut pt = Passthrough::new(24, 80);
        // \x1b[?1000h enables PressRelease mouse tracking.
        pt.process(b"\x1b[?1000h");
        assert!(pt.has_mouse_tracking());
    }

    #[test]
    fn has_mouse_tracking_after_mode_1002() {
        let mut pt = Passthrough::new(24, 80);
        // \x1b[?1002h enables ButtonMotion mouse tracking.
        pt.process(b"\x1b[?1002h");
        assert!(pt.has_mouse_tracking());
    }

    #[test]
    fn has_mouse_tracking_after_mode_1003() {
        let mut pt = Passthrough::new(24, 80);
        // \x1b[?1003h enables AnyMotion mouse tracking.
        pt.process(b"\x1b[?1003h");
        assert!(pt.has_mouse_tracking());
    }

    #[test]
    fn has_mouse_tracking_false_after_disable() {
        let mut pt = Passthrough::new(24, 80);
        pt.process(b"\x1b[?1000h");
        assert!(pt.has_mouse_tracking());
        pt.process(b"\x1b[?1000l");
        assert!(!pt.has_mouse_tracking());
    }

    #[test]
    fn input_mode_formatted_includes_bracketed_paste() {
        let mut pt = Passthrough::new(24, 80);
        // Enable bracketed paste in the child.
        pt.process(b"\x1b[?2004h");
        let modes = pt.vte.screen().input_mode_formatted();
        assert!(
            modes.windows(8).any(|w| w == b"\x1b[?2004h"),
            "expected bracketed paste enable in input_mode_formatted()"
        );
    }

    #[test]
    fn input_mode_formatted_includes_mouse_tracking() {
        let mut pt = Passthrough::new(24, 80);
        // Enable PressRelease mouse tracking + SGR encoding in the child.
        pt.process(b"\x1b[?1000h\x1b[?1006h");
        let modes = pt.vte.screen().input_mode_formatted();
        assert!(
            modes.windows(8).any(|w| w == b"\x1b[?1000h"),
            "expected mouse tracking enable in input_mode_formatted()"
        );
    }

    /// Helper: collect all non-empty scrollback lines as strings.
    fn collect_scrollback(pt: &mut Passthrough) -> Vec<String> {
        let was_alt = pt.vte.screen().alternate_screen();
        if was_alt {
            pt.vte.process(b"\x1b[?47l");
        }
        let screen = pt.vte.screen_mut();
        let cols = screen.size().1;
        screen.set_scrollback(usize::MAX);
        let total = screen.scrollback();
        let mut lines = Vec::new();
        for offset in (1..=total).rev() {
            screen.set_scrollback(offset);
            if let Some(row) = screen.rows(0, cols).next() {
                let text = row.to_string();
                let trimmed = text.trim_end().to_string();
                if !trimmed.is_empty() {
                    lines.push(trimmed);
                }
            }
        }
        screen.set_scrollback(0);
        if was_alt {
            pt.vte.process(b"\x1b[?47h");
        }
        lines
    }

    #[test]
    fn decrst_decset_47_cycle_preserves_scrollback() {
        // Verify that the DECRST/DECSET 47 trick doesn't change scrollback.
        let mut pt = Passthrough::new(5, 80);
        for i in 0..20 {
            pt.process(format!("line {i}\n").as_bytes());
        }
        pt.process(b"\x1b[?1049h"); // enter alt screen

        let before = collect_scrollback(&mut pt);

        // Do the DECRST/DECSET 47 cycle (no resize).
        pt.vte.process(b"\x1b[?47l");
        pt.vte.process(b"\x1b[?47h");

        let after = collect_scrollback(&mut pt);
        assert_eq!(
            before, after,
            "DECRST/DECSET 47 cycle should not change scrollback"
        );
    }

    #[test]
    fn resize_does_not_duplicate_scrollback() {
        let mut pt = Passthrough::new(10, 80);
        for i in 0..30 {
            pt.process(format!("unique line {i}\n").as_bytes());
        }
        pt.process(b"\x1b[?1049h"); // enter alt screen
        pt.process(b"alt screen content");

        let before = collect_scrollback(&mut pt);
        assert!(!before.is_empty(), "should have scrollback before resize");

        // Resize (uses our set_size which does DECRST/DECSET 47 trick).
        pt.set_size(8, 60);

        let after = collect_scrollback(&mut pt);

        // Check no line appears more than once (allowing for reflow
        // which may split long lines).
        let before_set: std::collections::HashSet<_> = before.iter().collect();

        // Every line from before should still exist (possibly reflowed).
        // No NEW unique content should appear.
        for line in &after {
            // Lines from reflow are OK (substrings of original lines).
            let is_original = before_set.contains(line);
            let is_reflow = before.iter().any(|b| b.contains(line.as_str()));
            assert!(
                is_original || is_reflow,
                "unexpected new line after resize: {line:?}"
            );
        }

        // The total number of non-empty lines should not increase
        // dramatically (reflow can increase count, but not double it
        // for 80→60 col resize of short lines).
        assert!(
            after.len() <= before.len() * 2,
            "scrollback grew too much: {} before, {} after",
            before.len(),
            after.len()
        );
    }

    #[test]
    fn multiple_resizes_do_not_accumulate_duplicates() {
        let mut pt = Passthrough::new(10, 80);
        for i in 0..30 {
            pt.process(format!("unique line {i}\n").as_bytes());
        }
        pt.process(b"\x1b[?1049h"); // enter alt screen

        let initial = collect_scrollback(&mut pt);

        // Resize back and forth 5 times.
        for _ in 0..5 {
            pt.set_size(8, 60);
            pt.set_size(12, 100);
            pt.set_size(10, 80);
        }

        let after = collect_scrollback(&mut pt);

        // After resizing back to the original size, the scrollback
        // should have roughly the same content (not 5x duplicated).
        assert!(
            after.len() <= initial.len() + 20,
            "scrollback grew excessively after multiple resizes: {} initial, {} after",
            initial.len(),
            after.len()
        );
    }

    #[test]
    fn render_screen_positioned_uses_cursor_positioning() {
        // Verify render_screen_positioned doesn't emit sequential newlines.
        let mut pt = Passthrough::new(5, 40);
        pt.process(b"row 1\r\nrow 2\r\nrow 3\r\nrow 4\r\nrow 5");
        let mut buf = Vec::new();
        pt.render_screen_positioned(&mut buf);
        let output = String::from_utf8_lossy(&buf);
        assert!(
            !output.contains("\r\n"),
            "render_screen_positioned should not contain \\r\\n"
        );
        assert!(
            !output.contains("\x1b[2J"),
            "render_screen_positioned should not contain CSI 2 J"
        );
    }

    #[test]
    fn render_screen_positioned_resets_attrs_before_each_row() {
        let mut pt = Passthrough::new(4, 80);
        // Write a line with reverse video.  When rendering, the reverse
        // video must NOT leak into \x1b[K of the following rows.
        pt.process(b"\x1b[7mreversed\x1b[0m\r\n");
        pt.process(b"normal line 1\r\n");
        pt.process(b"normal line 2\r\n");
        pt.process(b"normal line 3");

        let mut buf = Vec::new();
        pt.render_screen_positioned(&mut buf);
        let rendered = String::from_utf8_lossy(&buf);

        // Every \x1b[K must be preceded by \x1b[0m so the erase uses
        // default attributes, not stale attributes from a previous row.
        let el_positions: Vec<usize> = rendered.match_indices("\x1b[K").map(|(p, _)| p).collect();
        assert!(!el_positions.is_empty(), "expected at least one \\x1b[K");
        let mut prev_end = 0;
        for &pos in &el_positions {
            let segment = &rendered[prev_end..pos];
            assert!(
                segment.contains("\x1b[0m"),
                "no \\x1b[0m reset before \\x1b[K at byte {pos}; segment: {segment:?}"
            );
            prev_end = pos + 3;
        }
    }

    /// Helper: probe scrollback depth.
    fn scrollback_depth(pt: &mut Passthrough) -> usize {
        let screen = pt.vte.screen_mut();
        screen.set_scrollback(usize::MAX);
        let total = screen.scrollback();
        screen.set_scrollback(0);
        total
    }

    #[test]
    fn set_size_resizes_both_screens() {
        let mut pt = Passthrough::new(10, 80);
        // Write content on main screen, then enter alt screen.
        for i in 0..20 {
            pt.process(format!("main line {i}\n").as_bytes());
        }
        pt.process(b"\x1b[?1049h"); // enter alt screen
        pt.process(b"alt content");

        // Resize to a different width.
        pt.set_size(8, 40);

        // The alt screen should be at the new size.
        assert_eq!(pt.vte.screen().size(), (8, 40));

        // Check main screen size via DECRST 47 trick.
        pt.vte.process(b"\x1b[?47l"); // expose main grid
        assert_eq!(
            pt.vte.screen().size(),
            (8, 40),
            "main screen should also be resized"
        );
        pt.vte.process(b"\x1b[?47h"); // restore alt grid
    }

    #[test]
    fn scroll_mode_after_resize_uses_correct_dimensions() {
        let mut pt = Passthrough::new(10, 80);
        // Write content on main screen, then enter alt screen.
        for i in 0..20 {
            pt.process(format!("main line {i}\n").as_bytes());
        }
        pt.process(b"\x1b[?1049h"); // enter alt screen

        // Resize (should resize both screens).
        pt.set_size(8, 40);

        // Enter scroll mode — should see main screen at the new size.
        let total = pt.enter_scroll_mode();
        assert!(total > 0, "expected scrollback after resize");

        // Render scrollback — should work without issues at new width.
        let mut buf = Vec::new();
        let applied = pt.render_scrollback(&mut buf, total, 40);
        assert_eq!(applied, total);
        assert!(!buf.is_empty());
    }

    // === CSI 3 J (Erase Saved Lines) tests ===

    /// CSI 3 J should clear the scrollback buffer.
    /// This is the escape sequence Claude Code sends to clear history,
    /// which tmux honors but vt100 does not implement.
    #[test]
    fn csi_3j_clears_scrollback() {
        let mut pt = Passthrough::new(10, 80);

        // Accumulate scrollback
        for i in 0..30 {
            pt.process(format!("line {i}\r\n").as_bytes());
        }
        let depth_before = scrollback_depth(&mut pt);
        assert!(depth_before > 0, "should have scrollback before CSI 3 J");

        // Send CSI 3 J (Erase Saved Lines)
        pt.process(b"\x1b[3J");

        let depth_after = scrollback_depth(&mut pt);
        assert_eq!(
            depth_after, 0,
            "CSI 3 J should clear scrollback, but {depth_after} rows remain"
        );
    }

    /// CSI 3 J should not affect the visible screen content.
    #[test]
    fn csi_3j_preserves_visible_screen() {
        let mut pt = Passthrough::new(5, 80);

        for i in 0..20 {
            pt.process(format!("line {i}\r\n").as_bytes());
        }

        let screen_before = pt.vte.screen().contents();
        pt.process(b"\x1b[3J");
        let screen_after = pt.vte.screen().contents();

        assert_eq!(
            screen_before, screen_after,
            "CSI 3 J should not change visible screen"
        );
    }

    #[test]
    fn render_screen_positioned_resets_sgr_after_last_row() {
        // Bug: render_screen_positioned renders all rows with per-row SGR
        // reset, but after the last row, the terminal's SGR state is
        // whatever the last cell's attributes were.  If the last row has
        // reverse video (e.g. Claude Code's status bar), the terminal is
        // left with reverse video active.  When raw byte forwarding starts,
        // the child's output inherits this stale SGR — text and erased
        // areas appear "selected."
        //
        // The invariant: render_screen_positioned must leave the terminal
        // with SGR reset (default attributes) after all rendering is done.
        let mut pt = Passthrough::new(4, 40);
        // Simulate a screen where the last row has reverse video content,
        // like Claude Code's status bar.
        pt.process(b"\x1b[1;1Hnormal line 1\r\n");
        pt.process(b"normal line 2\r\n");
        pt.process(b"normal line 3\r\n");
        pt.process(b"\x1b[7mstatus bar with reverse video\x1b[0m");

        let mut buf = Vec::new();
        pt.render_screen_positioned(&mut buf);
        let rendered = String::from_utf8_lossy(&buf);

        // Find the last \x1b[K (erase-in-line for the last row).  After
        // that comes the last row's content, then cursor positioning and
        // mode sequences.  There must be an SGR reset (\x1b[0m or \x1b[m)
        // after the last row's content.
        let last_el = rendered
            .rfind("\x1b[K")
            .expect("expected at least one \\x1b[K");
        let after_last_row = &rendered[last_el..];

        // The row content includes reverse video (\x1b[7m).  After that
        // content, before cursor positioning, there must be an SGR reset.
        // Find cursor positioning (CSI row;col H) after the last row.
        let cursor_pos = after_last_row
            .find("\x1b[")
            .and_then(|start| after_last_row[start..].find('H').map(|end| start + end + 1));
        assert!(
            cursor_pos.is_some(),
            "expected cursor positioning after last row"
        );

        // Check that SGR is reset between the last row's content and the
        // cursor positioning sequence.
        let between = &after_last_row[..cursor_pos.unwrap()];
        assert!(
            between.contains("\x1b[0m") || between.contains("\x1b[m"),
            "render_screen_positioned must reset SGR after the last row's content, \
             but no reset found before cursor positioning.\n\
             After last \\x1b[K: {after_last_row:?}"
        );
    }
}
