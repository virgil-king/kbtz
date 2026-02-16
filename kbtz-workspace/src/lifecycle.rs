use std::time::{Duration, Instant};

pub const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);

// ── Snapshot types (pure data, no IO) ──────────────────────────────────

#[derive(Debug, Clone)]
pub enum SessionPhase {
    Running,
    Stopping { since: Instant },
    Exited,
}

#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub status: String,
    pub assignee: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub phase: SessionPhase,
    pub task: Option<TaskSnapshot>,
}

pub struct WorldSnapshot {
    pub sessions: Vec<SessionSnapshot>,
    /// Max sessions that tick() will auto-spawn. Set to 0 in manual mode
    /// to disable auto-spawning while preserving all reaping/cleanup logic.
    pub max_concurrency: usize,
    pub now: Instant,
}

// ── Actions (what to do, not how) ──────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum SessionAction {
    RequestExit { session_id: String },
    ForceKill { session_id: String },
    Remove { session_id: String },
    SpawnUpTo { count: usize },
}

// ── Pure decision function ─────────────────────────────────────────────

pub fn tick(world: &WorldSnapshot) -> Vec<SessionAction> {
    let mut actions = Vec::new();
    let mut running_count: usize = 0;

    for session in &world.sessions {
        match &session.phase {
            SessionPhase::Exited => {
                actions.push(SessionAction::Remove {
                    session_id: session.session_id.clone(),
                });
            }
            SessionPhase::Stopping { since } => {
                if world.now.duration_since(*since) >= GRACEFUL_TIMEOUT {
                    actions.push(SessionAction::ForceKill {
                        session_id: session.session_id.clone(),
                    });
                    actions.push(SessionAction::Remove {
                        session_id: session.session_id.clone(),
                    });
                }
                // Stopping sessions do NOT count toward concurrency.
            }
            SessionPhase::Running => {
                if should_reap_session(session) {
                    actions.push(SessionAction::RequestExit {
                        session_id: session.session_id.clone(),
                    });
                    // Will transition to Stopping; don't count toward concurrency.
                } else {
                    running_count += 1;
                }
            }
        }
    }

    if running_count < world.max_concurrency {
        let free = world.max_concurrency - running_count;
        actions.push(SessionAction::SpawnUpTo { count: free });
    }

    actions
}

/// Should this running session be reaped based on its task state?
fn should_reap_session(session: &SessionSnapshot) -> bool {
    match &session.task {
        None => true, // task was deleted
        Some(task) => should_reap_task(&session.session_id, task),
    }
}

fn should_reap_task(session_id: &str, task: &TaskSnapshot) -> bool {
    match task.status.as_str() {
        "done" | "paused" => true,
        "active" => task.assignee.as_deref() != Some(session_id),
        "open" => true, // agent released it
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        session_id: &str,
        phase: SessionPhase,
        task: Option<TaskSnapshot>,
    ) -> SessionSnapshot {
        SessionSnapshot {
            session_id: session_id.into(),
            phase,
            task,
        }
    }

    fn active_task(assignee: &str) -> Option<TaskSnapshot> {
        Some(TaskSnapshot {
            status: "active".into(),
            assignee: Some(assignee.into()),
        })
    }

    fn task_with_status(status: &str) -> Option<TaskSnapshot> {
        Some(TaskSnapshot {
            status: status.into(),
            assignee: Some("ws/1".into()),
        })
    }

    fn world(sessions: Vec<SessionSnapshot>, max_concurrency: usize) -> WorldSnapshot {
        WorldSnapshot {
            sessions,
            max_concurrency,
            now: Instant::now(),
        }
    }

    // 1. Exited session -> Remove + SpawnUpTo
    #[test]
    fn exited_session_removed_and_slot_filled() {
        let w = world(
            vec![snapshot("ws/1", SessionPhase::Exited, active_task("ws/1"))],
            2,
        );
        let actions = tick(&w);
        assert_eq!(
            actions,
            vec![
                SessionAction::Remove {
                    session_id: "ws/1".into()
                },
                SessionAction::SpawnUpTo { count: 2 },
            ]
        );
    }

    // 2. Done task -> RequestExit
    #[test]
    fn done_task_triggers_request_exit() {
        let w = world(
            vec![snapshot(
                "ws/1",
                SessionPhase::Running,
                task_with_status("done"),
            )],
            2,
        );
        let actions = tick(&w);
        assert!(actions.contains(&SessionAction::RequestExit {
            session_id: "ws/1".into()
        }));
    }

    // 3. Healthy session -> no action (only SpawnUpTo for free slots)
    #[test]
    fn healthy_session_no_action() {
        let w = world(
            vec![snapshot("ws/1", SessionPhase::Running, active_task("ws/1"))],
            2,
        );
        let actions = tick(&w);
        assert_eq!(actions, vec![SessionAction::SpawnUpTo { count: 1 },]);
    }

    // 4. Stopping within timeout -> no ForceKill
    #[test]
    fn stopping_within_timeout_no_force_kill() {
        let w = world(
            vec![snapshot(
                "ws/1",
                SessionPhase::Stopping {
                    since: Instant::now(),
                },
                active_task("ws/1"),
            )],
            2,
        );
        let actions = tick(&w);
        // Should only get SpawnUpTo (stopping doesn't count toward concurrency)
        assert_eq!(actions, vec![SessionAction::SpawnUpTo { count: 2 },]);
    }

    // 5. Stopping past timeout -> ForceKill + Remove
    #[test]
    fn stopping_past_timeout_force_killed() {
        let past = Instant::now() - Duration::from_secs(10);
        let w = world(
            vec![snapshot(
                "ws/1",
                SessionPhase::Stopping { since: past },
                active_task("ws/1"),
            )],
            2,
        );
        let actions = tick(&w);
        assert!(actions.contains(&SessionAction::ForceKill {
            session_id: "ws/1".into()
        }));
        assert!(actions.contains(&SessionAction::Remove {
            session_id: "ws/1".into()
        }));
    }

    // 6. Stopping sessions don't count toward concurrency
    #[test]
    fn stopping_sessions_dont_count_toward_concurrency() {
        let w = world(
            vec![
                snapshot("ws/1", SessionPhase::Running, active_task("ws/1")),
                snapshot(
                    "ws/2",
                    SessionPhase::Stopping {
                        since: Instant::now(),
                    },
                    active_task("ws/2"),
                ),
            ],
            2,
        );
        let actions = tick(&w);
        // ws/1 is running (counts as 1), ws/2 is stopping (doesn't count).
        // So 1 free slot.
        assert!(actions.contains(&SessionAction::SpawnUpTo { count: 1 }));
    }

    // 7. Spawn after force-kill fills the slot
    #[test]
    fn spawn_after_force_kill() {
        let past = Instant::now() - Duration::from_secs(10);
        let w = world(
            vec![snapshot(
                "ws/1",
                SessionPhase::Stopping { since: past },
                active_task("ws/1"),
            )],
            1,
        );
        let actions = tick(&w);
        assert!(actions.contains(&SessionAction::ForceKill {
            session_id: "ws/1".into()
        }));
        assert!(actions.contains(&SessionAction::SpawnUpTo { count: 1 }));
    }

    // 8. At capacity -> no SpawnUpTo
    #[test]
    fn at_capacity_no_spawn() {
        let w = world(
            vec![
                snapshot("ws/1", SessionPhase::Running, active_task("ws/1")),
                snapshot("ws/2", SessionPhase::Running, active_task("ws/2")),
            ],
            2,
        );
        let actions = tick(&w);
        assert!(actions.is_empty());
    }

    // 9. Deleted task -> RequestExit
    #[test]
    fn deleted_task_triggers_request_exit() {
        let w = world(vec![snapshot("ws/1", SessionPhase::Running, None)], 2);
        let actions = tick(&w);
        assert!(actions.contains(&SessionAction::RequestExit {
            session_id: "ws/1".into()
        }));
    }

    // 10. Reassigned task -> RequestExit
    #[test]
    fn reassigned_task_triggers_request_exit() {
        let w = world(
            vec![snapshot("ws/1", SessionPhase::Running, active_task("ws/2"))],
            2,
        );
        let actions = tick(&w);
        assert!(actions.contains(&SessionAction::RequestExit {
            session_id: "ws/1".into()
        }));
    }

    // 11. Manual mode (max_concurrency=0) -> no SpawnUpTo
    #[test]
    fn manual_mode_no_auto_spawn() {
        let w = world(vec![], 0);
        let actions = tick(&w);
        assert!(actions.is_empty());
    }

    // 12. Manual mode still reaps exited sessions
    #[test]
    fn manual_mode_still_reaps_exited() {
        let w = world(
            vec![snapshot("ws/1", SessionPhase::Exited, active_task("ws/1"))],
            0,
        );
        let actions = tick(&w);
        assert_eq!(
            actions,
            vec![SessionAction::Remove {
                session_id: "ws/1".into()
            },]
        );
    }

    // 13. Manual mode still requests exit for done tasks
    #[test]
    fn manual_mode_still_reaps_done() {
        let w = world(
            vec![snapshot(
                "ws/1",
                SessionPhase::Running,
                task_with_status("done"),
            )],
            0,
        );
        let actions = tick(&w);
        assert!(actions.contains(&SessionAction::RequestExit {
            session_id: "ws/1".into()
        }));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SessionAction::SpawnUpTo { .. })));
    }
}
