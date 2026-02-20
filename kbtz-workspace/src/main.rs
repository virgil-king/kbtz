mod app;
mod backend;
mod config;
mod lifecycle;
mod prompt;
mod scrollback;
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
        if kbtz::watch::wait_for_change(&self.db_rx, Duration::ZERO) {
            kbtz::watch::drain_events(&self.db_rx);
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
        rows,
        cols,
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
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Enter => {
                                if let Err(e) = kbtz::ops::mark_done(&app.conn, &name) {
                                    app.tree.error = Some(e.to_string());
                                }
                            }
                            _ => {}
                        }
                        mode = TreeMode::Normal;
                        continue;
                    }
                    TreeMode::ConfirmPause(name) => {
                        let name = name.clone();
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Enter => {
                                if let Err(e) = kbtz::ops::pause_task(&app.conn, &name) {
                                    app.tree.error = Some(e.to_string());
                                }
                            }
                            _ => {}
                        }
                        mode = TreeMode::Normal;
                        continue;
                    }
                    _ => {}
                }

                app.tree.error = None;

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
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
                            if let Err(e) = result {
                                app.tree.error = Some(e.to_string());
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
                                _ => {
                                    if let Err(e) = kbtz::ops::mark_done(&app.conn, &name) {
                                        app.tree.error = Some(e.to_string());
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Char('U') => {
                        if let Some(name) = app.selected_name() {
                            let name = name.to_string();
                            if let Err(e) = kbtz::ops::force_unassign_task(&app.conn, &name) {
                                app.tree.error = Some(e.to_string());
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
        app.tick()?;
    }
}

// ── Zoomed mode ────────────────────────────────────────────────────────

fn zoomed_mode(app: &mut App, task: &str, running: &Arc<AtomicBool>) -> Result<Action> {
    let session_id = match app.task_to_session.get(task) {
        Some(sid) => sid.clone(),
        None => return Ok(Action::ReturnToTree),
    };

    // Ensure raw mode
    terminal::enable_raw_mode()?;

    // Do NOT enter alternate screen here — the child (e.g. Claude Code)
    // manages its own alternate screen.  Adding a second layer would
    // prevent shift+pgup/pgdn from reaching the child (the terminal
    // intercepts them for scrollback, which is empty in alt screen) and
    // would conflict when the child's alt-screen escapes are forwarded.

    // Set scroll region to protect the status bar on the last line.
    // Child output stays within rows 1..(rows-1), last row is ours.
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[1;{}r", app.term.rows - 1)?;
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0)
    )?;

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

    // Reset scroll region
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[r")?;
    stdout.flush()?;

    terminal::disable_raw_mode()?;

    result
}

fn handle_zoomed_prefix_command(
    cmd: u8,
    app: &mut App,
    session_id: &str,
    task: &str,
    stdin: &mut io::StdinLock,
    last_status: &SessionStatus,
) -> Result<Option<Action>> {
    match cmd {
        b't' | b'd' => Ok(Some(Action::ReturnToTree)),
        b'c' => {
            if let Some(session) = app.sessions.get(session_id) {
                let _ = session.stop_passthrough();
            }
            Ok(Some(Action::TopLevel))
        }
        b'n' => {
            if let Some(next_task) = app.cycle_session(&Action::NextSession, task) {
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
            draw_status_bar(app.term.rows, app.term.cols, task, session_id, last_status, None);
            Ok(None)
        }
        b'q' => Ok(Some(Action::Quit)),
        _ => Ok(None),
    }
}

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

    let watchers = Watchers::new(app)?;
    let mut debug_msg: Option<String> = None;

    draw_status_bar(app.term.rows, app.term.cols, task, session_id, &last_status, None);

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
        if redraw {
            draw_status_bar(
                app.term.rows,
                app.term.cols,
                task,
                session_id,
                &last_status,
                debug_msg.take().as_deref(),
            );
        }

        // Poll stdin with a timeout so we can check session liveness
        // and watcher channels even when the user isn't typing.
        let stdin_fd = stdin.as_raw_fd();
        let mut pfd = libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready <= 0 {
            continue;
        }
        let n = match stdin.read(&mut buf) {
            Ok(0) => return Ok(Action::Quit),
            Err(_) => return Ok(Action::Quit),
            Ok(n) => n,
        };

        // Process the buffer, scanning for PREFIX_KEY
        let mut i = 0;
        while i < n {
            if buf[i] == PREFIX_KEY {
                i += 1;
                // Get the command byte (from remaining buffer or a fresh read)
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
                )? {
                    return Ok(action);
                }
            } else {
                // Find the next PREFIX_KEY or end of buffer and write the
                // entire chunk to the PTY in one call.
                let start = i;
                while i < n && buf[i] != PREFIX_KEY {
                    i += 1;
                }
                if let Some(session) = app.sessions.get_mut(session_id) {
                    session.write_input(&buf[start..i])?;
                }
            }
        }
    }
}

// ── Top-level mode ─────────────────────────────────────────────────────

fn toplevel_mode(app: &mut App, running: &Arc<AtomicBool>) -> Result<Action> {
    // Ensure the top-level session exists (respawn if exited).
    app.ensure_toplevel()?;

    // Ensure raw mode
    terminal::enable_raw_mode()?;

    // Set scroll region to protect the status bar on the last line.
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[1;{}r", app.term.rows - 1)?;
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0)
    )?;

    // Start passthrough
    if let Some(ref toplevel) = app.toplevel {
        toplevel.start_passthrough()?;
    }

    let result = toplevel_loop(app, running);

    // Stop passthrough (resets input modes like mouse tracking)
    if let Some(ref toplevel) = app.toplevel {
        let _ = toplevel.stop_passthrough();
    }

    // Reset scroll region
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[r")?;
    stdout.flush()?;

    terminal::disable_raw_mode()?;

    result
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

        // Poll stdin with a timeout.
        let stdin_fd = stdin.as_raw_fd();
        let mut pfd = libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready <= 0 {
            continue;
        }
        let n = match stdin.read(&mut buf) {
            Ok(0) => return Ok(Action::Quit),
            Err(_) => return Ok(Action::Quit),
            Ok(n) => n,
        };

        // Process the buffer, scanning for PREFIX_KEY
        let mut i = 0;
        while i < n {
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
                match cmd {
                    b't' | b'd' => return Ok(Action::ReturnToTree),
                    b'n' => {
                        // Cycle to first worker session.
                        let ids = app.session_ids_ordered();
                        if let Some(first_sid) = ids.first() {
                            if let Some(session) = app.sessions.get(first_sid) {
                                let task = session.task_name().to_string();
                                return Ok(Action::ZoomIn(task));
                            }
                        }
                    }
                    b'p' => {
                        // Cycle to last worker session.
                        let ids = app.session_ids_ordered();
                        if let Some(last_sid) = ids.last() {
                            if let Some(session) = app.sessions.get(last_sid) {
                                let task = session.task_name().to_string();
                                return Ok(Action::ZoomIn(task));
                            }
                        }
                    }
                    b'\t' => {
                        if let Some(task) = app.next_needs_input_session(None) {
                            return Ok(Action::ZoomIn(task));
                        } else {
                            draw_toplevel_status_bar(
                                app.term.rows,
                                app.term.cols,
                                Some("no sessions need input"),
                            );
                        }
                    }
                    PREFIX_KEY => {
                        if let Some(ref mut toplevel) = app.toplevel {
                            toplevel.write_input(&[PREFIX_KEY])?;
                        }
                    }
                    b'?' => {
                        draw_toplevel_help_bar(app.term.rows, app.term.cols);
                        let mut discard = [0u8; 1];
                        let _ = stdin.read(&mut discard);
                        draw_toplevel_status_bar(app.term.rows, app.term.cols, None);
                    }
                    b'q' => return Ok(Action::Quit),
                    _ => {}
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

fn draw_toplevel_status_bar(rows: u16, cols: u16, debug: Option<&str>) {
    let left = " ^B ? help \u{2502} task manager";
    let content = if let Some(dbg) = debug {
        let right = format!(" [{dbg}]");
        let gap = (cols as usize).saturating_sub(left.len() + right.len());
        format!("{left}{:gap$}{right}", "")
    } else {
        left.to_string()
    };
    let padding = (cols as usize).saturating_sub(content.len());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = write!(
        out,
        "\x1b7\x1b[{rows};1H\x1b[7m{content}{:padding$}\x1b[0m\x1b8",
        "",
    );
    let _ = out.flush();
}

fn draw_toplevel_help_bar(rows: u16, cols: u16) {
    let content =
        " ^B t:tree  ^B n:next worker  ^B p:prev worker  ^B Tab:input  ^B ^B:send ^B  ^B q:quit  ^B ?:help";
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
    let content = if let Some(dbg) = debug {
        let right = format!(" [{dbg}]");
        let gap = (cols as usize).saturating_sub(left.len() + right.len());
        format!("{left}{:gap$}{right}", "")
    } else {
        left.clone()
    };
    let padding = (cols as usize).saturating_sub(content.len());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = write!(
        out,
        "\x1b7\x1b[{rows};1H\x1b[7m{content}{:padding$}\x1b[0m\x1b8",
        "",
    );
    let _ = out.flush();
}

fn draw_help_bar(rows: u16, cols: u16) {
    let content =
        " ^B t:tree  ^B c:manager  ^B n:next  ^B p:prev  ^B Tab:input  ^B ^B:send ^B  ^B q:quit  ^B ?:help";
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
