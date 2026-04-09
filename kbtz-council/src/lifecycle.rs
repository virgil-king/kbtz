use crate::session::SessionRole;
use crate::step::StepPhase;

#[derive(Debug, Clone)]
pub struct StepSnapshot {
    pub id: String,
    pub phase: StepPhase,
    pub repos: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub step_id: String,
    pub role: SessionRole,
    pub exited: bool,
}

#[derive(Debug)]
pub struct WorldSnapshot {
    pub steps: Vec<StepSnapshot>,
    pub sessions: Vec<SessionSnapshot>,
    pub leader_busy: bool,
}

#[derive(Debug)]
pub enum Action {
    SpawnImplementation { step_id: String, repos: Vec<String> },
    SpawnStakeholders { step_id: String },
    InvokeLeader { step_ids: Vec<String> },
    TransitionStep { step_id: String, to: StepPhase },
}

pub fn tick(world: &WorldSnapshot) -> Vec<Action> {
    let mut actions = Vec::new();

    for step in &world.steps {
        match &step.phase {
            StepPhase::Dispatched => {
                let has_impl = world.sessions.iter().any(|s| {
                    s.step_id == step.id && matches!(s.role, SessionRole::Implementation)
                });
                if !has_impl {
                    actions.push(Action::SpawnImplementation {
                        step_id: step.id.clone(),
                        repos: step.repos.clone(),
                    });
                }
            }
            StepPhase::Running => {
                let impl_exited = world.sessions.iter().any(|s| {
                    s.step_id == step.id
                        && matches!(s.role, SessionRole::Implementation)
                        && s.exited
                });
                if impl_exited {
                    actions.push(Action::TransitionStep {
                        step_id: step.id.clone(),
                        to: StepPhase::Completed,
                    });
                }
            }
            StepPhase::Rework => {
                let has_impl = world.sessions.iter().any(|s| {
                    s.step_id == step.id && matches!(s.role, SessionRole::Implementation)
                });
                if !has_impl {
                    // No session yet — spawn one to resume rework
                    actions.push(Action::SpawnImplementation {
                        step_id: step.id.clone(),
                        repos: step.repos.clone(),
                    });
                } else {
                    // Session exists — check if it exited
                    let impl_exited = world.sessions.iter().any(|s| {
                        s.step_id == step.id
                            && matches!(s.role, SessionRole::Implementation)
                            && s.exited
                    });
                    if impl_exited {
                        actions.push(Action::TransitionStep {
                            step_id: step.id.clone(),
                            to: StepPhase::Completed,
                        });
                    }
                }
            }
            StepPhase::Completed => {
                let has_stakeholders = world.sessions.iter().any(|s| {
                    s.step_id == step.id && matches!(s.role, SessionRole::Stakeholder { .. })
                });
                if !has_stakeholders {
                    actions.push(Action::SpawnStakeholders {
                        step_id: step.id.clone(),
                    });
                }
            }
            StepPhase::Reviewing => {
                let stakeholder_sessions: Vec<_> = world
                    .sessions
                    .iter()
                    .filter(|s| {
                        s.step_id == step.id
                            && matches!(s.role, SessionRole::Stakeholder { .. })
                    })
                    .collect();
                if !stakeholder_sessions.is_empty()
                    && stakeholder_sessions.iter().all(|s| s.exited)
                {
                    actions.push(Action::TransitionStep {
                        step_id: step.id.clone(),
                        to: StepPhase::Reviewed,
                    });
                }
            }
            StepPhase::Reviewed | StepPhase::Merged => {}
        }
    }

    // Batch all reviewed steps into one leader invocation
    if !world.leader_busy {
        let reviewed_ids: Vec<String> = world
            .steps
            .iter()
            .filter(|s| matches!(s.phase, StepPhase::Reviewed))
            .map(|s| s.id.clone())
            .collect();
        if !reviewed_ids.is_empty() {
            actions.push(Action::InvokeLeader {
                step_ids: reviewed_ids,
            });
        }
    }

    actions
}
