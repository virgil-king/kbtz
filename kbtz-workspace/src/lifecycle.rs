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
pub struct SessionSnapshot {
    pub session_id: String,
    pub phase: SessionPhase,
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
                // Never auto-kill running sessions. The user decides
                // when to close them.
                running_count += 1;
            }
        }
    }

    if running_count < world.max_concurrency {
        let free = world.max_concurrency - running_count;
        actions.push(SessionAction::SpawnUpTo { count: free });
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(session_id: &str, phase: SessionPhase) -> SessionSnapshot {
        SessionSnapshot {
            session_id: session_id.into(),
            phase,
        }
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
        let w = world(vec![snapshot("ws/1", SessionPhase::Exited)], 2);
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

    // 2. Running session persists, counts toward concurrency
    #[test]
    fn running_session_persists() {
        let w = world(vec![snapshot("ws/1", SessionPhase::Running)], 2);
        let actions = tick(&w);
        assert_eq!(actions, vec![SessionAction::SpawnUpTo { count: 1 }]);
    }

    // 3. Stopping within timeout -> no ForceKill
    #[test]
    fn stopping_within_timeout_no_force_kill() {
        let w = world(
            vec![snapshot(
                "ws/1",
                SessionPhase::Stopping {
                    since: Instant::now(),
                },
            )],
            2,
        );
        let actions = tick(&w);
        assert_eq!(actions, vec![SessionAction::SpawnUpTo { count: 2 }]);
    }

    // 4. Stopping past timeout -> ForceKill + Remove
    #[test]
    fn stopping_past_timeout_force_killed() {
        let past = Instant::now() - Duration::from_secs(10);
        let w = world(
            vec![snapshot("ws/1", SessionPhase::Stopping { since: past })],
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

    // 5. Stopping sessions don't count toward concurrency
    #[test]
    fn stopping_sessions_dont_count_toward_concurrency() {
        let w = world(
            vec![
                snapshot("ws/1", SessionPhase::Running),
                snapshot(
                    "ws/2",
                    SessionPhase::Stopping {
                        since: Instant::now(),
                    },
                ),
            ],
            2,
        );
        let actions = tick(&w);
        assert!(actions.contains(&SessionAction::SpawnUpTo { count: 1 }));
    }

    // 6. Spawn after force-kill fills the slot
    #[test]
    fn spawn_after_force_kill() {
        let past = Instant::now() - Duration::from_secs(10);
        let w = world(
            vec![snapshot("ws/1", SessionPhase::Stopping { since: past })],
            1,
        );
        let actions = tick(&w);
        assert!(actions.contains(&SessionAction::ForceKill {
            session_id: "ws/1".into()
        }));
        assert!(actions.contains(&SessionAction::SpawnUpTo { count: 1 }));
    }

    // 7. At capacity -> no SpawnUpTo
    #[test]
    fn at_capacity_no_spawn() {
        let w = world(
            vec![
                snapshot("ws/1", SessionPhase::Running),
                snapshot("ws/2", SessionPhase::Running),
            ],
            2,
        );
        let actions = tick(&w);
        assert!(actions.is_empty());
    }

    // 8. Manual mode (max_concurrency=0) -> no SpawnUpTo
    #[test]
    fn manual_mode_no_auto_spawn() {
        let w = world(vec![], 0);
        let actions = tick(&w);
        assert!(actions.is_empty());
    }

    // 9. Manual mode still cleans up exited sessions
    #[test]
    fn manual_mode_still_cleans_exited() {
        let w = world(vec![snapshot("ws/1", SessionPhase::Exited)], 0);
        let actions = tick(&w);
        assert_eq!(
            actions,
            vec![SessionAction::Remove {
                session_id: "ws/1".into()
            }]
        );
    }

    // 10. Manual mode: running session persists, no auto-spawn
    #[test]
    fn manual_mode_running_persists() {
        let w = world(vec![snapshot("ws/1", SessionPhase::Running)], 0);
        let actions = tick(&w);
        assert!(actions.is_empty());
    }

    // 11. Over capacity (reconnected sessions) -> no reaping, no spawning
    #[test]
    fn over_capacity_preserves_all_sessions() {
        let w = world(
            vec![
                snapshot("ws/1", SessionPhase::Running),
                snapshot("ws/2", SessionPhase::Running),
                snapshot("ws/3", SessionPhase::Running),
                snapshot("ws/4", SessionPhase::Running),
                snapshot("ws/5", SessionPhase::Running),
            ],
            3,
        );
        let actions = tick(&w);
        assert!(actions.is_empty());
    }
}
