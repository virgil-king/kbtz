mod app;
mod backend;
mod config;
mod lifecycle;
mod prompt;
mod session;
mod tree;

use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self as ct_event, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;

use app::{Action, App};
use session::SessionStatus;

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
    ^B [            Scroll mode (also: scroll wheel, PgUp/PgDn)
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

    // Main loop
    let result = main_loop(&mut app, &running);

    // Graceful shutdown
    app.shutdown();

    // Clear the terminal so the user returns to a clean shell prompt
    // instead of stale rendering artifacts from zoomed/toplevel mode.
    let mut stdout = io::stdout();
    let _ = write!(
        stdout,
        concat!(
            "\x1b[r",      // reset scroll region
            "\x1b[?1004l", // disable focus event reporting
        )
    );
    let _ = execute!(
        stdout,
        crossterm::cursor::Show,
        terminal::Clear(terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    );

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
                action = zoomed_mode(app, &task, running)?;
            }
            Action::TopLevel => {
                action = toplevel_mode(app, running)?;
            }
            Action::NextSession | Action::PrevSession => {
                // Shouldn't happen at top level, treat as tree
                action = Action::Continue;
            }
            Action::Quit => return Ok(()),
        }
    }
}

// ── Passthrough screen helpers ────────────────────────────────────────

/// Set up the terminal for passthrough mode: raw mode, scroll region
/// protecting the last row (for the status bar), and clear screen.
///
/// Does NOT enter alternate screen — the child (e.g. Claude Code)
/// manages its own alternate screen.  Adding a second layer would
/// prevent shift+pgup/pgdn from reaching the child (the terminal
/// intercepts them for scrollback, which is empty in alt screen) and
/// would conflict when the child's alt-screen escapes are forwarded.
fn enter_passthrough_screen(rows: u16) -> Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[1;{}r", rows - 1)?;
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0)
    )?;
    Ok(())
}

/// Tear down passthrough mode: reset scroll region and disable raw mode.
fn leave_passthrough_screen() -> Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[r")?;
    stdout.flush()?;
    terminal::disable_raw_mode()?;
    Ok(())
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
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = tree_loop(&mut terminal, app, running);

    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

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

// ── Zoomed mode ────────────────────────────────────────────────────────

fn zoomed_mode(app: &mut App, task: &str, running: &Arc<AtomicBool>) -> Result<Action> {
    let session_id = match app.task_to_session.get(task) {
        Some(sid) => sid.clone(),
        None => return Ok(Action::ReturnToTree),
    };

    enter_passthrough_screen(app.term.rows)?;

    // Start passthrough: flushes buffered output to stdout (everything
    // the child wrote while we were in tree mode), then goes live.
    if let Some(session) = app.sessions.get(&session_id) {
        session.start_passthrough()?;
    }

    let result = zoomed_loop(app, task, &session_id, running);

    // Stop passthrough (resets input modes like mouse tracking)
    if let Some(session) = app.sessions.get(&session_id) {
        let _ = session.stop_passthrough();
    }

    leave_passthrough_screen()?;
    result
}

fn handle_zoomed_prefix_command(
    cmd: u8,
    app: &mut App,
    session_id: &str,
    task: &str,
    stdin: &mut io::StdinLock,
    last_status: &SessionStatus,
    scroll: &mut ScrollState,
) -> Result<Option<Action>> {
    match cmd {
        b't' | b'd' => {
            if scroll.active {
                exit_scroll_mode(app, session_id, scroll)?;
            }
            Ok(Some(Action::ReturnToTree))
        }
        b'c' => {
            if scroll.active {
                exit_scroll_mode(app, session_id, scroll)?;
            }
            if let Some(session) = app.sessions.get(session_id) {
                let _ = session.stop_passthrough();
            }
            Ok(Some(Action::TopLevel))
        }
        b'n' => {
            if let Some(next_task) = app.cycle_session(&Action::NextSession, task) {
                if scroll.active {
                    exit_scroll_mode(app, session_id, scroll)?;
                }
                if let Some(session) = app.sessions.get(session_id) {
                    let _ = session.stop_passthrough();
                }
                Ok(Some(Action::ZoomIn(next_task)))
            } else {
                Ok(None)
            }
        }
        b'p' => {
            if let Some(prev_task) = app.cycle_session(&Action::PrevSession, task) {
                if scroll.active {
                    exit_scroll_mode(app, session_id, scroll)?;
                }
                if let Some(session) = app.sessions.get(session_id) {
                    let _ = session.stop_passthrough();
                }
                Ok(Some(Action::ZoomIn(prev_task)))
            } else {
                Ok(None)
            }
        }
        b'\t' => {
            if let Some(next_task) = app.next_needs_input_session(Some(task)) {
                if scroll.active {
                    exit_scroll_mode(app, session_id, scroll)?;
                }
                if let Some(session) = app.sessions.get(session_id) {
                    let _ = session.stop_passthrough();
                }
                Ok(Some(Action::ZoomIn(next_task)))
            } else {
                draw_status_bar(
                    app.term.rows,
                    app.term.cols,
                    task,
                    session_id,
                    last_status,
                    Some("no sessions need input"),
                );
                Ok(None)
            }
        }
        b'[' => {
            if !scroll.active {
                enter_scroll_mode(app, session_id, scroll)?;
            }
            Ok(None)
        }
        PREFIX_KEY => {
            if let Some(session) = app.sessions.get_mut(session_id) {
                session.write_input(&[PREFIX_KEY])?;
            }
            Ok(None)
        }
        b'?' => {
            draw_help_bar(app.term.rows, app.term.cols);
            let mut discard = [0u8; 1];
            let _ = stdin.read(&mut discard);
            if scroll.active {
                draw_scroll_status_bar(app.term.rows, app.term.cols, scroll);
            } else {
                draw_status_bar(
                    app.term.rows,
                    app.term.cols,
                    task,
                    session_id,
                    last_status,
                    None,
                );
            }
            Ok(None)
        }
        b'q' => {
            if scroll.active {
                exit_scroll_mode(app, session_id, scroll)?;
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

/// Try to enter scroll mode.  If the session has no scrollback, this is
/// a no-op and `scroll.active` remains false.  When scroll mode IS
/// entered, the scroll status bar is drawn immediately so the bar and
/// `scroll.active` are always in sync.
fn enter_scroll_mode(app: &App, session_id: &str, scroll: &mut ScrollState) -> Result<()> {
    if let Some(session) = app.sessions.get(session_id) {
        scroll.total = session.enter_scroll_mode()?;
        if scroll.total == 0 {
            // Session had no scrollback — it left state untouched.
            return Ok(());
        }
        scroll.offset = 0;
        scroll.active = true;
        // Render the current viewport (offset 0 = live screen).
        session.render_scrollback(0, app.term.cols)?;
        draw_scroll_status_bar(app.term.rows, app.term.cols, scroll);
    }
    Ok(())
}

fn exit_scroll_mode(app: &App, session_id: &str, scroll: &mut ScrollState) -> Result<()> {
    scroll.active = false;
    scroll.offset = 0;
    if let Some(session) = app.sessions.get(session_id) {
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
    if let Some(session) = app.sessions.get(session_id) {
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
        // SGR mouse: \x1b[<...M or \x1b[<...m
        if buf[*i + 2] == b'<' {
            if let Some(consumed) = parse_sgr_mouse_scroll(buf, *i, n) {
                *i += consumed.len;
                match consumed.button {
                    64 => {
                        // Scroll up
                        let new = scroll.offset.saturating_add(3).min(scroll.total);
                        scroll_to(app, session_id, scroll, new)?;
                        return Ok(true);
                    }
                    65 => {
                        // Scroll down
                        scroll_to(app, session_id, scroll, scroll.offset.saturating_sub(3))?;
                        return Ok(true);
                    }
                    _ => return Ok(true), // consume other mouse events
                }
            }
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

// ── Zoomed loop ────────────────────────────────────────────────────────

fn zoomed_loop(
    app: &mut App,
    task: &str,
    session_id: &str,
    running: &Arc<AtomicBool>,
) -> Result<Action> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut buf = [0u8; 4096];
    let mut last_status = SessionStatus::Starting;
    let mut scroll = ScrollState::new();

    let watchers = Watchers::new(app)?;
    let mut debug_msg: Option<String> = None;

    draw_status_bar(
        app.term.rows,
        app.term.cols,
        task,
        session_id,
        &last_status,
        None,
    );

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(Action::Quit);
        }

        if !app.sessions.contains_key(session_id) {
            return Ok(Action::ReturnToTree);
        }

        watchers.poll(app)?;

        // Run lifecycle tick (reaps exited, enforces timeouts, spawns)
        if let Some(msg) = app.tick()? {
            debug_msg = Some(msg);
        }

        if !app.sessions.contains_key(session_id) {
            return Ok(Action::ReturnToTree);
        }

        // Redraw status bar when status or debug info changes
        let mut redraw = false;
        if let Some(session) = app.sessions.get(session_id) {
            let status = session.status().clone();
            if status != last_status {
                last_status = status;
                redraw = true;
            }
        }
        if debug_msg.is_some() {
            redraw = true;
        }
        if redraw && !scroll.active {
            draw_status_bar(
                app.term.rows,
                app.term.cols,
                task,
                session_id,
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
            // ── Scroll mode input ──────────────────────────────────
            if scroll.active {
                // PREFIX_KEY commands still work in scroll mode
                if buf[i] == PREFIX_KEY {
                    i += 1;
                    let cmd = if i < n {
                        let b = buf[i];
                        i += 1;
                        b
                    } else {
                        let mut cmd_buf = [0u8; 1];
                        match stdin.read(&mut cmd_buf) {
                            Ok(0) | Err(_) => return Ok(Action::Quit),
                            Ok(_) => cmd_buf[0],
                        }
                    };
                    if let Some(action) = handle_zoomed_prefix_command(
                        cmd,
                        app,
                        session_id,
                        task,
                        &mut stdin,
                        &last_status,
                        &mut scroll,
                    )? {
                        return Ok(action);
                    }
                    if scroll.active {
                        draw_scroll_status_bar(app.term.rows, app.term.cols, &scroll);
                    } else {
                        draw_status_bar(
                            app.term.rows,
                            app.term.cols,
                            task,
                            session_id,
                            &last_status,
                            None,
                        );
                    }
                    continue;
                }

                match handle_scroll_input(
                    app,
                    session_id,
                    &mut scroll,
                    &buf,
                    &mut i,
                    n,
                    app.term.rows,
                )? {
                    true => {
                        // Still in scroll mode — update status bar
                        draw_scroll_status_bar(app.term.rows, app.term.cols, &scroll);
                    }
                    false => {
                        // Exited scroll mode — restore screen and status
                        draw_status_bar(
                            app.term.rows,
                            app.term.cols,
                            task,
                            session_id,
                            &last_status,
                            None,
                        );
                    }
                }
                continue;
            }

            // ── Normal mode input ──────────────────────────────────

            // Check for SGR mouse events
            if buf[i] == 0x1b && i + 2 < n && buf[i + 1] == b'[' && buf[i + 2] == b'<' {
                if let Some(evt) = parse_sgr_mouse_scroll(&buf, i, n) {
                    if evt.button == 64 {
                        // Scroll up → enter scroll mode and scroll up
                        enter_scroll_mode(app, session_id, &mut scroll)?;
                        if scroll.active {
                            let new = scroll.offset.saturating_add(3).min(scroll.total);
                            scroll_to(app, session_id, &mut scroll, new)?;
                            draw_scroll_status_bar(app.term.rows, app.term.cols, &scroll);
                        }
                        i += evt.len;
                        continue;
                    }
                    if evt.button == 65 {
                        // Scroll down → enter scroll mode (at bottom)
                        enter_scroll_mode(app, session_id, &mut scroll)?;
                        i += evt.len;
                        continue;
                    }
                    // Non-scroll mouse: forward to child if it
                    // requested mouse tracking (so it can handle its
                    // own clicks/drags).  Otherwise discard — the
                    // event is an artifact of kbtz's forced mouse
                    // enable for scroll wheel detection.
                    if let Some(session) = app.sessions.get_mut(session_id) {
                        if session.has_mouse_tracking() {
                            session.write_input(&buf[i..i + evt.len])?;
                        }
                    }
                    i += evt.len;
                    continue;
                }
            }

            // Check for PgUp/PgDn → enter scroll mode
            if buf[i] == 0x1b && i + 3 < n && buf[i + 1] == b'[' {
                let page = (app.term.rows.saturating_sub(2)) as usize;
                if buf[i + 2] == b'5' && buf[i + 3] == b'~' {
                    // PgUp → enter scroll mode and scroll up a page
                    enter_scroll_mode(app, session_id, &mut scroll)?;
                    if scroll.active {
                        let new = scroll.offset.saturating_add(page).min(scroll.total);
                        scroll_to(app, session_id, &mut scroll, new)?;
                        draw_scroll_status_bar(app.term.rows, app.term.cols, &scroll);
                    }
                    i += 4;
                    continue;
                }
                if buf[i + 2] == b'6' && buf[i + 3] == b'~' {
                    // PgDn → enter scroll mode (at bottom)
                    enter_scroll_mode(app, session_id, &mut scroll)?;
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
                if let Some(action) = handle_zoomed_prefix_command(
                    cmd,
                    app,
                    session_id,
                    task,
                    &mut stdin,
                    &last_status,
                    &mut scroll,
                )? {
                    return Ok(action);
                }
                if scroll.active {
                    draw_scroll_status_bar(app.term.rows, app.term.cols, &scroll);
                }
            } else {
                // Find the next PREFIX_KEY or ESC sequence we intercept,
                // and write the entire chunk to the PTY in one call.
                let start = i;
                while i < n && buf[i] != PREFIX_KEY {
                    if buf[i] == 0x1b && i + 2 < n && buf[i + 1] == b'[' {
                        // Stop before SGR mouse sequence (scroll wheel
                        // events are intercepted for scroll mode)
                        if buf[i + 2] == b'<' {
                            break;
                        }
                        // Stop before PgUp/PgDn
                        if i + 3 < n
                            && (buf[i + 2] == b'5' || buf[i + 2] == b'6')
                            && buf[i + 3] == b'~'
                        {
                            break;
                        }
                    }
                    i += 1;
                }
                if i > start {
                    if let Some(session) = app.sessions.get_mut(session_id) {
                        session.write_input(&buf[start..i])?;
                    }
                }
            }
        }
    }
}

fn draw_scroll_status_bar(rows: u16, cols: u16, scroll: &ScrollState) {
    let content = format!(
        " [SCROLL] line {}/{}  q:exit  k/\u{2191}:up  j/\u{2193}:down  PgUp/PgDn  g/G:top/bottom",
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

// ── Top-level mode ─────────────────────────────────────────────────────

fn toplevel_mode(app: &mut App, running: &Arc<AtomicBool>) -> Result<Action> {
    // Ensure the top-level session exists (respawn if exited).
    app.ensure_toplevel()?;

    enter_passthrough_screen(app.term.rows)?;

    if let Some(ref toplevel) = app.toplevel {
        toplevel.start_passthrough()?;
    }

    let result = toplevel_loop(app, running);

    // Stop passthrough (resets input modes like mouse tracking)
    if let Some(ref toplevel) = app.toplevel {
        let _ = toplevel.stop_passthrough();
    }

    leave_passthrough_screen()?;
    result
}

fn handle_toplevel_prefix_command(
    cmd: u8,
    app: &mut App,
    stdin: &mut io::StdinLock,
) -> Result<Option<Action>> {
    match cmd {
        b't' | b'd' => Ok(Some(Action::ReturnToTree)),
        b'n' => {
            let ids = app.session_ids_ordered();
            if let Some(first_sid) = ids.first() {
                if let Some(session) = app.sessions.get(first_sid) {
                    let task = session.task_name().to_string();
                    return Ok(Some(Action::ZoomIn(task)));
                }
            }
            Ok(None)
        }
        b'p' => {
            let ids = app.session_ids_ordered();
            if let Some(last_sid) = ids.last() {
                if let Some(session) = app.sessions.get(last_sid) {
                    let task = session.task_name().to_string();
                    return Ok(Some(Action::ZoomIn(task)));
                }
            }
            Ok(None)
        }
        b'\t' => {
            if let Some(task) = app.next_needs_input_session(None) {
                Ok(Some(Action::ZoomIn(task)))
            } else {
                draw_toplevel_status_bar(
                    app.term.rows,
                    app.term.cols,
                    Some("no sessions need input"),
                );
                Ok(None)
            }
        }
        PREFIX_KEY => {
            if let Some(ref mut toplevel) = app.toplevel {
                toplevel.write_input(&[PREFIX_KEY])?;
            }
            Ok(None)
        }
        b'?' => {
            draw_toplevel_help_bar(app.term.rows, app.term.cols);
            let mut discard = [0u8; 1];
            let _ = stdin.read(&mut discard);
            draw_toplevel_status_bar(app.term.rows, app.term.cols, None);
            Ok(None)
        }
        b'q' => Ok(Some(Action::Quit)),
        _ => Ok(None),
    }
}

fn toplevel_loop(app: &mut App, running: &Arc<AtomicBool>) -> Result<Action> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut buf = [0u8; 4096];

    let watchers = Watchers::new(app)?;

    draw_toplevel_status_bar(app.term.rows, app.term.cols, None);

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(Action::Quit);
        }

        // If top-level session has exited, return to tree.
        if app.toplevel.as_mut().is_none_or(|s| !s.is_alive()) {
            return Ok(Action::ReturnToTree);
        }

        watchers.poll(app)?;

        // Run lifecycle tick for worker sessions.
        app.tick()?;

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
                if let Some(action) = handle_toplevel_prefix_command(cmd, app, &mut stdin)? {
                    return Ok(action);
                }
            } else {
                let start = i;
                while i < n && buf[i] != PREFIX_KEY {
                    i += 1;
                }
                if let Some(ref mut toplevel) = app.toplevel {
                    toplevel.write_input(&buf[start..i])?;
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

fn draw_toplevel_status_bar(rows: u16, cols: u16, debug: Option<&str>) {
    draw_bar(rows, cols, "7", " ^B ? help \u{2502} task manager", debug);
}

fn draw_toplevel_help_bar(rows: u16, cols: u16) {
    draw_bar(
        rows,
        cols,
        "7;33",
        " ^B t:tree  ^B n:next worker  ^B p:prev worker  ^B Tab:input  ^B ^B:send ^B  ^B q:quit  ^B ?:help",
        None,
    );
}

fn draw_status_bar(
    rows: u16,
    cols: u16,
    task: &str,
    session_id: &str,
    status: &SessionStatus,
    debug: Option<&str>,
) {
    let left = format!(
        " ^B ? help │ {} ({}) │ {} {}",
        task,
        session_id,
        status.indicator(),
        status.label(),
    );
    draw_bar(rows, cols, "7", &left, debug);
}

fn draw_help_bar(rows: u16, cols: u16) {
    draw_bar(
        rows,
        cols,
        "7;33",
        " ^B t:tree  ^B c:manager  ^B n:next  ^B p:prev  ^B Tab:input  ^B [:scroll  ^B ^B:send ^B  ^B q:quit  ^B ?:help",
        None,
    );
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
