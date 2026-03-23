use std::collections::HashMap;
use std::io;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use log::{error, info, warn};
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use kbtz::config::Config;
use kbtz::model::Task;
use kbtz::ops;
use kbtz_workspace_core::lifecycle::{
    SessionAction, SessionPhase, SessionSnapshot, TaskSnapshot, WorldSnapshot,
};
use kbtz_workspace_core::prompt::AGENT_PROMPT;

use crate::protocol::{self, AgentEvent, Message, ShepherdState};
use crate::ws_messages::SessionStatusKind;

const SESSION_ID_PREFIX: &str = "web/";
const TICK_INTERVAL: Duration = Duration::from_secs(5);
const SPAWN_READINESS_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// A tracked web session connected to a json-shepherd process.
pub struct TrackedSession {
    pub session_id: String,
    pub task_name: String,
    pub agent_type: String,
    pub socket_path: PathBuf,
    pub state_path: PathBuf,
    pub stream: Option<UnixStream>,
    pub shepherd_child: Option<Child>,
    pub stopping_since: Option<Instant>,
}

impl TrackedSession {
    fn phase(&mut self) -> SessionPhase {
        if let Some(ref mut child) = self.shepherd_child {
            match child.try_wait() {
                Ok(Some(_)) => SessionPhase::Exited,
                Ok(None) => {
                    if let Some(since) = self.stopping_since {
                        SessionPhase::Stopping { since }
                    } else {
                        SessionPhase::Running
                    }
                }
                Err(_) => SessionPhase::Exited,
            }
        } else {
            SessionPhase::Exited
        }
    }

    fn force_kill(&mut self) {
        if let Ok(state) = ShepherdState::read_state_file(&self.state_path) {
            if let Some(child_pid) = state.child_pid {
                unsafe { libc::kill(-(child_pid as i32), libc::SIGKILL) };
            }
        }
        if let Some(ref mut child) = self.shepherd_child {
            let _ = child.kill();
        }
    }

    fn request_exit(&mut self) {
        if self.stopping_since.is_some() {
            return;
        }
        self.stopping_since = Some(Instant::now());
        if let Some(ref mut stream) = self.stream {
            let _ = protocol::write_message(stream, &Message::Shutdown);
        }
    }

    fn send_input(&mut self, data: &str) -> Result<()> {
        let stream = self.stream.as_mut().context("no shepherd connection")?;
        protocol::write_message(stream, &Message::Input { data: data.to_string() })
    }

    fn read_events(&mut self) -> Vec<AgentEvent> {
        let stream = match self.stream.as_mut() {
            Some(s) => s,
            None => return vec![],
        };
        stream.set_nonblocking(true).ok();
        let mut events = Vec::new();
        loop {
            match protocol::read_message(stream) {
                Ok(Some(Message::Event { event })) => events.push(event),
                Ok(Some(Message::EventBatch { events: batch })) => events.extend(batch),
                Ok(Some(_)) => {} // ignore input/shutdown echoes
                Ok(None) => {
                    // EOF — shepherd disconnected
                    self.stream = None;
                    break;
                }
                Err(e) => {
                    let is_would_block = e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::WouldBlock);
                    if is_would_block {
                        break;
                    }
                    // Chain through: check if inner is WouldBlock
                    if let Some(source) = e.source() {
                        if let Some(io_err) = source.downcast_ref::<io::Error>() {
                            if io_err.kind() == io::ErrorKind::WouldBlock {
                                break;
                            }
                        }
                    }
                    warn!("error reading from shepherd {}: {e:#}", self.session_id);
                    self.stream = None;
                    break;
                }
            }
        }
        events
    }

    fn cleanup_files(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.state_path);
    }
}

/// Shared state for the session manager.
pub struct SessionManager {
    pub sessions: HashMap<String, TrackedSession>,
    task_to_session: HashMap<String, String>,
    counter: u64,
    db_path: String,
    workspace_dir: PathBuf,
    max_concurrency: usize,
    default_backend: String,
    event_cap: usize,
    default_directory: PathBuf,
    /// Broadcast channel for session events (session_id, event).
    event_tx: broadcast::Sender<SessionEvent>,
}

/// An event broadcast to WebSocket clients.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    AgentEvent {
        session_id: String,
        event: AgentEvent,
    },
    StatusChange {
        session_id: String,
        status: SessionStatusKind,
        task_name: Option<String>,
    },
    TreeChanged,
}

impl SessionManager {
    pub fn new(
        db_path: String,
        workspace_dir: PathBuf,
        max_concurrency: usize,
        default_backend: String,
        event_cap: usize,
        default_directory: PathBuf,
    ) -> (Self, broadcast::Receiver<SessionEvent>) {
        let (event_tx, event_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        (
            Self {
                sessions: HashMap::new(),
                task_to_session: HashMap::new(),
                counter: 0,
                db_path,
                workspace_dir,
                max_concurrency,
                default_backend,
                event_cap,
                default_directory,
                event_tx,
            },
            event_rx,
        )
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.event_tx.subscribe()
    }

    /// Run one lifecycle tick: poll shepherd events, snapshot world, execute actions.
    pub fn tick(&mut self) -> Result<()> {
        // 1. Poll events from all connected shepherds
        let session_ids: Vec<String> = self.sessions.keys().cloned().collect();
        for sid in session_ids {
            let events = self.sessions.get_mut(&sid).unwrap().read_events();
            for event in events {
                let _ = self.event_tx.send(SessionEvent::AgentEvent {
                    session_id: sid.clone(),
                    event,
                });
            }
        }

        // 2. Build world snapshot and run lifecycle tick
        let conn = kbtz::db::open(&self.db_path).context("opening database for tick")?;
        kbtz::db::init(&conn)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;

        let world = self.build_snapshot(&conn);
        let actions = kbtz_workspace_core::lifecycle::tick(&world);
        if actions.is_empty() {
            return Ok(());
        }

        self.execute_actions(&conn, actions)
    }

    fn build_snapshot(&mut self, conn: &rusqlite::Connection) -> WorldSnapshot {
        let sessions = self
            .sessions
            .iter_mut()
            .map(|(session_id, ts)| {
                let phase = ts.phase();
                let task = ops::get_task(conn, &ts.task_name)
                    .ok()
                    .map(|t| {
                        let blocked = !ops::get_blockers(conn, &ts.task_name)
                            .unwrap_or_default()
                            .is_empty();
                        TaskSnapshot {
                            status: t.status,
                            assignee: t.assignee,
                            blocked,
                        }
                    });
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
            now: Instant::now(),
        }
    }

    fn execute_actions(
        &mut self,
        conn: &rusqlite::Connection,
        actions: Vec<SessionAction>,
    ) -> Result<()> {
        for action in actions {
            match action {
                SessionAction::RequestExit { session_id, reason } => {
                    info!("requesting exit for {session_id}: {reason}");
                    if let Some(ts) = self.sessions.get_mut(&session_id) {
                        ts.request_exit();
                    }
                }
                SessionAction::ForceKill { session_id } => {
                    info!("force killing {session_id}");
                    if let Some(ts) = self.sessions.get_mut(&session_id) {
                        ts.force_kill();
                    }
                }
                SessionAction::Remove { session_id } => {
                    info!("removing session {session_id}");
                    self.remove_session(&session_id);
                    let _ = self.event_tx.send(SessionEvent::TreeChanged);
                }
                SessionAction::SpawnUpTo { count } => {
                    self.spawn_up_to(conn, count)?;
                    let _ = self.event_tx.send(SessionEvent::TreeChanged);
                }
            }
        }
        Ok(())
    }

    fn spawn_up_to(&mut self, conn: &rusqlite::Connection, count: usize) -> Result<()> {
        for _ in 0..count {
            self.counter += 1;
            let session_id = format!("{SESSION_ID_PREFIX}{}", self.counter);

            let claim = ops::claim_next_task(conn, &session_id, None, None);
            let task_name = match claim {
                Ok(Some(name)) => name,
                Ok(None) => {
                    self.counter -= 1;
                    break;
                }
                Err(e) => {
                    self.counter -= 1;
                    warn!("failed to claim task: {e:#}");
                    break;
                }
            };

            info!("claimed {task_name} as {session_id}");
            let task = ops::get_task(conn, &task_name)?;
            let agent_type = task
                .agent
                .as_deref()
                .unwrap_or(&self.default_backend)
                .to_string();

            match self.spawn_session(&task, &session_id, &agent_type) {
                Ok(tracked) => {
                    let _ = self.event_tx.send(SessionEvent::StatusChange {
                        session_id: session_id.clone(),
                        status: SessionStatusKind::Running,
                        task_name: Some(task_name.clone()),
                    });
                    self.task_to_session
                        .insert(task_name, session_id.clone());
                    self.sessions.insert(session_id, tracked);
                }
                Err(e) => {
                    error!("failed to spawn session for {task_name}: {e:#}");
                    let _ = ops::release_task(conn, &task_name, &session_id);
                    self.counter -= 1;
                }
            }
        }
        Ok(())
    }

    fn spawn_session(
        &self,
        task: &Task,
        session_id: &str,
        agent_type: &str,
    ) -> Result<TrackedSession> {
        let filename = session_id.replace('/', "-");
        let socket_path = self.workspace_dir.join(format!("{filename}.sock"));
        let state_path = self.workspace_dir.join(format!("{filename}.state"));

        // Find kbtz-json-shepherd binary
        let self_exe =
            std::env::current_exe().context("failed to get current executable path")?;
        let shepherd_bin = self_exe.with_file_name("kbtz-json-shepherd");
        if !shepherd_bin.exists() {
            bail!(
                "kbtz-json-shepherd not found at {}",
                shepherd_bin.display()
            );
        }

        // Create a pipe for spawn readiness
        let (read_fd, write_fd) = {
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                bail!("pipe() failed: {}", io::Error::last_os_error());
            }
            (fds[0], fds[1])
        };

        let initial_prompt = format!("Work on task '{}': {}", task.name, task.description);
        let config = Config::load().unwrap_or_default();
        let agent_config = config.agent.get(agent_type);

        let (command, prefix_args) = match agent_config {
            Some(ac) => (
                ac.binary().unwrap_or(agent_type).to_string(),
                ac.prefix_args().to_vec(),
            ),
            None => (agent_type.to_string(), vec![]),
        };

        let mut cmd = Command::new(&shepherd_bin);
        cmd.arg(&socket_path)
            .arg(&state_path)
            .arg(session_id)
            .arg(self.event_cap.to_string())
            .arg("--ready-fd")
            .arg(write_fd.to_string())
            .arg(&command);

        for arg in &prefix_args {
            cmd.arg(arg);
        }

        // Build agent args based on backend type
        let system_arg = format!("--append-system-prompt={AGENT_PROMPT}");
        let agent_args = vec![
            "--print".to_string(),
            "--output-format=stream-json".to_string(),
            system_arg,
            "-p".to_string(),
            initial_prompt.clone(),
        ];
        for arg in &agent_args {
            cmd.arg(arg);
        }

        // Session directory
        let session_dir = task
            .directory
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_directory.clone());
        cmd.current_dir(&session_dir);

        // Environment
        let workspace_dir_str = self.workspace_dir.to_string_lossy().to_string();
        cmd.env("KBTZ_DB", &self.db_path)
            .env("KBTZ_SESSION_ID", session_id)
            .env("KBTZ_TASK", &task.name)
            .env("KBTZ_WORKSPACE_DIR", &workspace_dir_str)
            .env("KBTZ_AGENT_TYPE", agent_type);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Pass the write end of the pipe to the child.
        // The child inherits it; we close it in the parent after spawn.
        unsafe {
            let w = write_fd;
            cmd.pre_exec(move || {
                // Ensure the write FD is not close-on-exec
                let flags = libc::fcntl(w, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(w, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                }
                Ok(())
            });
        }

        let child = cmd.spawn().with_context(|| {
            format!("failed to spawn kbtz-json-shepherd at {}", shepherd_bin.display())
        })?;

        // Close write end in parent
        unsafe { libc::close(write_fd) };

        // Wait for readiness (shepherd closes write end or writes a byte)
        let mut ready_buf = [0u8; 1];
        let read_file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut read_file = read_file;
        let deadline = Instant::now() + SPAWN_READINESS_TIMEOUT;
        loop {
            match std::io::Read::read(&mut read_file, &mut ready_buf) {
                Ok(_) => break, // ready signal or EOF
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    if Instant::now() >= deadline {
                        bail!("shepherd readiness timed out");
                    }
                    continue;
                }
                Err(e) => bail!("reading ready pipe: {e}"),
            }
        }
        drop(read_file);

        info!("shepherd ready for session {session_id} (task={})", task.name);

        // Connect to the shepherd Unix socket
        let stream = UnixStream::connect(&socket_path)
            .with_context(|| format!("connecting to shepherd at {}", socket_path.display()))?;

        // Read initial event batch
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;

        // Set back to blocking for the initial connect, then switch to nonblocking for polling
        let mut stream = stream;
        if let Ok(Some(Message::EventBatch { events })) = protocol::read_message(&mut stream) {
            for event in events {
                let _ = self.event_tx.send(SessionEvent::AgentEvent {
                    session_id: session_id.to_string(),
                    event,
                });
            }
        }

        Ok(TrackedSession {
            session_id: session_id.to_string(),
            task_name: task.name.clone(),
            agent_type: agent_type.to_string(),
            socket_path,
            state_path,
            stream: Some(stream),
            shepherd_child: Some(child),
            stopping_since: None,
        })
    }

    fn remove_session(&mut self, session_id: &str) {
        if let Some(ts) = self.sessions.remove(session_id) {
            self.task_to_session.remove(&ts.task_name);
            ts.cleanup_files();
            let _ = self.event_tx.send(SessionEvent::StatusChange {
                session_id: session_id.to_string(),
                status: SessionStatusKind::Exited,
                task_name: Some(ts.task_name),
            });
        }
    }

    /// Reconnect to any running shepherd processes found in workspace_dir.
    pub fn reconnect_existing(&mut self) -> Result<()> {
        let entries = match std::fs::read_dir(&self.workspace_dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };

        let conn = kbtz::db::open(&self.db_path)?;
        kbtz::db::init(&conn)?;

        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !name.ends_with(".state") {
                continue;
            }
            let stem = name.trim_end_matches(".state");
            if !stem.starts_with("web-") {
                continue;
            }

            let state = match ShepherdState::read_state_file(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let socket_path = self.workspace_dir.join(format!("{stem}.sock"));
            if !socket_path.exists() {
                continue;
            }

            // Check if shepherd process is alive
            let alive = unsafe { libc::kill(state.shepherd_pid as i32, 0) == 0 };
            if !alive {
                // Stale files — clean up
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(&socket_path);
                continue;
            }

            let session_id = &state.session_id;
            let tasks = ops::list_tasks(&conn, None, true, None, Some(session_id), None)?;
            let task = match tasks.into_iter().next() {
                Some(t) => t,
                None => continue,
            };

            info!(
                "reconnecting to existing shepherd: session={session_id} task={}",
                task.name
            );

            match UnixStream::connect(&socket_path) {
                Ok(stream) => {
                    // Parse counter from session ID (e.g., "web/3" -> 3)
                    if let Some(num_str) = session_id.strip_prefix(SESSION_ID_PREFIX) {
                        if let Ok(num) = num_str.parse::<u64>() {
                            if num >= self.counter {
                                self.counter = num;
                            }
                        }
                    }

                    let tracked = TrackedSession {
                        session_id: session_id.clone(),
                        task_name: task.name.clone(),
                        agent_type: task.agent.unwrap_or_else(|| self.default_backend.clone()),
                        socket_path: socket_path.clone(),
                        state_path: path.clone(),
                        stream: Some(stream),
                        shepherd_child: None, // we didn't spawn it
                        stopping_since: None,
                    };
                    self.task_to_session
                        .insert(task.name.clone(), session_id.clone());
                    self.sessions.insert(session_id.clone(), tracked);
                }
                Err(e) => {
                    warn!("failed to reconnect to {session_id}: {e}");
                }
            }
        }
        Ok(())
    }

    pub fn send_input(&mut self, session_id: &str, data: &str) -> Result<()> {
        let ts = self
            .sessions
            .get_mut(session_id)
            .context("session not found")?;
        ts.send_input(data)
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Get session history by connecting to the shepherd and reading the initial batch.
    pub fn get_session_history(&self, session_id: &str) -> Result<Vec<AgentEvent>> {
        let ts = self
            .sessions
            .get(session_id)
            .context("session not found")?;

        // Connect a fresh stream to get the history replay
        let mut stream = UnixStream::connect(&ts.socket_path)
            .context("connecting to shepherd for history")?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;

        match protocol::read_message(&mut stream)? {
            Some(Message::EventBatch { events }) => Ok(events),
            _ => Ok(vec![]),
        }
    }
}

/// Run the session manager lifecycle loop in a background task.
pub async fn run_lifecycle_loop(manager: Arc<Mutex<SessionManager>>) {
    let mut interval = tokio::time::interval(TICK_INTERVAL);
    loop {
        interval.tick().await;
        let mut mgr = manager.lock().await;
        if let Err(e) = mgr.tick() {
            error!("lifecycle tick error: {e:#}");
        }
    }
}
