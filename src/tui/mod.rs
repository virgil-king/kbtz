mod app;
mod editor;
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

use crate::ops;
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
    conn: &Connection,
    root: Option<&str>,
    poll_interval: u64,
) -> Result<()> {
    let poll_duration = Duration::from_millis(poll_interval);

    // Set up file watcher
    let (_watcher, rx) = watch::watch_db(db_path)?;

    loop {
        terminal.draw(|frame| tree::render(frame, app))?;

        if ct_event::poll(poll_duration)? {
            if let Event::Key(key) = ct_event::read()? {
                if key.kind == KeyEventKind::Press {
                    match event::handle_key(app, key) {
                        KeyAction::Quit => return Ok(()),
                        KeyAction::Submit => {
                            app.submit_add(conn, root)?;
                        }
                        KeyAction::OpenEditor => {
                            let initial = &app.add_form.as_ref().unwrap().note;
                            match editor::open_editor(terminal, initial) {
                                Ok(content) => {
                                    let form = app.add_form.as_mut().unwrap();
                                    form.note = content.trim_end().to_string();
                                    form.error = None;
                                }
                                Err(e) => {
                                    app.add_form.as_mut().unwrap().error =
                                        Some(e.to_string());
                                }
                            }
                        }
                        KeyAction::AddNote => {
                            if let Some(task_name) = app.selected_name() {
                                let task_name = task_name.to_string();
                                app.error = None;
                                match editor::open_editor(terminal, "") {
                                    Ok(content) => {
                                        let content = content.trim_end();
                                        if !content.is_empty() {
                                            if let Err(e) = ops::add_note(conn, &task_name, content) {
                                                app.error = Some(e.to_string());
                                            } else {
                                                app.show_notes = true;
                                                app.load_notes(conn)?;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        app.error = Some(e.to_string());
                                    }
                                }
                            }
                        }
                        KeyAction::TogglePause => {
                            if let Some(task_name) = app.selected_name() {
                                let task_name = task_name.to_string();
                                let status = app.rows[app.cursor].status.as_str();
                                let result = match status {
                                    "paused" => ops::unpause_task(conn, &task_name),
                                    "open" => ops::pause_task(conn, &task_name),
                                    _ => {
                                        app.error = Some(format!("cannot pause {status} task"));
                                        Ok(())
                                    }
                                };
                                if let Err(e) = result {
                                    app.error = Some(e.to_string());
                                } else {
                                    app.refresh(conn, root)?;
                                }
                            }
                        }
                        KeyAction::MarkDone => {
                            if let Some(task_name) = app.selected_name() {
                                let task_name = task_name.to_string();
                                let status = app.rows[app.cursor].status.as_str();
                                match status {
                                    "done" => {
                                        app.error = Some("task is already done".into());
                                    }
                                    "active" => {
                                        app.error = Some("task is assigned; release or pause it first".into());
                                    }
                                    _ => {
                                        if let Err(e) = ops::mark_done(conn, &task_name) {
                                            app.error = Some(e.to_string());
                                        } else {
                                            app.refresh(conn, root)?;
                                        }
                                    }
                                }
                            }
                        }
                        KeyAction::Refresh => {
                            app.refresh(conn, root)?;
                        }
                        KeyAction::Continue => {}
                    }
                    if app.show_notes {
                        app.load_notes(conn)?;
                    }
                }
            }
        }

        // Check for file changes (non-blocking)
        if watch::wait_for_change(&rx, Duration::ZERO) {
            watch::drain_events(&rx);
            app.refresh(conn, root)?;
        }
    }
}
