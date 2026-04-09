use crate::session::SessionKey;
use crate::job::JobPhase;

#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: String,
    pub phase: JobPhase,
    pub repos: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub job_id: String,
    pub key: SessionKey,
    pub exited: bool,
}

#[derive(Debug)]
pub struct WorldSnapshot {
    pub jobs: Vec<JobSnapshot>,
    pub sessions: Vec<SessionSnapshot>,
    pub leader_busy: bool,
}

#[derive(Debug)]
pub enum Action {
    SpawnImplementation { job_id: String, repos: Vec<String> },
    SpawnStakeholders { job_id: String },
    InvokeLeader { job_ids: Vec<String> },
    TransitionJob { job_id: String, to: JobPhase },
}

pub fn tick(world: &WorldSnapshot) -> Vec<Action> {
    let mut actions = Vec::new();

    for job in &world.jobs {
        match &job.phase {
            JobPhase::Dispatched => {
                let has_impl = world.sessions.iter().any(|s| {
                    s.job_id == job.id && matches!(s.key, SessionKey::Implementation { .. })
                });
                if !has_impl {
                    actions.push(Action::SpawnImplementation {
                        job_id: job.id.clone(),
                        repos: job.repos.clone(),
                    });
                }
            }
            JobPhase::Running => {
                let impl_exited = world.sessions.iter().any(|s| {
                    s.job_id == job.id
                        && matches!(s.key, SessionKey::Implementation { .. })
                        && s.exited
                });
                if impl_exited {
                    actions.push(Action::TransitionJob {
                        job_id: job.id.clone(),
                        to: JobPhase::Completed,
                    });
                }
            }
            JobPhase::Rework => {
                let has_impl = world.sessions.iter().any(|s| {
                    s.job_id == job.id && matches!(s.key, SessionKey::Implementation { .. })
                });
                if !has_impl {
                    // No session yet — spawn one to resume rework
                    actions.push(Action::SpawnImplementation {
                        job_id: job.id.clone(),
                        repos: job.repos.clone(),
                    });
                } else {
                    // Session exists — check if it exited
                    let impl_exited = world.sessions.iter().any(|s| {
                        s.job_id == job.id
                            && matches!(s.key, SessionKey::Implementation { .. })
                            && s.exited
                    });
                    if impl_exited {
                        actions.push(Action::TransitionJob {
                            job_id: job.id.clone(),
                            to: JobPhase::Completed,
                        });
                    }
                }
            }
            JobPhase::Completed => {
                let has_stakeholders = world.sessions.iter().any(|s| {
                    s.job_id == job.id && matches!(s.key, SessionKey::Stakeholder { .. })
                });
                if !has_stakeholders {
                    actions.push(Action::SpawnStakeholders {
                        job_id: job.id.clone(),
                    });
                }
            }
            JobPhase::Reviewing => {
                let stakeholder_sessions: Vec<_> = world
                    .sessions
                    .iter()
                    .filter(|s| {
                        s.job_id == job.id
                            && matches!(s.key, SessionKey::Stakeholder { .. })
                    })
                    .collect();
                if !stakeholder_sessions.is_empty()
                    && stakeholder_sessions.iter().all(|s| s.exited)
                {
                    actions.push(Action::TransitionJob {
                        job_id: job.id.clone(),
                        to: JobPhase::Reviewed,
                    });
                }
            }
            JobPhase::Reviewed | JobPhase::Merged => {}
        }
    }

    // Batch all reviewed steps into one leader invocation
    if !world.leader_busy {
        let reviewed_ids: Vec<String> = world
            .jobs
            .iter()
            .filter(|s| matches!(s.phase, JobPhase::Reviewed))
            .map(|s| s.id.clone())
            .collect();
        if !reviewed_ids.is_empty() {
            actions.push(Action::InvokeLeader {
                job_ids: reviewed_ids,
            });
        }
    }

    actions
}
