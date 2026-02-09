mod app;
mod event;
mod tree;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self as ct_event, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;
use rusqlite::Connection;

use crate::db;
use crate::watch;
use app::App;
use event::KeyAction;

pub fn run(
    db_path: &str,
    conn: &Connection,
    root: Option<&str>,
    poll_interval: u64,
) -> Result<()> {
    let mut app = App::new(conn, root)?;

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app, db_path, conn, root, poll_interval);

    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db_path: &str,
    initial_conn: &Connection,
    root: Option<&str>,
    poll_interval: u64,
) -> Result<()> {
    let poll_duration = Duration::from_millis(poll_interval);

    // Set up file watcher
    let (_watcher, rx) = watch::watch_db(db_path)?;

    // We keep a separate owned connection that we reopen on DB changes.
    let mut conn: Option<Connection> = None;

    fn get_conn<'a>(owned: &'a Option<Connection>, fallback: &'a Connection) -> &'a Connection {
        owned.as_ref().unwrap_or(fallback)
    }

    loop {
        terminal.draw(|frame| tree::render(frame, app))?;

        if ct_event::poll(poll_duration)? {
            if let Event::Key(key) = ct_event::read()? {
                if key.kind == KeyEventKind::Press {
                    match event::handle_key(app, key) {
                        KeyAction::Quit => return Ok(()),
                        KeyAction::Submit => {
                            let c = get_conn(&conn, initial_conn);
                            app.submit_add(c, root)?;
                        }
                        KeyAction::Refresh => {
                            let c = get_conn(&conn, initial_conn);
                            app.refresh(c, root)?;
                        }
                        KeyAction::Continue => {}
                    }
                    if app.show_notes {
                        app.load_notes(get_conn(&conn, initial_conn))?;
                    }
                }
            }
        }

        // Check for file changes (non-blocking)
        if watch::wait_for_change(&rx, Duration::ZERO) {
            watch::drain_events(&rx);
            let new_conn = db::open(db_path)?;
            app.refresh(&new_conn, root)?;
            conn = Some(new_conn);
        }
    }
}
