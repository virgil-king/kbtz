use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{error, info, warn};
use rusqlite::Connection;

use kbtz::config::Config;
use kbtz::{db, ops, paths, watch};
use kbtz_workspace::prompt::AGENT_PROMPT;

use kbtz_tmux::lifecycle::{self, Action, WindowPhase, WindowSnapshot, WorldSnapshot};
use kbtz_tmux::tmux;

/// Send a signal to a process, logging unexpected errors.
/// ESRCH (process already exited) is silently ignored.
fn send_signal(pid: u32, signal: libc::c_int) {
    let pid_i32 = match i32::try_from(pid) {
        Ok(p) => p,
        Err(_) => {
            warn!("PID {pid} out of i32 range, skipping signal");
            return;
        }
    };
    let rc = unsafe { libc::kill(pid_i32, signal) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH = process already exited, expected during shutdown races.
        if err.raw_os_error() != Some(libc::ESRCH) {
            warn!("kill({pid}, {signal}) failed: {err}");
        }
    }
}

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
        let db_path = paths::db_path();
        let workspace_dir = paths::workspace_dir();
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
            let sid = format!("{}{slot}", paths::SESSION_ID_PREFIX);
            if !self.windows.contains_key(&sid) {
                return slot;
            }
            slot += 1;
        }
    }

    /// Build a world snapshot for the pure tick function.
    fn snapshot_world(&self) -> WorldSnapshot {
        let windows: Vec<WindowSnapshot> = self
            .windows
            .values()
            .map(|tw| WindowSnapshot {
                session_id: tw.session_id.clone(),
                task_name: tw.task_name.clone(),
                window_id: tw.window_id.clone(),
                phase: tw.phase.clone(),
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
            Action::ForceKill { session_id } => {
                if let Some(tw) = self.windows.get(session_id) {
                    info!("Force-killing {} (task={})", session_id, tw.task_name);
                    if let Ok(Some(pid)) = tmux::pane_pid(&tw.window_id) {
                        send_signal(pid, libc::SIGKILL);
                    }
                }
            }
            Action::Remove { session_id } => {
                if let Some(tw) = self.windows.remove(session_id) {
                    info!("Removing {} (task={})", session_id, tw.task_name);
                    let _ = tmux::kill_window(&tw.window_id);
                    if let Err(e) = ops::release_task(&self.conn, &tw.task_name, &tw.session_id) {
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
        let session_id = format!("{}{slot}", paths::SESSION_ID_PREFIX);

        let task_name =
            ops::claim_next_task(&self.conn, &session_id, self.prefer.as_deref(), None)?
                .context("no tasks available")?;

        let task = ops::get_task(&self.conn, &task_name)?;

        info!("Spawning {task_name} (slot {slot})");

        let backend_name = self.config.workspace.backend.as_deref().unwrap_or("claude");
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

        let window_title = format!("🚀 {task_name}");
        let window_id =
            match tmux::spawn_window(&self.session, &window_title, &env, &command, &args) {
                Ok(wid) => wid,
                Err(e) => {
                    error!("Failed to spawn window for {task_name}: {e}");
                    let _ = ops::release_task(&self.conn, &task_name, &session_id);
                    return Err(e);
                }
            };

        // Tag window for crash recovery. If tagging fails, the window is
        // invisible to reconcile — kill it and release the claim.
        if let Err(e) = tmux::set_window_option(&window_id, "@kbtz_task", &task_name)
            .and_then(|()| tmux::set_window_option(&window_id, "@kbtz_sid", &session_id))
        {
            error!("Failed to tag window {window_id} for {task_name}: {e}");
            let _ = tmux::kill_window(&window_id);
            let _ = ops::release_task(&self.conn, &task_name, &session_id);
            return Err(e);
        }

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

    /// Check which tracked windows are still alive in tmux.
    /// Calls list_window_ids once and does set lookups instead of O(N) tmux calls.
    fn detect_dead_windows(&mut self) {
        let alive: HashSet<String> = tmux::list_window_ids(&self.session)
            .unwrap_or_default()
            .into_iter()
            .collect();

        for tw in self.windows.values_mut() {
            if matches!(tw.phase, WindowPhase::Running) && !alive.contains(&tw.window_id) {
                info!("Window gone for {} (task={})", tw.session_id, tw.task_name);
                tw.phase = WindowPhase::Gone;
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

        self.cleanup_stale_status_files();

        info!("Reconciliation done ({} adopted)", self.windows.len());
        Ok(())
    }

    /// Delete status files that don't correspond to any tracked window.
    fn cleanup_stale_status_files(&self) {
        let live_sids: HashSet<&str> = self.windows.keys().map(|s| s.as_str()).collect();

        let entries = match std::fs::read_dir(&self.workspace_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to read workspace dir for cleanup: {e}");
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            // Skip non-status files (locks, sentinels, sockets, pid files).
            if name.contains('.') || name == "pane-exited" || name == "orchestrator" {
                continue;
            }
            let sid = paths::filename_to_session_id(name);
            if !live_sids.contains(sid.as_str()) {
                info!("Removing stale status file: {name} (sid={sid})");
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!("Failed to remove stale status file {name}: {e}");
                }
            }
        }
    }

    /// Run the main loop. Fully event-driven:
    /// - DB changes wake instantly (via watch_db inotify)
    /// - Pane exits wake instantly (via tmux hook -> sentinel file -> watch_dir inotify)
    /// - Signal handler wakes instantly (via wake_tx)
    /// - Fallback poll interval catches edge cases where hooks fail
    pub fn run(
        &mut self,
        wake_tx: std::sync::mpsc::Sender<()>,
        wake_rx: std::sync::mpsc::Receiver<()>,
    ) -> Result<()> {
        self.reconcile()?;

        // Install tmux hook for event-driven dead-window detection.
        let sentinel_path = format!("{}/pane-exited", self.workspace_dir);
        tmux::install_pane_hook(&self.session, &sentinel_path)?;

        // Set up watchers, merging all event sources into the wake channel.
        let (_db_watcher, db_rx) = watch::watch_db(&self.db_path)?;
        let tx1 = wake_tx.clone();
        std::thread::spawn(move || {
            for _ in db_rx {
                let _ = tx1.send(());
            }
        });

        let workspace_path = std::path::Path::new(&self.workspace_dir);
        let (_dir_watcher, dir_rx) = watch::watch_dir(workspace_path)?;
        let tx2 = wake_tx;
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

            // If session vanished, stop the loop.
            if !tmux::has_session(&self.session) {
                info!("Tmux session '{}' gone, exiting", self.session);
                break;
            }

            watch::drain_events(&wake_rx);
            watch::wait_for_change(&wake_rx, self.poll_interval);
        }

        let _ = tmux::remove_pane_hook(&self.session);

        Ok(())
    }

    pub fn shutdown(&mut self) {
        info!("Shutting down...");

        for tw in self.windows.values() {
            let _ = ops::release_task(&self.conn, &tw.task_name, &tw.session_id);
        }
        self.windows.clear();

        info!("Shutdown complete");
    }
}
