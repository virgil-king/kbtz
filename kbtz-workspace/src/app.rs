use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::ffi::ErrorCode;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use kbtz::model::Task;
use kbtz::ops;
use kbtz::ui::{ActiveTaskPolicy, NotesPanel, TreeView};

use crate::backend::Backend;
use crate::lifecycle::{
    self, SessionAction, SessionPhase, SessionSnapshot, WorldSnapshot, GRACEFUL_TIMEOUT,
};
use crate::session::{PtySpawner, SessionHandle, SessionSpawner, SessionStatus, ShepherdSpawner};
use crate::shepherd_session::ShepherdSession;

pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
}

pub struct TrackedSession {
    pub handle: Box<dyn SessionHandle>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub agent_type: String,
    pub unread: bool,
}

pub struct App {
    // kbtz state
    pub db_path: String,
    pub conn: Connection,

    // Session management
    pub sessions: HashMap<String, TrackedSession>, // session_id -> tracked session
    pub task_to_session: HashMap<String, String>,  // task_name -> session_id
    counter: u64,
    pub status_dir: PathBuf,
    /// Directory for storing session state per task, enabling session resume
    /// when agents exit mid-task.
    pub claude_sessions_dir: PathBuf,
    /// In auto mode, caps how many sessions lifecycle::tick will auto-spawn.
    /// In manual mode (--manual), this field is ignored — the user controls
    /// spawning via the 's' keybinding with no concurrency limit.
    pub max_concurrency: usize,
    pub manual: bool,
    pub prefer: Option<String>,
    pub backends: HashMap<String, Box<dyn Backend>>,
    pub default_backend: String,
    pub spawner: Box<dyn SessionSpawner>,
    pub persistent_sessions: bool,
    codex_home: Option<PathBuf>,
    /// Default working directory for agent sessions.
    /// Resolved at startup: config directory > workspace cwd.
    pub default_directory: PathBuf,

    // Top-level task management session (not tied to any task)
    pub toplevel: Option<Box<dyn SessionHandle>>,

    pub term: TermSize,
    pub tree: TreeView,
    pub tree_dirty: bool,
    pub notes_panel: Option<NotesPanel>,

    /// The session_id currently being viewed in passthrough mode (None in tree view).
    pub zoomed_session: Option<String>,
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
    kbtz::paths::session_id_to_filename(session_id)
}

/// Remove the status, socket, PID, and child-PID files for a session.
fn cleanup_session_files(status_dir: &Path, codex_home: Option<&Path>, session_id: &str) {
    let filename = session_id_to_filename(session_id);
    let profile_marker = status_dir.join(format!("{filename}.codex-profile"));
    cleanup_codex_profile_marker(&profile_marker, codex_home);
    let _ = std::fs::remove_file(status_dir.join(&filename));
    let _ = std::fs::remove_file(status_dir.join(format!("{filename}.sock")));
    let _ = std::fs::remove_file(status_dir.join(format!("{filename}.pid")));
    let _ = std::fs::remove_file(status_dir.join(format!("{filename}.child-pid")));
}

fn cleanup_codex_profile_marker(profile_marker: &Path, codex_home: Option<&Path>) {
    if let Ok(profile_path) = std::fs::read_to_string(profile_marker) {
        if let Some(codex_home) = codex_home {
            let profile_path = Path::new(profile_path.trim());
            if is_generated_codex_profile(profile_path, codex_home) {
                let _ = std::fs::remove_file(profile_path);
            }
        }
    }
    let _ = std::fs::remove_file(profile_marker);
}

fn is_generated_codex_profile(path: &Path, codex_home: &Path) -> bool {
    path.parent() == Some(codex_home)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("kbtz-") && name.ends_with(".config.toml"))
}

fn codex_home_from_env() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

/// Kill the agent child process group from the `.child-pid` file next to a shepherd `.pid` file.
fn kill_child_from_pid_file(pid_path: &Path) {
    let child_pid_path = pid_path.with_extension("child-pid");
    if let Ok(pid_str) = std::fs::read_to_string(&child_pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }
}

/// Kill a shepherd and its child, then remove all files for the session.
fn cleanup_failed_reconnect(
    status_dir: &Path,
    codex_home: Option<&Path>,
    session_id: &str,
    pid_path: &Path,
) {
    kill_child_from_pid_file(pid_path);
    if let Ok(pid_str) = std::fs::read_to_string(pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    cleanup_session_files(status_dir, codex_home, session_id);
}

/// Returns true if the error is an SQLite SQLITE_BUSY (database locked)
/// error, indicating transient lock contention rather than a real failure.
fn is_db_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<rusqlite::Error>().is_some_and(
        |e| matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::DatabaseBusy),
    )
}

/// Persisted identity for a task's agent conversation.
///
/// Older kbtz-workspace versions stored only the Claude session UUID. Those
/// files are still accepted as legacy Claude state so changing the workspace
/// default does not reinterpret an existing conversation as Codex.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct StoredSession {
    agent_type: String,
    session_id: String,
}

impl StoredSession {
    fn new(agent_type: &str, session_id: &str) -> Self {
        Self {
            agent_type: agent_type.to_string(),
            session_id: session_id.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct CodexSessionStart {
    session_id: String,
    hook_event_name: String,
}

pub fn capture_codex_session_from_hook() -> Result<()> {
    let path = std::env::var_os("KBTZ_SESSION_STATE_FILE")
        .map(PathBuf::from)
        .context("KBTZ_SESSION_STATE_FILE is not set")?;
    let agent_type = std::env::var("KBTZ_AGENT_TYPE").context("KBTZ_AGENT_TYPE is not set")?;
    capture_codex_session(std::io::stdin().lock(), &path, &agent_type)?;
    let codex_home = codex_home_from_env().context("neither CODEX_HOME nor HOME is set")?;
    if let Some(path) = std::env::var_os("KBTZ_CODEX_PROFILE_FILE") {
        let path = PathBuf::from(path);
        if !is_generated_codex_profile(&path, &codex_home) {
            bail!(
                "refusing to remove unexpected Codex profile {}",
                path.display()
            );
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    if let Some(path) = std::env::var_os("KBTZ_CODEX_PROFILE_MARKER") {
        let path = PathBuf::from(path);
        let status_dir = std::env::var_os("KBTZ_WORKSPACE_DIR")
            .map(PathBuf::from)
            .context("KBTZ_WORKSPACE_DIR is not set")?;
        let session_id = std::env::var("KBTZ_SESSION_ID").context("KBTZ_SESSION_ID is not set")?;
        let expected = status_dir.join(format!(
            "{}.codex-profile",
            session_id_to_filename(&session_id)
        ));
        if path != expected {
            bail!(
                "refusing to remove unexpected Codex profile marker {}",
                path.display()
            );
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn capture_codex_session(reader: impl std::io::Read, path: &Path, agent_type: &str) -> Result<()> {
    let event: CodexSessionStart =
        serde_json::from_reader(reader).context("invalid Codex hook input")?;
    if event.hook_event_name != "SessionStart" {
        bail!(
            "expected Codex SessionStart hook, got '{}'",
            event.hook_event_name
        );
    }
    if event.session_id.is_empty() {
        bail!("Codex SessionStart hook returned an empty session ID");
    }
    if agent_type.is_empty() {
        bail!("Codex SessionStart hook received an empty agent type");
    }
    App::write_session_file(path, &StoredSession::new(agent_type, &event.session_id))
}

struct CodexProfile {
    name: String,
    path: PathBuf,
    marker: PathBuf,
}

impl CodexProfile {
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.marker);
    }
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db_path: String,
        status_dir: PathBuf,
        max_concurrency: usize,
        manual: bool,
        prefer: Option<String>,
        backends: HashMap<String, Box<dyn Backend>>,
        default_backend: String,
        term: TermSize,
        persistent_sessions: bool,
        default_directory: PathBuf,
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
        let claude_sessions_dir = status_dir.join("claude-sessions");
        std::fs::create_dir_all(&claude_sessions_dir)
            .context("failed to create claude-sessions directory")?;
        let codex_home = codex_home_from_env();
        if !backends.contains_key(&default_backend) {
            bail!(
                "default backend '{}' not found in configured backends",
                default_backend
            );
        }
        let mut app = App {
            db_path,
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.clone(),
            claude_sessions_dir,
            max_concurrency,
            manual,
            prefer,
            backends,
            default_backend,
            spawner,
            persistent_sessions,
            codex_home,
            default_directory,
            toplevel: None,
            term,
            tree: TreeView::new(ActiveTaskPolicy::Confirm),
            tree_dirty: false,
            notes_panel: None,
            zoomed_session: None,
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
            self.sessions.get(session_id).map(|ts| ts.handle.as_ref())
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
                Some(ts) => Some(&mut *ts.handle),
                None => None,
            }
        }
    }

    /// Rebuild the tree view from the database.
    pub fn refresh_tree(&mut self) -> Result<()> {
        let mut tasks = ops::list_tasks(&self.conn, None, true, None, None, None)?;
        let session_tasks: std::collections::HashSet<String> =
            self.task_to_session.keys().cloned().collect();
        self.tree.filter_tasks(&mut tasks, &session_tasks);
        let rows = kbtz::ui::flatten_tree(&tasks, &self.tree.collapsed, &self.conn)?;
        self.tree.rows = match &self.tree.filter {
            Some(query) => kbtz::ui::filter_rows(&rows, query),
            None => rows,
        };
        self.tree.clamp_cursor();
        if let Some(panel) = &mut self.notes_panel {
            if let Some(name) = self.tree.selected_name() {
                panel.load(&self.conn, name)?;
            }
        }
        Ok(())
    }

    /// Toggle the notes panel for the currently selected task.
    pub fn toggle_notes(&mut self) -> Result<()> {
        if self.notes_panel.is_some() {
            self.notes_panel = None;
        } else {
            let mut panel = NotesPanel::new();
            if let Some(name) = self.tree.selected_name() {
                panel.load(&self.conn, name)?;
            }
            self.notes_panel = Some(panel);
        }
        Ok(())
    }

    // ── Lifecycle state machine ────────────────────────────────────────

    /// Resolve the agent type for a task.
    ///
    /// Stored session state wins over both the task's current agent field and
    /// the workspace default. This keeps an existing Claude conversation on
    /// Claude when new sessions default to Codex.
    fn resolve_agent_type(&self, task: &Task) -> Result<String> {
        if let Some(state) = Self::read_session_file(&self.session_file(task))? {
            return Ok(state.agent_type);
        }
        Ok(task
            .agent
            .as_deref()
            .unwrap_or(&self.default_backend)
            .to_string())
    }

    fn resolve_reconnected_agent_type(&self, task: &Task, session_id: &str) -> Result<String> {
        let state = Self::read_session_file(&self.session_file(task))?;
        if let Some(state) = state {
            return Ok(state.agent_type);
        }
        let marker = self.status_dir.join(format!(
            "{}.codex-profile",
            session_id_to_filename(session_id)
        ));
        if marker.exists() {
            return Ok("codex".to_string());
        }
        Ok(task
            .agent
            .as_deref()
            .unwrap_or(&self.default_backend)
            .to_string())
    }

    /// Ensure a backend exists for the given agent type. If no configured
    /// backend matches, creates and caches a generic backend using the
    /// type name as the command.
    fn ensure_backend(&mut self, agent_type: &str) {
        if !self.backends.contains_key(agent_type) {
            let backend = match agent_type {
                // Preserve the built-in behavior for named backends even when
                // the user did not keep an explicit [agent.*] section.
                "claude" | "codex" => crate::backend::from_name(agent_type, None, &[], &[]),
                _ => crate::backend::generic(agent_type),
            };
            self.backends.insert(agent_type.to_string(), backend);
        }
    }

    fn session_file(&self, task: &Task) -> PathBuf {
        self.claude_sessions_dir.join(&task.name)
    }

    fn create_codex_profile(
        &self,
        backend: &dyn Backend,
        session_id: &str,
    ) -> Result<CodexProfile> {
        let content = backend
            .session_start_profile()
            .context("Codex backend did not provide a SessionStart profile")?;
        let codex_home = self
            .codex_home
            .as_deref()
            .context("neither CODEX_HOME nor HOME is set for Codex session")?;
        if !codex_home.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(codex_home)
                .with_context(|| format!("failed to create Codex home {}", codex_home.display()))?;
        } else if !codex_home.is_dir() {
            bail!("Codex home does not exist: {}", codex_home.display());
        }
        let marker = self.status_dir.join(format!(
            "{}.codex-profile",
            session_id_to_filename(session_id)
        ));
        cleanup_codex_profile_marker(&marker, Some(codex_home));

        let name = format!("kbtz-{}", uuid::Uuid::new_v4());
        let path = codex_home.join(format!("{name}.config.toml"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to create Codex profile {}", path.display()))?;
        if let Err(error) = file.write_all(content.as_bytes()) {
            let _ = std::fs::remove_file(&path);
            return Err(error)
                .with_context(|| format!("failed to write Codex profile {}", path.display()));
        }

        if let Err(error) = std::fs::write(&marker, path.to_string_lossy().as_bytes()) {
            let _ = std::fs::remove_file(&path);
            return Err(error).with_context(|| {
                format!("failed to write Codex profile marker {}", marker.display())
            });
        }
        Ok(CodexProfile { name, path, marker })
    }

    /// Get the default backend (used for toplevel session and request_exit fallback).
    fn default_backend(&self) -> &dyn Backend {
        self.backends[&self.default_backend].as_ref()
    }

    fn request_session_exit(&mut self, session_id: &str) {
        let Some(agent_type) = self
            .sessions
            .get(session_id)
            .map(|session| session.agent_type.clone())
        else {
            return;
        };
        self.ensure_backend(&agent_type);
        let backend = self.backends[&agent_type].as_ref();
        if let Some(session) = self.sessions.get_mut(session_id) {
            backend.request_exit(session.handle.as_mut());
        }
    }

    pub fn stop_task_session(&mut self, task_name: &str) {
        if let Some(session_id) = self.task_to_session.get(task_name).cloned() {
            self.request_session_exit(&session_id);
        }
    }

    fn reconcile_task_sessions(&mut self) -> Result<()> {
        let tasks = match ops::list_tasks(&self.conn, None, true, None, None, None) {
            Ok(tasks) => tasks,
            Err(error) if is_db_busy(&error) => return Ok(()),
            Err(error) => return Err(error).context("failed to reconcile task sessions"),
        };
        let mut sessions_to_stop = Vec::new();
        for (session_id, session) in &self.sessions {
            if session.handle.stopping_since().is_some() {
                continue;
            }
            let task_name = session.handle.task_name();
            let still_owned = tasks.iter().any(|task| {
                task.name == task_name
                    && task.status == "active"
                    && task.assignee.as_deref() == Some(session_id.as_str())
            });
            if !still_owned {
                sessions_to_stop.push(session_id.clone());
            }
        }
        for session_id in sessions_to_stop {
            kbtz::debug_log::log(&format!(
                "reconcile: requesting exit for {session_id} because its task is no longer owned"
            ));
            self.request_session_exit(&session_id);
        }
        Ok(())
    }

    /// Build a snapshot of the current world for the pure tick function.
    fn snapshot(&mut self) -> WorldSnapshot {
        let sessions = self
            .sessions
            .iter_mut()
            .map(|(session_id, ts)| {
                let phase = if !ts.handle.is_alive() {
                    SessionPhase::Exited
                } else if !ts.handle.reader_alive() {
                    // Reader thread died while child is still alive.
                    // The session is frozen (no output forwarding).
                    // Kill the child so the lifecycle can reap and
                    // respawn a fresh session for this task.
                    kbtz::debug_log::log(&format!(
                        "snapshot({session_id}): reader thread dead, child alive — \
                         killing frozen session"
                    ));
                    ts.handle.force_kill();
                    SessionPhase::Exited
                } else if let Some(since) = ts.handle.stopping_since() {
                    SessionPhase::Stopping { since }
                } else {
                    SessionPhase::Running
                };

                SessionSnapshot {
                    session_id: session_id.clone(),
                    phase,
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
                SessionAction::ForceKill { session_id } => {
                    if let Some(ts) = self.sessions.get_mut(&session_id) {
                        kbtz::debug_log::log(&format!(
                            "action: force_kill {} (task={})",
                            session_id,
                            ts.handle.task_name()
                        ));
                        ts.handle.force_kill();
                        descriptions.push(format!("{session_id} killed"));
                    }
                }
                SessionAction::Remove { session_id } => {
                    if self.sessions.contains_key(&session_id) {
                        let task = self.sessions[&session_id].handle.task_name().to_string();
                        kbtz::debug_log::log(&format!(
                            "action: remove {} (task={})",
                            session_id, task
                        ));
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
        self.conn
            .execute_batch("PRAGMA busy_timeout = 0;")
            .context("failed to set non-blocking busy_timeout")?;
        let result = (|| {
            self.reconcile_task_sessions()?;
            let world = self.snapshot();
            let actions = lifecycle::tick(&world);
            if actions.is_empty() {
                Ok(None)
            } else {
                self.execute_actions(actions)
            }
        })();
        let _ = self.conn.execute_batch("PRAGMA busy_timeout = 60000;");
        result
    }

    /// Spawn sessions for claimable tasks, up to `count` new sessions.
    fn spawn_up_to(&mut self, count: usize) -> Result<()> {
        for _ in 0..count {
            self.counter += 1;
            let session_id = format!("{}{}", kbtz::paths::SESSION_ID_PREFIX, self.counter);

            let claim =
                match ops::claim_next_task(&self.conn, &session_id, self.prefer.as_deref(), None) {
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
                    kbtz::debug_log::log(&format!("spawn: claimed {task_name} as {session_id}"));
                    let task = match ops::get_task(&self.conn, &task_name) {
                        Ok(task) => task,
                        Err(error) => {
                            let release = ops::release_task(&self.conn, &task_name, &session_id);
                            self.counter -= 1;
                            if let Err(release_error) = release {
                                return Err(error).context(format!(
                                    "also failed to release {task_name} after task lookup error: \
                                     {release_error:#}"
                                ));
                            }
                            return Err(error)
                                .context(format!("failed to load claimed task {task_name}"));
                        }
                    };
                    let agent_type = match self.resolve_agent_type(&task) {
                        Ok(agent_type) => agent_type,
                        Err(error) => {
                            let release = ops::release_task(&self.conn, &task_name, &session_id);
                            self.counter -= 1;
                            if let Err(release_error) = release {
                                return Err(error).context(format!(
                                    "also failed to release {task_name} after session-state error: \
                                     {release_error:#}"
                                ));
                            }
                            return Err(error).context(format!(
                                "failed to resolve session state for {task_name}"
                            ));
                        }
                    };
                    self.ensure_backend(&agent_type);
                    let backend = self.backends[&agent_type].as_ref();

                    match self.spawn_session_with(backend, &agent_type, &task, &session_id) {
                        Ok(handle) => {
                            kbtz::debug_log::log(&format!(
                                "spawn: session started for {task_name} ({session_id}, agent={agent_type})"
                            ));
                            self.task_to_session
                                .insert(task_name.clone(), session_id.clone());
                            self.sessions.insert(
                                session_id,
                                TrackedSession {
                                    handle,
                                    agent_type,
                                    unread: false,
                                },
                            );
                        }
                        Err(e) => {
                            kbtz::debug_log::log(&format!(
                                "spawn: FAILED for {task_name} ({session_id}): {e}"
                            ));
                            // Failed to spawn — release the claim
                            if let Err(release_error) =
                                ops::release_task(&self.conn, &task_name, &session_id)
                            {
                                return Err(e).context(format!(
                                    "also failed to release {task_name} after spawn error: \
                                     {release_error:#}"
                                ));
                            }
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

        let task = ops::get_task(&self.conn, task_name)?;
        if task.status != "open" {
            bail!("task '{task_name}' is {}, not open", task.status);
        }
        let agent_type = self.resolve_agent_type(&task)?;

        self.counter += 1;
        let session_id = format!("{}{}", kbtz::paths::SESSION_ID_PREFIX, self.counter);

        kbtz::debug_log::log(&format!(
            "spawn_for_task: claiming {task_name} as {session_id}"
        ));
        ops::claim_task(&self.conn, task_name, &session_id)?;

        self.ensure_backend(&agent_type);
        let backend = self.backends[&agent_type].as_ref();
        match self.spawn_session_with(backend, &agent_type, &task, &session_id) {
            Ok(handle) => {
                kbtz::debug_log::log(&format!(
                    "spawn_for_task: session started for {task_name} ({session_id}, agent={agent_type})"
                ));
                self.task_to_session
                    .insert(task_name.to_string(), session_id.clone());
                self.sessions.insert(
                    session_id,
                    TrackedSession {
                        handle,
                        agent_type,
                        unread: false,
                    },
                );
                Ok(())
            }
            Err(e) => {
                kbtz::debug_log::log(&format!(
                    "spawn_for_task: FAILED for {task_name} ({session_id}): {e}"
                ));
                if let Err(release_error) = ops::release_task(&self.conn, task_name, &session_id) {
                    return Err(e).context(format!(
                        "also failed to release {task_name} after spawn error: {release_error:#}"
                    ));
                }
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
        let initial_prompt =
            "You are the top-level task management agent. Help the user manage the kbtz task list.";
        let backend = self.default_backend();
        let args = backend.toplevel_args(crate::prompt::TOPLEVEL_PROMPT, initial_prompt);
        let initial_status = backend.initial_status();
        let command = backend.command().to_string();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let session_id = TOPLEVEL_SESSION_ID;
        let env_vars: Vec<(&str, &str)> = vec![("KBTZ_DB", &self.db_path)];
        let mut session = PtySpawner.spawn(
            &command,
            &arg_refs,
            "toplevel",
            session_id,
            self.term.rows,
            self.term.cols,
            &env_vars,
            &self.default_directory,
        )?;
        if let Some(status) = initial_status {
            session.set_status(status);
        }
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

    fn spawn_session_with(
        &self,
        backend: &dyn Backend,
        agent_type: &str,
        task: &Task,
        session_id: &str,
    ) -> Result<Box<dyn SessionHandle>> {
        let initial_prompt = format!("Work on task '{}': {}", task.name, task.description);
        let system_instructions = crate::prompt::AGENT_PROMPT;
        let session_file = self.session_file(task);

        // Try to resume a previous session if one exists.
        let resume_prompt = format!(
            "Your previous session was interrupted. Continue working on task '{}': {}",
            task.name, task.description
        );
        let stored_state = Self::read_session_file(&session_file)?;
        let profile = if stored_state.is_none() && backend.captures_session_id_on_start() {
            Some(self.create_codex_profile(backend, session_id)?)
        } else {
            None
        };
        let profile_name = profile.as_ref().map(|profile| profile.name.as_str());
        // new_state is Some when the backend accepts a caller-provided ID.
        // It is written only after a successful spawn.
        let (args, is_resume, new_state) = if let Some(stored_state) = stored_state {
            if let Some(resume_args) = backend.resume_args(
                system_instructions,
                &stored_state.session_id,
                &resume_prompt,
            ) {
                kbtz::debug_log::log(&format!(
                    "spawn_session: resuming {} (session {}, agent={})",
                    task.name, stored_state.session_id, agent_type
                ));
                (resume_args, true, None)
            } else {
                // Backend doesn't support resume; start fresh (no tracking).
                let _ = std::fs::remove_file(&session_file);
                (
                    backend.worker_args(system_instructions, &initial_prompt, None),
                    false,
                    None,
                )
            }
        } else if backend.captures_session_id_on_start() {
            (
                backend.worker_args(system_instructions, &initial_prompt, profile_name),
                false,
                None,
            )
        } else {
            // No stored session. Try fresh_args for session tracking, fall back to worker_args.
            let uuid = uuid::Uuid::new_v4().to_string();
            if let Some(fresh_args) =
                backend.fresh_args(system_instructions, &initial_prompt, &uuid)
            {
                kbtz::debug_log::log(&format!(
                    "spawn_session: fresh {} (session {}, agent={})",
                    task.name, uuid, agent_type
                ));
                (
                    fresh_args,
                    false,
                    Some(StoredSession::new(agent_type, &uuid)),
                )
            } else {
                (
                    backend.worker_args(system_instructions, &initial_prompt, None),
                    false,
                    None,
                )
            }
        };

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let command = backend.command();
        let status_dir_str = self.status_dir.to_string_lossy().to_string();
        let session_file_str = session_file.to_string_lossy().to_string();
        let profile_file_str = profile
            .as_ref()
            .map(|profile| profile.path.to_string_lossy().into_owned());
        let profile_marker_str = profile
            .as_ref()
            .map(|profile| profile.marker.to_string_lossy().into_owned());
        let debug_path = std::env::var("KBTZ_DEBUG").unwrap_or_default();
        let mut env_vars: Vec<(&str, &str)> = vec![
            ("KBTZ_DB", &self.db_path),
            ("KBTZ_SESSION_ID", session_id),
            ("KBTZ_TASK", &task.name),
            ("KBTZ_WORKSPACE_DIR", &status_dir_str),
            ("KBTZ_AGENT_TYPE", agent_type),
            ("KBTZ_SESSION_STATE_FILE", &session_file_str),
        ];
        if let Some(path) = &profile_file_str {
            env_vars.push(("KBTZ_CODEX_PROFILE_FILE", path));
        }
        if let Some(path) = &profile_marker_str {
            env_vars.push(("KBTZ_CODEX_PROFILE_MARKER", path));
        }
        if !debug_path.is_empty() {
            env_vars.push(("KBTZ_DEBUG", &debug_path));
        }
        // Resolve session working directory: task override > config default
        let session_dir = task
            .directory
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_directory.clone());
        let result = self.spawner.spawn(
            command,
            &arg_refs,
            &task.name,
            session_id,
            self.term.rows,
            self.term.cols,
            &env_vars,
            &session_dir,
        );

        match result {
            Ok(mut session) => {
                if let Some(status) = backend.initial_status() {
                    session.set_status(status);
                }

                // Write the session file only after a successful spawn and
                // prompt injection. Writing earlier would leave a stale
                // session ID if initialization failed.
                if let Some(state) = &new_state {
                    if let Err(e) = Self::write_session_file(&session_file, state) {
                        session.force_kill();
                        cleanup_session_files(
                            &self.status_dir,
                            self.codex_home.as_deref(),
                            session_id,
                        );
                        return Err(e).with_context(|| {
                            format!("failed to persist session state for {}", task.name)
                        });
                    }
                }
                Ok(session)
            }
            Err(e) if is_resume => {
                // Resume failed at the spawn level — delete the session
                // file so the next attempt starts fresh.
                kbtz::debug_log::log(&format!(
                    "spawn_session: resume failed for {}, clearing session file",
                    task.name
                ));
                let _ = std::fs::remove_file(&session_file);
                Err(e)
            }
            Err(e) => {
                if let Some(profile) = &profile {
                    profile.cleanup();
                }
                Err(e)
            }
        }
    }

    /// Read stored session state, accepting the legacy raw Claude UUID format.
    fn read_session_file(path: &std::path::Path) -> Result<Option<StoredSession>> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read session file {}", path.display()))
            }
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        if trimmed.starts_with('{') {
            let state: StoredSession = serde_json::from_str(trimmed)
                .with_context(|| format!("invalid session state in {}", path.display()))?;
            if state.agent_type.is_empty() || state.session_id.is_empty() {
                bail!("session state in {} has an empty field", path.display());
            }
            return Ok(Some(state));
        }
        Ok(Some(StoredSession::new("claude", trimmed)))
    }

    fn write_session_file(path: &std::path::Path, state: &StoredSession) -> Result<()> {
        let content = serde_json::to_vec(state).context("failed to encode session state")?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("session state path has no file name")?;
        let temp_path = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temp_path, content)
            .with_context(|| format!("failed to write session file {}", temp_path.display()))?;
        if let Err(error) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error)
                .with_context(|| format!("failed to replace session file {}", path.display()));
        }
        Ok(())
    }

    /// Read status files from the status directory and update session statuses.
    pub fn read_status_files(&mut self) -> Result<()> {
        for (session_id, ts) in &mut self.sessions {
            let path = self.status_dir.join(session_id_to_filename(session_id));
            if let Ok(content) = std::fs::read_to_string(&path) {
                let new_status = SessionStatus::from_str(&content);
                if *ts.handle.status() != new_status {
                    kbtz::debug_log::log(&format!(
                        "status: {} {} -> {} (task={})",
                        session_id,
                        ts.handle.status().label(),
                        new_status.label(),
                        ts.handle.task_name()
                    ));
                    // Mark unread if the user is not currently viewing this session.
                    if self.zoomed_session.as_deref() != Some(session_id.as_str()) {
                        ts.unread = true;
                    }
                }
                ts.handle.set_status(new_status);
            }
        }
        Ok(())
    }

    /// Remove a session, cleaning up its status/socket/pid files and releasing the task.
    ///
    /// The session file is preserved across all task states (done,
    /// paused, blocked, active) so that reopened or re-spawned tasks can
    /// resume the previous conversation. The file is only deleted when the
    /// child crashed during init (non-zero exit without a prior graceful-exit
    /// request), indicating a broken session that would cause a crash loop,
    /// or when the task has been deleted from the database.
    fn remove_session(&mut self, session_id: &str) {
        if let Some(mut ts) = self.sessions.remove(session_id) {
            let task_name = ts.handle.task_name().to_string();
            let sid = ts.handle.session_id().to_string();
            kbtz::debug_log::log(&format!(
                "remove_session: {sid} (task={task_name}, status={})",
                ts.handle.status().label()
            ));
            // Check if the task still exists. Distinguish "task deleted" from
            // transient DB errors (lock contention) — only treat missing tasks
            // as deleted. Preserving the session file on DB errors avoids
            // losing conversation context due to a transient lock.
            let task_deleted = match ops::get_task(&self.conn, &task_name) {
                Ok(_) => false,
                Err(e) if is_db_busy(&e) => {
                    kbtz::debug_log::log(&format!(
                        "remove_session: DB busy looking up {task_name}, \
                         preserving session file"
                    ));
                    false
                }
                Err(_) => true, // task deleted or other error
            };
            let _ = ops::release_task(&self.conn, &task_name, &sid);
            // Only remove the task->session mapping if it still points to this
            // session. A new session may have already claimed the same task
            // (e.g. after a pause->unpause cycle), and we must not clobber it.
            if self.task_to_session.get(&task_name).map(String::as_str) == Some(session_id) {
                self.task_to_session.remove(&task_name);
            }
            cleanup_session_files(&self.status_dir, self.codex_home.as_deref(), session_id);

            // Only treat a non-zero exit as a crash if we never requested
            // the session to stop. When we send SIGTERM for blocking/done/
            // paused reaps, the child may be killed by the signal (exit
            // code 1 via portable_pty) even though the conversation is
            // valid and resumable. Only unsolicited non-zero exits indicate
            // a crash during init that would cause a resume-crash loop.
            let was_requested = ts.handle.stopping_since().is_some();
            let child_failed =
                !was_requested && ts.handle.exit_code().is_some_and(|code| code != 0);

            if task_deleted || child_failed {
                if child_failed {
                    kbtz::debug_log::log(&format!(
                        "remove_session: clearing session file for {task_name} \
                         (child exited with non-zero code)"
                    ));
                }
                let _ = std::fs::remove_file(self.claude_sessions_dir.join(&task_name));
            }
        }
    }

    /// Reconnect to shepherd sessions from a previous workspace instance.
    ///
    /// This deliberately ignores `max_concurrency` — ALL surviving sessions
    /// are reconnected so that work-in-progress is never lost on restart.
    /// The concurrency limit is only applied later when deciding whether to
    /// spawn *new* sessions via `lifecycle::tick()`.
    pub fn reconnect_sessions(&mut self) -> Result<()> {
        kbtz::debug_log::log("reconnect: scanning for shepherd sockets");
        let entries = std::fs::read_dir(&self.status_dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_string_lossy();
            let session_id = kbtz::paths::filename_to_session_id(&stem);
            let pid_path = path.with_extension("pid");

            // Verify the shepherd process is still alive before attempting to connect.
            let pid_result = std::fs::read_to_string(&pid_path);
            if pid_result.is_err() {
                kbtz::debug_log::log(&format!(
                    "reconnect: no PID file for {session_id} at {}, skipping",
                    pid_path.display()
                ));
                cleanup_session_files(&self.status_dir, self.codex_home.as_deref(), &session_id);
                continue;
            }
            if let Ok(pid_str) = pid_result {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    let ret = unsafe { libc::kill(pid, 0) };
                    let errno = std::io::Error::last_os_error();
                    let alive =
                        ret == 0 || (ret == -1 && errno.raw_os_error() == Some(libc::EPERM));
                    kbtz::debug_log::log(&format!(
                        "reconnect: checking {session_id} shepherd pid={pid} \
                         kill(0)={ret} errno={errno} alive={alive}"
                    ));
                    if !alive {
                        // Shepherd died — clean up stale files and kill orphaned child
                        kill_child_from_pid_file(&pid_path);
                        cleanup_session_files(
                            &self.status_dir,
                            self.codex_home.as_deref(),
                            &session_id,
                        );
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
                    // Resolve the stored backend before connecting so a
                    // malformed session-state file fails before a live
                    // shepherd is adopted.
                    let task = match ops::get_task(&self.conn, &task_name) {
                        Ok(task) => task,
                        Err(error) => {
                            cleanup_failed_reconnect(
                                &self.status_dir,
                                self.codex_home.as_deref(),
                                &session_id,
                                &pid_path,
                            );
                            let release = ops::release_task(&self.conn, &task_name, &session_id);
                            if let Err(release_error) = release {
                                return Err(error).context(format!(
                                    "also failed to release {task_name} after reconnect task \
                                     lookup error: {release_error:#}"
                                ));
                            }
                            return Err(error).context(format!(
                                "failed to load task {task_name} while reconnecting {session_id}"
                            ));
                        }
                    };
                    let agent_type = match self.resolve_reconnected_agent_type(&task, &session_id) {
                        Ok(agent_type) => agent_type,
                        Err(error) => {
                            cleanup_failed_reconnect(
                                &self.status_dir,
                                self.codex_home.as_deref(),
                                &session_id,
                                &pid_path,
                            );
                            let release = ops::release_task(&self.conn, &task_name, &session_id);
                            if let Err(release_error) = release {
                                return Err(error).context(format!(
                                    "also failed to release {task_name} after reconnect session \
                                     state error: {release_error:#}"
                                ));
                            }
                            return Err(error).context(format!(
                                "failed to resolve session state for {task_name} while \
                                 reconnecting {session_id}"
                            ));
                        }
                    };
                    self.ensure_backend(&agent_type);
                    match ShepherdSession::connect(
                        &path,
                        &pid_path,
                        &task_name,
                        &session_id,
                        self.term.rows,
                        self.term.cols,
                        None, // no Child handle for reconnected sessions
                    ) {
                        Ok(mut session) => {
                            if let Some(status) = self.backends[&agent_type].initial_status() {
                                session.set_status(status);
                            }
                            kbtz::debug_log::log(&format!(
                                "reconnect: adopted {session_id} (task={task_name}, agent={agent_type})"
                            ));
                            if let Some(n) = session_id
                                .strip_prefix(kbtz::paths::SESSION_ID_PREFIX)
                                .and_then(|s| s.parse::<u64>().ok())
                            {
                                self.counter = self.counter.max(n);
                            }
                            self.task_to_session.insert(task_name, session_id.clone());
                            self.sessions.insert(
                                session_id,
                                TrackedSession {
                                    handle: Box::new(session),
                                    agent_type,
                                    unread: false,
                                },
                            );
                        }
                        Err(e) => {
                            kbtz::debug_log::log(&format!(
                                "reconnect: connect FAILED for {session_id} \
                                 (task={task_name}): {e:#}"
                            ));
                            cleanup_failed_reconnect(
                                &self.status_dir,
                                self.codex_home.as_deref(),
                                &session_id,
                                &pid_path,
                            );
                            let _ = ops::release_task(&self.conn, &task_name, &session_id);
                        }
                    }
                }
                None => {
                    kbtz::debug_log::log(&format!(
                        "reconnect: orphaned shepherd {session_id}, killing"
                    ));
                    // No task claim -- orphaned shepherd. Kill child and shepherd, clean up.
                    cleanup_failed_reconnect(
                        &self.status_dir,
                        self.codex_home.as_deref(),
                        &session_id,
                        &pid_path,
                    );
                }
            }
        }
        let count = self.sessions.len();
        if count > self.max_concurrency {
            kbtz::debug_log::log(&format!(
                "reconnect: done, {count} sessions active (exceeds max_concurrency={}; \
                 all reconnected sessions preserved)",
                self.max_concurrency
            ));
        } else {
            kbtz::debug_log::log(&format!("reconnect: done, {count} sessions active"));
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
            if !assignee.starts_with(kbtz::paths::SESSION_ID_PREFIX) {
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
        for ts in self.sessions.values() {
            let _ = ts.handle.resize(rows, cols);
        }
        if let Some(ref toplevel) = self.toplevel {
            let _ = toplevel.resize(rows, cols);
        }
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
            .map(|ts| ts.handle.task_name().to_string())
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
                    .is_some_and(|ts| *ts.handle.status() == SessionStatus::NeedsInput)
            })
            .collect();
        if needs_input.is_empty() {
            return None;
        }
        let current_sid = current_task.and_then(|task| self.task_to_session.get(task));
        let idx = cycle_after(&needs_input, current_sid.as_ref());
        let sid = needs_input[idx];
        self.sessions
            .get(sid)
            .map(|ts| ts.handle.task_name().to_string())
    }

    /// Find the next unread session, cycling from current_task.
    /// Returns the task name if found.
    pub fn next_unread_session(&self, current_task: Option<&str>) -> Option<String> {
        let ids = self.session_ids_ordered();
        let unread: Vec<&String> = ids
            .iter()
            .filter(|id| self.sessions.get(*id).is_some_and(|ts| ts.unread))
            .collect();
        if unread.is_empty() {
            return None;
        }
        let current_sid = current_task.and_then(|task| self.task_to_session.get(task));
        let idx = cycle_after(&unread, current_sid.as_ref());
        let sid = unread[idx];
        self.sessions
            .get(sid)
            .map(|ts| ts.handle.task_name().to_string())
    }

    /// Mark a session as read (clear unread flag). Called when zooming in.
    pub fn mark_read(&mut self, session_id: &str) {
        if let Some(ts) = self.sessions.get_mut(session_id) {
            ts.unread = false;
        }
    }

    /// Kill and release a session for a task so it can be respawned.
    ///
    /// Deletes the stored session file so the respawn starts a fresh
    /// conversation rather than resuming.
    pub fn restart_session(&mut self, task_name: &str) {
        if let Some(session_id) = self.task_to_session.get(task_name).cloned() {
            kbtz::debug_log::log(&format!(
                "restart_session: killing {session_id} (task={task_name})"
            ));
            if let Some(ts) = self.sessions.get_mut(&session_id) {
                ts.handle.force_kill();
            }
            // Delete session file before remove_session (which would preserve
            // it for active tasks). Restart means the user wants fresh.
            let _ = std::fs::remove_file(self.claude_sessions_dir.join(task_name));
            self.remove_session(&session_id);
        }
    }

    /// Shut down all sessions.
    ///
    /// With persistent sessions, worker sessions survive via their shepherd
    /// processes and task claims are left intact.  Without persistent sessions,
    /// workers are killed and task claims are released.
    pub fn shutdown(&mut self) {
        kbtz::debug_log::log(&format!(
            "shutdown: {} sessions, persistent={}",
            self.sessions.len(),
            self.persistent_sessions
        ));
        if self.persistent_sessions {
            // Persistent mode: detach from sockets, leave shepherds running.
            for (_, _ts) in self.sessions.drain() {}
            self.task_to_session.clear();
        } else {
            // Non-persistent mode: ask every backend to exit, then force-kill
            // survivors after one shared grace period.
            let session_ids: Vec<String> = self.sessions.keys().cloned().collect();
            for session_id in &session_ids {
                self.request_session_exit(session_id);
            }
            let deadline = std::time::Instant::now() + GRACEFUL_TIMEOUT;
            while std::time::Instant::now() < deadline
                && self
                    .sessions
                    .values_mut()
                    .any(|session| session.handle.is_alive())
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            for session_id in &session_ids {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    if session.handle.is_alive() {
                        session.handle.force_kill();
                    }
                }
            }
            for session_id in session_ids {
                self.remove_session(&session_id);
            }
        }

        // Kill the toplevel session (it's ephemeral, not persistent).
        if let Some(ref mut toplevel) = self.toplevel {
            // default_backend is validated at construction, so this is safe.
            self.backends[&self.default_backend].request_exit(toplevel.as_mut());
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

        // Clean up session status/socket/pid files.
        if let Ok(entries) = std::fs::read_dir(&self.status_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                if !kbtz::paths::is_session_filename(name) {
                    continue;
                }
                let ext = path.extension().and_then(|e| e.to_str());
                if self.persistent_sessions {
                    if matches!(ext, Some("sock" | "pid" | "child-pid")) {
                        continue;
                    }
                    if ext == Some("codex-profile") {
                        let socket_name = format!(
                            "{}.sock",
                            name.strip_suffix(".codex-profile")
                                .expect("extension was checked")
                        );
                        if self.status_dir.join(socket_name).exists() {
                            continue;
                        }
                    }
                }
                if ext == Some("codex-profile") {
                    cleanup_codex_profile_marker(&path, self.codex_home.as_deref());
                } else {
                    let _ = std::fs::remove_file(path);
                }
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
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    use tempfile::TempDir;

    struct StubSession {
        task_name: String,
        session_id: String,
        status: SessionStatus,
        alive: bool,
        stopping_since: Option<Instant>,
        exit_code: Option<i32>,
        input: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
    }

    impl StubSession {
        fn new(task_name: &str, session_id: &str, alive: bool) -> Self {
            Self {
                task_name: task_name.to_string(),
                session_id: session_id.to_string(),
                status: SessionStatus::Starting,
                alive,
                stopping_since: None,
                exit_code: None,
                input: None,
            }
        }

        fn with_input(
            task_name: &str,
            session_id: &str,
            alive: bool,
            input: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        ) -> Self {
            let mut session = Self::new(task_name, session_id, alive);
            session.input = Some(input);
            session
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
        fn write_input(&mut self, buf: &[u8]) -> Result<()> {
            if let Some(input) = &self.input {
                input.lock().unwrap().extend_from_slice(buf);
            }
            Ok(())
        }
        fn resize(&self, _rows: u16, _cols: u16) -> Result<()> {
            Ok(())
        }
        fn terminal_sync_bytes(&self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
        fn reader_alive(&self) -> bool {
            true
        }
        fn exit_code(&mut self) -> Option<i32> {
            self.exit_code
        }
    }

    struct StubBackend;

    impl Backend for StubBackend {
        fn command(&self) -> &str {
            "true"
        }
        fn worker_args(
            &self,
            _system_instructions: &str,
            _initial_prompt: &str,
            _session_profile: Option<&str>,
        ) -> Vec<String> {
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
            _cwd: &std::path::Path,
        ) -> Result<Box<dyn SessionHandle>> {
            Ok(Box::new(StubSession::new(task_name, session_id, true)))
        }
    }

    struct StateWriteFailureSpawner;

    impl SessionSpawner for StateWriteFailureSpawner {
        fn spawn(
            &self,
            _command: &str,
            _args: &[&str],
            task_name: &str,
            session_id: &str,
            _rows: u16,
            _cols: u16,
            env_vars: &[(&str, &str)],
            _cwd: &std::path::Path,
        ) -> Result<Box<dyn SessionHandle>> {
            let env = |name| {
                env_vars
                    .iter()
                    .find_map(|(key, value)| (*key == name).then_some(*value))
                    .context(format!("missing {name}"))
            };
            std::fs::create_dir(env("KBTZ_SESSION_STATE_FILE")?)?;
            let status_dir = Path::new(env("KBTZ_WORKSPACE_DIR")?);
            let filename = session_id_to_filename(session_id);
            for suffix in ["sock", "pid", "child-pid"] {
                std::fs::write(status_dir.join(format!("{filename}.{suffix}")), b"test")?;
            }
            Ok(Box::new(StubSession::new(task_name, session_id, true)))
        }
    }

    type CapturedEnv = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<(String, String)>)>>>;

    /// Spawner that captures env vars for test assertions.
    struct CapturingSpawner {
        captured_env: CapturedEnv,
    }

    impl CapturingSpawner {
        fn new() -> (Self, CapturedEnv) {
            let captured: CapturedEnv = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let spawner = Self {
                captured_env: captured.clone(),
            };
            (spawner, captured)
        }
    }

    fn env_for_task(captured: &CapturedEnv, task_name: &str) -> Vec<(String, String)> {
        let data = captured.lock().unwrap();
        data.iter()
            .find(|(name, _)| name == task_name)
            .map(|(_, env)| env.clone())
            .unwrap_or_default()
    }

    impl SessionSpawner for CapturingSpawner {
        fn spawn(
            &self,
            _command: &str,
            _args: &[&str],
            task_name: &str,
            session_id: &str,
            _rows: u16,
            _cols: u16,
            env_vars: &[(&str, &str)],
            _cwd: &std::path::Path,
        ) -> Result<Box<dyn SessionHandle>> {
            let env: Vec<(String, String)> = env_vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            self.captured_env
                .lock()
                .unwrap()
                .push((task_name.to_string(), env));
            Ok(Box::new(StubSession::new(task_name, session_id, true)))
        }
    }

    struct SpawnRecord {
        task_name: String,
        command: String,
        args: Vec<String>,
        input: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    type SpawnRecords = std::sync::Arc<std::sync::Mutex<Vec<SpawnRecord>>>;

    struct RecordingSpawner {
        records: SpawnRecords,
        capture_codex_state: bool,
    }

    impl RecordingSpawner {
        fn with_codex_capture(capture_codex_state: bool) -> (Self, SpawnRecords) {
            let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Self {
                    records: records.clone(),
                    capture_codex_state,
                },
                records,
            )
        }
    }

    impl SessionSpawner for RecordingSpawner {
        fn spawn(
            &self,
            command: &str,
            args: &[&str],
            task_name: &str,
            session_id: &str,
            _rows: u16,
            _cols: u16,
            env_vars: &[(&str, &str)],
            _cwd: &std::path::Path,
        ) -> Result<Box<dyn SessionHandle>> {
            let input = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            self.records.lock().unwrap().push(SpawnRecord {
                task_name: task_name.to_string(),
                command: command.to_string(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                input: input.clone(),
            });
            let env = |name| {
                env_vars
                    .iter()
                    .find_map(|(key, value)| (*key == name).then_some(*value))
            };
            if self.capture_codex_state && env("KBTZ_CODEX_PROFILE_FILE").is_some() {
                let state_path = env("KBTZ_SESSION_STATE_FILE")
                    .map(std::path::Path::new)
                    .context("missing Codex session state path")?;
                let agent_type = env("KBTZ_AGENT_TYPE").context("missing agent type")?;
                if !state_path.exists() {
                    App::write_session_file(
                        state_path,
                        &StoredSession::new(agent_type, "codex-session-1"),
                    )?;
                }
            }
            Ok(Box::new(StubSession::with_input(
                task_name, session_id, true, input,
            )))
        }
    }

    fn test_app() -> (App, TempDir) {
        let status_dir = TempDir::new().unwrap();
        let claude_sessions_dir = status_dir.path().join("claude-sessions");
        std::fs::create_dir_all(&claude_sessions_dir).unwrap();
        let conn = kbtz::db::open_memory().unwrap();
        let mut backends: HashMap<String, Box<dyn Backend>> = HashMap::new();
        backends.insert("claude".to_string(), Box::new(StubBackend));
        let app = App {
            db_path: ":memory:".to_string(),
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.path().to_path_buf(),
            claude_sessions_dir,
            max_concurrency: 2,
            manual: false,
            prefer: None,
            backends,
            default_backend: "claude".to_string(),
            spawner: Box::new(StubSpawner),
            persistent_sessions: false,
            codex_home: Some(status_dir.path().to_path_buf()),
            default_directory: std::env::current_dir().unwrap(),
            toplevel: None,
            term: TermSize { rows: 24, cols: 80 },
            tree: TreeView::new(ActiveTaskPolicy::Confirm),
            tree_dirty: false,
            notes_panel: None,
            zoomed_session: None,
        };
        (app, status_dir)
    }

    #[test]
    fn remove_session_cleans_up_mapping() {
        let (mut app, _dir) = test_app();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
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
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        // Simulate: ws/2 has already claimed the same task and updated the mapping.
        app.task_to_session
            .insert("task-a".to_string(), "ws/2".to_string());
        // But ws/1 is still in the sessions map (hasn't been cleaned up yet).
        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
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
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
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
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-b",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
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
        let new_task = app
            .sessions
            .get("ws/2")
            .unwrap()
            .handle
            .task_name()
            .to_string();
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
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "orphan",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
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
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "live",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "live", "ws/1").unwrap();

        // Simulate a live session by adding to task_to_session.
        app.task_to_session
            .insert("live".to_string(), "ws/1".to_string());

        app.release_orphaned_tasks().unwrap();

        let task = ops::get_task(&app.conn, "live").unwrap();
        assert_eq!(task.status, "active");
        assert_eq!(task.assignee.as_deref(), Some("ws/1"));
    }

    // ── Session resume tests ──────────────────────────────────────────

    struct ResumableStubBackend;

    impl Backend for ResumableStubBackend {
        fn command(&self) -> &str {
            "true"
        }
        fn worker_args(
            &self,
            _system_instructions: &str,
            _initial_prompt: &str,
            _session_profile: Option<&str>,
        ) -> Vec<String> {
            vec!["worker".into()]
        }
        fn fresh_args(
            &self,
            _system_instructions: &str,
            _initial_prompt: &str,
            session_id: &str,
        ) -> Option<Vec<String>> {
            Some(vec!["--session-id".into(), session_id.into()])
        }
        fn resume_args(
            &self,
            _system_instructions: &str,
            session_id: &str,
            initial_prompt: &str,
        ) -> Option<Vec<String>> {
            Some(vec![
                "--resume".into(),
                session_id.into(),
                initial_prompt.into(),
            ])
        }
        fn request_exit(&self, session: &mut dyn SessionHandle) {
            session.mark_stopping();
        }
    }

    fn test_app_resumable() -> (App, TempDir) {
        let status_dir = TempDir::new().unwrap();
        let claude_sessions_dir = status_dir.path().join("claude-sessions");
        std::fs::create_dir_all(&claude_sessions_dir).unwrap();
        let conn = kbtz::db::open_memory().unwrap();
        let mut backends: HashMap<String, Box<dyn Backend>> = HashMap::new();
        backends.insert("claude".to_string(), Box::new(ResumableStubBackend));
        let app = App {
            db_path: ":memory:".to_string(),
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.path().to_path_buf(),
            claude_sessions_dir,
            max_concurrency: 2,
            manual: false,
            prefer: None,
            backends,
            default_backend: "claude".to_string(),
            spawner: Box::new(StubSpawner),
            persistent_sessions: false,
            codex_home: Some(status_dir.path().to_path_buf()),
            default_directory: std::env::current_dir().unwrap(),
            toplevel: None,
            term: TermSize { rows: 24, cols: 80 },
            tree: TreeView::new(ActiveTaskPolicy::Confirm),
            tree_dirty: false,
            notes_panel: None,
            zoomed_session: None,
        };
        (app, status_dir)
    }

    #[test]
    fn spawn_session_creates_session_file() {
        let (app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        let task = ops::get_task(&app.conn, "task-a").unwrap();

        let backend = app.default_backend();
        app.spawn_session_with(backend, "claude", &task, "ws/1")
            .unwrap();

        let session_file = app.claude_sessions_dir.join("task-a");
        assert!(session_file.exists(), "session file should be created");
        let encoded = std::fs::read_to_string(&session_file).unwrap();
        assert!(
            !encoded.trim().is_empty(),
            "session file should not be empty"
        );
        let state = App::read_session_file(&session_file).unwrap().unwrap();
        assert_eq!(state.agent_type, "claude");
        assert!(!state.session_id.is_empty());
    }

    #[test]
    fn session_state_write_failure_cleans_spawned_session_files() {
        let (mut app, _dir) = test_app_resumable();
        app.spawner = Box::new(StateWriteFailureSpawner);
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();

        let error = app.spawn_for_task("task-a").unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to persist session state"));
        for suffix in ["sock", "pid", "child-pid"] {
            assert!(!app.status_dir.join(format!("ws-1.{suffix}")).exists());
        }
        let task = ops::get_task(&app.conn, "task-a").unwrap();
        assert_eq!(task.status, "open");
        assert!(task.assignee.is_none());
    }

    #[test]
    fn legacy_and_structured_session_files_decode_to_same_state() {
        let dir = TempDir::new().unwrap();
        let legacy = dir.path().join("legacy");
        std::fs::write(&legacy, "legacy-uuid").unwrap();
        assert_eq!(
            App::read_session_file(&legacy).unwrap(),
            Some(StoredSession::new("claude", "legacy-uuid"))
        );

        let structured = dir.path().join("structured");
        let expected = StoredSession::new("claude", "structured-uuid");
        App::write_session_file(&structured, &expected).unwrap();
        assert_eq!(App::read_session_file(&structured).unwrap(), Some(expected));
    }

    #[test]
    fn codex_session_start_hook_persists_exact_session_id() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("task-a");
        let input = br#"{
            "session_id": "019ffd8e-3b9b-70e2-b58f-a0a4e4a097d5",
            "hook_event_name": "SessionStart"
        }"#;

        capture_codex_session(input.as_slice(), &path, "codex-yolo").unwrap();

        assert_eq!(
            App::read_session_file(&path).unwrap(),
            Some(StoredSession::new(
                "codex-yolo",
                "019ffd8e-3b9b-70e2-b58f-a0a4e4a097d5"
            ))
        );
    }

    #[test]
    fn codex_session_hook_rejects_wrong_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("task-a");
        let input = br#"{"session_id":"session-1","hook_event_name":"Stop"}"#;

        let error = capture_codex_session(input.as_slice(), &path, "codex").unwrap_err();

        assert!(error.to_string().contains("expected Codex SessionStart"));
        assert!(!path.exists());
    }

    #[test]
    fn spawn_session_resumes_from_existing_file() {
        let (app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        let task = ops::get_task(&app.conn, "task-a").unwrap();

        // Write a fake UUID to the session file
        let session_file = app.claude_sessions_dir.join("task-a");
        std::fs::write(&session_file, "fake-uuid-12345").unwrap();

        // Spawn should read the stored UUID (resume path)
        let backend = app.default_backend();
        let session = app
            .spawn_session_with(backend, "claude", &task, "ws/1")
            .unwrap();
        assert_eq!(session.task_name(), "task-a");

        // Session file should still exist with the same UUID
        let uuid = std::fs::read_to_string(&session_file).unwrap();
        assert_eq!(uuid, "fake-uuid-12345");
    }

    #[test]
    fn remove_session_preserves_file_for_active_task() {
        let (mut app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        // Write a structured session file.
        let session_file = app.claude_sessions_dir.join("task-a");
        let expected_state = StoredSession::new("claude", "some-uuid");
        App::write_session_file(&session_file, &expected_state).unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        // Session file should be preserved (task is still active -> open after release)
        assert!(
            session_file.exists(),
            "session file should be preserved for active task"
        );
    }

    #[test]
    fn remove_session_preserves_file_for_done_task() {
        let (mut app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();
        // Mark the task as done
        ops::mark_done(&app.conn, "task-a").unwrap();

        // Write a structured session file.
        let session_file = app.claude_sessions_dir.join("task-a");
        let expected_state = StoredSession::new("claude", "some-uuid");
        App::write_session_file(&session_file, &expected_state).unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        // Session file should be preserved so reopened tasks can resume
        assert!(
            session_file.exists(),
            "session file should be preserved for done task (enables resume after reopen)"
        );
        assert_eq!(
            App::read_session_file(&session_file).unwrap(),
            Some(expected_state)
        );
    }

    #[test]
    fn remove_session_preserves_file_for_paused_task() {
        let (mut app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();
        ops::pause_task(&app.conn, "task-a").unwrap();

        let session_file = app.claude_sessions_dir.join("task-a");
        std::fs::write(&session_file, "some-uuid").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        assert!(
            session_file.exists(),
            "session file should be preserved for paused task"
        );
    }

    #[test]
    fn remove_session_deletes_file_on_nonzero_exit() {
        let (mut app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        let session_file = app.claude_sessions_dir.join("task-a");
        std::fs::write(&session_file, "some-uuid").unwrap();

        let mut stub = StubSession::new("task-a", "ws/1", false);
        stub.exit_code = Some(1);
        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(stub),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        assert!(
            !session_file.exists(),
            "session file should be deleted when child exited with non-zero code"
        );
    }

    #[test]
    fn remove_session_preserves_file_when_stopped_with_nonzero_exit() {
        // When a session is intentionally stopped (e.g. task blocked),
        // the child may exit non-zero (signal kill via portable_pty).
        // The session file should still be preserved for resume.
        let (mut app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        let session_file = app.claude_sessions_dir.join("task-a");
        std::fs::write(&session_file, "some-uuid").unwrap();

        let mut stub = StubSession::new("task-a", "ws/1", false);
        stub.exit_code = Some(1);
        stub.mark_stopping(); // session was intentionally stopped
        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(stub),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        assert!(
            session_file.exists(),
            "session file should be preserved when intentionally stopped, \
             even with non-zero exit code"
        );
    }

    #[test]
    fn remove_session_preserves_file_on_zero_exit() {
        let (mut app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        let session_file = app.claude_sessions_dir.join("task-a");
        std::fs::write(&session_file, "some-uuid").unwrap();

        let mut stub = StubSession::new("task-a", "ws/1", false);
        stub.exit_code = Some(0);
        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(stub),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        assert!(
            session_file.exists(),
            "session file should be preserved when child exited with zero code"
        );
    }

    #[test]
    fn remove_session_deletes_file_for_deleted_task() {
        let (mut app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();
        // Delete the task from the database
        ops::remove_task(&app.conn, "task-a", false).unwrap();

        let session_file = app.claude_sessions_dir.join("task-a");
        std::fs::write(&session_file, "some-uuid").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        assert!(
            !session_file.exists(),
            "session file should be deleted when task is deleted from database"
        );
    }

    #[test]
    fn restart_session_deletes_session_file() {
        let (mut app, _dir) = test_app_resumable();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        // Write a session file
        let session_file = app.claude_sessions_dir.join("task-a");
        std::fs::write(&session_file, "some-uuid").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", true)),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.restart_session("task-a");

        // Session file should be deleted (user wants fresh start)
        assert!(!session_file.exists(), "restart should delete session file");
    }

    #[test]
    fn non_resumable_backend_creates_no_session_file() {
        // Uses default StubBackend which returns None for fresh_args/resume_args
        let (app, _dir) = test_app();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        let task = ops::get_task(&app.conn, "task-a").unwrap();

        let backend = app.default_backend();
        app.spawn_session_with(backend, "claude", &task, "ws/1")
            .unwrap();

        let session_file = app.claude_sessions_dir.join("task-a");
        assert!(
            !session_file.exists(),
            "non-resumable backend should not create session file"
        );
    }

    #[test]
    fn claude_spawn_does_not_require_codex_home() {
        let (mut app, _dir) = test_app();
        app.codex_home = None;
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "claude-only",
                description: "start without Codex configuration",
                ..Default::default()
            },
        )
        .unwrap();

        app.spawn_for_task("claude-only").unwrap();

        assert!(app.task_to_session.contains_key("claude-only"));
    }

    /// Create a test app with multiple backends: "claude" (default) and "gemini".
    fn test_app_multi_backend() -> (App, TempDir) {
        let status_dir = TempDir::new().unwrap();
        let claude_sessions_dir = status_dir.path().join("claude-sessions");
        std::fs::create_dir_all(&claude_sessions_dir).unwrap();
        let conn = kbtz::db::open_memory().unwrap();
        let mut backends: HashMap<String, Box<dyn Backend>> = HashMap::new();
        backends.insert("claude".to_string(), Box::new(StubBackend));
        backends.insert("gemini".to_string(), Box::new(StubBackend));
        let app = App {
            db_path: ":memory:".to_string(),
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.path().to_path_buf(),
            claude_sessions_dir,
            max_concurrency: 2,
            manual: false,
            prefer: None,
            backends,
            default_backend: "claude".to_string(),
            spawner: Box::new(StubSpawner),
            persistent_sessions: false,
            codex_home: Some(status_dir.path().to_path_buf()),
            default_directory: std::env::current_dir().unwrap(),
            toplevel: None,
            term: TermSize { rows: 24, cols: 80 },
            tree: TreeView::new(ActiveTaskPolicy::Confirm),
            tree_dirty: false,
            notes_panel: None,
            zoomed_session: None,
        };
        (app, status_dir)
    }

    fn test_app_codex_recording_with_capture(
        capture_codex_state: bool,
    ) -> (App, TempDir, SpawnRecords) {
        let status_dir = TempDir::new().unwrap();
        let claude_sessions_dir = status_dir.path().join("claude-sessions");
        std::fs::create_dir_all(&claude_sessions_dir).unwrap();
        let conn = kbtz::db::open_memory().unwrap();
        let mut backends: HashMap<String, Box<dyn Backend>> = HashMap::new();
        backends.insert(
            "claude".to_string(),
            crate::backend::from_name("claude", None, &[], &[]),
        );
        backends.insert(
            "codex".to_string(),
            crate::backend::from_name("codex", None, &[], &[]),
        );
        let (spawner, records) = RecordingSpawner::with_codex_capture(capture_codex_state);
        let app = App {
            db_path: ":memory:".to_string(),
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.path().to_path_buf(),
            claude_sessions_dir,
            max_concurrency: 2,
            manual: false,
            prefer: None,
            backends,
            default_backend: "codex".to_string(),
            spawner: Box::new(spawner),
            persistent_sessions: false,
            codex_home: Some(status_dir.path().to_path_buf()),
            default_directory: std::env::current_dir().unwrap(),
            toplevel: None,
            term: TermSize { rows: 24, cols: 80 },
            tree: TreeView::new(ActiveTaskPolicy::Confirm),
            tree_dirty: false,
            notes_panel: None,
            zoomed_session: None,
        };
        (app, status_dir, records)
    }

    fn test_app_codex_recording() -> (App, TempDir, SpawnRecords) {
        test_app_codex_recording_with_capture(true)
    }

    #[test]
    fn spawn_uses_correct_backend_for_task_with_agent() {
        let (mut app, _dir) = test_app_multi_backend();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "gemini-task",
                description: "test gemini",
                agent: Some("gemini"),
                ..Default::default()
            },
        )
        .unwrap();

        app.spawn_up_to(1).unwrap();

        assert_eq!(app.sessions.len(), 1);
        let session_id = app.task_to_session.get("gemini-task").unwrap();
        assert_eq!(
            app.sessions
                .get(session_id)
                .map(|ts| ts.agent_type.as_str()),
            Some("gemini")
        );
    }

    #[test]
    fn spawn_uses_default_backend_for_task_without_agent() {
        let (mut app, _dir) = test_app_multi_backend();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "plain-task",
                description: "no agent specified",
                ..Default::default()
            },
        )
        .unwrap();

        app.spawn_up_to(1).unwrap();

        assert_eq!(app.sessions.len(), 1);
        let session_id = app.task_to_session.get("plain-task").unwrap();
        assert_eq!(
            app.sessions
                .get(session_id)
                .map(|ts| ts.agent_type.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn stored_legacy_claude_session_keeps_claude_when_default_is_codex() {
        let (mut app, _dir, records) = test_app_codex_recording();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "legacy-claude",
                description: "resume old conversation",
                ..Default::default()
            },
        )
        .unwrap();
        let session_file = app.claude_sessions_dir.join("legacy-claude");
        std::fs::write(&session_file, "claude-session-1").unwrap();

        app.spawn_for_task("legacy-claude").unwrap();

        let session_id = app.task_to_session.get("legacy-claude").unwrap();
        assert_eq!(
            app.sessions
                .get(session_id)
                .map(|session| session.agent_type.as_str()),
            Some("claude")
        );
        let records = records.lock().unwrap();
        let record = records
            .iter()
            .find(|record| record.task_name == "legacy-claude")
            .unwrap();
        assert_eq!(record.command, "claude");
        assert_eq!(record.args[0..2], ["--resume", "claude-session-1"]);
        assert!(
            record.input.lock().unwrap().is_empty(),
            "Claude receives its prompt through CLI arguments"
        );
    }

    #[test]
    fn structured_session_state_wins_over_task_agent_and_default() {
        let (mut app, _dir, records) = test_app_codex_recording();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "structured-claude",
                description: "resume with stored backend",
                agent: Some("codex"),
                ..Default::default()
            },
        )
        .unwrap();
        let session_file = app.claude_sessions_dir.join("structured-claude");
        let state = StoredSession::new("claude", "claude-session-2");
        App::write_session_file(&session_file, &state).unwrap();

        app.spawn_for_task("structured-claude").unwrap();

        let session_id = app.task_to_session.get("structured-claude").unwrap();
        assert_eq!(
            app.sessions
                .get(session_id)
                .map(|session| session.agent_type.as_str()),
            Some("claude")
        );
        let records = records.lock().unwrap();
        let record = records
            .iter()
            .find(|record| record.task_name == "structured-claude")
            .unwrap();
        assert_eq!(record.args[0..2], ["--resume", "claude-session-2"]);
    }

    #[test]
    fn fresh_task_uses_airchat_codex_and_injects_prompt() {
        let (mut app, _dir, records) = test_app_codex_recording();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "fresh-codex",
                description: "start a Codex task",
                ..Default::default()
            },
        )
        .unwrap();

        app.spawn_for_task("fresh-codex").unwrap();

        let session_id = app.task_to_session.get("fresh-codex").unwrap();
        let session = app.sessions.get(session_id).unwrap();
        assert_eq!(session.agent_type, "codex");
        assert_eq!(*session.handle.status(), SessionStatus::Active);

        let records = records.lock().unwrap();
        let record = records
            .iter()
            .find(|record| record.task_name == "fresh-codex")
            .unwrap();
        assert_eq!(record.command, "airchat");
        assert_eq!(
            &record.args[..4],
            [
                "codex",
                "--",
                "--dangerously-bypass-hook-trust",
                "--profile",
            ]
        );
        assert!(record.args[4].starts_with("kbtz-"));
        assert!(record
            .args
            .last()
            .is_some_and(|arg| arg.contains(crate::prompt::AGENT_PROMPT)));
        assert!(record
            .args
            .last()
            .is_some_and(|arg| arg.contains("Work on task 'fresh-codex': start a Codex task")));
        assert!(record.input.lock().unwrap().is_empty());
        assert_eq!(
            App::read_session_file(&app.claude_sessions_dir.join("fresh-codex"))
                .unwrap()
                .unwrap(),
            StoredSession::new("codex", "codex-session-1")
        );
    }

    #[test]
    fn fresh_codex_session_can_wait_for_first_turn_capture() {
        let (mut app, _dir, records) = test_app_codex_recording_with_capture(false);
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "pending-codex",
                description: "wait for trust confirmation",
                ..Default::default()
            },
        )
        .unwrap();

        app.spawn_for_task("pending-codex").unwrap();

        assert!(app.sessions.contains_key("ws/1"));
        assert!(!app.claude_sessions_dir.join("pending-codex").exists());
        let record = records.lock().unwrap();
        let profile_name = &record[0].args[4];
        let profile_path = app
            .codex_home
            .as_ref()
            .unwrap()
            .join(format!("{profile_name}.config.toml"));
        let marker_path = app.status_dir.join("ws-1.codex-profile");
        assert!(profile_path.exists());
        assert_eq!(
            std::fs::read_to_string(&marker_path).unwrap(),
            profile_path.to_string_lossy()
        );

        cleanup_session_files(&app.status_dir, app.codex_home.as_deref(), "ws/1");
        assert!(!profile_path.exists());
        assert!(!marker_path.exists());
    }

    #[test]
    fn fresh_codex_session_creates_missing_codex_home() {
        let (mut app, _dir, _records) = test_app_codex_recording_with_capture(false);
        let codex_home = app.status_dir.join("missing-codex-home");
        app.codex_home = Some(codex_home.clone());
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "fresh-codex",
                description: "create Codex configuration",
                ..Default::default()
            },
        )
        .unwrap();

        app.spawn_for_task("fresh-codex").unwrap();

        assert!(codex_home.is_dir());
        assert_eq!(
            std::fs::metadata(&codex_home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        cleanup_session_files(&app.status_dir, app.codex_home.as_deref(), "ws/1");
    }

    #[test]
    fn codex_profile_cleanup_rejects_untrusted_marker_target() {
        let (app, _dir) = test_app();
        let protected = app.status_dir.join("important.txt");
        let marker = app.status_dir.join("ws-1.codex-profile");
        std::fs::write(&protected, b"keep").unwrap();
        std::fs::write(&marker, protected.to_string_lossy().as_bytes()).unwrap();

        cleanup_codex_profile_marker(&marker, app.codex_home.as_deref());

        assert!(protected.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn codex_profile_relaunch_reclaims_stale_profile_for_reused_session_id() {
        let (app, _dir) = test_app();
        let backend = crate::backend::from_name("codex", None, &[], &[]);
        let first = app.create_codex_profile(backend.as_ref(), "ws/1").unwrap();
        let stale_path = first.path.clone();

        let second = app.create_codex_profile(backend.as_ref(), "ws/1").unwrap();

        assert!(!stale_path.exists());
        assert!(second.path.exists());
        assert_eq!(
            std::fs::read_to_string(&second.marker).unwrap(),
            second.path.to_string_lossy()
        );
        second.cleanup();
    }

    #[test]
    fn codex_task_resumes_exact_stored_session() {
        let (mut app, _dir, records) = test_app_codex_recording();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "resumed-codex",
                description: "continue a Codex task",
                ..Default::default()
            },
        )
        .unwrap();
        App::write_session_file(
            &app.claude_sessions_dir.join("resumed-codex"),
            &StoredSession::new("codex", "019ffd8e-3b9b-70e2-b58f-a0a4e4a097d5"),
        )
        .unwrap();

        app.spawn_for_task("resumed-codex").unwrap();

        let records = records.lock().unwrap();
        let record = records
            .iter()
            .find(|record| record.task_name == "resumed-codex")
            .unwrap();
        assert_eq!(
            &record.args[..4],
            [
                "codex",
                "--",
                "resume",
                "019ffd8e-3b9b-70e2-b58f-a0a4e4a097d5",
            ]
        );
        assert!(record
            .args
            .last()
            .is_some_and(|arg| arg.contains("Your previous session was interrupted.")));
        assert!(record.input.lock().unwrap().is_empty());
    }

    #[test]
    fn codex_alias_is_persisted_and_reused_for_resume() {
        let (mut app, _dir, records) = test_app_codex_recording();
        app.backends.insert(
            "codex-yolo".to_string(),
            crate::backend::from_name(
                "codex",
                Some("airchat"),
                &[
                    "codex".to_string(),
                    "-a".to_string(),
                    "never".to_string(),
                    "--".to_string(),
                ],
                &[],
            ),
        );
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "alias-codex",
                description: "retain alias configuration",
                agent: Some("codex-yolo"),
                ..Default::default()
            },
        )
        .unwrap();

        app.spawn_for_task("alias-codex").unwrap();

        let state_path = app.claude_sessions_dir.join("alias-codex");
        assert_eq!(
            App::read_session_file(&state_path).unwrap(),
            Some(StoredSession::new("codex-yolo", "codex-session-1"))
        );
        app.remove_session("ws/1");
        app.spawn_for_task("alias-codex").unwrap();

        let records = records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            &records[1].args[..6],
            ["codex", "-a", "never", "--", "resume", "codex-session-1",]
        );
    }

    #[test]
    fn unconfigured_agent_creates_generic_backend_on_spawn_up_to() {
        let (mut app, _dir) = test_app();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "custom-task",
                description: "test custom agent",
                agent: Some("custom-tool"),
                ..Default::default()
            },
        )
        .unwrap();

        // Before spawn, no backend for "custom-tool".
        assert!(!app.backends.contains_key("custom-tool"));

        app.spawn_up_to(1).unwrap();

        // After spawn, a generic backend should have been created.
        assert!(app.backends.contains_key("custom-tool"));
    }

    #[test]
    fn unconfigured_agent_creates_generic_backend_on_spawn_for_task() {
        let (mut app, _dir) = test_app();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "custom-task",
                description: "test custom agent",
                agent: Some("custom-tool"),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(!app.backends.contains_key("custom-tool"));

        // spawn_for_task may fail (command doesn't exist) but should
        // still create the generic backend before attempting spawn.
        let _ = app.spawn_for_task("custom-task");
        assert!(app.backends.contains_key("custom-tool"));
        assert_eq!(app.backends["custom-tool"].command(), "custom-tool");
    }

    #[test]
    fn spawn_for_task_rejects_non_open_tasks() {
        let (mut app, _dir) = test_app();
        for (name, paused) in [("paused-task", true), ("done-task", false)] {
            ops::add_task(
                &app.conn,
                ops::AddTaskParams {
                    name,
                    description: "test",
                    paused,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        ops::mark_done(&app.conn, "done-task").unwrap();

        let err = app.spawn_for_task("paused-task").unwrap_err();
        assert!(err.to_string().contains("paused"), "{err}");

        let err = app.spawn_for_task("done-task").unwrap_err();
        assert!(err.to_string().contains("done"), "{err}");

        assert_eq!(app.counter, 0);
        assert!(app.task_to_session.is_empty());
    }

    #[test]
    fn tick_requests_exit_when_tasks_are_no_longer_owned() {
        let (mut app, _dir) = test_app();
        for (index, name) in ["paused", "done", "reassigned", "deleted"]
            .into_iter()
            .enumerate()
        {
            let session_id = format!("ws/{}", index + 1);
            ops::add_task(
                &app.conn,
                ops::AddTaskParams {
                    name,
                    description: "test",
                    ..Default::default()
                },
            )
            .unwrap();
            ops::claim_task(&app.conn, name, &session_id).unwrap();
            app.sessions.insert(
                session_id.clone(),
                TrackedSession {
                    handle: Box::new(StubSession::new(name, &session_id, true)),
                    agent_type: "claude".to_string(),
                    unread: false,
                },
            );
            app.task_to_session
                .insert(name.to_string(), session_id.clone());
        }

        ops::pause_task(&app.conn, "paused").unwrap();
        ops::mark_done(&app.conn, "done").unwrap();
        ops::steal_task(&app.conn, "reassigned", "another-agent").unwrap();
        ops::remove_task(&app.conn, "deleted", false).unwrap();

        app.tick().unwrap();

        for session in app.sessions.values() {
            assert!(
                session.handle.stopping_since().is_some(),
                "{} should be stopping",
                session.handle.task_name()
            );
        }
    }

    #[test]
    fn malformed_session_state_after_auto_claim_releases_task() {
        let (mut app, _dir) = test_app();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "test",
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::write(app.claude_sessions_dir.join("task-a"), "{not-json").unwrap();

        let error = app.spawn_up_to(1).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to resolve session state"),
            "{error:#}"
        );
        let task = ops::get_task(&app.conn, "task-a").unwrap();
        assert_eq!(task.status, "open");
        assert!(task.assignee.is_none());
        assert_eq!(app.counter, 0);
        assert!(app.sessions.is_empty());
        assert!(app.task_to_session.is_empty());
    }

    #[test]
    fn spawn_sets_kbtz_agent_type_env_var() {
        let status_dir = TempDir::new().unwrap();
        let claude_sessions_dir = status_dir.path().join("claude-sessions");
        std::fs::create_dir_all(&claude_sessions_dir).unwrap();
        let conn = kbtz::db::open_memory().unwrap();
        let mut backends: HashMap<String, Box<dyn Backend>> = HashMap::new();
        backends.insert("claude".to_string(), Box::new(StubBackend));
        backends.insert("gemini".to_string(), Box::new(StubBackend));
        let (spawner, captured) = CapturingSpawner::new();
        let mut app = App {
            db_path: ":memory:".to_string(),
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.path().to_path_buf(),
            claude_sessions_dir,
            max_concurrency: 2,
            manual: false,
            prefer: None,
            backends,
            default_backend: "claude".to_string(),
            spawner: Box::new(spawner),
            persistent_sessions: false,
            codex_home: Some(status_dir.path().to_path_buf()),
            default_directory: std::env::current_dir().unwrap(),
            toplevel: None,
            term: TermSize { rows: 24, cols: 80 },
            tree: TreeView::new(ActiveTaskPolicy::Confirm),
            tree_dirty: false,
            notes_panel: None,
            zoomed_session: None,
        };

        // Create a task with agent="gemini" and one with no agent.
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "gemini-task",
                description: "uses gemini",
                agent: Some("gemini"),
                ..Default::default()
            },
        )
        .unwrap();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "default-task",
                description: "uses default",
                ..Default::default()
            },
        )
        .unwrap();

        app.spawn_up_to(2).unwrap();

        let gemini_env = env_for_task(&captured, "gemini-task");
        let agent_type = gemini_env
            .iter()
            .find(|(k, _)| k == "KBTZ_AGENT_TYPE")
            .map(|(_, v)| v.as_str());
        assert_eq!(agent_type, Some("gemini"));

        let default_env = env_for_task(&captured, "default-task");
        let agent_type = default_env
            .iter()
            .find(|(k, _)| k == "KBTZ_AGENT_TYPE")
            .map(|(_, v)| v.as_str());
        assert_eq!(agent_type, Some("claude"));
    }

    #[test]
    fn remove_session_cleans_up_tracked_session() {
        let (mut app, _dir) = test_app();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        assert!(!app.sessions.contains_key("ws/1"));
        assert!(!app.task_to_session.contains_key("task-a"));
    }

    #[test]
    fn remove_session_cleans_up_child_pid_file() {
        let (mut app, _dir) = test_app();
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "task-a",
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, "task-a", "ws/1").unwrap();

        // Create the child-pid file that the shepherd would write.
        let filename = session_id_to_filename("ws/1");
        let child_pid_file = app.status_dir.join(format!("{filename}.child-pid"));
        std::fs::write(&child_pid_file, "12345").unwrap();

        app.sessions.insert(
            "ws/1".to_string(),
            TrackedSession {
                handle: Box::new(StubSession::new("task-a", "ws/1", false)),
                agent_type: "claude".to_string(),
                unread: false,
            },
        );
        app.task_to_session
            .insert("task-a".to_string(), "ws/1".to_string());

        app.remove_session("ws/1");

        assert!(
            !child_pid_file.exists(),
            ".child-pid file should be removed"
        );
    }

    #[test]
    fn kill_child_from_pid_file_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("nonexistent.pid");
        // Should not panic when the file doesn't exist.
        kill_child_from_pid_file(&pid_path);
    }

    #[test]
    fn kill_child_from_pid_file_kills_process_group() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");

        // Spawn a sleep in its own process group.
        let mut child = unsafe {
            std::process::Command::new("sleep")
                .arg("999")
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()
                .unwrap()
        };
        let child_pid = child.id();

        std::fs::write(pid_path.with_extension("child-pid"), format!("{child_pid}")).unwrap();

        kill_child_from_pid_file(&pid_path);

        // wait() reaps the zombie and confirms the process exited.
        let exited = child.wait().unwrap().code().is_none(); // killed by signal → no code
        assert!(exited, "child should have been killed by signal");
    }

    #[test]
    fn release_orphaned_tasks_ignores_non_ws_assignees() {
        let (app, _dir) = test_app();
        // Task claimed by an external agent, not a workspace session.
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "external",
                description: "desc",
                claim: Some("agent-1"),
                ..Default::default()
            },
        )
        .unwrap();

        app.release_orphaned_tasks().unwrap();

        let task = ops::get_task(&app.conn, "external").unwrap();
        assert_eq!(task.status, "active");
        assert_eq!(task.assignee.as_deref(), Some("agent-1"));
    }

    // ── Reconnected sessions exceeding concurrency ───────────────────

    /// Simulate the startup flow where persistent sessions are reconnected
    /// and the number of reconnected sessions exceeds max_concurrency.
    /// All reconnected sessions must be preserved; the concurrency limit
    /// only prevents spawning NEW sessions.
    #[test]
    fn reconnected_sessions_exceeding_concurrency_are_preserved() {
        let (mut app, _dir) = test_app();
        // max_concurrency is 2, but we'll simulate 4 reconnected sessions.

        // Create and claim 4 tasks as if they were owned by a previous workspace.
        for i in 1..=4 {
            let name = format!("task-{i}");
            ops::add_task(
                &app.conn,
                ops::AddTaskParams {
                    name: &name,
                    description: "desc",
                    ..Default::default()
                },
            )
            .unwrap();
            let sid = format!("ws/{i}");
            ops::claim_task(&app.conn, &name, &sid).unwrap();

            // Simulate reconnect_sessions() having adopted these sessions.
            app.sessions.insert(
                sid.clone(),
                TrackedSession {
                    handle: Box::new(StubSession::new(&name, &sid, true)),
                    agent_type: "claude".to_string(),
                    unread: false,
                },
            );
            app.task_to_session.insert(name, sid);
        }
        app.counter = 4;

        // release_orphaned_tasks should NOT release any of the 4 tasks.
        app.release_orphaned_tasks().unwrap();
        for i in 1..=4 {
            let task = ops::get_task(&app.conn, &format!("task-{i}")).unwrap();
            assert_eq!(task.status, "active", "task-{i} should remain active");
        }

        // tick() should NOT reap any of the 4 sessions, even though
        // we're over the concurrency limit (4 > 2).
        let world = app.snapshot();
        let actions = lifecycle::tick(&world);
        assert!(
            actions.is_empty(),
            "no sessions should be reaped and no new sessions spawned, got: {actions:?}"
        );
        assert_eq!(app.sessions.len(), 4);
    }

    /// When reconnected sessions exceed concurrency and there are also
    /// open tasks waiting, tick() should not spawn new sessions.
    #[test]
    fn over_capacity_from_reconnect_blocks_new_spawns() {
        let (mut app, _dir) = test_app();
        // max_concurrency is 2.

        // Simulate 3 reconnected sessions (over capacity).
        for i in 1..=3 {
            let name = format!("task-{i}");
            ops::add_task(
                &app.conn,
                ops::AddTaskParams {
                    name: &name,
                    description: "desc",
                    ..Default::default()
                },
            )
            .unwrap();
            let sid = format!("ws/{i}");
            ops::claim_task(&app.conn, &name, &sid).unwrap();

            app.sessions.insert(
                sid.clone(),
                TrackedSession {
                    handle: Box::new(StubSession::new(&name, &sid, true)),
                    agent_type: "claude".to_string(),
                    unread: false,
                },
            );
            app.task_to_session.insert(name, sid);
        }
        app.counter = 3;

        // Add an open task waiting to be claimed.
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: "waiting-task",
                description: "should not be spawned yet",
                ..Default::default()
            },
        )
        .unwrap();

        // tick() should not spawn for the waiting task.
        let world = app.snapshot();
        let actions = lifecycle::tick(&world);
        assert!(
            actions.is_empty(),
            "should not spawn while over capacity, got: {actions:?}"
        );

        // The waiting task should remain open.
        let task = ops::get_task(&app.conn, "waiting-task").unwrap();
        assert_eq!(task.status, "open");
    }

    #[test]
    fn shutdown_does_not_delete_database_in_status_dir() {
        let status_dir = TempDir::new().unwrap();
        let claude_sessions_dir = status_dir.path().join("claude-sessions");
        std::fs::create_dir_all(&claude_sessions_dir).unwrap();

        // Place the database *inside* the status directory.
        let db_path = status_dir.path().join("kbtz.db");
        let conn = kbtz::db::open(db_path.to_str().unwrap()).unwrap();
        kbtz::db::init(&conn).unwrap();
        assert!(db_path.exists(), "database should exist before shutdown");

        // Create WAL and SHM files alongside the database (SQLite would do this).
        let wal_path = status_dir.path().join("kbtz.db-wal");
        std::fs::write(&wal_path, b"fake wal").unwrap();
        let shm_path = status_dir.path().join("kbtz.db-shm");
        std::fs::write(&shm_path, b"fake shm").unwrap();

        // Create session files that SHOULD be cleaned up.
        let status_file = status_dir.path().join("ws-1");
        std::fs::write(&status_file, b"active").unwrap();
        let sock_file = status_dir.path().join("ws-1.sock");
        std::fs::write(&sock_file, b"").unwrap();
        let codex_profile = status_dir.path().join("kbtz-test.config.toml");
        std::fs::write(&codex_profile, b"[[hooks.SessionStart]]").unwrap();
        let codex_marker = status_dir.path().join("ws-1.codex-profile");
        std::fs::write(&codex_marker, codex_profile.to_string_lossy().as_bytes()).unwrap();

        // Create an unrelated file that must NOT be cleaned up.
        let unrelated = status_dir.path().join("something-else.txt");
        std::fs::write(&unrelated, b"important").unwrap();

        let mut backends: HashMap<String, Box<dyn Backend>> = HashMap::new();
        backends.insert("claude".to_string(), Box::new(StubBackend));
        let mut app = App {
            db_path: db_path.to_str().unwrap().to_string(),
            conn,
            sessions: HashMap::new(),
            task_to_session: HashMap::new(),
            counter: 0,
            status_dir: status_dir.path().to_path_buf(),
            claude_sessions_dir,
            max_concurrency: 2,
            manual: false,
            prefer: None,
            backends,
            default_backend: "claude".to_string(),
            spawner: Box::new(StubSpawner),
            persistent_sessions: false,
            codex_home: Some(status_dir.path().to_path_buf()),
            default_directory: std::env::current_dir().unwrap(),
            toplevel: None,
            term: TermSize { rows: 24, cols: 80 },
            tree: TreeView::new(ActiveTaskPolicy::Confirm),
            tree_dirty: false,
            notes_panel: None,
            zoomed_session: None,
        };

        app.shutdown();

        assert!(db_path.exists(), "database must survive shutdown");
        assert!(wal_path.exists(), "WAL file must survive shutdown");
        assert!(shm_path.exists(), "SHM file must survive shutdown");
        assert!(unrelated.exists(), "unrelated files must survive shutdown");
        assert!(
            !status_file.exists(),
            "session status file should be cleaned up"
        );
        assert!(
            !sock_file.exists(),
            "session socket file should be cleaned up"
        );
        assert!(
            !codex_profile.exists(),
            "temporary Codex profile should be cleaned up"
        );
        assert!(
            !codex_marker.exists(),
            "temporary Codex profile marker should be cleaned up"
        );
    }

    #[test]
    fn persistent_shutdown_only_preserves_profiles_for_live_shepherds() {
        let (mut app, _dir) = test_app();
        app.persistent_sessions = true;

        let stale_profile = app.status_dir.join("kbtz-stale.config.toml");
        let stale_marker = app.status_dir.join("ws-1.codex-profile");
        std::fs::write(&stale_profile, b"stale").unwrap();
        std::fs::write(&stale_marker, stale_profile.to_string_lossy().as_bytes()).unwrap();

        let live_profile = app.status_dir.join("kbtz-live.config.toml");
        let live_marker = app.status_dir.join("ws-2.codex-profile");
        let live_socket = app.status_dir.join("ws-2.sock");
        std::fs::write(&live_profile, b"live").unwrap();
        std::fs::write(&live_marker, live_profile.to_string_lossy().as_bytes()).unwrap();
        std::fs::write(&live_socket, b"").unwrap();

        app.shutdown();

        assert!(!stale_profile.exists());
        assert!(!stale_marker.exists());
        assert!(live_profile.exists());
        assert!(live_marker.exists());
        assert!(live_socket.exists());
    }

    /// When reconnect_sessions encounters a dead shepherd, the orchestrator
    /// files should be cleaned up but the claude-sessions/<task> file must
    /// be preserved so the next spawn can resume the conversation.
    #[test]
    fn reconnect_dead_shepherd_preserves_session_file() {
        let (mut app, _dir) = test_app();

        let task_name = "task-a";
        let session_id = "ws/1";
        let uuid = "test-uuid-dead-shepherd";

        // Create task and claim it as the session.
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: task_name,
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, task_name, session_id).unwrap();

        // Write the claude session file (UUID for resume).
        let session_file = app.claude_sessions_dir.join(task_name);
        std::fs::write(&session_file, uuid).unwrap();

        // Create orchestrator files simulating a previously running shepherd.
        let filename = session_id_to_filename(session_id);
        let sock_path = app.status_dir.join(format!("{filename}.sock"));
        let pid_path = app.status_dir.join(format!("{filename}.pid"));
        std::fs::write(&sock_path, "").unwrap();
        // Use an impossible PID so the alive check fails (dead shepherd).
        std::fs::write(&pid_path, "999999999").unwrap();

        app.reconnect_sessions().unwrap();

        // Claude session file must be preserved for future resume.
        assert!(
            session_file.exists(),
            "claude session file must survive reconnect failure (dead shepherd)"
        );
        assert_eq!(
            std::fs::read_to_string(&session_file).unwrap(),
            uuid,
            "session UUID must be intact"
        );

        // Orchestrator files should be cleaned up.
        assert!(!sock_path.exists(), "stale socket should be removed");
        assert!(!pid_path.exists(), "stale pid file should be removed");

        // Task should be released (available for re-spawn).
        let task = ops::get_task(&app.conn, task_name).unwrap();
        assert_eq!(
            task.status, "open",
            "task should be released after reconnect failure"
        );
    }

    /// When reconnect_sessions fails to connect to a shepherd (stale socket),
    /// the claude-sessions/<task> file must be preserved for future resume.
    #[test]
    fn reconnect_stale_socket_preserves_session_file() {
        let (mut app, _dir) = test_app();

        let task_name = "task-a";
        let session_id = "ws/1";
        let uuid = "test-uuid-stale-socket";

        // Create task and claim it.
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: task_name,
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, task_name, session_id).unwrap();

        // Write the claude session file (UUID for resume).
        let session_file = app.claude_sessions_dir.join(task_name);
        std::fs::write(&session_file, uuid).unwrap();

        // Create a .sock as a regular file (not a real socket) so connect fails.
        // Use PID 1 (init) so the alive check passes (EPERM counts as alive)
        // but SIGKILL in the cleanup path is harmless (also EPERM, ignored).
        let filename = session_id_to_filename(session_id);
        let sock_path = app.status_dir.join(format!("{filename}.sock"));
        let pid_path = app.status_dir.join(format!("{filename}.pid"));
        std::fs::write(&sock_path, "").unwrap();
        std::fs::write(&pid_path, "1").unwrap();

        app.reconnect_sessions().unwrap();

        // Claude session file must be preserved for future resume.
        assert!(
            session_file.exists(),
            "claude session file must survive reconnect failure (stale socket)"
        );
        assert_eq!(
            std::fs::read_to_string(&session_file).unwrap(),
            uuid,
            "session UUID must be intact"
        );

        // Orchestrator files should be cleaned up.
        assert!(!sock_path.exists(), "stale socket should be removed");
        assert!(!pid_path.exists(), "stale pid file should be removed");

        // Task should be released.
        let task = ops::get_task(&app.conn, task_name).unwrap();
        assert_eq!(
            task.status, "open",
            "task should be released after reconnect failure"
        );
    }

    #[test]
    fn reconnect_malformed_session_state_cleans_up_and_releases_before_error() {
        let (mut app, _dir) = test_app();
        let task_name = "task-a";
        let session_id = "ws/1";
        ops::add_task(
            &app.conn,
            ops::AddTaskParams {
                name: task_name,
                description: "desc",
                ..Default::default()
            },
        )
        .unwrap();
        ops::claim_task(&app.conn, task_name, session_id).unwrap();
        std::fs::write(app.claude_sessions_dir.join(task_name), "{not-json").unwrap();

        let filename = session_id_to_filename(session_id);
        let sock_path = app.status_dir.join(format!("{filename}.sock"));
        let pid_path = app.status_dir.join(format!("{filename}.pid"));
        std::fs::write(&sock_path, "").unwrap();
        // PID 1 passes the liveness check but cannot be killed by this process.
        std::fs::write(&pid_path, "1").unwrap();

        let error = app.reconnect_sessions().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to resolve session state"),
            "{error:#}"
        );
        assert!(!sock_path.exists());
        assert!(!pid_path.exists());
        let task = ops::get_task(&app.conn, task_name).unwrap();
        assert_eq!(task.status, "open");
        assert!(task.assignee.is_none());
        assert!(app.sessions.is_empty());
        assert!(app.task_to_session.is_empty());
    }
}
