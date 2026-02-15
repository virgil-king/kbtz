use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use kbtz::model::Task;
use kbtz::ops;
use kbtz::ui::TreeRow;

use crate::lifecycle::{self, SessionAction, SessionPhase, SessionSnapshot, TaskSnapshot, WorldSnapshot, GRACEFUL_TIMEOUT};
use crate::session::{Session, SessionStatus};

pub struct App {
    // kbtz state
    pub db_path: String,
    pub conn: Connection,
    pub tree_rows: Vec<TreeRow>,

    // Session management
    pub sessions: HashMap<String, Session>,     // session_id -> session
    pub task_to_session: HashMap<String, String>, // task_name -> session_id
    counter: u64,
    pub mux_dir: PathBuf,
    pub max_concurrency: usize,
    pub prefer: Option<String>,
    pub command: String,

    // Top-level task management session (not tied to any task)
    pub toplevel: Option<Session>,

    // Terminal
    pub rows: u16,
    pub cols: u16,

    // UI state (tree mode)
    pub cursor: usize,
    pub collapsed: HashSet<String>,
    pub error: Option<String>,
}

/// What the top-level loop should do next.
pub enum Action {
    Continue,
    ZoomIn(String),   // task_name
    NextSession,
    PrevSession,
    ReturnToTree,
    TopLevel,
    Quit,
}

fn session_id_to_filename(session_id: &str) -> String {
    session_id.replace('/', "-")
}

impl App {
    pub fn new(
        db_path: String,
        mux_dir: PathBuf,
        max_concurrency: usize,
        prefer: Option<String>,
        command: String,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let conn = kbtz::db::open(&db_path).context("failed to open kbtz database")?;
        let mut app = App {
            db_path,
            conn,
            tree_rows: Vec::new(),
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            mux_dir,
            max_concurrency,
            prefer,
            command,
            toplevel: None,
            rows,
            cols,
            cursor: 0,
            collapsed: HashSet::new(),
            error: None,
        };
        app.refresh_tree()?;
        app.spawn_toplevel()?;
        Ok(app)
    }

    /// Rebuild the tree view from the database.
    pub fn refresh_tree(&mut self) -> Result<()> {
        let mut tasks = ops::list_tasks(&self.conn, None, true, None)?;
        tasks.retain(|t| t.status != "done");
        self.tree_rows = kbtz::ui::flatten_tree(&tasks, &self.collapsed, &self.conn)?;
        if !self.tree_rows.is_empty() {
            if self.cursor >= self.tree_rows.len() {
                self.cursor = self.tree_rows.len() - 1;
            }
        } else {
            self.cursor = 0;
        }
        Ok(())
    }

    // ── Lifecycle state machine ────────────────────────────────────────

    /// Build a snapshot of the current world for the pure tick function.
    fn snapshot(&mut self) -> WorldSnapshot {
        let sessions = self
            .sessions
            .iter_mut()
            .map(|(session_id, session)| {
                let phase = if !session.is_alive() {
                    SessionPhase::Exited
                } else if let Some(since) = session.stopping_since {
                    SessionPhase::Stopping { since }
                } else {
                    SessionPhase::Running
                };

                let task = match ops::get_task(&self.conn, &session.task_name) {
                    Ok(t) => Some(TaskSnapshot {
                        status: t.status,
                        assignee: t.assignee,
                    }),
                    Err(_) => None, // task was deleted
                };

                SessionSnapshot {
                    session_id: session_id.clone(),
                    phase,
                    task,
                }
            })
            .collect();

        WorldSnapshot {
            sessions,
            max_concurrency: self.max_concurrency,
            now: std::time::Instant::now(),
        }
    }

    /// Execute lifecycle actions. Returns a debug description if anything notable happened.
    fn execute_actions(&mut self, actions: Vec<SessionAction>) -> Result<Option<String>> {
        let mut descriptions: Vec<String> = Vec::new();

        for action in actions {
            match action {
                SessionAction::RequestExit { session_id } => {
                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        session.request_exit();
                    }
                }
                SessionAction::ForceKill { session_id } => {
                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        session.force_kill();
                        descriptions.push(format!("{session_id} killed"));
                    }
                }
                SessionAction::Remove { session_id } => {
                    if self.sessions.contains_key(&session_id) {
                        // If it wasn't force-killed, it exited on its own.
                        if !descriptions.iter().any(|d| d.starts_with(&session_id)) {
                            descriptions.push(format!("{session_id} exited"));
                        }
                        self.remove_session(&session_id);
                    }
                }
                SessionAction::SpawnUpTo { count } => {
                    self.spawn_up_to(count)?;
                }
            }
        }

        if descriptions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(descriptions.join(", ")))
        }
    }

    /// Run one lifecycle tick: snapshot the world, compute actions, execute them.
    /// Returns a debug description of notable events (kills, exits).
    pub fn tick(&mut self) -> Result<Option<String>> {
        let world = self.snapshot();
        let actions = lifecycle::tick(&world);
        self.execute_actions(actions)
    }

    /// Spawn sessions for claimable tasks, up to `count` new sessions.
    fn spawn_up_to(&mut self, count: usize) -> Result<()> {
        for _ in 0..count {
            self.counter += 1;
            let session_id = format!("mux/{}", self.counter);

            match ops::claim_next_task(&self.conn, &session_id, self.prefer.as_deref())? {
                Some(task_name) => {
                    let task = ops::get_task(&self.conn, &task_name)?;
                    match self.spawn_session(&task, &session_id) {
                        Ok(session) => {
                            self.task_to_session
                                .insert(task_name.clone(), session_id.clone());
                            self.sessions.insert(session_id, session);
                        }
                        Err(e) => {
                            // Failed to spawn — release the claim
                            let _ =
                                ops::release_task(&self.conn, &task_name, &session_id);
                            self.counter -= 1;
                            self.error = Some(format!("failed to spawn session: {e}"));
                            break;
                        }
                    }
                }
                None => {
                    self.counter -= 1;
                    break;
                }
            }
        }
        Ok(())
    }

    /// Spawn the top-level task management session.
    fn spawn_toplevel(&mut self) -> Result<()> {
        let prompt = "You are the top-level task management agent. Help the user manage the kbtz task list.".to_string();
        let args: Vec<String> = vec![
            "--append-system-prompt".into(),
            crate::skill::TOPLEVEL_SKILL.into(),
            prompt,
        ];
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let session_id = "mux/toplevel";
        let env_vars: Vec<(&str, &str)> = vec![
            ("KBTZ_DB", &self.db_path),
        ];
        let session = Session::spawn(
            &self.command,
            &arg_refs,
            "toplevel",
            session_id,
            self.rows,
            self.cols,
            &env_vars,
        )?;
        self.toplevel = Some(session);
        Ok(())
    }

    /// Respawn the top-level session if it has exited.
    pub fn ensure_toplevel(&mut self) -> Result<()> {
        let needs_respawn = match &mut self.toplevel {
            Some(s) => !s.is_alive(),
            None => true,
        };
        if needs_respawn {
            self.spawn_toplevel()?;
        }
        Ok(())
    }

    fn spawn_session(&self, task: &Task, session_id: &str) -> Result<Session> {
        let prompt = format!("Work on task '{}': {}", task.name, task.description);
        let args: Vec<String> = vec![
            "--append-system-prompt".into(),
            crate::skill::AGENT_SKILL.into(),
            prompt,
        ];
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mux_dir_str = self.mux_dir.to_string_lossy().to_string();
        let env_vars: Vec<(&str, &str)> = vec![
            ("KBTZ_DB", &self.db_path),
            ("KBTZ_SESSION_ID", session_id),
            ("KBTZ_TASK", &task.name),
            ("KBTZ_MUX_DIR", &mux_dir_str),
        ];
        let session = Session::spawn(
            &self.command,
            &arg_refs,
            &task.name,
            session_id,
            self.rows,
            self.cols,
            &env_vars,
        )?;
        Ok(session)
    }

    /// Read status files from the mux directory and update session statuses.
    pub fn read_status_files(&mut self) -> Result<()> {
        for (session_id, session) in &mut self.sessions {
            let path = self.mux_dir.join(session_id_to_filename(session_id));
            if let Ok(content) = std::fs::read_to_string(&path) {
                session.status = SessionStatus::from_str(&content);
            }
        }
        Ok(())
    }

    /// Remove a session, cleaning up its status file and releasing the task.
    fn remove_session(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.remove(session_id) {
            let _ = session.stop_passthrough();
            let _ = ops::release_task(&self.conn, &session.task_name, &session.session_id);
            self.task_to_session.remove(&session.task_name);
            let _ = std::fs::remove_file(self.mux_dir.join(session_id_to_filename(session_id)));
        }
    }

    /// Propagate terminal resize to all PTYs.
    pub fn handle_resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        for session in self.sessions.values() {
            let _ = session.resize(rows, cols);
        }
        if let Some(ref toplevel) = self.toplevel {
            let _ = toplevel.resize(rows, cols);
        }
    }

    // Navigation

    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if !self.tree_rows.is_empty() && self.cursor < self.tree_rows.len() - 1 {
            self.cursor += 1;
        }
    }

    pub fn toggle_collapse(&mut self) {
        if let Some(row) = self.tree_rows.get(self.cursor) {
            if row.has_children {
                let name = row.name.clone();
                if !self.collapsed.remove(&name) {
                    self.collapsed.insert(name);
                }
            }
        }
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.tree_rows.get(self.cursor).map(|r| r.name.as_str())
    }

    /// Get an ordered list of session IDs for cycling.
    pub fn session_ids_ordered(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sessions.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Cycle to next/prev session, returning the task name.
    pub fn cycle_session(&self, action: &Action, current_task: &str) -> Option<String> {
        let ids = self.session_ids_ordered();
        if ids.is_empty() {
            return None;
        }
        let current_sid = self.task_to_session.get(current_task)?;
        let current_idx = ids.iter().position(|id| id == current_sid)?;
        let next_idx = match action {
            Action::NextSession => (current_idx + 1) % ids.len(),
            Action::PrevSession => {
                if current_idx == 0 {
                    ids.len() - 1
                } else {
                    current_idx - 1
                }
            }
            _ => return None,
        };
        let next_sid = &ids[next_idx];
        self.sessions.get(next_sid).map(|s| s.task_name.clone())
    }

    /// Kill and release a session for a task so it can be respawned.
    pub fn restart_session(&mut self, task_name: &str) {
        if let Some(session_id) = self.task_to_session.get(task_name).cloned() {
            if let Some(session) = self.sessions.get_mut(&session_id) {
                session.force_kill();
            }
            self.remove_session(&session_id);
        }
    }

    /// Gracefully shut down all sessions and clean up status files.
    ///
    /// Sends `/exit` to all sessions in parallel, then waits up to
    /// GRACEFUL_TIMEOUT for them to exit before force-killing.
    pub fn shutdown(&mut self) {
        // Send /exit to all sessions (workers + toplevel).
        for session in self.sessions.values_mut() {
            let _ = session.stop_passthrough();
            session.request_exit();
        }
        if let Some(ref mut toplevel) = self.toplevel {
            let _ = toplevel.stop_passthrough();
            toplevel.request_exit();
        }

        // Wait for all to exit, up to the timeout.
        let deadline = std::time::Instant::now() + GRACEFUL_TIMEOUT;
        loop {
            let workers_dead = self
                .sessions
                .values_mut()
                .all(|s| !s.is_alive());
            let toplevel_dead = self
                .toplevel
                .as_mut()
                .map_or(true, |s| !s.is_alive());
            if (workers_dead && toplevel_dead) || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Force-kill any stragglers and release tasks.
        for (_, mut session) in self.sessions.drain() {
            if session.is_alive() {
                session.force_kill();
            }
            let _ = ops::release_task(&self.conn, &session.task_name, &session.session_id);
        }
        self.task_to_session.clear();

        // Force-kill top-level if still alive.
        if let Some(ref mut toplevel) = self.toplevel {
            if toplevel.is_alive() {
                toplevel.force_kill();
            }
        }
        self.toplevel = None;

        // Clean up status files.
        if let Ok(entries) = std::fs::read_dir(&self.mux_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}
