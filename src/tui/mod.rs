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
use app::App;

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
    let mut last_mtime = std::fs::metadata(db_path)
        .and_then(|m| m.modified())
        .ok();

    // We keep a separate owned connection that we reopen on DB changes.
    // For the initial state, we use initial_conn for notes loading.
    let mut conn: Option<Connection> = None;

    // Helper: get a reference to the best available connection
    fn get_conn<'a>(owned: &'a Option<Connection>, fallback: &'a Connection) -> &'a Connection {
        owned.as_ref().unwrap_or(fallback)
    }

    loop {
        terminal.draw(|frame| tree::render(frame, app))?;

        if ct_event::poll(poll_duration)? {
            if let Event::Key(key) = ct_event::read()? {
                if key.kind == KeyEventKind::Press {
                    if event::handle_key(app, key) {
                        return Ok(());
                    }
                    if app.show_notes {
                        app.load_notes(get_conn(&conn, initial_conn))?;
                    }
                }
            }
        }

        // Check if DB file changed
        let current_mtime = std::fs::metadata(db_path)
            .and_then(|m| m.modified())
            .ok();
        if current_mtime != last_mtime {
            last_mtime = current_mtime;
            let new_conn = db::open(db_path)?;
            app.refresh(&new_conn, root)?;
            conn = Some(new_conn);
        }
    }
}
