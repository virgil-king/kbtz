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

TREE MODE KEYS:
    j/k, Up/Down   Navigate
    Enter           Zoom into session
    Tab             Jump to next session needing input
    s               Spawn session for task
    c               Switch to task manager session
    Space           Collapse/expand
    p               Pause/unpause task
    d               Mark task done
    U               Force-unassign task
    ?               Help
    q               Quit

ZOOMED MODE / TASK MANAGER:
    ^B t            Return to tree
    ^B c            Switch to task manager session
    ^B n/p          Next/prev session
    ^B Tab          Jump to next session needing input
    ^B [            Scroll mode (also: Shift+Up, PgUp, left-click)
    ^B ^B           Send literal Ctrl-B
    ^B ?            Help
    ^B q            Quit"
)]
struct Cli {
    /// Path to kbtz database [default: $KBTZ_DB or ~/.kbtz/kbtz.db]
    #[arg(long, env = "KBTZ_DB")]
    db: Option<String>,

    /// Max concurrent sessions [default: 8]
    #[arg(short = 'j', long)]
    concurrency: Option<usize>,

    /// Preference hint for task selection (FTS match)
    #[arg(long)]
    prefer: Option<String>,

    /// Agent backend to use for sessions [default: claude]
    #[arg(long)]
    backend: Option<String>,

    /// Override the backend's default command binary
    #[arg(long)]
    command: Option<String>,

    /// Disable automatic session spawning; use 's' in tree mode to spawn manually
    #[arg(long)]
    manual: bool,
}

const PREFIX_KEY: u8 = 0x02; // Ctrl-B

/// Watches the kbtz database and status directory for changes.
/// Polling with `poll()` checks both channels and refreshes app state.
struct Watchers {
    _db_watcher: notify::RecommendedWatcher,
    db_rx: std::sync::mpsc::Receiver<()>,
    _status_watcher: notify::RecommendedWatcher,
    status_rx: std::sync::mpsc::Receiver<()>,
}

impl Watchers {
    fn new(app: &App) -> Result<Self> {
        let (_db_watcher, db_rx) = kbtz::watch::watch_db(&app.db_path)?;
        let (_status_watcher, status_rx) = kbtz::watch::watch_dir(&app.status_dir)?;
        Ok(Watchers {
            _db_watcher,
            db_rx,
            _status_watcher,
            status_rx,
        })
    }

    fn poll(&self, app: &mut App) -> Result<()> {
        let db_event = kbtz::watch::wait_for_change(&self.db_rx, Duration::ZERO);
        if db_event {
            kbtz::watch::drain_events(&self.db_rx);
            kbtz::debug_log::log("watchers.poll: db event -> refresh_tree");
            app.refresh_tree()?;
        }
        if kbtz::watch::wait_for_change(&self.status_rx, Duration::ZERO) {
            kbtz::watch::drain_events(&self.status_rx);
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

/// Poll stdin with a 100ms timeout. Returns `Some(n)` if `n` bytes were
/// read (`0` means EOF/error), or `None` on timeout.
fn poll_stdin(stdin: &mut io::StdinLock, buf: &mut [u8]) -> Option<usize> {
    let stdin_fd = stdin.as_raw_fd();
    let mut pfd = libc::pollfd {
        fd: stdin_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 100) } <= 0 {
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
    let stdout = io::stdout();
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
    write!(stdout, "\x1b[1;{}r", app.term.rows - 1)?;
    execute!(
        stdout,
        terminal::Clear(terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    )?;

    let result = passthrough_loop(app, kind, running);

    // Reset scroll region and raw mode.
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[r")?;
    stdout.flush()?;
    terminal::disable_raw_mode()?;
    result
}

fn handle_prefix_command(
    cmd: u8,
    app: &mut App,
    kind: &SessionKind,
    stdin: &mut io::StdinLock,
    scroll: &mut ScrollState,
    last_status: &SessionStatus,
) -> Result<Option<Action>> {
    let sid = kind.session_id();
    let rows = app.term.rows;
    let cols = app.term.cols;

    match cmd {
        b't' | b'd' => {
            if scroll.active {
                exit_scroll_mode(app, sid, scroll)?;
            }
            Ok(Some(Action::ReturnToTree))
        }
        b'c' if !kind.is_toplevel() => {
            if scroll.active {
                exit_scroll_mode(app, sid, scroll)?;
            }
            Ok(Some(Action::TopLevel))
        }
        b'n' => {
            let next_task = match kind {
                SessionKind::Worker { task, .. } => app.cycle_session(&Action::NextSession, task),
                SessionKind::TopLevel => app
                    .session_ids_ordered()
                    .first()
                    .and_then(|sid| app.sessions.get(sid).map(|s| s.task_name().to_string())),
            };
            if let Some(next_task) = next_task {
                if scroll.active {
                    exit_scroll_mode(app, sid, scroll)?;
                }
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
                if scroll.active {
                    exit_scroll_mode(app, sid, scroll)?;
                }
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
                if scroll.active {
                    exit_scroll_mode(app, sid, scroll)?;
                }
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
        b'[' => {
            if !scroll.active {
                enter_scroll_mode(app, sid, scroll)?;
            }
            Ok(None)
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
            if scroll.active {
                draw_scroll_status_bar(rows, cols, scroll);
            } else {
                draw_normal_status_bar(rows, cols, kind, last_status, None);
            }
            Ok(None)
        }
        b'q' => {
            if scroll.active {
                exit_scroll_mode(app, sid, scroll)?;
            }
            Ok(Some(Action::Quit))
        }
        _ => Ok(None),
    }
}

// ── Scroll mode ────────────────────────────────────────────────────────

struct ScrollState {
    active: bool,
    offset: usize,
    total: usize,
}

impl ScrollState {
    fn new() -> Self {
        Self {
            active: false,
            offset: 0,
            total: 0,
        }
    }
}

/// Enter scroll mode.  Freezes the screen and disables mouse tracking
/// so the terminal can handle native text selection.  Works even when
/// there is no scrollback (the user can still select visible text).
fn enter_scroll_mode(app: &App, session_id: &str, scroll: &mut ScrollState) -> Result<()> {
    if let Some(session) = app.get_session(session_id) {
        scroll.total = session.enter_scroll_mode()?;
        scroll.offset = 0;
        scroll.active = true;
        // Disable mouse tracking so the terminal handles native text selection.
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = write!(out, "\x1b[?1000l\x1b[?1006l\x1b[?25h");
        let _ = out.flush();
        // Render the current viewport (offset 0 = live screen).
        session.render_scrollback(0, app.term.cols)?;
        draw_scroll_status_bar(app.term.rows, app.term.cols, scroll);
    }
    Ok(())
}

fn exit_scroll_mode(app: &App, session_id: &str, scroll: &mut ScrollState) -> Result<()> {
    scroll.active = false;
    scroll.offset = 0;
    if let Some(session) = app.get_session(session_id) {
        session.exit_scroll_mode()?;
    }
    Ok(())
}

fn scroll_to(
    app: &App,
    session_id: &str,
    scroll: &mut ScrollState,
    new_offset: usize,
) -> Result<()> {
    if let Some(session) = app.get_session(session_id) {
        scroll.offset = session.render_scrollback(new_offset, app.term.cols)?;
        // Update total in case more scrollback accumulated while scrolled.
        scroll.total = session.scrollback_available()?;
    }
    Ok(())
}

/// Handle input while in scroll mode.  Returns `Ok(true)` if the event
/// was consumed (caller should redraw scroll status bar), `Ok(false)` if
/// scroll mode was exited (caller should redraw normal status bar).
fn handle_scroll_input(
    app: &App,
    session_id: &str,
    scroll: &mut ScrollState,
    buf: &[u8],
    i: &mut usize,
    n: usize,
    rows: u16,
) -> Result<bool> {
    let page = (rows.saturating_sub(2)) as usize; // leave room for status bar

    // Check for CSI sequences (arrow keys, PgUp/PgDn, mouse, etc.)
    if buf[*i] == 0x1b && *i + 2 < n && buf[*i + 1] == b'[' {
        if buf[*i + 2] == b'A' {
            // Up arrow
            *i += 3;
            let new = scroll.offset.saturating_add(1).min(scroll.total);
            scroll_to(app, session_id, scroll, new)?;
            return Ok(true);
        }
        if buf[*i + 2] == b'B' {
            // Down arrow
            *i += 3;
            scroll_to(app, session_id, scroll, scroll.offset.saturating_sub(1))?;
            return Ok(true);
        }
        // Shift+Up (CSI 1;2A)
        if buf[*i + 2] == b'1'
            && *i + 5 < n
            && buf[*i + 3] == b';'
            && buf[*i + 4] == b'2'
            && buf[*i + 5] == b'A'
        {
            *i += 6;
            let new = scroll.offset.saturating_add(1).min(scroll.total);
            scroll_to(app, session_id, scroll, new)?;
            return Ok(true);
        }
        // Shift+Down (CSI 1;2B)
        if buf[*i + 2] == b'1'
            && *i + 5 < n
            && buf[*i + 3] == b';'
            && buf[*i + 4] == b'2'
            && buf[*i + 5] == b'B'
        {
            *i += 6;
            scroll_to(app, session_id, scroll, scroll.offset.saturating_sub(1))?;
            return Ok(true);
        }
        if buf[*i + 2] == b'5' && *i + 3 < n && buf[*i + 3] == b'~' {
            // Page Up
            *i += 4;
            let new = scroll.offset.saturating_add(page).min(scroll.total);
            scroll_to(app, session_id, scroll, new)?;
            return Ok(true);
        }
        if buf[*i + 2] == b'6' && *i + 3 < n && buf[*i + 3] == b'~' {
            // Page Down
            *i += 4;
            scroll_to(app, session_id, scroll, scroll.offset.saturating_sub(page))?;
            return Ok(true);
        }
        // Consume unrecognized CSI sequences
        *i += 1;
        return Ok(true);
    }

    match buf[*i] {
        b'q' | 0x1b => {
            *i += 1;
            exit_scroll_mode(app, session_id, scroll)?;
            Ok(false)
        }
        b'k' => {
            *i += 1;
            let new = scroll.offset.saturating_add(1).min(scroll.total);
            scroll_to(app, session_id, scroll, new)?;
            Ok(true)
        }
        b'j' => {
            *i += 1;
            scroll_to(app, session_id, scroll, scroll.offset.saturating_sub(1))?;
            Ok(true)
        }
        b'g' => {
            // Go to top of scrollback
            *i += 1;
            scroll_to(app, session_id, scroll, scroll.total)?;
            Ok(true)
        }
        b'G' => {
            // Go to bottom (exit scroll mode)
            *i += 1;
            exit_scroll_mode(app, session_id, scroll)?;
            Ok(false)
        }
        _ => {
            *i += 1;
            Ok(true) // consume unknown keys in scroll mode
        }
    }
}

struct SgrMouseEvent {
    button: u16,
    len: usize,
}

/// Try to parse an SGR mouse sequence starting at `buf[start]`.
/// Expected format: \x1b[<button;x;yM (or m for release).
/// Returns None if the sequence is incomplete.
fn parse_sgr_mouse_scroll(buf: &[u8], start: usize, n: usize) -> Option<SgrMouseEvent> {
    // We expect buf[start..start+3] == \x1b[<
    if start + 3 >= n {
        return None;
    }
    let mut pos = start + 3; // skip \x1b[<
                             // Parse button number (digits before first ';')
    let btn_start = pos;
    while pos < n && buf[pos].is_ascii_digit() {
        pos += 1;
    }
    if pos >= n || pos == btn_start {
        return None;
    }
    let button: u16 = std::str::from_utf8(&buf[btn_start..pos])
        .ok()?
        .parse()
        .ok()?;
    // Skip ;x;y and find M or m
    while pos < n && buf[pos] != b'M' && buf[pos] != b'm' {
        pos += 1;
    }
    if pos >= n {
        return None; // incomplete
    }
    pos += 1; // consume M/m
    Some(SgrMouseEvent {
        button,
        len: pos - start,
    })
}

/// Refresh the terminal after a resize or wake from sleep.
///
/// Exits scroll mode if active, updates dimensions if the terminal size
/// changed, re-establishes the scroll region, clears the screen, and does a
/// full VTE render.  Returns the new screen state (or None if the session
/// disappeared).
fn refresh_passthrough_screen(
    app: &mut App,
    kind: &SessionKind,
    last_status: &SessionStatus,
    scroll: &mut ScrollState,
) -> Result<Option<vt100::Screen>> {
    let sid = kind.session_id();
    if scroll.active {
        exit_scroll_mode(app, sid, scroll)?;
    }
    let (cols, rows) = terminal::size()?;
    if cols != app.term.cols || rows != app.term.rows {
        app.handle_resize(cols, rows);
    }
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[1;{}r", app.term.rows - 1)?;
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    )?;
    let prev = if let Some(session) = app.get_session(sid) {
        Some(session.render_screen_full()?)
    } else {
        None
    };
    draw_normal_status_bar(app.term.rows, app.term.cols, kind, last_status, None);
    Ok(prev)
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
    let mut scroll = ScrollState::new();

    let sid = kind.session_id();
    let watchers = Watchers::new(app)?;
    let mut debug_msg: Option<String> = None;
    let mut last_iter = Instant::now();

    // Initial full render of the session's VTE state.
    // `prev_screen` tracks the last rendered state for efficient diff updates.
    // When None, the next render will be a full render (state_formatted).
    let mut prev_screen: Option<vt100::Screen> = if let Some(session) = app.get_session(sid) {
        Some(session.render_screen_full()?)
    } else {
        None
    };

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
            prev_screen = refresh_passthrough_screen(app, kind, &last_status, &mut scroll)?;
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
        if redraw && !scroll.active {
            draw_normal_status_bar(
                app.term.rows,
                app.term.cols,
                kind,
                &last_status,
                debug_msg.take().as_deref(),
            );
        }

        // Render new output from the child's VTE.
        if !scroll.active {
            if let Some(session) = app.get_session(sid) {
                if session.has_new_output() {
                    prev_screen = Some(match prev_screen {
                        Some(ref prev) => session.render_screen(prev)?,
                        None => session.render_screen_full()?,
                    });
                }
            }
        }

        let n = match poll_stdin(&mut stdin, &mut buf) {
            None => continue,
            Some(0) => return Ok(Action::Quit),
            Some(n) => n,
        };

        let rows = app.term.rows;
        let cols = app.term.cols;

        let mut i = 0;
        while i < n {
            // ── Scroll mode input ──────────────────────────────────
            if scroll.active {
                // PREFIX_KEY commands still work in scroll mode
                if buf[i] == PREFIX_KEY {
                    i += 1;
                    let cmd = match read_prefix_cmd(&buf, &mut i, n, &mut stdin) {
                        Some(b) => b,
                        None => return Ok(Action::Quit),
                    };
                    if let Some(action) = handle_prefix_command(
                        cmd,
                        app,
                        kind,
                        &mut stdin,
                        &mut scroll,
                        &last_status,
                    )? {
                        return Ok(action);
                    }
                    if scroll.active {
                        draw_scroll_status_bar(rows, cols, &scroll);
                    } else {
                        // Scroll mode was exited; force full re-render.
                        prev_screen = None;
                        draw_normal_status_bar(rows, cols, kind, &last_status, None);
                    }
                    continue;
                }

                match handle_scroll_input(app, sid, &mut scroll, &buf, &mut i, n, rows)? {
                    true => draw_scroll_status_bar(rows, cols, &scroll),
                    false => {
                        // Scroll mode was exited; force full re-render.
                        prev_screen = None;
                        draw_normal_status_bar(rows, cols, kind, &last_status, None);
                    }
                }
                continue;
            }

            // ── Normal mode input ──────────────────────────────────

            // Check for SGR mouse events
            if buf[i] == 0x1b && i + 2 < n && buf[i + 1] == b'[' && buf[i + 2] == b'<' {
                if let Some(evt) = parse_sgr_mouse_scroll(&buf, i, n) {
                    if evt.button == 0 {
                        // Left click → enter scroll mode for text selection.
                        enter_scroll_mode(app, sid, &mut scroll)?;
                        if scroll.active {
                            draw_scroll_status_bar(rows, cols, &scroll);
                        }
                        i += evt.len;
                        continue;
                    }
                    // Forward other mouse events to child if it requested
                    // mouse tracking, otherwise discard.
                    if let Some(session) = app.get_session_mut(sid) {
                        if session.has_mouse_tracking() {
                            session.write_input(&buf[i..i + evt.len])?;
                        }
                    }
                    i += evt.len;
                    continue;
                }
            }

            // Check for Shift+Up → enter scroll mode and scroll up 1 line
            if buf[i] == 0x1b
                && i + 5 < n
                && buf[i + 1] == b'['
                && buf[i + 2] == b'1'
                && buf[i + 3] == b';'
                && buf[i + 4] == b'2'
                && buf[i + 5] == b'A'
            {
                enter_scroll_mode(app, sid, &mut scroll)?;
                if scroll.active {
                    let new = scroll.offset.saturating_add(1).min(scroll.total);
                    scroll_to(app, sid, &mut scroll, new)?;
                    draw_scroll_status_bar(rows, cols, &scroll);
                }
                i += 6;
                continue;
            }

            // Check for PgUp → enter scroll mode
            if buf[i] == 0x1b && i + 3 < n && buf[i + 1] == b'[' {
                let page = (rows.saturating_sub(2)) as usize;
                if buf[i + 2] == b'5' && buf[i + 3] == b'~' {
                    enter_scroll_mode(app, sid, &mut scroll)?;
                    if scroll.active {
                        let new = scroll.offset.saturating_add(page).min(scroll.total);
                        scroll_to(app, sid, &mut scroll, new)?;
                        draw_scroll_status_bar(rows, cols, &scroll);
                    }
                    i += 4;
                    continue;
                }
            }

            if buf[i] == PREFIX_KEY {
                i += 1;
                let cmd = match read_prefix_cmd(&buf, &mut i, n, &mut stdin) {
                    Some(b) => b,
                    None => return Ok(Action::Quit),
                };
                if let Some(action) =
                    handle_prefix_command(cmd, app, kind, &mut stdin, &mut scroll, &last_status)?
                {
                    return Ok(action);
                }
                if scroll.active {
                    draw_scroll_status_bar(rows, cols, &scroll);
                }
            } else {
                // Find the next PREFIX_KEY or ESC sequence we intercept,
                // and write the entire chunk to the PTY in one call.
                let start = i;
                while i < n && buf[i] != PREFIX_KEY {
                    if buf[i] == 0x1b && i + 2 < n && buf[i + 1] == b'[' {
                        // Stop before SGR mouse sequence
                        if buf[i + 2] == b'<' {
                            break;
                        }
                        // Stop before Shift+Up
                        if buf[i + 2] == b'1'
                            && i + 5 < n
                            && buf[i + 3] == b';'
                            && buf[i + 4] == b'2'
                            && buf[i + 5] == b'A'
                        {
                            break;
                        }
                        // Stop before PgUp
                        if buf[i + 2] == b'5' && i + 3 < n && buf[i + 3] == b'~' {
                            break;
                        }
                    }
                    i += 1;
                }
                if i > start {
                    if let Some(session) = app.get_session_mut(sid) {
                        session.write_input(&buf[start..i])?;
                    }
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

fn draw_scroll_status_bar(rows: u16, cols: u16, scroll: &ScrollState) {
    let content = format!(
        " [SCROLL] line {}/{}  q:exit  k/\u{2191}/S-\u{2191}:up  j/\u{2193}:down  PgUp/PgDn  g/G:top/bot  click+drag:select",
        scroll.offset, scroll.total,
    );
    let padding = (cols as usize).saturating_sub(content.len());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = write!(
        out,
        "\x1b7\x1b[{rows};1H\x1b[7;33m{content}{:padding$}\x1b[0m\x1b8",
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
            " ^B t:tree  ^B n:next worker  ^B p:prev worker  ^B Tab:input  ^B [:scroll  ^B ^B:send ^B  ^B q:quit  ^B ?:help"
        }
        SessionKind::Worker { .. } => {
            " ^B t:tree  ^B c:manager  ^B n:next  ^B p:prev  ^B Tab:input  ^B [:scroll  ^B ^B:send ^B  ^B q:quit  ^B ?:help"
        }
    };
    draw_bar(rows, cols, "7;33", content, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sgr_scroll_up() {
        let buf = b"\x1b[<64;10;5M";
        let evt = parse_sgr_mouse_scroll(buf, 0, buf.len()).unwrap();
        assert_eq!(evt.button, 64);
        assert_eq!(evt.len, buf.len());
    }

    #[test]
    fn parse_sgr_scroll_down() {
        let buf = b"\x1b[<65;10;5M";
        let evt = parse_sgr_mouse_scroll(buf, 0, buf.len()).unwrap();
        assert_eq!(evt.button, 65);
        assert_eq!(evt.len, buf.len());
    }

    #[test]
    fn parse_sgr_click() {
        let buf = b"\x1b[<0;10;5M";
        let evt = parse_sgr_mouse_scroll(buf, 0, buf.len()).unwrap();
        assert_eq!(evt.button, 0);
    }

    #[test]
    fn parse_sgr_release() {
        // SGR release uses lowercase 'm'
        let buf = b"\x1b[<0;10;5m";
        let evt = parse_sgr_mouse_scroll(buf, 0, buf.len()).unwrap();
        assert_eq!(evt.button, 0);
        assert_eq!(evt.len, buf.len());
    }

    #[test]
    fn parse_sgr_incomplete_returns_none() {
        // Missing terminator
        let buf = b"\x1b[<64;10;5";
        assert!(parse_sgr_mouse_scroll(buf, 0, buf.len()).is_none());
    }

    #[test]
    fn parse_sgr_too_short_returns_none() {
        let buf = b"\x1b[<";
        assert!(parse_sgr_mouse_scroll(buf, 0, buf.len()).is_none());
    }

    #[test]
    fn parse_sgr_no_button_digits_returns_none() {
        let buf = b"\x1b[<;10;5M";
        assert!(parse_sgr_mouse_scroll(buf, 0, buf.len()).is_none());
    }

    #[test]
    fn parse_sgr_at_offset() {
        let buf = b"xxxxx\x1b[<64;1;1M";
        let evt = parse_sgr_mouse_scroll(buf, 5, buf.len()).unwrap();
        assert_eq!(evt.button, 64);
        assert_eq!(evt.len, buf.len() - 5);
    }

    #[test]
    fn parse_sgr_large_coordinates() {
        let buf = b"\x1b[<64;200;100M";
        let evt = parse_sgr_mouse_scroll(buf, 0, buf.len()).unwrap();
        assert_eq!(evt.button, 64);
        assert_eq!(evt.len, buf.len());
    }
}
