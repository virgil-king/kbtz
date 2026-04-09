use crate::git;
use crate::lifecycle::{self, Action, SessionSnapshot, StepSnapshot, WorldSnapshot};
use crate::project::ProjectDir;
use crate::prompt;
use crate::session::{AgentSessionId, HeadlessSession, SessionMessage, SessionRole};
use crate::step::StepPhase;
use crate::stream::StreamEvent;
use crate::tui::AppState;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub struct Orchestrator {
    pub project_dir: Arc<Mutex<ProjectDir>>,
    pub sessions: Vec<HeadlessSession>,
    pub app: AppState,
    pub leader_busy: bool,
    exited_session_ids: HashSet<String>,
    trace_dir: PathBuf,
    trace_files: HashMap<String, std::fs::File>,
}

impl Orchestrator {
    pub fn new(project_dir: Arc<Mutex<ProjectDir>>) -> Self {
        let trace_dir = {
            let dir = project_dir.lock().unwrap();
            let td = dir.root().join("traces");
            let _ = fs::create_dir_all(&td);
            td
        };
        Self {
            project_dir,
            sessions: vec![],
            app: AppState::new(),
            leader_busy: false,
            exited_session_ids: HashSet::new(),
            trace_dir,
            trace_files: HashMap::new(),
        }
    }

    fn write_trace(&mut self, session_key: &str, line: &str) {
        let file = self.trace_files.entry(session_key.to_string()).or_insert_with(|| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.trace_dir.join(format!("{}.jsonl", session_key)))
                .expect("failed to open trace file")
        });
        let _ = writeln!(file, "{}", line);
    }

    fn build_world(&self) -> WorldSnapshot {
        let dir = self.project_dir.lock().unwrap();
        let steps = dir
            .state()
            .steps
            .iter()
            .map(|s| StepSnapshot {
                id: s.id.clone(),
                phase: s.phase.clone(),
                repos: s.dispatch.repos.clone(),
            })
            .collect();

        let sessions = self
            .sessions
            .iter()
            .map(|s| SessionSnapshot {
                step_id: s.step_id.clone(),
                role: s.role.clone(),
                exited: self.exited_session_ids.contains(&s.key.0),
            })
            .collect();

        WorldSnapshot {
            steps,
            sessions,
            leader_busy: self.leader_busy,
        }
    }

    pub fn poll_sessions(&mut self) {
        let mut exited_indices = vec![];
        let mut trace_lines: Vec<(String, String)> = vec![];

        for (i, session) in self.sessions.iter_mut().enumerate() {
            while let Ok(msg) = session.rx.try_recv() {
                match msg {
                    SessionMessage::Event(event) => {
                        self.app.push_event(&session.key.0, event);
                    }
                    SessionMessage::RawLine(line) => {
                        trace_lines.push((session.key.0.clone(), line));
                    }
                    SessionMessage::Exited { .. } => {}
                }
            }

            if let Ok(Some(_code)) = session.try_wait() {
                self.exited_session_ids.insert(session.key.0.clone());
                exited_indices.push(i);
            }
        }

        for (key, line) in trace_lines {
            self.write_trace(&key, &line);
        }

        for &i in exited_indices.iter().rev() {
            let session = &self.sessions[i];
            let step_id = session.step_id.clone();
            let role = session.role.clone();
            let key_str = session.key.0.clone();

            // Persist agent session UUID for future --resume
            {
                let mut dir = self.project_dir.lock().unwrap();
                dir.state_mut()
                    .session_ids
                    .insert(key_str.clone(), session.agent_session_id.0.to_string());
                let _ = dir.persist();
            }

            match role {
                SessionRole::Implementation => {
                    let summary = self.extract_summary(&key_str);
                    let session_dir = {
                        let dir = self.project_dir.lock().unwrap();
                        dir.root()
                            .join("sessions")
                            .join(format!("{}-impl", step_id))
                    };
                    self.fetch_step_commits(&step_id, &session_dir);

                    let mut dir = self.project_dir.lock().unwrap();
                    if let Some(step) =
                        dir.state_mut().steps.iter_mut().find(|s| s.id == step_id)
                    {
                        step.summary = Some(summary);
                    }
                    let _ = dir.persist();
                }
                SessionRole::Stakeholder { ref name } => {
                    let feedback = self.extract_summary(&session.key.0);
                    let mut dir = self.project_dir.lock().unwrap();
                    if let Some(step) =
                        dir.state_mut().steps.iter_mut().find(|s| s.id == step_id)
                    {
                        step.feedback.push(crate::step::Feedback {
                            stakeholder: name.clone(),
                            content: feedback,
                        });
                    }
                    let _ = dir.persist();
                }
                SessionRole::LeaderDecision => {
                    self.leader_busy = false;
                }
            }

            self.sessions.remove(i);
        }
    }

    pub fn process_tick(&mut self) -> io::Result<()> {
        let world = self.build_world();
        let actions = lifecycle::tick(&world);

        for action in actions {
            match action {
                Action::SpawnImplementation { step_id, repos } => {
                    self.spawn_implementation(&step_id, &repos)?;
                }
                Action::SpawnStakeholders { step_id } => {
                    self.spawn_stakeholders(&step_id)?;
                }
                Action::InvokeLeader { step_ids } => {
                    self.invoke_leader(&step_ids)?;
                }
                Action::TransitionStep { step_id, to } => {
                    let mut dir = self.project_dir.lock().unwrap();
                    if let Some(step) =
                        dir.state_mut().steps.iter_mut().find(|s| s.id == step_id)
                    {
                        step.phase = to;
                    }
                    dir.persist()?;
                }
            }
        }

        Ok(())
    }

    fn spawn_implementation(&mut self, step_id: &str, repos: &[String]) -> io::Result<()> {
        let (session_dir, owned_pairs, step_prompt, existing_id) = {
            let dir = self.project_dir.lock().unwrap();
            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", step_id));
            let owned_pairs: Vec<(String, std::path::PathBuf)> = repos
                .iter()
                .filter_map(|name| {
                    let source = dir.root().join("repos").join(name);
                    if source.exists() {
                        Some((name.clone(), source))
                    } else {
                        None
                    }
                })
                .collect();
            let step_prompt = dir
                .state()
                .steps
                .iter()
                .find(|s| s.id == step_id)
                .map(|s| s.dispatch.prompt.clone())
                .unwrap_or_default();
            let existing_id = self.lookup_agent_session_id_locked(&dir, step_id, &SessionRole::Implementation);
            (session_dir, owned_pairs, step_prompt, existing_id)
        };

        let ref_pairs: Vec<(&str, &Path)> = owned_pairs
            .iter()
            .map(|(n, p)| (n.as_str(), p.as_path()))
            .collect();
        git::setup_session_dir(&session_dir, &ref_pairs)?;

        let prompt_text = prompt::implementation_prompt(&step_prompt);
        let session = HeadlessSession::spawn(
            step_id,
            SessionRole::Implementation,
            &prompt_text,
            &session_dir,
            existing_id,
        )?;

        {
            let mut dir = self.project_dir.lock().unwrap();
            if let Some(step) =
                dir.state_mut().steps.iter_mut().find(|s| s.id == step_id)
            {
                step.phase = StepPhase::Running;
            }
            dir.persist()?;
        }

        self.sessions.push(session);
        Ok(())
    }

    fn spawn_stakeholders(&mut self, step_id: &str) -> io::Result<()> {
        let (step, stakeholders, session_dir) = {
            let dir = self.project_dir.lock().unwrap();
            let step = dir.state().steps.iter().find(|s| s.id == step_id).cloned();
            let stakeholders = dir.state().project.stakeholders.clone();
            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", step_id));
            (step, stakeholders, session_dir)
        };

        for stakeholder in &stakeholders {
            let prompt_text = prompt::stakeholder_prompt(
                &stakeholder.persona,
                &step
                    .as_ref()
                    .map(|s| s.dispatch.prompt.as_str())
                    .unwrap_or(""),
                &step
                    .as_ref()
                    .and_then(|s| s.summary.as_deref())
                    .unwrap_or("No summary"),
            );

            let role = SessionRole::Stakeholder {
                name: stakeholder.name.clone(),
            };
            let existing_id = {
                let dir = self.project_dir.lock().unwrap();
                self.lookup_agent_session_id_locked(&dir, step_id, &role)
            };
            let session = HeadlessSession::spawn(
                step_id,
                role,
                &prompt_text,
                &session_dir,
                existing_id,
            )?;
            self.sessions.push(session);
        }

        {
            let mut dir = self.project_dir.lock().unwrap();
            if let Some(step) =
                dir.state_mut().steps.iter_mut().find(|s| s.id == step_id)
            {
                step.phase = StepPhase::Reviewing;
            }
            dir.persist()?;
        }

        Ok(())
    }

    fn invoke_leader(&mut self, step_ids: &[String]) -> io::Result<()> {
        let (state, project_md, working_dir, existing_id) = {
            let dir = self.project_dir.lock().unwrap();
            let state = dir.state().clone();
            let project_md_path = dir.root().join("project.md");
            let project_md = std::fs::read_to_string(&project_md_path).ok();
            let working_dir = dir.root().to_path_buf();
            let existing_id =
                self.lookup_agent_session_id_locked(&dir, "leader", &SessionRole::LeaderDecision);
            (state, project_md, working_dir, existing_id)
        };

        let step_feedback: Vec<(String, Vec<(String, String)>)> = step_ids
            .iter()
            .filter_map(|id| {
                let step = state.steps.iter().find(|s| &s.id == id)?;
                let feedbacks: Vec<(String, String)> = step
                    .feedback
                    .iter()
                    .map(|f| (f.stakeholder.clone(), f.content.clone()))
                    .collect();
                Some((id.clone(), feedbacks))
            })
            .collect();

        let prompt_text =
            prompt::leader_decision_prompt(&state, &step_feedback, project_md.as_deref());

        let session = HeadlessSession::spawn(
            "leader",
            SessionRole::LeaderDecision,
            &prompt_text,
            &working_dir,
            existing_id,
        )?;

        self.leader_busy = true;
        self.sessions.push(session);
        Ok(())
    }

    fn extract_summary(&self, session_id: &str) -> String {
        if let Some((_, events)) = self
            .app
            .session_events
            .iter()
            .find(|(id, _)| id == session_id)
        {
            for event in events.iter().rev() {
                if let StreamEvent::Result { result, .. } = event {
                    return result.clone();
                }
            }
            for event in events.iter().rev() {
                if let StreamEvent::AssistantText(text) = event {
                    return text.clone();
                }
            }
        }
        "No summary available".to_string()
    }

    fn lookup_agent_session_id_locked(
        &self,
        dir: &ProjectDir,
        step_id: &str,
        role: &SessionRole,
    ) -> Option<AgentSessionId> {
        let key = format!(
            "{}-{}",
            step_id,
            match role {
                SessionRole::Implementation => "impl".to_string(),
                SessionRole::Stakeholder { name } => name.clone(),
                SessionRole::LeaderDecision => "leader".to_string(),
            }
        );
        dir.state()
            .session_ids
            .get(&key)
            .and_then(|s| Uuid::parse_str(s).ok())
            .map(AgentSessionId)
    }

    fn fetch_step_commits(&self, step_id: &str, session_dir: &Path) {
        let dir = self.project_dir.lock().unwrap();
        let repos = &dir.state().project.repos;
        let multi_repo = repos.len() > 1;
        for repo in repos {
            let clone_path = session_dir.join(&repo.name);
            let target_path = dir.root().join("repos").join(&repo.name);
            if clone_path.exists() && target_path.exists() {
                let branch_name = if multi_repo {
                    format!("{}/{}", step_id, repo.name)
                } else {
                    step_id.to_string()
                };
                let _ = git::fetch_branch(&target_path, &clone_path, &branch_name);
            }
        }
    }
}
