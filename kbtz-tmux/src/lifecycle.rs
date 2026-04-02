use std::time::{Duration, Instant};

pub const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum WindowPhase {
    Running,
    Stopping { since: Instant },
    Gone,
}

#[derive(Debug, Clone)]
pub struct WindowSnapshot {
    pub session_id: String,
    pub task_name: String,
    pub window_id: String,
    pub phase: WindowPhase,
}

pub struct WorldSnapshot {
    pub windows: Vec<WindowSnapshot>,
    pub max_concurrency: usize,
    pub now: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    ForceKill { session_id: String },
    Remove { session_id: String },
    SpawnUpTo { count: usize },
}

pub fn tick(world: &WorldSnapshot) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut running_count: usize = 0;

    for window in &world.windows {
        match &window.phase {
            WindowPhase::Gone => {
                actions.push(Action::Remove {
                    session_id: window.session_id.clone(),
                });
            }
            WindowPhase::Stopping { since } => {
                if world.now.duration_since(*since) >= GRACEFUL_TIMEOUT {
                    actions.push(Action::ForceKill {
                        session_id: window.session_id.clone(),
                    });
                    actions.push(Action::Remove {
                        session_id: window.session_id.clone(),
                    });
                }
            }
            WindowPhase::Running => {
                // Never auto-kill running sessions. The user decides
                // when to close them.
                running_count += 1;
            }
        }
    }

    if running_count < world.max_concurrency {
        actions.push(Action::SpawnUpTo {
            count: world.max_concurrency - running_count,
        });
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(session_id: &str, task_name: &str, phase: WindowPhase) -> WindowSnapshot {
        WindowSnapshot {
            session_id: session_id.into(),
            task_name: task_name.into(),
            window_id: "@0".into(),
            phase,
        }
    }

    fn world(windows: Vec<WindowSnapshot>, max: usize) -> WorldSnapshot {
        WorldSnapshot {
            windows,
            max_concurrency: max,
            now: Instant::now(),
        }
    }

    #[test]
    fn gone_window_removed_and_slot_freed() {
        let w = world(vec![snapshot("ws/1", "task-a", WindowPhase::Gone)], 2);
        let actions = tick(&w);
        assert_eq!(
            actions,
            vec![
                Action::Remove {
                    session_id: "ws/1".into()
                },
                Action::SpawnUpTo { count: 2 },
            ]
        );
    }

    #[test]
    fn running_session_persists() {
        let w = world(vec![snapshot("ws/1", "task-a", WindowPhase::Running)], 2);
        let actions = tick(&w);
        assert_eq!(actions, vec![Action::SpawnUpTo { count: 1 }]);
    }

    #[test]
    fn stopping_within_timeout_waits() {
        let w = world(
            vec![snapshot(
                "ws/1",
                "task-a",
                WindowPhase::Stopping {
                    since: Instant::now(),
                },
            )],
            2,
        );
        let actions = tick(&w);
        assert_eq!(actions, vec![Action::SpawnUpTo { count: 2 }]);
    }

    #[test]
    fn stopping_past_timeout_force_killed() {
        let past = Instant::now() - Duration::from_secs(10);
        let w = world(
            vec![snapshot(
                "ws/1",
                "task-a",
                WindowPhase::Stopping { since: past },
            )],
            2,
        );
        let actions = tick(&w);
        assert!(actions.contains(&Action::ForceKill {
            session_id: "ws/1".into()
        }));
        assert!(actions.contains(&Action::Remove {
            session_id: "ws/1".into()
        }));
    }

    #[test]
    fn at_capacity_no_spawn() {
        let w = world(
            vec![
                snapshot("ws/0", "task-a", WindowPhase::Running),
                snapshot("ws/1", "task-b", WindowPhase::Running),
            ],
            2,
        );
        let actions = tick(&w);
        assert!(actions.is_empty());
    }

    #[test]
    fn stopping_windows_dont_count_toward_concurrency() {
        let w = world(
            vec![
                snapshot("ws/0", "task-a", WindowPhase::Running),
                snapshot(
                    "ws/1",
                    "task-b",
                    WindowPhase::Stopping {
                        since: Instant::now(),
                    },
                ),
            ],
            2,
        );
        let actions = tick(&w);
        assert!(actions.contains(&Action::SpawnUpTo { count: 1 }));
    }
}
