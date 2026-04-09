use kbtz_council::lifecycle::{tick, Action, SessionSnapshot, StepSnapshot, WorldSnapshot};
use kbtz_council::session::SessionRole;
use kbtz_council::step::StepPhase;

fn empty_world() -> WorldSnapshot {
    WorldSnapshot {
        steps: vec![],
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
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Dispatched,
            repos: vec!["backend".into()],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::SpawnImplementation { step_id, repos } => {
            assert_eq!(step_id, "step-001");
            assert_eq!(repos, &vec!["backend".to_string()]);
        }
        other => panic!("expected SpawnImplementation, got {:?}", other),
    }
}

#[test]
fn running_step_with_exited_session_transitions_to_completed() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Running,
            repos: vec![],
        }],
        sessions: vec![SessionSnapshot {
            step_id: "step-001".into(),
            role: SessionRole::Implementation,
            exited: true,
        }],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::TransitionStep { step_id, to }
            if step_id == "step-001" && *to == StepPhase::Completed
    )));
}

#[test]
fn running_step_with_active_session_does_nothing() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Running,
            repos: vec![],
        }],
        sessions: vec![SessionSnapshot {
            step_id: "step-001".into(),
            role: SessionRole::Implementation,
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
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Completed,
            repos: vec![],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions
        .iter()
        .any(|a| matches!(a, Action::SpawnStakeholders { step_id } if step_id == "step-001")));
}

#[test]
fn reviewing_step_all_stakeholders_exited_transitions_to_reviewed() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Reviewing,
            repos: vec![],
        }],
        sessions: vec![
            SessionSnapshot {
                step_id: "step-001".into(),
                role: SessionRole::Stakeholder {
                    name: "security".into(),
                },
                exited: true,
            },
            SessionSnapshot {
                step_id: "step-001".into(),
                role: SessionRole::Stakeholder {
                    name: "api".into(),
                },
                exited: true,
            },
        ],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::TransitionStep { step_id, to }
            if step_id == "step-001" && *to == StepPhase::Reviewed
    )));
}

#[test]
fn reviewing_step_with_active_stakeholder_does_nothing() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Reviewing,
            repos: vec![],
        }],
        sessions: vec![
            SessionSnapshot {
                step_id: "step-001".into(),
                role: SessionRole::Stakeholder {
                    name: "security".into(),
                },
                exited: true,
            },
            SessionSnapshot {
                step_id: "step-001".into(),
                role: SessionRole::Stakeholder {
                    name: "api".into(),
                },
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
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Reviewed,
            repos: vec![],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(
        |a| matches!(a, Action::InvokeLeader { step_ids } if step_ids.contains(&"step-001".to_string()))
    ));
}

#[test]
fn reviewed_step_waits_when_leader_busy() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Reviewed,
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
        steps: vec![
            StepSnapshot {
                id: "step-001".into(),
                phase: StepPhase::Reviewed,
                repos: vec![],
            },
            StepSnapshot {
                id: "step-002".into(),
                phase: StepPhase::Reviewed,
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
        Action::InvokeLeader { step_ids } => {
            assert_eq!(step_ids.len(), 2);
            assert!(step_ids.contains(&"step-001".to_string()));
            assert!(step_ids.contains(&"step-002".to_string()));
        }
        _ => unreachable!(),
    }
}

#[test]
fn rework_step_with_no_session_spawns_implementation() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Rework,
            repos: vec!["backend".into()],
        }],
        sessions: vec![],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(
        |a| matches!(a, Action::SpawnImplementation { step_id, .. } if step_id == "step-001")
    ));
}

#[test]
fn rework_step_with_exited_session_transitions_to_completed() {
    let world = WorldSnapshot {
        steps: vec![StepSnapshot {
            id: "step-001".into(),
            phase: StepPhase::Rework,
            repos: vec![],
        }],
        sessions: vec![SessionSnapshot {
            step_id: "step-001".into(),
            role: SessionRole::Implementation,
            exited: true,
        }],
        leader_busy: false,
    };

    let actions = tick(&world);
    assert!(actions.iter().any(|a| matches!(
        a,
        Action::TransitionStep { step_id, to }
            if step_id == "step-001" && *to == StepPhase::Completed
    )));
}
