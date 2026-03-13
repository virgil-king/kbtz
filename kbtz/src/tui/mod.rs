mod app;
mod editor;
mod event;
mod tree;

use std::io;
use std::path::PathBuf;
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
    action: Option<&str>,
    workspace_dir: Option<&str>,
) -> Result<()> {
    let workspace_dir = workspace_dir.map(PathBuf::from);
    let mut app = App::new(conn, root, workspace_dir.as_deref())?;

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(
        &mut terminal,
        &mut app,
        db_path,
        conn,
        root,
        poll_interval,
        action,
    );

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
    action: Option<&str>,
) -> Result<()> {
    let poll_duration = Duration::from_millis(poll_interval);

    // Set up file watcher for DB
    let (_watcher, rx) = watch::watch_db(db_path)?;

    // Set up file watcher for workspace status directory
    let status_watcher = app
        .workspace_dir
        .as_deref()
        .map(watch::watch_dir)
        .transpose()?;

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
                                    app.add_form.as_mut().unwrap().error = Some(e.to_string());
                                }
                            }
                        }
                        KeyAction::ToggleNotes => {
                            app.toggle_notes(conn)?;
                        }
                        KeyAction::AddNote => {
                            if let Some(task_name) = app.selected_name() {
                                let task_name = task_name.to_string();
                                app.tree.error = None;
                                match editor::open_editor(terminal, "") {
                                    Ok(content) => {
                                        let content = content.trim_end();
                                        if !content.is_empty() {
                                            if let Err(e) = ops::add_note(conn, &task_name, content)
                                            {
                                                app.tree.error = Some(e.to_string());
                                            } else {
                                                // Open notes panel showing the newly added note
                                                let mut panel = crate::ui::NotesPanel::new();
                                                panel.load(conn, &task_name)?;
                                                app.notes_panel = Some(panel);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        app.tree.error = Some(e.to_string());
                                    }
                                }
                            }
                        }
                        KeyAction::Pause(name) => {
                            if let Err(e) = ops::pause_task(conn, &name) {
                                app.tree.error = Some(e.to_string());
                            } else {
                                app.refresh(conn, root)?;
                            }
                        }
                        KeyAction::Unpause(name) => {
                            if let Err(e) = ops::unpause_task(conn, &name) {
                                app.tree.error = Some(e.to_string());
                            } else {
                                app.refresh(conn, root)?;
                            }
                        }
                        KeyAction::MarkDone(name) => {
                            if let Err(e) = ops::mark_done(conn, &name) {
                                app.tree.error = Some(e.to_string());
                            } else {
                                app.refresh(conn, root)?;
                            }
                        }
                        KeyAction::ForceUnassign(name) => {
                            if let Err(e) = ops::force_unassign_task(conn, &name) {
                                app.tree.error = Some(e.to_string());
                            } else {
                                app.refresh(conn, root)?;
                            }
                        }
                        KeyAction::RunAction => {
                            if let Some(cmd) = action {
                                if let Some(row) = app.tree.rows.get(app.tree.cursor) {
                                    terminal::disable_raw_mode()?;
                                    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

                                    let result = std::process::Command::new("sh")
                                        .arg("-c")
                                        .arg(cmd)
                                        .env("KBTZ_TASK", &row.name)
                                        .env("KBTZ_TASK_STATUS", &row.status)
                                        .env(
                                            "KBTZ_TASK_ASSIGNEE",
                                            row.assignee.as_deref().unwrap_or(""),
                                        )
                                        .status();

                                    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                                    terminal::enable_raw_mode()?;
                                    terminal.clear()?;

                                    match result {
                                        Err(e) => {
                                            app.tree.error = Some(format!("action failed: {e}"));
                                        }
                                        Ok(exit) if !exit.success() => {
                                            app.tree.error =
                                                Some(format!("action exited with {exit}"));
                                        }
                                        Ok(_) => {}
                                    }

                                    app.refresh(conn, root)?;
                                }
                            }
                        }
                        KeyAction::Refresh => {
                            app.refresh(conn, root)?;
                        }
                        KeyAction::Continue => {}
                    }
                    if let Some(panel) = &mut app.notes_panel {
                        if let Some(name) = app.tree.selected_name() {
                            panel.load(conn, name)?;
                        }
                    }
                }
            }
        }

        // Check for DB file changes (non-blocking)
        if watch::wait_for_change(&rx, Duration::ZERO) {
            watch::drain_events(&rx);
            app.refresh(conn, root)?;
        }

        // Check for workspace status file changes (non-blocking)
        if let Some((_, ref status_rx)) = status_watcher {
            if watch::wait_for_change(status_rx, Duration::ZERO) {
                watch::drain_events(status_rx);
                app.refresh_statuses();
            }
        }
    }
}
