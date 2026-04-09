use crate::git;
use crate::lifecycle::{self, Action, SessionSnapshot, StepSnapshot, WorldSnapshot};
use crate::project::ProjectDir;
use crate::prompt;
use crate::session::{ManagedSession, QueueItem, SessionKey, SessionMessage};
use crate::step::StepPhase;
use crate::stream::StreamEvent;
use crate::tui::AppState;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Orchestrator {
    pub project_dir: Arc<Mutex<ProjectDir>>,
    pub sessions: HashMap<SessionKey, ManagedSession>,
    pub app: AppState,
    trace_dir: PathBuf,
    trace_files: HashMap<String, std::fs::File>,
    mcp_config_path: PathBuf,
}

impl Orchestrator {
    pub fn new(project_dir: Arc<Mutex<ProjectDir>>, mcp_config_path: PathBuf) -> Self {
        let trace_dir = {
            let dir = project_dir.lock().unwrap();
            let td = dir.root().join("traces");
            let _ = fs::create_dir_all(&td);
            td
        };
        Self {
            project_dir,
            sessions: HashMap::new(),
            app: AppState::new(),
            trace_dir,
            trace_files: HashMap::new(),
            mcp_config_path,
        }
    }

    /// Get or create a managed session for the given key.
    fn ensure_session(&mut self, key: SessionKey) -> &mut ManagedSession {
        self.sessions
            .entry(key.clone())
            .or_insert_with(|| ManagedSession::new(key))
    }

    /// Enqueue a user message for any session.
    pub fn send_message(&mut self, key: &SessionKey, message: String) {
        let working_dir = {
            let dir = self.project_dir.lock().unwrap();
            dir.root().to_path_buf()
        };
        let mcp_config = if matches!(key, SessionKey::Leader) {
            Some(self.mcp_config_path.clone())
        } else {
            None
        };
        let system_prompt = match key {
            SessionKey::Leader => Some(prompt::leader_system_prompt()),
            _ => None,
        };
        let session = self.ensure_session(key.clone());
        session.enqueue(QueueItem {
            prompt: message,
            system_prompt,
            step_id: None,
            working_dir,
            mcp_config,
        });
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
            .values()
            .filter_map(|ms| {
                let active = ms.active.as_ref()?;
                Some(SessionSnapshot {
                    step_id: active.step_id.clone().unwrap_or_default(),
                    key: ms.key.clone(),
                    exited: false, // if it's in active, it's running
                })
            })
            .collect();

        let leader_busy = self
            .sessions
            .get(&SessionKey::Leader)
            .map(|ms| ms.is_running())
            .unwrap_or(false);

        WorldSnapshot {
            steps,
            sessions,
            leader_busy,
        }
    }

    /// Poll all sessions: drain events, detect exits, dispatch queued items.
    pub fn poll_sessions(&mut self) {
        let mut trace_lines: Vec<(String, String)> = Vec::new();
        let mut exited_keys: Vec<(SessionKey, Option<String>)> = Vec::new();

        for (key, ms) in &mut self.sessions {
            if let Some(ref mut active) = ms.active {
                // Drain events
                while let Ok(msg) = active.rx.try_recv() {
                    match msg {
                        SessionMessage::Event(event) => {
                            self.app.push_event(&key.to_string(), event);
                        }
                        SessionMessage::RawLine(line) => {
                            trace_lines.push((key.to_string(), line));
                        }
                    }
                }

                // Check exit
                if let Ok(Some(_)) = active.try_wait() {
                    exited_keys.push((key.clone(), active.step_id.clone()));
                }
            }
        }

        // Write traces
        for (key, line) in trace_lines {
            self.write_trace(&key, &line);
        }

        // Process exits
        for (key, step_id) in &exited_keys {
            // Persist agent session ID
            {
                let ms = self.sessions.get(key).unwrap();
                let mut dir = self.project_dir.lock().unwrap();
                dir.state_mut()
                    .session_ids
                    .insert(key.clone(), ms.agent_session_id.clone());
                let _ = dir.persist();
            }

            if let Some(step_id) = step_id {
                let key_str = key.to_string();
                match key {
                    SessionKey::Implementation { .. } => {
                        let summary = self.extract_summary(&key_str);
                        let session_dir = {
                            let dir = self.project_dir.lock().unwrap();
                            dir.root()
                                .join("sessions")
                                .join(format!("{}-impl", step_id))
                        };
                        self.fetch_step_commits(step_id, &session_dir);

                        let mut dir = self.project_dir.lock().unwrap();
                        if let Some(step) =
                            dir.state_mut().steps.iter_mut().find(|s| s.id == *step_id)
                        {
                            step.summary = Some(summary);
                        }
                        let _ = dir.persist();
                    }
                    SessionKey::Stakeholder { name } => {
                        let feedback = self.extract_summary(&key_str);
                        let mut dir = self.project_dir.lock().unwrap();
                        if let Some(step) =
                            dir.state_mut().steps.iter_mut().find(|s| s.id == *step_id)
                        {
                            step.feedback.push(crate::step::Feedback {
                                stakeholder: name.clone(),
                                content: feedback,
                            });
                        }
                        let _ = dir.persist();
                    }
                    SessionKey::Leader => {}
                }
            }

            // Reap the active session
            self.sessions.get_mut(key).unwrap().reap();
        }

        // Try to dispatch queued items for all sessions
        for ms in self.sessions.values_mut() {
            let _ = ms.try_dispatch();
        }
    }

    /// Process lifecycle tick — translate actions into session queue items.
    pub fn process_tick(&mut self) -> io::Result<()> {
        // Reload state from disk (MCP subprocess may have modified it)
        {
            let mut dir = self.project_dir.lock().unwrap();
            let _ = dir.reload();
        }
        let world = self.build_world();
        let actions = lifecycle::tick(&world);

        for action in actions {
            match action {
                Action::SpawnImplementation { step_id, repos } => {
                    self.enqueue_implementation(&step_id, &repos)?;
                }
                Action::SpawnStakeholders { step_id } => {
                    self.enqueue_stakeholders(&step_id)?;
                }
                Action::InvokeLeader { step_ids } => {
                    self.enqueue_leader_decision(&step_ids)?;
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

    fn enqueue_implementation(&mut self, step_id: &str, repos: &[String]) -> io::Result<()> {
        let (session_dir, step_prompt) = {
            let dir = self.project_dir.lock().unwrap();
            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", step_id));
            let step_prompt = dir
                .state()
                .steps
                .iter()
                .find(|s| s.id == step_id)
                .map(|s| s.dispatch.prompt.clone())
                .unwrap_or_default();
            (session_dir, step_prompt)
        };

        // Set up clones
        let owned_pairs: Vec<(String, PathBuf)> = {
            let dir = self.project_dir.lock().unwrap();
            repos
                .iter()
                .filter_map(|name| {
                    let source = dir.root().join("repos").join(name);
                    if source.exists() {
                        Some((name.clone(), source))
                    } else {
                        None
                    }
                })
                .collect()
        };

        let ref_pairs: Vec<(&str, &Path)> = owned_pairs
            .iter()
            .map(|(n, p)| (n.as_str(), p.as_path()))
            .collect();
        git::setup_session_dir(&session_dir, &ref_pairs)?;

        let key = SessionKey::Implementation {
            step_id: step_id.to_string(),
        };
        let session = self.ensure_session(key);
        session.enqueue(QueueItem {
            prompt: prompt::implementation_prompt(Some(&session_dir), &step_prompt),
            system_prompt: None,
            step_id: Some(step_id.to_string()),
            working_dir: session_dir,
            mcp_config: None,
        });

        // Transition to running
        {
            let mut dir = self.project_dir.lock().unwrap();
            if let Some(step) =
                dir.state_mut().steps.iter_mut().find(|s| s.id == step_id)
            {
                step.phase = StepPhase::Running;
            }
            dir.persist()?;
        }

        Ok(())
    }

    fn enqueue_stakeholders(&mut self, step_id: &str) -> io::Result<()> {
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
                Some(&session_dir),
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

            let key = SessionKey::Stakeholder {
                name: stakeholder.name.clone(),
            };
            let session = self.ensure_session(key);
            session.enqueue(QueueItem {
                prompt: prompt_text,
                system_prompt: None,
                step_id: Some(step_id.to_string()),
                working_dir: session_dir.clone(),
                mcp_config: None,
            });
        }

        // Transition to reviewing
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

    fn enqueue_leader_decision(&mut self, step_ids: &[String]) -> io::Result<()> {
        let (state, project_md, working_dir) = {
            let dir = self.project_dir.lock().unwrap();
            let state = dir.state().clone();
            let project_md_path = dir.root().join("project.md");
            let project_md = std::fs::read_to_string(&project_md_path).ok();
            let working_dir = dir.root().to_path_buf();
            (state, project_md, working_dir)
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

        let mcp_config = self.mcp_config_path.clone();
        let session = self.ensure_session(SessionKey::Leader);
        session.enqueue(QueueItem {
            prompt: prompt_text,
            system_prompt: Some(prompt::leader_system_prompt()),
            step_id: None,
            working_dir,
            mcp_config: Some(mcp_config),
        });

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

    fn write_trace(&mut self, session_key: &str, line: &str) {
        let file = self
            .trace_files
            .entry(session_key.to_string())
            .or_insert_with(|| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.trace_dir.join(format!("{}.jsonl", session_key)))
                    .expect("failed to open trace file")
            });
        let _ = writeln!(file, "{}", line);
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
