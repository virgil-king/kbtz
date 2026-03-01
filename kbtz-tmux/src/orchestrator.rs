use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{error, info, warn};
use rusqlite::Connection;

use kbtz::{db, ops, watch};
use kbtz_workspace::config::Config;
use kbtz_workspace::prompt::AGENT_PROMPT;

use crate::lifecycle::{self, Action, TaskSnapshot, WindowPhase, WindowSnapshot, WorldSnapshot};
use crate::tmux;

struct TrackedWindow {
    window_id: String,
    task_name: String,
    session_id: String,
    phase: WindowPhase,
}

pub struct Orchestrator {
    session: String,
    max_concurrent: usize,
    poll_interval: Duration,
    prefer: Option<String>,
    db_path: String,
    workspace_dir: String,
    conn: Connection,
    windows: HashMap<String, TrackedWindow>,
    running: Arc<AtomicBool>,
    config: Config,
}

impl Orchestrator {
    pub fn new(
        session: String,
        max_concurrent: usize,
        poll_interval: Duration,
        prefer: Option<String>,
        running: Arc<AtomicBool>,
    ) -> Result<Self> {
        let db_path = std::env::var("KBTZ_DB").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.kbtz/kbtz.db")
        });
        let workspace_dir = std::env::var("KBTZ_WORKSPACE_DIR").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.kbtz/workspace")
        });
        let conn = db::open(&db_path)?;
        db::init(&conn)?;
        let config = Config::load()?;

        Ok(Self {
            session,
            max_concurrent,
            poll_interval,
            prefer,
            db_path,
            workspace_dir,
            conn,
            windows: HashMap::new(),
            running,
            config,
        })
    }

    fn next_free_slot(&self) -> usize {
        let mut slot = 0;
        loop {
            let sid = format!("ws/{slot}");
            if !self.windows.contains_key(&sid) {
                return slot;
            }
            slot += 1;
        }
    }

    /// Batch-fetch task statuses for all tracked windows in a single SQL query.
    fn batch_task_statuses(&self) -> HashMap<String, TaskSnapshot> {
        if self.windows.is_empty() {
            return HashMap::new();
        }
        let names: Vec<&str> = self
            .windows
            .values()
            .map(|tw| tw.task_name.as_str())
            .collect();
        let placeholders: String = names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT name, status, assignee FROM tasks WHERE name IN ({placeholders})"
        );
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to prepare batch query: {e}");
                return HashMap::new();
            }
        };
        let params: Vec<&dyn rusqlite::types::ToSql> = names
            .iter()
            .map(|n| n as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = match stmt.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to execute batch query: {e}");
                return HashMap::new();
            }
        };
        let mut map = HashMap::new();
        for row in rows {
            if let Ok((name, status, assignee)) = row {
                map.insert(name, TaskSnapshot { status, assignee });
            }
        }
        map
    }

    fn snapshot_world(&self) -> WorldSnapshot {
        let task_statuses = self.batch_task_statuses();
        let windows: Vec<WindowSnapshot> = self
            .windows
            .values()
            .map(|tw| {
                let task = task_statuses.get(&tw.task_name).cloned();
                WindowSnapshot {
                    session_id: tw.session_id.clone(),
                    task_name: tw.task_name.clone(),
                    window_id: tw.window_id.clone(),
                    phase: tw.phase.clone(),
                    task,
                }
            })
            .collect();

        WorldSnapshot {
            windows,
            max_concurrency: self.max_concurrent,
            now: Instant::now(),
        }
    }

    fn apply_action(&mut self, action: &Action) {
        match action {
            Action::RequestExit { session_id } => {
                if let Some(tw) = self.windows.get_mut(session_id) {
                    info!(
                        "Requesting exit for {} (task={})",
                        session_id, tw.task_name
                    );
                    if let Ok(Some(pid)) = tmux::pane_pid(&tw.window_id) {
                        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                    }
                    tw.phase = WindowPhase::Stopping {
                        since: Instant::now(),
                    };
                }
            }
            Action::ForceKill { session_id } => {
                if let Some(tw) = self.windows.get(session_id) {
                    info!("Force-killing {} (task={})", session_id, tw.task_name);
                    if let Ok(Some(pid)) = tmux::pane_pid(&tw.window_id) {
                        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                    }
                }
            }
            Action::Remove { session_id } => {
                if let Some(tw) = self.windows.remove(session_id) {
                    info!("Removing {} (task={})", session_id, tw.task_name);
                    let _ = tmux::kill_window(&tw.window_id);
                    if let Err(e) =
                        ops::release_task(&self.conn, &tw.task_name, &tw.session_id)
                    {
                        warn!("Failed to release {}: {e}", tw.task_name);
                    }
                }
            }
            Action::SpawnUpTo { count } => {
                for _ in 0..*count {
                    if let Err(e) = self.spawn_one() {
                        info!("Spawn stopped: {e}");
                        break;
                    }
                }
            }
        }
    }

    fn spawn_one(&mut self) -> Result<()> {
        let slot = self.next_free_slot();
        let session_id = format!("ws/{slot}");

        let task_name =
            ops::claim_next_task(&self.conn, &session_id, self.prefer.as_deref())?
                .context("no tasks available")?;

        let task = ops::get_task(&self.conn, &task_name)?;

        info!("Spawning {task_name} (slot {slot})");

        let backend_name = self
            .config
            .workspace
            .backend
            .as_deref()
            .unwrap_or("claude");
        let agent_cfg = self.config.agent.get(backend_name);
        let command = agent_cfg
            .and_then(|a| a.binary())
            .unwrap_or("claude")
            .to_string();

        let task_prompt = format!("Work on task '{}': {}", task_name, task.description);

        let mut args: Vec<String> = Vec::new();
        if let Some(cfg) = agent_cfg {
            args.extend(cfg.prefix_args().iter().map(|s| s.to_string()));
        }
        args.extend([
            "--append-system-prompt".into(),
            AGENT_PROMPT.into(),
            task_prompt,
        ]);
        if let Some(cfg) = agent_cfg {
            args.extend(cfg.args.iter().cloned());
        }

        let mut env = HashMap::new();
        env.insert("KBTZ_DB".into(), self.db_path.clone());
        env.insert("KBTZ_TASK".into(), task_name.clone());
        env.insert("KBTZ_SESSION_ID".into(), session_id.clone());
        env.insert("KBTZ_WORKSPACE_DIR".into(), self.workspace_dir.clone());

        let window_id =
            match tmux::spawn_window(&self.session, &task_name, &env, &command, &args) {
                Ok(wid) => wid,
                Err(e) => {
                    error!("Failed to spawn window for {task_name}: {e}");
                    let _ = ops::release_task(&self.conn, &task_name, &session_id);
                    return Err(e);
                }
            };

        let _ = tmux::set_window_option(&window_id, "@kbtz_task", &task_name);
        let _ = tmux::set_window_option(&window_id, "@kbtz_sid", &session_id);

        self.windows.insert(
            session_id.clone(),
            TrackedWindow {
                window_id,
                task_name,
                session_id,
                phase: WindowPhase::Running,
            },
        );

        Ok(())
    }

    fn detect_dead_windows(&mut self) {
        for tw in self.windows.values_mut() {
            if matches!(tw.phase, WindowPhase::Running) {
                if let Ok(alive) = tmux::window_alive(&self.session, &tw.window_id) {
                    if !alive {
                        info!(
                            "Window gone for {} (task={})",
                            tw.session_id, tw.task_name
                        );
                        tw.phase = WindowPhase::Gone;
                    }
                }
            }
        }
    }

    pub fn reconcile(&mut self) -> Result<()> {
        info!("Reconciling state...");
        let window_ids = tmux::list_window_ids(&self.session)?;

        for wid in window_ids {
            let task = match tmux::get_window_option(&wid, "@kbtz_task")? {
                Some(t) => t,
                None => continue,
            };
            let sid = match tmux::get_window_option(&wid, "@kbtz_sid")? {
                Some(s) => s,
                None => continue,
            };

            match ops::get_task(&self.conn, &task) {
                Ok(t) if t.status == "active" && t.assignee.as_deref() == Some(&sid) => {
                    info!("Adopting orphaned window: {task} ({wid}, {sid})");
                    self.windows.insert(
                        sid.clone(),
                        TrackedWindow {
                            window_id: wid,
                            task_name: task,
                            session_id: sid,
                            phase: WindowPhase::Running,
                        },
                    );
                }
                _ => {
                    info!("Releasing orphaned claim: {task} ({sid})");
                    let _ = ops::release_task(&self.conn, &task, &sid);
                    let _ = tmux::kill_window(&wid);
                }
            }
        }

        info!("Reconciliation done ({} adopted)", self.windows.len());
        Ok(())
    }

    pub fn run(&mut self) -> Result<()> {
        self.reconcile()?;

        // Install tmux hook for event-driven dead-window detection.
        let sentinel_path = format!("{}/pane-exited", self.workspace_dir);
        tmux::install_pane_hook(&self.session, &sentinel_path)?;

        // Set up watchers for both DB changes and pane exit events.
        let (unified_tx, unified_rx) = std::sync::mpsc::channel();

        let (_db_watcher, db_rx) = watch::watch_db(&self.db_path)?;
        let tx1 = unified_tx.clone();
        std::thread::spawn(move || {
            for _ in db_rx {
                let _ = tx1.send(());
            }
        });

        let workspace_path = std::path::Path::new(&self.workspace_dir);
        let (_dir_watcher, dir_rx) = watch::watch_dir(workspace_path)?;
        let tx2 = unified_tx;
        std::thread::spawn(move || {
            for _ in dir_rx {
                let _ = tx2.send(());
            }
        });

        while self.running.load(Ordering::SeqCst) {
            self.detect_dead_windows();

            let world = self.snapshot_world();
            let actions = lifecycle::tick(&world);

            for action in &actions {
                self.apply_action(action);
            }

            watch::drain_events(&unified_rx);
            watch::wait_for_change(&unified_rx, self.poll_interval);
        }

        let _ = tmux::remove_pane_hook(&self.session);

        Ok(())
    }

    pub fn shutdown(&mut self) {
        info!("Shutting down...");

        let sids: Vec<String> = self.windows.keys().cloned().collect();
        for sid in &sids {
            if let Some(tw) = self.windows.get(sid) {
                if let Ok(Some(pid)) = tmux::pane_pid(&tw.window_id) {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                }
                let _ = ops::release_task(&self.conn, &tw.task_name, &tw.session_id);
            }
        }

        std::thread::sleep(lifecycle::GRACEFUL_TIMEOUT);

        for sid in &sids {
            if let Some(tw) = self.windows.remove(sid) {
                if let Ok(Some(pid)) = tmux::pane_pid(&tw.window_id) {
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                }
                let _ = tmux::kill_window(&tw.window_id);
            }
        }

        info!("Shutdown complete");
    }
}
