use kbtz_council::lifecycle::{tick, Action, SessionSnapshot, JobSnapshot, WorldSnapshot};
use kbtz_council::session::SessionKey;
use kbtz_council::job::JobPhase;

fn empty_world() -> WorldSnapshot {
    WorldSnapshot {
        jobs: vec![],
        sessions: vec![],
        leader_busy: false,
    }
}

#[test]
fn empty_world_produces_no_actions() {
    let actions = tick(&empty_world());
    assert!(actions.is_empty());
}

#[test]
fn dispatched_step_with_no_session_spawns_implementation() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Dispatched,
            repos: vec!["backend".into()],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::SpawnImplementation { job_id, repos } => {
            assert_eq!(job_id, "job-001");
            assert_eq!(repos, &vec!["backend".to_string()]);
        }
        other => panic!("expected SpawnImplementation, got {:?}", other),
    }
}

#[test]
fn running_step_with_exited_session_transitions_to_completed() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Running,
            repos: vec![],
        }],
        sessions: vec![SessionSnapshot {
            job_id: "job-001".into(),
            key: SessionKey::Implementation { job_id: "job-001".into() },
            exited: true,
        }],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::TransitionJob { job_id, to }
            if job_id == "job-001" && *to == JobPhase::Completed
    )));
}

#[test]
fn running_step_with_active_session_does_nothing() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Running,
            repos: vec![],
        }],
        sessions: vec![SessionSnapshot {
            job_id: "job-001".into(),
            key: SessionKey::Implementation { job_id: "job-001".into() },
            exited: false,
        }],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.is_empty());
}

#[test]
fn completed_step_spawns_stakeholders() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Completed,
            repos: vec![],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions
        .iter()
        .any(|a| matches!(a, Action::SpawnStakeholders { job_id } if job_id == "job-001")));
}

#[test]
fn reviewing_step_all_stakeholders_exited_transitions_to_reviewed() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Reviewing,
            repos: vec![],
        }],
        sessions: vec![
            SessionSnapshot {
                job_id: "job-001".into(),
                key: SessionKey::Stakeholder { name: "security".into() },
                exited: true,
            },
            SessionSnapshot {
                job_id: "job-001".into(),
                key: SessionKey::Stakeholder { name: "api".into() },
                exited: true,
            },
        ],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::TransitionJob { job_id, to }
            if job_id == "job-001" && *to == JobPhase::Reviewed
    )));
}

#[test]
fn reviewing_step_with_active_stakeholder_does_nothing() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Reviewing,
            repos: vec![],
        }],
        sessions: vec![
            SessionSnapshot {
                job_id: "job-001".into(),
                key: SessionKey::Stakeholder { name: "security".into() },
                exited: true,
            },
            SessionSnapshot {
                job_id: "job-001".into(),
                key: SessionKey::Stakeholder { name: "api".into() },
                exited: false,
            },
        ],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.is_empty());
}

#[test]
fn reviewed_step_invokes_leader_when_not_busy() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Reviewed,
            repos: vec![],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(
        |a| matches!(a, Action::InvokeLeader { job_ids } if job_ids.contains(&"job-001".to_string()))
    ));
}

#[test]
fn reviewed_step_waits_when_leader_busy() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Reviewed,
            repos: vec![],
        }],
        sessions: vec![],
        leader_busy: true,
    };

    let actions = tick(&world);
    assert!(actions.is_empty());
}

#[test]
fn multiple_reviewed_steps_batched_into_one_leader_invocation() {
    let world = WorldSnapshot {
        jobs: vec![
            JobSnapshot {
                id: "job-001".into(),
                phase: JobPhase::Reviewed,
                repos: vec![],
            },
            JobSnapshot {
                id: "job-002".into(),
                phase: JobPhase::Reviewed,
                repos: vec![],
            },
        ],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    let leader_actions: Vec<_> = actions
        .iter()
        .filter(|a| matches!(a, Action::InvokeLeader { .. }))
        .collect();
    assert_eq!(leader_actions.len(), 1);
    match &leader_actions[0] {
        Action::InvokeLeader { job_ids } => {
            assert_eq!(job_ids.len(), 2);
            assert!(job_ids.contains(&"job-001".to_string()));
            assert!(job_ids.contains(&"job-002".to_string()));
        }
        _ => unreachable!(),
    }
}

#[test]
fn rework_job_with_no_session_spawns_implementation() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Rework,
            repos: vec!["backend".into()],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(
        |a| matches!(a, Action::SpawnImplementation { job_id, .. } if job_id == "job-001")
    ));
}

#[test]
fn rework_job_with_exited_session_transitions_to_completed() {
    let world = WorldSnapshot {
        jobs: vec![JobSnapshot {
            id: "job-001".into(),
            phase: JobPhase::Rework,
            repos: vec![],
        }],
        sessions: vec![SessionSnapshot {
            job_id: "job-001".into(),
            key: SessionKey::Implementation { job_id: "job-001".into() },
            exited: true,
        }],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::TransitionJob { job_id, to }
            if job_id == "job-001" && *to == JobPhase::Completed
    )));
}
