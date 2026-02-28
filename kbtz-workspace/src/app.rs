use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ratatui::widgets::ListState;
use rusqlite::ffi::ErrorCode;
use rusqlite::Connection;

use kbtz::model::Task;
use kbtz::ops;
use kbtz::ui::TreeRow;

use crate::backend::Backend;
use crate::lifecycle::{
    self, SessionAction, SessionPhase, SessionSnapshot, TaskSnapshot, WorldSnapshot,
    GRACEFUL_TIMEOUT,
};
use crate::session::{PtySpawner, SessionHandle, SessionSpawner, SessionStatus, ShepherdSpawner};
use crate::shepherd_session::ShepherdSession;

pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
}

pub struct TreeView {
    pub rows: Vec<TreeRow>,
    pub cursor: usize,
    pub list_state: ListState,
    pub collapsed: HashSet<String>,
    pub error: Option<String>,
}

pub struct App {
    // kbtz state
    pub db_path: String,
    pub conn: Connection,

    // Session management
    pub sessions: HashMap<String, Box<dyn SessionHandle>>, // session_id -> session
    pub task_to_session: HashMap<String, String>,          // task_name -> session_id
    counter: u64,
    pub status_dir: PathBuf,
    /// In auto mode, caps how many sessions lifecycle::tick will auto-spawn.
    /// In manual mode (--manual), this field is ignored — the user controls
    /// spawning via the 's' keybinding with no concurrency limit.
    pub max_concurrency: usize,
    pub manual: bool,
    pub prefer: Option<String>,
    pub backend: Box<dyn Backend>,
    pub spawner: Box<dyn SessionSpawner>,
    pub persistent_sessions: bool,

    // Top-level task management session (not tied to any task)
    pub toplevel: Option<Box<dyn SessionHandle>>,

    pub term: TermSize,
    pub tree: TreeView,
    pub tree_dirty: bool,
}

pub const TOPLEVEL_SESSION_ID: &str = "ws/toplevel";

/// What the top-level loop should do next.
pub enum Action {
    Continue,
    ZoomIn(String), // task_name
    NextSession,
    PrevSession,
    ReturnToTree,
    TopLevel,
    Quit,
}

fn session_id_to_filename(session_id: &str) -> String {
    session_id.replace('/', "-")
}

/// Returns true if the error is an SQLite SQLITE_BUSY (database locked)
/// error, indicating transient lock contention rather than a real failure.
fn is_db_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<rusqlite::Error>().is_some_and(
        |e| matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::DatabaseBusy),
    )
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db_path: String,
        status_dir: PathBuf,
        max_concurrency: usize,
        manual: bool,
        prefer: Option<String>,
        backend: Box<dyn Backend>,
        term: TermSize,
        persistent_sessions: bool,
    ) -> Result<Self> {
        let conn = kbtz::db::open(&db_path).context("failed to open kbtz database")?;
        kbtz::db::init(&conn).context("failed to initialize kbtz database")?;
        // Agent sessions run concurrent kbtz commands that hold BEGIN IMMEDIATE
        // transactions.  With up to max_concurrency sessions all writing at
        // once, the workspace's own writes may need to queue behind them.
        // 60 seconds is generous enough that normal DB contention never crashes
        // the workspace, while still failing fast on genuine lock problems.
        conn.execute_batch("PRAGMA busy_timeout = 60000;")
            .context("failed to set workspace busy_timeout")?;
        let spawner: Box<dyn SessionSpawner> = if persistent_sessions {
            Box::new(ShepherdSpawner {
                status_dir: status_dir.clone(),
            })
        } else {
            Box::new(PtySpawner)
        };
        let mut app = App {
            db_path,
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.clone(),
            max_concurrency,
            manual,
            prefer,
            backend,
            spawner,
            persistent_sessions,
            toplevel: None,
            term,
            tree: TreeView {
                rows: Vec::new(),
                cursor: 0,
                list_state: ListState::default(),
                collapsed: HashSet::new(),
                error: None,
            },
            tree_dirty: false,
        };
        app.refresh_tree()?;
        if persistent_sessions {
            app.reconnect_sessions()?;
        }
        app.release_orphaned_tasks()?;
        app.spawn_toplevel()?;
        Ok(app)
    }

    /// Look up any session by its session_id, checking both worker sessions
    /// and the toplevel session.
    pub fn get_session(&self, session_id: &str) -> Option<&dyn SessionHandle> {
        if session_id == TOPLEVEL_SESSION_ID {
            self.toplevel.as_ref().map(|s| s.as_ref())
        } else {
            self.sessions.get(session_id).map(|s| s.as_ref())
        }
    }

    /// Mutable variant of `get_session`.
    pub fn get_session_mut(&mut self, session_id: &str) -> Option<&mut dyn SessionHandle> {
        if session_id == TOPLEVEL_SESSION_ID {
            match self.toplevel {
                Some(ref mut s) => Some(&mut **s),
                None => None,
            }
        } else {
            match self.sessions.get_mut(session_id) {
                Some(s) => Some(&mut **s),
                None => None,
            }
        }
    }

    /// Rebuild the tree view from the database.
    pub fn refresh_tree(&mut self) -> Result<()> {
        let mut tasks = ops::list_tasks(&self.conn, None, true, None, None, None)?;
        tasks.retain(|t| t.status != "done");
        self.tree.rows = kbtz::ui::flatten_tree(&tasks, &self.tree.collapsed, &self.conn)?;
        if !self.tree.rows.is_empty() {
            if self.tree.cursor >= self.tree.rows.len() {
                self.tree.cursor = self.tree.rows.len() - 1;
            }
            self.tree.list_state.select(Some(self.tree.cursor));
        } else {
            self.tree.cursor = 0;
            self.tree.list_state.select(None);
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
                } else if !session.reader_alive() {
                    // Reader thread died while child is still alive.
                    // The session is frozen (no output forwarding).
                    // Kill the child so the lifecycle can reap and
                    // respawn a fresh session for this task.
                    kbtz::debug_log::log(&format!(
                        "snapshot({session_id}): reader thread dead, child alive — \
                         killing frozen session"
                    ));
                    session.force_kill();
                    SessionPhase::Exited
                } else if let Some(since) = session.stopping_since() {
                    SessionPhase::Stopping { since }
                } else {
                    SessionPhase::Running
                };

                let task = match ops::get_task(&self.conn, session.task_name()) {
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

        // In manual mode, report max_concurrency as 0 so lifecycle::tick
        // never emits SpawnUpTo. Reaping/cleanup still runs normally.
        let effective_concurrency = if self.manual { 0 } else { self.max_concurrency };
        WorldSnapshot {
            sessions,
            max_concurrency: effective_concurrency,
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
                        self.backend.request_exit(session.as_mut());
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
    ///
    /// Uses a non-blocking busy_timeout so that write lock contention from
    /// concurrent agent sessions never stalls the UI thread.  If a write
    /// can't be acquired immediately, the tick is skipped and retried next
    /// time — the existing `is_db_busy` error handling in `spawn_up_to`
    /// and `remove_session` already handles this gracefully.
    pub fn tick(&mut self) -> Result<Option<String>> {
        let world = self.snapshot();
        let actions = lifecycle::tick(&world);
        if actions.is_empty() {
            return Ok(None);
        }
        self.conn
            .execute_batch("PRAGMA busy_timeout = 0;")
            .context("failed to set non-blocking busy_timeout")?;
        let result = self.execute_actions(actions);
        let _ = self.conn.execute_batch("PRAGMA busy_timeout = 60000;");
        result
    }

    /// Spawn sessions for claimable tasks, up to `count` new sessions.
    fn spawn_up_to(&mut self, count: usize) -> Result<()> {
        for _ in 0..count {
            self.counter += 1;
            let session_id = format!("ws/{}", self.counter);

            let claim = match ops::claim_next_task(&self.conn, &session_id, self.prefer.as_deref())
            {
                Ok(v) => v,
                Err(e) if is_db_busy(&e) => {
                    // Transient lock contention — skip this tick, try again next time.
                    self.counter -= 1;
                    break;
                }
                Err(e) => return Err(e),
            };
            match claim {
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
                            let _ = ops::release_task(&self.conn, &task_name, &session_id);
                            self.counter -= 1;
                            self.tree.error = Some(format!("failed to spawn session: {e}"));
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

    /// Claim and spawn a session for a specific task by name.
    pub fn spawn_for_task(&mut self, task_name: &str) -> Result<()> {
        if self.task_to_session.contains_key(task_name) {
            bail!("task already has an active session");
        }

        self.counter += 1;
        let session_id = format!("ws/{}", self.counter);

        ops::claim_task(&self.conn, task_name, &session_id)?;

        let task = match ops::get_task(&self.conn, task_name) {
            Ok(t) => t,
            Err(e) => {
                let _ = ops::release_task(&self.conn, task_name, &session_id);
                self.counter -= 1;
                return Err(e);
            }
        };

        match self.spawn_session(&task, &session_id) {
            Ok(session) => {
                self.task_to_session
                    .insert(task_name.to_string(), session_id.clone());
                self.sessions.insert(session_id, session);
                Ok(())
            }
            Err(e) => {
                let _ = ops::release_task(&self.conn, task_name, &session_id);
                self.counter -= 1;
                Err(e)
            }
        }
    }

    /// Spawn the top-level task management session.
    ///
    /// The toplevel is ephemeral — it's killed on quit and respawned on start.
    /// We use PtySpawner (not ShepherdSpawner) because there's no value in
    /// persisting a session that has no task claim and is cheap to recreate.
    fn spawn_toplevel(&mut self) -> Result<()> {
        let task_prompt =
            "You are the top-level task management agent. Help the user manage the kbtz task list.";
        let args = self
            .backend
            .toplevel_args(crate::prompt::TOPLEVEL_PROMPT, task_prompt);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let session_id = TOPLEVEL_SESSION_ID;
        let env_vars: Vec<(&str, &str)> = vec![("KBTZ_DB", &self.db_path)];
        let session = PtySpawner.spawn(
            self.backend.command(),
            &arg_refs,
            "toplevel",
            session_id,
            self.term.rows,
            self.term.cols,
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

    fn spawn_session(&self, task: &Task, session_id: &str) -> Result<Box<dyn SessionHandle>> {
        let task_prompt = format!("Work on task '{}': {}", task.name, task.description);
        let args = self
            .backend
            .worker_args(crate::prompt::AGENT_PROMPT, &task_prompt);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let status_dir_str = self.status_dir.to_string_lossy().to_string();
        let env_vars: Vec<(&str, &str)> = vec![
            ("KBTZ_DB", &self.db_path),
            ("KBTZ_SESSION_ID", session_id),
            ("KBTZ_TASK", &task.name),
            ("KBTZ_WORKSPACE_DIR", &status_dir_str),
        ];
        self.spawner.spawn(
            self.backend.command(),
            &arg_refs,
            &task.name,
            session_id,
            self.term.rows,
            self.term.cols,
            &env_vars,
        )
    }

    /// Read status files from the status directory and update session statuses.
    pub fn read_status_files(&mut self) -> Result<()> {
        for (session_id, session) in &mut self.sessions {
            let path = self.status_dir.join(session_id_to_filename(session_id));
            if let Ok(content) = std::fs::read_to_string(&path) {
                session.set_status(SessionStatus::from_str(&content));
            }
        }
        Ok(())
    }

    /// Remove a session, cleaning up its status/socket/pid files and releasing the task.
    fn remove_session(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.remove(session_id) {
            let task_name = session.task_name().to_string();
            let sid = session.session_id().to_string();
            let _ = ops::release_task(&self.conn, &task_name, &sid);
            // Only remove the task->session mapping if it still points to this
            // session. A new session may have already claimed the same task
            // (e.g. after a pause->unpause cycle), and we must not clobber it.
            if self.task_to_session.get(&task_name).map(String::as_str) == Some(session_id) {
                self.task_to_session.remove(&task_name);
            }
            let filename = session_id_to_filename(session_id);
            let _ = std::fs::remove_file(self.status_dir.join(&filename));
            let _ = std::fs::remove_file(self.status_dir.join(format!("{filename}.sock")));
            let _ = std::fs::remove_file(self.status_dir.join(format!("{filename}.pid")));
        }
    }

    /// Reconnect to shepherd sessions from a previous workspace instance.
    pub fn reconnect_sessions(&mut self) -> Result<()> {
        let entries = std::fs::read_dir(&self.status_dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_string_lossy();
            let session_id = stem.replacen('-', "/", 1);
            let pid_path = path.with_extension("pid");

            // Verify the shepherd process is still alive before attempting to connect.
            if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    let alive = unsafe { libc::kill(pid, 0) } == 0;
                    if !alive {
                        let err = std::io::Error::last_os_error();
                        if err.raw_os_error() == Some(libc::EPERM) {
                            kbtz::debug_log::log(&format!(
                                "reconnect_sessions: kill({pid}, 0) returned EPERM"
                            ));
                        }
                        // Shepherd died — clean up stale files
                        let _ = std::fs::remove_file(&path);
                        let _ = std::fs::remove_file(&pid_path);
                        if let Some(task_name) = self.find_task_for_session(&session_id) {
                            let _ = ops::release_task(&self.conn, &task_name, &session_id);
                        }
                        continue;
                    }
                }
            }

            // Look up the task claim in the DB
            match self.find_task_for_session(&session_id) {
                Some(task_name) => {
                    match ShepherdSession::connect(
                        &path,
                        &pid_path,
                        &task_name,
                        &session_id,
                        self.term.rows,
                        self.term.cols,
                    ) {
                        Ok(session) => {
                            if let Some(n) = session_id
                                .strip_prefix("ws/")
                                .and_then(|s| s.parse::<u64>().ok())
                            {
                                self.counter = self.counter.max(n);
                            }
                            self.task_to_session.insert(task_name, session_id.clone());
                            self.sessions.insert(session_id, Box::new(session));
                        }
                        Err(_) => {
                            // Stale socket -- clean up
                            let _ = std::fs::remove_file(&path);
                            let _ = std::fs::remove_file(&pid_path);
                            let _ = ops::release_task(&self.conn, &task_name, &session_id);
                        }
                    }
                }
                None => {
                    // No task claim -- orphaned shepherd. Kill and clean up.
                    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
                        if let Ok(pid) = pid_str.trim().parse::<i32>() {
                            unsafe { libc::kill(pid, libc::SIGKILL) };
                        }
                    }
                    let _ = std::fs::remove_file(&path);
                    let _ = std::fs::remove_file(&pid_path);
                }
            }
        }
        Ok(())
    }

    /// Release tasks that are "active" with a workspace assignee (ws/*)
    /// but have no corresponding in-memory session.
    ///
    /// This handles orphaned tasks left behind by a crashed workspace or
    /// failed session reconnections.  Releasing them to "open" allows
    /// the next tick() to re-claim and spawn sessions for them.
    fn release_orphaned_tasks(&self) -> Result<()> {
        let tasks = ops::list_tasks(&self.conn, None, true, None, None, None)?;
        for task in &tasks {
            if task.status != "active" {
                continue;
            }
            let Some(ref assignee) = task.assignee else {
                continue;
            };
            if !assignee.starts_with("ws/") {
                continue;
            }
            if self.task_to_session.contains_key(&task.name) {
                continue;
            }
            // Task is "active" with a workspace assignee but no session — orphaned.
            let _ = ops::release_task(&self.conn, &task.name, assignee);
        }
        Ok(())
    }

    fn find_task_for_session(&self, session_id: &str) -> Option<String> {
        ops::list_tasks(&self.conn, None, true, None, None, None)
            .ok()?
            .into_iter()
            .find(|t| t.assignee.as_deref() == Some(session_id))
            .map(|t| t.name)
    }

    /// Propagate terminal resize to all PTYs.
    pub fn handle_resize(&mut self, cols: u16, rows: u16) {
        self.term = TermSize { rows, cols };
        for session in self.sessions.values() {
            let _ = session.resize(rows, cols);
        }
        if let Some(ref toplevel) = self.toplevel {
            let _ = toplevel.resize(rows, cols);
        }
    }

    // Navigation

    pub fn move_up(&mut self) {
        if self.tree.cursor > 0 {
            self.tree.cursor -= 1;
            self.tree.list_state.select(Some(self.tree.cursor));
        }
    }

    pub fn move_down(&mut self) {
        if !self.tree.rows.is_empty() && self.tree.cursor < self.tree.rows.len() - 1 {
            self.tree.cursor += 1;
            self.tree.list_state.select(Some(self.tree.cursor));
        }
    }

    pub fn toggle_collapse(&mut self) {
        if let Some(row) = self.tree.rows.get(self.tree.cursor) {
            if row.has_children {
                let name = row.name.clone();
                if !self.tree.collapsed.remove(&name) {
                    self.tree.collapsed.insert(name);
                }
            }
        }
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.tree
            .rows
            .get(self.tree.cursor)
            .map(|r| r.name.as_str())
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
        self.sessions
            .get(next_sid)
            .map(|s| s.task_name().to_string())
    }

    /// Find the next session with NeedsInput status, cycling from current_task.
    /// Returns the task name if found.
    pub fn next_needs_input_session(&self, current_task: Option<&str>) -> Option<String> {
        let ids = self.session_ids_ordered();
        let needs_input: Vec<&String> = ids
            .iter()
            .filter(|id| {
                self.sessions
                    .get(*id)
                    .is_some_and(|s| *s.status() == SessionStatus::NeedsInput)
            })
            .collect();
        if needs_input.is_empty() {
            return None;
        }
        let current_sid = current_task.and_then(|task| self.task_to_session.get(task));
        let idx = cycle_after(&needs_input, current_sid.as_ref());
        let sid = needs_input[idx];
        self.sessions.get(sid).map(|s| s.task_name().to_string())
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

    /// Shut down all sessions.
    ///
    /// With persistent sessions, worker sessions survive via their shepherd
    /// processes and task claims are left intact.  Without persistent sessions,
    /// workers are killed and task claims are released.
    pub fn shutdown(&mut self) {
        if self.persistent_sessions {
            // Persistent mode: detach from sockets, leave shepherds running.
            for (_, _session) in self.sessions.drain() {}
            self.task_to_session.clear();
        } else {
            // Non-persistent mode: kill workers and release claims.
            let session_ids: Vec<String> = self.sessions.keys().cloned().collect();
            for session_id in session_ids {
                self.remove_session(&session_id);
            }
        }

        // Kill the toplevel session (it's ephemeral, not persistent).
        if let Some(ref mut toplevel) = self.toplevel {
            self.backend.request_exit(toplevel.as_mut());
        }
        let deadline = std::time::Instant::now() + GRACEFUL_TIMEOUT;
        loop {
            let toplevel_dead = self.toplevel.as_mut().is_none_or(|s| !s.is_alive());
            if toplevel_dead || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Some(ref mut toplevel) = self.toplevel {
            if toplevel.is_alive() {
                toplevel.force_kill();
            }
        }
        self.toplevel = None;

        // Clean up status files.  In persistent mode, preserve .sock/.pid/.lock
        // files that belong to shepherds.  Otherwise, clean everything except the
        // workspace lock.
        if let Ok(entries) = std::fs::read_dir(&self.status_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str());
                if ext == Some("lock") {
                    continue;
                }
                if self.persistent_sessions && (ext == Some("sock") || ext == Some("pid")) {
                    continue;
                }
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Given a sorted list of IDs and an optional current ID, return the index of
/// the first entry that comes after `current`. Wraps to 0 if `current` is past
/// all entries or is `None`.
fn cycle_after<T: Ord>(sorted: &[T], current: Option<&T>) -> usize {
    current
        .and_then(|cur| sorted.iter().position(|id| id > cur))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tempfile::TempDir;

    struct StubSession {
        task_name: String,
        session_id: String,
        status: SessionStatus,
        alive: bool,
        stopping_since: Option<Instant>,
    }

    impl StubSession {
        fn new(task_name: &str, session_id: &str, alive: bool) -> Self {
            Self {
                task_name: task_name.to_string(),
                session_id: session_id.to_string(),
                status: SessionStatus::Starting,
                alive,
                stopping_since: None,
            }
        }
    }

    impl SessionHandle for StubSession {
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
            self.alive
        }
        fn mark_stopping(&mut self) {
            if self.stopping_since.is_none() {
                self.stopping_since = Some(Instant::now());
            }
        }
        fn force_kill(&mut self) {
            self.alive = false;
        }
        fn start_passthrough(&self) -> Result<()> {
            Ok(())
        }
        fn stop_passthrough(&self) -> Result<()> {
            Ok(())
        }
        fn enter_scroll_mode(&self) -> Result<usize> {
            Ok(0)
        }
        fn exit_scroll_mode(&self) -> Result<()> {
            Ok(())
        }
        fn render_scrollback(&self, _offset: usize, _cols: u16) -> Result<usize> {
            Ok(0)
        }
        fn scrollback_available(&self) -> Result<usize> {
            Ok(0)
        }
        fn has_mouse_tracking(&self) -> bool {
            false
        }
        fn write_input(&mut self, _buf: &[u8]) -> Result<()> {
            Ok(())
        }
        fn resize(&self, _rows: u16, _cols: u16) -> Result<()> {
            Ok(())
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
        fn reader_alive(&self) -> bool {
            true
        }
    }

    struct StubBackend;

    impl Backend for StubBackend {
        fn command(&self) -> &str {
            "true"
        }
        fn worker_args(&self, _protocol_prompt: &str, _task_prompt: &str) -> Vec<String> {
            vec![]
        }
        fn request_exit(&self, session: &mut dyn SessionHandle) {
            session.mark_stopping();
        }
    }

    struct StubSpawner;

    impl SessionSpawner for StubSpawner {
        fn spawn(
            &self,
            _command: &str,
            _args: &[&str],
            task_name: &str,
            session_id: &str,
            _rows: u16,
            _cols: u16,
            _env_vars: &[(&str, &str)],
        ) -> Result<Box<dyn SessionHandle>> {
            Ok(Box::new(StubSession::new(task_name, session_id, true)))
        }
    }

    fn test_app() -> (App, TempDir) {
        let status_dir = TempDir::new().unwrap();
        let conn = kbtz::db::open_memory().unwrap();
        let app = App {
            db_path: ":memory:".to_string(),
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.path().to_path_buf(),
            max_concurrency: 2,
            manual: false,
            prefer: None,
            backend: Box::new(StubBackend),
            spawner: Box::new(StubSpawner),
            persistent_sessions: false,
            toplevel: None,
            term: TermSize { rows: 24, cols: 80 },
            tree: TreeView {
                rows: Vec::new(),
                cursor: 0,
                list_state: ListState::default(),
                collapsed: HashSet::new(),
                error: None,
            },
            tree_dirty: false,
        };
        (app, status_dir)
    }

    #[test]
    fn remove_session_cleans_up_mapping() {
        let (mut app, _dir) = test_app();
        ops::add_task(&app.conn, "task-a", None, "desc", None, None, false).unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            Box::new(StubSession::new("task-a", "ws/1", false)),
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        assert!(!app.sessions.contains_key("ws/1"));
        assert!(!app.task_to_session.contains_key("task-a"));
    }

    #[test]
    fn remove_session_preserves_newer_mapping() {
        let (mut app, _dir) = test_app();
        ops::add_task(&app.conn, "task-a", None, "desc", None, None, false).unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        // Simulate: ws/2 has already claimed the same task and updated the mapping.
        app.task_to_session
            .insert("task-a".to_string(), "ws/2".to_string());
        // But ws/1 is still in the sessions map (hasn't been cleaned up yet).
        app.sessions.insert(
            "ws/1".to_string(),
            Box::new(StubSession::new("task-a", "ws/1", false)),
        );

        app.remove_session("ws/1");

        // ws/1 should be removed from sessions...
        assert!(!app.sessions.contains_key("ws/1"));
        // ...but the task_to_session mapping should still point to ws/2.
        assert_eq!(
            app.task_to_session.get("task-a").map(String::as_str),
            Some("ws/2")
        );
    }

    #[test]
    fn execute_actions_processes_remove() {
        let (mut app, _dir) = test_app();
        ops::add_task(&app.conn, "task-a", None, "desc", None, None, false).unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            Box::new(StubSession::new("task-a", "ws/1", false)),
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        let actions = vec![lifecycle::SessionAction::Remove {
            session_id: "ws/1".to_string(),
        }];

        let result = app.execute_actions(actions).unwrap();
        assert!(result.is_some()); // should report "ws/1 exited"

        assert!(!app.sessions.contains_key("ws/1"));
        assert!(!app.task_to_session.contains_key("task-a"));
    }

    #[test]
    fn execute_actions_remove_then_spawn() {
        let (mut app, _dir) = test_app();
        ops::add_task(&app.conn, "task-a", None, "desc", None, None, false).unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();
        ops::add_task(&app.conn, "task-b", None, "desc", None, None, false).unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            Box::new(StubSession::new("task-a", "ws/1", false)),
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());
        app.counter = 1; // so next session is ws/2, not ws/1

        let actions = vec![
            lifecycle::SessionAction::Remove {
                session_id: "ws/1".to_string(),
            },
            lifecycle::SessionAction::SpawnUpTo { count: 1 },
        ];

        app.execute_actions(actions).unwrap();

        // ws/1 removed
        assert!(!app.sessions.contains_key("ws/1"));
        // A new session ws/2 was spawned for a claimable task
        assert!(app.sessions.contains_key("ws/2"));
        let new_task = app.sessions.get("ws/2").unwrap().task_name().to_string();
        assert_eq!(
            app.task_to_session.get(&new_task).map(String::as_str),
            Some("ws/2")
        );
    }

    #[test]
    fn cycle_after_no_current_returns_first() {
        let ids = vec!["a", "b", "c"];
        assert_eq!(cycle_after(&ids, None), 0);
    }

    #[test]
    fn cycle_after_advances_past_current() {
        let ids = vec!["ws/1", "ws/2", "ws/3"];
        assert_eq!(cycle_after(&ids, Some(&"ws/1")), 1);
        assert_eq!(cycle_after(&ids, Some(&"ws/2")), 2);
    }

    #[test]
    fn cycle_after_wraps_past_last() {
        let ids = vec!["ws/1", "ws/2", "ws/3"];
        assert_eq!(cycle_after(&ids, Some(&"ws/3")), 0);
    }

    #[test]
    fn cycle_after_wraps_when_current_beyond_all() {
        let ids = vec!["ws/1", "ws/2"];
        assert_eq!(cycle_after(&ids, Some(&"ws/9")), 0);
    }

    #[test]
    fn cycle_after_skips_gap_in_ids() {
        // Current is between entries (e.g. ws/2 deleted, current was ws/2)
        let ids = vec!["ws/1", "ws/3", "ws/5"];
        assert_eq!(cycle_after(&ids, Some(&"ws/2")), 1); // ws/3
        assert_eq!(cycle_after(&ids, Some(&"ws/4")), 2); // ws/5
    }

    #[test]
    fn cycle_after_single_entry_wraps() {
        let ids = vec!["ws/1"];
        assert_eq!(cycle_after(&ids, Some(&"ws/1")), 0);
    }

    #[test]
    fn cycle_after_single_entry_no_current() {
        let ids = vec!["ws/1"];
        assert_eq!(cycle_after(&ids, None), 0);
    }

    #[test]
    fn release_orphaned_tasks_releases_stale_ws_claims() {
        let (app, _dir) = test_app();
        // Create a task and claim it as if a previous workspace session owned it.
        ops::add_task(&app.conn, "orphan", None, "desc", None, None, false).unwrap();
        ops::claim_task(&app.conn, "orphan", "ws/99").unwrap();

        // No session exists in app.task_to_session — this is the orphan case.
        app.release_orphaned_tasks().unwrap();

        let task = ops::get_task(&app.conn, "orphan").unwrap();
        assert_eq!(task.status, "open");
        assert!(task.assignee.is_none());
    }

    #[test]
    fn release_orphaned_tasks_preserves_live_sessions() {
        let (mut app, _dir) = test_app();
        ops::add_task(&app.conn, "live", None, "desc", None, None, false).unwrap();
        ops::claim_task(&app.conn, "live", "ws/1").unwrap();

        // Simulate a live session by adding to task_to_session.
        app.task_to_session
            .insert("live".to_string(), "ws/1".to_string());

        app.release_orphaned_tasks().unwrap();

        let task = ops::get_task(&app.conn, "live").unwrap();
        assert_eq!(task.status, "active");
        assert_eq!(task.assignee.as_deref(), Some("ws/1"));
    }

    #[test]
    fn release_orphaned_tasks_ignores_non_ws_assignees() {
        let (app, _dir) = test_app();
        // Task claimed by an external agent, not a workspace session.
        ops::add_task(
            &app.conn,
            "external",
            None,
            "desc",
            None,
            Some("agent-1"),
            false,
        )
        .unwrap();

        app.release_orphaned_tasks().unwrap();

        let task = ops::get_task(&app.conn, "external").unwrap();
        assert_eq!(task.status, "active");
        assert_eq!(task.assignee.as_deref(), Some("agent-1"));
    }
}
