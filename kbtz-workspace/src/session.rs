use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use crate::shepherd_session::ShepherdSession;

/// Reset terminal input modes that a child may have set.
/// Called when stopping raw byte forwarding to prevent mode leaks
/// into other UI modes (tree view, other sessions).
pub fn reset_terminal_modes() {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(
        concat!(
            "\x1b[m",      // reset all SGR attributes
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

pub trait SessionHandle: Send {
    fn task_name(&self) -> &str;
    fn session_id(&self) -> &str;
    fn status(&self) -> &SessionStatus;
    fn set_status(&mut self, status: SessionStatus);
    fn stopping_since(&self) -> Option<Instant>;
    fn is_alive(&mut self) -> bool;
    fn mark_stopping(&mut self);
    fn force_kill(&mut self);
    /// Enable raw byte forwarding from the reader thread to stdout.
    fn start_forwarding(&self);
    /// Disable raw byte forwarding and reset terminal input modes
    /// so they don't leak into other UI modes.
    fn stop_forwarding(&self);
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
    forwarding: Arc<AtomicBool>,
    pub status: SessionStatus,
    pub task_name: String,
    pub session_id: String,
    pub stopping_since: Option<Instant>,
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

    fn start_forwarding(&self) {
        self.forwarding.store(true, Ordering::SeqCst);
    }

    fn stop_forwarding(&self) {
        self.forwarding.store(false, Ordering::SeqCst);
        reset_terminal_modes();
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

        let forwarding = Arc::new(AtomicBool::new(false));
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let fwd = Arc::clone(&forwarding);
        std::thread::spawn(move || reader_thread(reader, fwd));

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(Session {
            master: pair.master,
            writer,
            child,
            forwarding,
            status: SessionStatus::Starting,
            task_name: task_name.to_string(),
            session_id: session_id.to_string(),
            stopping_since: None,
        })
    }
}

fn reader_thread(mut reader: Box<dyn Read + Send>, forwarding: Arc<AtomicBool>) {
    let mut buf = [0u8; 4096];
    let stdout = std::io::stdout();

    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if forwarding.load(Ordering::Relaxed) {
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
    fn forwarding_flag_starts_false() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn forwarding_flag_toggles() {
        let flag = Arc::new(AtomicBool::new(false));
        flag.store(true, Ordering::SeqCst);
        assert!(flag.load(Ordering::Relaxed));
        flag.store(false, Ordering::SeqCst);
        assert!(!flag.load(Ordering::Relaxed));
    }
}
