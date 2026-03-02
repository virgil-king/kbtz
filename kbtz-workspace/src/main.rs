mod app;
mod backend;
mod config;
mod lifecycle;
mod prompt;
mod session;
mod shepherd_session;
mod tree;

use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self as ct_event, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;

use app::{Action, App, TOPLEVEL_SESSION_ID};
use session::SessionStatus;

/// Identifies the session that a passthrough loop is attached to.
enum SessionKind<'a> {
    TopLevel,
    Worker { session_id: &'a str, task: &'a str },
}

impl SessionKind<'_> {
    fn session_id(&self) -> &str {
        match self {
            SessionKind::TopLevel => TOPLEVEL_SESSION_ID,
            SessionKind::Worker { session_id, .. } => session_id,
        }
    }

    fn is_toplevel(&self) -> bool {
        matches!(self, SessionKind::TopLevel)
    }
}

#[derive(Parser)]
#[command(
    name = "kbtz-workspace",
    about = "Task workspace for kbtz",
    after_help = "\
CONFIG FILE:
    Settings are loaded from ~/.kbtz/workspace.toml (if it exists).
    CLI args take precedence over config values. Example:

        [workspace]
        concurrency = 3
        backend = \"claude\"

        [agent.claude]
        command = \"/usr/local/bin/claude\"
        args = [\"--verbose\"]

USAGE:
    Start a workspace for the default database (~/.kbtz/kbtz.db):
        kbtz-workspace

    Start with a specific database and concurrency:
        kbtz-workspace --db ./project.db --concurrency 4"
)]
struct Cli {
    /// Path to kbtz database
    #[arg(long)]
    db: Option<String>,

    /// Maximum number of concurrent agent sessions
    #[arg(long)]
    concurrency: Option<usize>,

    /// Backend (agent runtime) to use
    #[arg(long)]
    backend: Option<String>,

    /// Custom agent command (overrides backend default)
    #[arg(long)]
    command: Option<String>,

    /// Start in manual mode (no auto-spawning)
    #[arg(long)]
    manual: bool,

    /// Prefer tasks matching this prefix when auto-spawning
    #[arg(long)]
    prefer: Option<String>,

    /// Enable persistent sessions (survive workspace restart)
    #[arg(long)]
    persistent_sessions: bool,
}

/// Prefix key for passthrough commands (Ctrl-B, like tmux).
const PREFIX_KEY: u8 = 0x02;

/// Watches the kbtz database and status files for changes.
struct Watchers {
    _db_watcher: notify::RecommendedWatcher,
    _status_watcher: notify::RecommendedWatcher,
    rx: std::sync::mpsc::Receiver<std::result::Result<notify::Event, notify::Error>>,
}

impl Watchers {
    fn new(app: &App) -> Result<Self> {
        use notify::Watcher;
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();

        let mut db_watcher =
            notify::recommended_watcher(move |res: std::result::Result<notify::Event, _>| {
                let _ = tx.send(res);
            })
            .context("failed to create DB watcher")?;
        db_watcher
            .watch(
                std::path::Path::new(&app.db_path),
                notify::RecursiveMode::NonRecursive,
            )
            .context("failed to watch database file")?;

        let mut status_watcher =
            notify::recommended_watcher(move |res: std::result::Result<notify::Event, _>| {
                let _ = tx2.send(res);
            })
            .context("failed to create status watcher")?;
        status_watcher
            .watch(&app.status_dir, notify::RecursiveMode::NonRecursive)
            .context("failed to watch status directory")?;

        Ok(Self {
            _db_watcher: db_watcher,
            _status_watcher: status_watcher,
            rx,
        })
    }

    /// Drain all pending filesystem events and refresh app state if anything changed.
    fn poll(&self, app: &mut App) -> Result<()> {
        let mut changed = false;
        while self.rx.try_recv().is_ok() {
            changed = true;
        }
        if changed {
            app.refresh_tree()?;
            app.read_status_files()?;
        }
        Ok(())
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("kbtz-workspace: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = config::Config::load()?;

    let db_path = cli.db.unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.kbtz/kbtz.db")
    });

    // Ensure the parent directory exists for DB auto-creation
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).context("failed to create database directory")?;
    }

    // Status directory for session state files
    let status_dir = PathBuf::from(std::env::var("KBTZ_WORKSPACE_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.kbtz/workspace")
    }));
    std::fs::create_dir_all(&status_dir).context("failed to create status directory")?;

    // Acquire exclusive lock on the status directory to prevent concurrent instances.
    let lock_path = status_dir.join("workspace.lock");
    let _lock_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&lock_path)
        .context("failed to create lock file")?;
    let lock_fd = _lock_file.as_raw_fd();
    let lock_result = unsafe { libc::flock(lock_fd, libc::LOCK_EX | libc::LOCK_NB) };
    if lock_result != 0 {
        anyhow::bail!(
            "another kbtz-workspace instance is already running on this database. \
             If this is incorrect, remove {}",
            lock_path.display()
        );
    }
    // _lock_file must stay alive for the duration of run() — the lock is released when the fd is closed.

    let (cols, rows) = terminal::size().context("failed to get terminal size")?;

    // Disable focus event reporting that may be left over from a previous
    // session.  Some terminals (notably on macOS) keep DECSET 1004 enabled
    // across process boundaries, so a prior child that requested focus
    // events causes the terminal to send CSI I / CSI O into our stdin
    // before we enter raw mode — which gets echoed as "^[[I" / "^[[O".
    let _ = write!(io::stdout(), "\x1b[?1004l");
    let _ = io::stdout().flush();

    // Merge: CLI > config > defaults
    let ws = config.workspace;
    let concurrency = cli.concurrency.or(ws.concurrency).unwrap_or(8);
    let manual = cli.manual || ws.manual.unwrap_or(false);
    let prefer = cli.prefer.or(ws.prefer);
    let backend_name = cli
        .backend
        .or(ws.backend)
        .unwrap_or_else(|| "claude".into());
    let persistent_sessions = cli.persistent_sessions || ws.persistent_sessions.unwrap_or(false);

    let agent_config = config.agent.get(&backend_name);
    let command_override = cli
        .command
        .as_deref()
        .or_else(|| agent_config.and_then(|a| a.binary()));
    // CLI --command is a plain string override with no prefix args.
    let prefix_args: Vec<String> = if cli.command.is_some() {
        vec![]
    } else {
        agent_config
            .map(|a| a.prefix_args().to_vec())
            .unwrap_or_default()
    };
    let extra_args: Vec<String> = agent_config.map(|a| a.args.clone()).unwrap_or_default();

    let backend = backend::from_name(&backend_name, command_override, &prefix_args, &extra_args)?;

    let mut app = App::new(
        db_path,
        status_dir,
        concurrency,
        manual,
        prefer,
        backend,
        app::TermSize { rows, cols },
        persistent_sessions,
    )?;

    // Initial session spawning
    app.tick()?;

    // Set up Ctrl+C handler for graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .context("failed to set Ctrl+C handler")?;

    // Enter alternate screen once for the entire session.
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Main loop
    let result = main_loop(&mut app, &running);

    // Graceful shutdown
    app.shutdown();

    // Leave alternate screen and clean up terminal state.
    let mut stdout = io::stdout();
    let _ = write!(
        stdout,
        concat!(
            "\x1b[r",      // reset scroll region
            "\x1b[?1004l", // disable focus event reporting
        )
    );
    let _ = execute!(stdout, crossterm::cursor::Show, LeaveAlternateScreen,);

    result
}

fn main_loop(app: &mut App, running: &Arc<AtomicBool>) -> Result<()> {
    let mut action = Action::Continue;

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(());
        }

        match action {
            Action::Continue | Action::ReturnToTree => {
                action = tree_mode(app, running)?;
            }
            Action::ZoomIn(ref task) => {
                let task = task.clone();
                let sid = match app.task_to_session.get(&task) {
                    Some(sid) => sid.clone(),
                    None => {
                        action = Action::ReturnToTree;
                        continue;
                    }
                };
                let kind = SessionKind::Worker {
                    session_id: &sid,
                    task: &task,
                };
                action = passthrough_mode(app, &kind, running)?;
            }
            Action::TopLevel => {
                action = passthrough_mode(app, &SessionKind::TopLevel, running)?;
            }
            Action::NextSession | Action::PrevSession => {
                // Shouldn't happen at top level, treat as tree
                action = Action::Continue;
            }
            Action::Quit => return Ok(()),
        }
    }
}

// ── Stdin helpers ─────────────────────────────────────────────────────

/// Poll stdin with a 16ms (~60 fps) timeout. Returns `Some(n)` if `n`
/// bytes were read (`0` means EOF/error), or `None` on timeout.
fn poll_stdin(stdin: &mut io::StdinLock, buf: &mut [u8]) -> Option<usize> {
    let stdin_fd = stdin.as_raw_fd();
    let mut pfd = libc::pollfd {
        fd: stdin_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 16) } <= 0 {
        return None;
    }
    match stdin.read(buf) {
        Ok(n) if n > 0 => Some(n),
        _ => Some(0),
    }
}

/// Read the command byte after a PREFIX_KEY, either from the remaining
/// buffer or by reading one more byte from stdin. Returns `None` on
/// EOF/error.
fn read_prefix_cmd(buf: &[u8], i: &mut usize, n: usize, stdin: &mut io::StdinLock) -> Option<u8> {
    if *i < n {
        let b = buf[*i];
        *i += 1;
        Some(b)
    } else {
        let mut cmd_buf = [0u8; 1];
        match stdin.read(&mut cmd_buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(cmd_buf[0]),
        }
    }
}

// ── Tree mode ──────────────────────────────────────────────────────────

fn tree_mode(app: &mut App, running: &Arc<AtomicBool>) -> Result<Action> {
    terminal::enable_raw_mode()?;
    // Reset scroll region and clear stale passthrough content so ratatui
    // starts with a clean slate.
    //
    // Use CSI H + CSI J (home + erase from cursor to end) instead of
    // CSI 2 J (erase entire display).  Some terminal emulators (iTerm2,
    // Terminal.app) save the current screen to their scrollback buffer
    // when they receive CSI 2 J on the alt screen, causing duplicate
    // content in the terminal's scrollback.  CSI 0 J does not trigger
    // this behaviour.
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[r\x1b[H\x1b[J")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = tree_loop(&mut terminal, app, running);

    terminal::disable_raw_mode()?;

    result
}

enum TreeMode {
    Normal,
    Help,
    ConfirmDone(String),
    ConfirmPause(String),
}

fn tree_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    running: &Arc<AtomicBool>,
) -> Result<Action> {
    // Catch any DB changes that happened during zoomed mode.
    app.refresh_tree()?;
    app.tick()?;

    let watchers = Watchers::new(app)?;
    let mut mode = TreeMode::Normal;

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(Action::Quit);
        }

        terminal.draw(|frame| {
            tree::render(frame, app);
            match &mode {
                TreeMode::Help => tree::render_help(frame),
                TreeMode::ConfirmDone(name) => {
                    tree::render_confirm(frame, "Done", name);
                }
                TreeMode::ConfirmPause(name) => {
                    tree::render_confirm(frame, "Pause", name);
                }
                TreeMode::Normal => {}
            }
        })?;

        if ct_event::poll(Duration::from_millis(100))? {
            let event = ct_event::read()?;
            if let Event::Resize(cols, rows) = event {
                app.handle_resize(cols, rows);
                continue;
            }
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if matches!(mode, TreeMode::Help) {
                    match key.code {
                        KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                            mode = TreeMode::Normal;
                        }
                        _ => {}
                    }
                    continue;
                }

                match &mode {
                    TreeMode::ConfirmDone(name) => {
                        let name = name.clone();
                        if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                            kbtz::debug_log::log(&format!("confirm done: {name}"));
                            if let Err(e) = kbtz::ops::mark_done(&app.conn, &name) {
                                app.tree.error = Some(e.to_string());
                            }
                            app.refresh_tree()?;
                            kbtz::debug_log::log("confirm done: refresh_tree complete");
                        }
                        mode = TreeMode::Normal;
                        continue;
                    }
                    TreeMode::ConfirmPause(name) => {
                        let name = name.clone();
                        if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                            kbtz::debug_log::log(&format!("confirm pause: {name}"));
                            if let Err(e) = kbtz::ops::pause_task(&app.conn, &name) {
                                app.tree.error = Some(e.to_string());
                            }
                            app.refresh_tree()?;
                            kbtz::debug_log::log("confirm pause: refresh_tree complete");
                        }
                        mode = TreeMode::Normal;
                        continue;
                    }
                    _ => {}
                }

                app.tree.error = None;

                match key.code {
                    KeyCode::Char('q') => {
                        return Ok(Action::Quit);
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.move_down();
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.move_up();
                    }
                    KeyCode::Char(' ') => {
                        app.toggle_collapse();
                        app.refresh_tree()?;
                    }
                    KeyCode::Enter => {
                        if let Some(name) = app.selected_name() {
                            if app.task_to_session.contains_key(name) {
                                return Ok(Action::ZoomIn(name.to_string()));
                            } else {
                                app.tree.error = Some("no active session for this task".into());
                            }
                        }
                    }
                    KeyCode::Char('p') => {
                        if let Some(name) = app.selected_name() {
                            let name = name.to_string();
                            let status = app.tree.rows[app.tree.cursor].status.as_str();
                            let result = match status {
                                "paused" => kbtz::ops::unpause_task(&app.conn, &name),
                                "open" => kbtz::ops::pause_task(&app.conn, &name),
                                "active" => {
                                    mode = TreeMode::ConfirmPause(name);
                                    continue;
                                }
                                _ => {
                                    app.tree.error = Some(format!("cannot pause {status} task"));
                                    Ok(())
                                }
                            };
                            match result {
                                Ok(()) => app.refresh_tree()?,
                                Err(e) => app.tree.error = Some(e.to_string()),
                            }
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(name) = app.selected_name() {
                            let name = name.to_string();
                            let status = app.tree.rows[app.tree.cursor].status.as_str();
                            match status {
                                "done" => {
                                    app.tree.error = Some("task is already done".into());
                                }
                                "active" => {
                                    mode = TreeMode::ConfirmDone(name);
                                    continue;
                                }
                                _ => match kbtz::ops::mark_done(&app.conn, &name) {
                                    Ok(()) => app.refresh_tree()?,
                                    Err(e) => app.tree.error = Some(e.to_string()),
                                },
                            }
                        }
                    }
                    KeyCode::Char('U') => {
                        if let Some(name) = app.selected_name() {
                            let name = name.to_string();
                            match kbtz::ops::force_unassign_task(&app.conn, &name) {
                                Ok(()) => app.refresh_tree()?,
                                Err(e) => app.tree.error = Some(e.to_string()),
                            }
                        }
                    }
                    KeyCode::Char('s') => {
                        if let Some(name) = app.selected_name() {
                            let name = name.to_string();
                            if let Err(e) = app.spawn_for_task(&name) {
                                app.tree.error = Some(e.to_string());
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        if let Some(name) = app.selected_name() {
                            let name = name.to_string();
                            app.restart_session(&name);
                        }
                    }
                    KeyCode::Tab => {
                        if let Some(task) = app.next_needs_input_session(None) {
                            return Ok(Action::ZoomIn(task));
                        } else {
                            app.tree.error = Some("no sessions need input".into());
                        }
                    }
                    KeyCode::Char('c') => {
                        return Ok(Action::TopLevel);
                    }
                    KeyCode::Char('?') => {
                        mode = TreeMode::Help;
                    }
                    _ => {}
                }
            }
        }

        watchers.poll(app)?;
        if let Some(desc) = app.tick()? {
            kbtz::debug_log::log(&format!("tick: {desc}"));
        }
    }
}

// ── Passthrough mode (unified for both worker and toplevel sessions) ──

fn passthrough_mode(
    app: &mut App,
    kind: &SessionKind,
    running: &Arc<AtomicBool>,
) -> Result<Action> {
    if kind.is_toplevel() {
        app.ensure_toplevel()?;
    }

    // Set up passthrough screen: raw mode, scroll region for status bar.
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[1;{}r\x1b[H\x1b[J", app.term.rows - 1)?;
    stdout.flush()?;

    // Enable raw byte forwarding from the reader thread to stdout.
    if let Some(session) = app.get_session(kind.session_id()) {
        session.start_forwarding();
    }

    // Trigger SIGWINCH so the child repaints at the current terminal size.
    // Use a two-step resize (size-1 then size) to guarantee a repaint
    // even if the size didn't actually change.
    trigger_repaint(app, kind.session_id());

    let result = passthrough_loop(app, kind, running);

    // Stop raw byte forwarding before leaving passthrough mode.
    if let Some(session) = app.get_session(kind.session_id()) {
        session.stop_forwarding();
    }

    // Reset scroll region and raw mode.
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[r")?;
    stdout.flush()?;
    terminal::disable_raw_mode()?;
    result
}

/// Trigger a SIGWINCH-based repaint by doing a two-step resize.
/// This guarantees the child repaints even if the size didn't change.
fn trigger_repaint(app: &App, session_id: &str) {
    if let Some(session) = app.get_session(session_id) {
        // Resize to (rows-1, cols) then back to (rows, cols).
        // The intermediate size differs, so the child sees SIGWINCH.
        let _ = session.resize(app.term.rows.saturating_sub(1), app.term.cols);
        let _ = session.resize(app.term.rows, app.term.cols);
    }
}

fn handle_prefix_command(
    cmd: u8,
    app: &mut App,
    kind: &SessionKind,
    stdin: &mut io::StdinLock,
    last_status: &SessionStatus,
) -> Result<Option<Action>> {
    let sid = kind.session_id();
    let rows = app.term.rows;
    let cols = app.term.cols;

    match cmd {
        b't' | b'd' => Ok(Some(Action::ReturnToTree)),
        b'c' if !kind.is_toplevel() => Ok(Some(Action::TopLevel)),
        b'n' => {
            let next_task = match kind {
                SessionKind::Worker { task, .. } => app.cycle_session(&Action::NextSession, task),
                SessionKind::TopLevel => app
                    .session_ids_ordered()
                    .first()
                    .and_then(|sid| app.sessions.get(sid).map(|s| s.task_name().to_string())),
            };
            if let Some(next_task) = next_task {
                Ok(Some(Action::ZoomIn(next_task)))
            } else {
                Ok(None)
            }
        }
        b'p' => {
            let prev_task = match kind {
                SessionKind::Worker { task, .. } => app.cycle_session(&Action::PrevSession, task),
                SessionKind::TopLevel => app
                    .session_ids_ordered()
                    .last()
                    .and_then(|sid| app.sessions.get(sid).map(|s| s.task_name().to_string())),
            };
            if let Some(prev_task) = prev_task {
                Ok(Some(Action::ZoomIn(prev_task)))
            } else {
                Ok(None)
            }
        }
        b'\t' => {
            let exclude = match kind {
                SessionKind::Worker { task, .. } => Some(*task),
                SessionKind::TopLevel => None,
            };
            if let Some(next_task) = app.next_needs_input_session(exclude) {
                Ok(Some(Action::ZoomIn(next_task)))
            } else {
                draw_normal_status_bar(
                    rows,
                    cols,
                    kind,
                    last_status,
                    Some("no sessions need input"),
                );
                Ok(None)
            }
        }
        PREFIX_KEY => {
            if let Some(session) = app.get_session_mut(sid) {
                session.write_input(&[PREFIX_KEY])?;
            }
            Ok(None)
        }
        b'?' => {
            draw_help_bar(rows, cols, kind);
            let mut discard = [0u8; 1];
            let _ = stdin.read(&mut discard);
            draw_normal_status_bar(rows, cols, kind, last_status, None);
            Ok(None)
        }
        b'q' => Ok(Some(Action::Quit)),
        _ => Ok(None),
    }
}


/// Refresh the terminal after a resize or wake from sleep.
///
/// Updates dimensions if the terminal size changed, re-establishes
/// the scroll region, clears the screen, and triggers a SIGWINCH
/// so the child repaints.
fn refresh_passthrough_screen(
    app: &mut App,
    kind: &SessionKind,
    last_status: &SessionStatus,
) -> Result<()> {
    let sid = kind.session_id();
    let (cols, rows) = terminal::size()?;
    if cols != app.term.cols || rows != app.term.rows {
        app.handle_resize(cols, rows);
    }
    // Stop forwarding, re-establish the scroll region, clear screen,
    // then resume forwarding and trigger repaint.
    if let Some(session) = app.get_session(sid) {
        session.stop_forwarding();
    }
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[1;{}r\x1b[H\x1b[J", app.term.rows - 1)?;
    stdout.flush()?;
    if let Some(session) = app.get_session(sid) {
        session.start_forwarding();
    }
    trigger_repaint(app, sid);
    draw_normal_status_bar(app.term.rows, app.term.cols, kind, last_status, None);
    Ok(())
}

/// Duration threshold for detecting a sleep/wake cycle.  If the main loop
/// iteration takes longer than this, the system was likely sleeping and
/// the terminal needs a full refresh.
const SLEEP_THRESHOLD: Duration = Duration::from_secs(2);

// ── Passthrough loop ──────────────────────────────────────────────────

fn passthrough_loop(
    app: &mut App,
    kind: &SessionKind,
    running: &Arc<AtomicBool>,
) -> Result<Action> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut buf = [0u8; 4096];
    let mut last_status = SessionStatus::Starting;

    let sid = kind.session_id();
    let watchers = Watchers::new(app)?;
    let mut debug_msg: Option<String> = None;
    let mut last_iter = Instant::now();

    draw_normal_status_bar(app.term.rows, app.term.cols, kind, &last_status, None);

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(Action::Quit);
        }

        // Check session liveness (is_alive needs &mut).
        if app.get_session_mut(sid).is_none_or(|s| !s.is_alive()) {
            return Ok(Action::ReturnToTree);
        }

        watchers.poll(app)?;

        // Run lifecycle tick (reaps exited, enforces timeouts, spawns)
        if let Some(msg) = app.tick()? {
            debug_msg = Some(msg);
        }

        // Re-check after tick (session may have been removed).
        if app.get_session(sid).is_none() {
            return Ok(Action::ReturnToTree);
        }

        // Detect terminal resize or wake from sleep.  The passthrough loop
        // bypasses crossterm's event system (using raw libc::poll), so
        // SIGWINCH is not delivered as a Resize event.  We detect resize
        // by polling terminal::size() each iteration, and detect sleep
        // via a time-jump (the 100ms poll timeout took >2s).
        let elapsed = last_iter.elapsed();
        last_iter = Instant::now();
        let (cur_cols, cur_rows) = terminal::size()?;
        let size_changed = cur_cols != app.term.cols || cur_rows != app.term.rows;
        if size_changed || elapsed > SLEEP_THRESHOLD {
            kbtz::debug_log::log(&format!(
                "passthrough refresh: size_changed={size_changed} elapsed={elapsed:?}"
            ));
            refresh_passthrough_screen(app, kind, &last_status)?;
            if app.get_session(sid).is_none() {
                return Ok(Action::ReturnToTree);
            }
        }

        // Redraw status bar when status or debug info changes.
        let mut redraw = false;
        if let SessionKind::Worker { session_id, .. } = kind {
            if let Some(session) = app.sessions.get(*session_id) {
                let status = session.status().clone();
                if status != last_status {
                    last_status = status;
                    redraw = true;
                }
            }
        }
        if debug_msg.is_some() {
            redraw = true;
        }
        if redraw {
            draw_normal_status_bar(
                app.term.rows,
                app.term.cols,
                kind,
                &last_status,
                debug_msg.take().as_deref(),
            );
        }

        let n = match poll_stdin(&mut stdin, &mut buf) {
            None => continue,
            Some(0) => return Ok(Action::Quit),
            Some(n) => n,
        };

        let mut i = 0;
        while i < n {
            if buf[i] == PREFIX_KEY {
                i += 1;
                let cmd = match read_prefix_cmd(&buf, &mut i, n, &mut stdin) {
                    Some(b) => b,
                    None => return Ok(Action::Quit),
                };
                if let Some(action) =
                    handle_prefix_command(cmd, app, kind, &mut stdin, &last_status)?
                {
                    return Ok(action);
                }
            } else {
                // Find the next PREFIX_KEY and write the entire chunk
                // to the PTY in one call.
                let start = i;
                while i < n && buf[i] != PREFIX_KEY {
                    i += 1;
                }
                if let Some(session) = app.get_session_mut(sid) {
                    session.write_input(&buf[start..i])?;
                }
            }
        }
    }
}

/// Draws a full-width bar on the last terminal row.
///
/// `style` is the ANSI SGR parameter string (e.g. "7" for reverse, "7;33" for
/// reverse+yellow). `left` is the primary content, and `right` is an optional
/// right-aligned annotation rendered as `" [right]"`.
fn draw_bar(rows: u16, cols: u16, style: &str, left: &str, right: Option<&str>) {
    let content = if let Some(r) = right {
        let right_str = format!(" [{r}]");
        let gap = (cols as usize).saturating_sub(left.len() + right_str.len());
        format!("{left}{:gap$}{right_str}", "")
    } else {
        left.to_string()
    };
    let padding = (cols as usize).saturating_sub(content.len());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = write!(
        out,
        "\x1b7\x1b[{rows};1H\x1b[{style}m{content}{:padding$}\x1b[0m\x1b8",
        "",
    );
    let _ = out.flush();
}

fn draw_normal_status_bar(
    rows: u16,
    cols: u16,
    kind: &SessionKind,
    status: &SessionStatus,
    debug: Option<&str>,
) {
    let left = match kind {
        SessionKind::TopLevel => " ^B ? help \u{2502} task manager".to_string(),
        SessionKind::Worker { task, session_id } => {
            format!(
                " ^B ? help │ {} ({}) │ {} {}",
                task,
                session_id,
                status.indicator(),
                status.label(),
            )
        }
    };
    draw_bar(rows, cols, "7", &left, debug);
}

fn draw_help_bar(rows: u16, cols: u16, kind: &SessionKind) {
    let content = match kind {
        SessionKind::TopLevel => {
            " ^B t:tree  ^B n:next worker  ^B p:prev worker  ^B Tab:input  ^B ^B:send ^B  ^B q:quit  ^B ?:help"
        }
        SessionKind::Worker { .. } => {
            " ^B t:tree  ^B c:manager  ^B n:next  ^B p:prev  ^B Tab:input  ^B ^B:send ^B  ^B q:quit  ^B ?:help"
        }
    };
    draw_bar(rows, cols, "7;33", content, None);
}

