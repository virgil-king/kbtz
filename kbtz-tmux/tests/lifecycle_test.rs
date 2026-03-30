use std::time::Instant;

use kbtz_tmux::lifecycle::*;

#[test]
fn full_lifecycle_scenario() {
    // Start with 2 slots, no windows.
    let w = WorldSnapshot {
        windows: vec![],
        max_concurrency: 2,
        now: Instant::now(),
    };
    let actions = tick(&w);
    assert_eq!(
        actions,
        vec![Action::SpawnUpTo { count: 2 }],
        "empty world should spawn up to max"
    );

    // After spawning, both slots running.
    let w = WorldSnapshot {
        windows: vec![
            WindowSnapshot {
                session_id: "ws/0".into(),
                task_name: "task-a".into(),
                window_id: "@1".into(),
                phase: WindowPhase::Running,
                task: Some(TaskSnapshot {
                    status: "active".into(),
                    assignee: Some("ws/0".into()),
                    blocked: false,
                }),
            },
            WindowSnapshot {
                session_id: "ws/1".into(),
                task_name: "task-b".into(),
                window_id: "@2".into(),
                phase: WindowPhase::Running,
                task: Some(TaskSnapshot {
                    status: "active".into(),
                    assignee: Some("ws/1".into()),
                    blocked: false,
                }),
            },
        ],
        max_concurrency: 2,
        now: Instant::now(),
    };
    let actions = tick(&w);
    assert!(actions.is_empty(), "at capacity with running sessions");

    // task-a completes. Session persists, still at capacity.
    let w = WorldSnapshot {
        windows: vec![
            WindowSnapshot {
                session_id: "ws/0".into(),
                task_name: "task-a".into(),
                window_id: "@1".into(),
                phase: WindowPhase::Running,
                task: Some(TaskSnapshot {
                    status: "done".into(),
                    assignee: Some("ws/0".into()),
                    blocked: false,
                }),
            },
            WindowSnapshot {
                session_id: "ws/1".into(),
                task_name: "task-b".into(),
                window_id: "@2".into(),
                phase: WindowPhase::Running,
                task: Some(TaskSnapshot {
                    status: "active".into(),
                    assignee: Some("ws/1".into()),
                    blocked: false,
                }),
            },
        ],
        max_concurrency: 2,
        now: Instant::now(),
    };
    let actions = tick(&w);
    // Sessions are never auto-killed. Both persist, still at capacity.
    assert!(actions.is_empty(), "done task session persists");

    // ws/0 process exits on its own -> Gone. Now a slot opens.
    let w = WorldSnapshot {
        windows: vec![
            WindowSnapshot {
                session_id: "ws/0".into(),
                task_name: "task-a".into(),
                window_id: "@1".into(),
                phase: WindowPhase::Gone,
                task: Some(TaskSnapshot {
                    status: "done".into(),
                    assignee: Some("ws/0".into()),
                    blocked: false,
                }),
            },
            WindowSnapshot {
                session_id: "ws/1".into(),
                task_name: "task-b".into(),
                window_id: "@2".into(),
                phase: WindowPhase::Running,
                task: Some(TaskSnapshot {
                    status: "active".into(),
                    assignee: Some("ws/1".into()),
                    blocked: false,
                }),
            },
        ],
        max_concurrency: 2,
        now: Instant::now(),
    };
    let actions = tick(&w);
    assert!(actions.contains(&Action::Remove {
        session_id: "ws/0".into()
    }));
    assert!(actions.contains(&Action::SpawnUpTo { count: 1 }));
}
