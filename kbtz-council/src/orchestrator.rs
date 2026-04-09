use crate::git;
use crate::lifecycle::{self, Action, SessionSnapshot, JobSnapshot, WorldSnapshot};
use crate::project::ProjectDir;
use crate::prompt;
use crate::session::{ManagedSession, QueueItem, SessionKey, SessionMessage};
use crate::job::JobPhase;
use crate::stream::StreamEvent;
#[allow(unused_imports)]
use crate::session::AgentSessionId;
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
        // Show user message in the stream view
        self.app
            .push_event(&key.to_string(), StreamEvent::UserMessage(message.clone()));

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
            job_id: None,
            working_dir,
            mcp_config,
        });
    }

    fn build_world(&self) -> WorldSnapshot {
        let dir = self.project_dir.lock().unwrap();
        let jobs = dir
            .state()
            .jobs
            .iter()
            .map(|s| JobSnapshot {
                id: s.id.clone(),
                phase: s.phase.clone(),
                repos: s.dispatch.repos.iter().map(|r| r.name.clone()).collect(),
            })
            .collect();

        let sessions = self
            .sessions
            .values()
            .filter_map(|ms| {
                let active = ms.active.as_ref()?;
                Some(SessionSnapshot {
                    job_id: active.job_id.clone().unwrap_or_default(),
                    key: ms.key.clone(),
                    exited: active.exited,
                })
            })
            .collect();

        let leader_busy = self
            .sessions
            .get(&SessionKey::Leader)
            .map(|ms| ms.is_running())
            .unwrap_or(false);

        WorldSnapshot {
            jobs,
            sessions,
            leader_busy,
        }
    }

    /// Poll all sessions: drain events, detect exits. Does NOT reap or
    /// transition phases — that's tick's job.
    pub fn poll_sessions(&mut self) {
        let mut trace_lines: Vec<(String, String)> = Vec::new();
        let mut newly_exited: Vec<(SessionKey, Option<String>)> = Vec::new();

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

                // Detect exit (marks active.exited = true)
                if !active.exited {
                    if let Ok(Some(_)) = active.try_wait() {
                        active.exited = true;
                        newly_exited.push((key.clone(), active.job_id.clone()));
                    }
                }
            }
        }

        // Write traces
        for (key, line) in trace_lines {
            self.write_trace(&key, &line);
        }

        // Extract results from newly exited sessions (summaries, feedback, commits)
        // but do NOT transition phases — tick handles that.
        for (key, job_id) in &newly_exited {
            // Persist agent session ID for future --resume
            {
                let ms = self.sessions.get(key).unwrap();
                let mut dir = self.project_dir.lock().unwrap();
                dir.state_mut()
                    .session_ids
                    .insert(key.clone(), ms.agent_session_id.clone());
                let _ = dir.persist();
            }

            if let Some(job_id) = job_id {
                let key_str = key.to_string();
                match key {
                    SessionKey::Implementation { .. } => {
                        let summary = self.extract_summary(&key_str);
                        let session_dir = {
                            let dir = self.project_dir.lock().unwrap();
                            dir.root()
                                .join("sessions")
                                .join(format!("{}-impl", job_id))
                        };
                        self.fetch_job_commits(job_id, &session_dir);

                        let mut dir = self.project_dir.lock().unwrap();
                        if let Some(job) =
                            dir.state_mut().jobs.iter_mut().find(|s| s.id == *job_id)
                        {
                            job.summary = Some(summary);
                        }
                        let _ = dir.persist();
                    }
                    SessionKey::Stakeholder { name, .. } => {
                        let feedback = self.extract_summary(&key_str);
                        let mut dir = self.project_dir.lock().unwrap();
                        if let Some(job) =
                            dir.state_mut().jobs.iter_mut().find(|s| s.id == *job_id)
                        {
                            job.feedback.push(crate::job::Feedback {
                                stakeholder: name.clone(),
                                content: feedback,
                            });
                        }
                        let _ = dir.persist();
                    }
                    SessionKey::Leader => {}
                }
            }
        }
    }

    /// Reap exited sessions after tick has processed them, then dispatch
    /// queued items.
    pub fn reap_and_dispatch(&mut self) {
        for ms in self.sessions.values_mut() {
            if ms.has_exited() {
                ms.reap();
            }
        }
        for ms in self.sessions.values_mut() {
            let _ = ms.try_dispatch();
        }
    }

    /// Process lifecycle tick — translate actions into session queue items.
    pub fn process_tick(&mut self) -> io::Result<()> {
        let world = self.build_world();
        let actions = lifecycle::tick(&world);

        for action in actions {
            match action {
                Action::SpawnImplementation { job_id, .. } => {
                    self.enqueue_implementation(&job_id)?;
                }
                Action::SpawnStakeholders { job_id } => {
                    self.enqueue_stakeholders(&job_id)?;
                }
                Action::InvokeLeader { job_ids } => {
                    self.enqueue_leader_decision(&job_ids)?;
                }
                Action::TransitionJob { job_id, to } => {
                    let mut dir = self.project_dir.lock().unwrap();
                    if let Some(job) =
                        dir.state_mut().jobs.iter_mut().find(|s| s.id == job_id)
                    {
                        job.phase = to;
                    }
                    dir.persist()?;
                }
            }
        }

        Ok(())
    }

    fn enqueue_implementation(&mut self, job_id: &str) -> io::Result<()> {
        let (session_dir, prompt_text, repo_refs) = {
            let dir = self.project_dir.lock().unwrap();
            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", job_id));
            let job = dir.state().jobs.iter().find(|j| j.id == job_id);

            // Use rework feedback as prompt if available, otherwise original dispatch
            let prompt_text = match job.and_then(|j| j.decision.as_ref()) {
                Some(crate::job::Decision::Rework { feedback }) => {
                    format!(
                        "Your previous implementation needs changes. Here is the feedback:\n\n{}\n\nThe original task was:\n\n{}",
                        feedback,
                        job.map(|j| j.dispatch.prompt.as_str()).unwrap_or("")
                    )
                }
                _ => job.map(|j| j.dispatch.prompt.clone()).unwrap_or_default(),
            };

            let repo_refs = job
                .map(|j| j.dispatch.repos.clone())
                .unwrap_or_default();
            (session_dir, prompt_text, repo_refs)
        };

        // Ensure pool clones exist and have the needed branches, then set up session dir
        let pool_dir = {
            let dir = self.project_dir.lock().unwrap();
            dir.root().join("pool")
        };
        std::fs::create_dir_all(&pool_dir)?;

        // Find source URLs from project config
        let project_repos = {
            let dir = self.project_dir.lock().unwrap();
            dir.state().project.repos.clone()
        };

        // Build session dir repos: ensure pool has each branch, then clone from pool
        let mut session_repos: Vec<(String, PathBuf, Option<String>)> = Vec::new();
        for repo_ref in &repo_refs {
            if let Some(config) = project_repos.iter().find(|r| r.name == repo_ref.name) {
                let repo_pool = pool_dir.join(&repo_ref.name);
                git::ensure_pool_branch(&repo_pool, &config.url, repo_ref.branch.as_deref())?;
                session_repos.push((
                    repo_ref.name.clone(),
                    repo_pool,
                    repo_ref.branch.clone(),
                ));
            }
        }

        let ref_tuples: Vec<(&str, &Path, Option<&str>)> = session_repos
            .iter()
            .map(|(n, p, b)| (n.as_str(), p.as_path(), b.as_deref()))
            .collect();
        git::setup_session_dir(&session_dir, &ref_tuples)?;

        let key = SessionKey::Implementation {
            job_id: job_id.to_string(),
        };
        let session = self.ensure_session(key);
        session.enqueue(QueueItem {
            prompt: prompt::implementation_prompt(Some(&session_dir), &prompt_text),
            system_prompt: None,
            job_id: Some(job_id.to_string()),
            working_dir: session_dir,
            mcp_config: None,
        });

        // Transition to running
        {
            let mut dir = self.project_dir.lock().unwrap();
            if let Some(job) =
                dir.state_mut().jobs.iter_mut().find(|s| s.id == job_id)
            {
                job.phase = JobPhase::Running;
            }
            dir.persist()?;
        }

        Ok(())
    }

    fn enqueue_stakeholders(&mut self, job_id: &str) -> io::Result<()> {
        let (current_job, stakeholders, session_dir) = {
            let dir = self.project_dir.lock().unwrap();
            let current_job = dir.state().jobs.iter().find(|s| s.id == job_id).cloned();
            let stakeholders = dir.state().project.stakeholders.clone();
            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", job_id));
            (current_job, stakeholders, session_dir)
        };

        for stakeholder in &stakeholders {
            let prompt_text = prompt::stakeholder_prompt(
                Some(&session_dir),
                &stakeholder.persona,
                &current_job
                    .as_ref()
                    .map(|s| s.dispatch.prompt.as_str())
                    .unwrap_or(""),
                &current_job
                    .as_ref()
                    .and_then(|s| s.summary.as_deref())
                    .unwrap_or("No summary"),
            );

            let key = SessionKey::Stakeholder {
                job_id: job_id.to_string(),
                name: stakeholder.name.clone(),
            };
            let session = self.ensure_session(key);
            session.enqueue(QueueItem {
                prompt: prompt_text,
                system_prompt: None,
                job_id: Some(job_id.to_string()),
                working_dir: session_dir.clone(),
                mcp_config: None,
            });
        }

        // Transition to reviewing
        {
            let mut dir = self.project_dir.lock().unwrap();
            if let Some(job) =
                dir.state_mut().jobs.iter_mut().find(|s| s.id == job_id)
            {
                job.phase = JobPhase::Reviewing;
            }
            dir.persist()?;
        }

        Ok(())
    }

    fn enqueue_leader_decision(&mut self, job_ids: &[String]) -> io::Result<()> {
        let (state, project_md, working_dir) = {
            let dir = self.project_dir.lock().unwrap();
            let state = dir.state().clone();
            let project_md_path = dir.root().join("project.md");
            let project_md = std::fs::read_to_string(&project_md_path).ok();
            let working_dir = dir.root().to_path_buf();
            (state, project_md, working_dir)
        };

        let job_feedback: Vec<(String, Vec<(String, String)>)> = job_ids
            .iter()
            .filter_map(|id| {
                let found_job = state.jobs.iter().find(|s| &s.id == id)?;
                let feedbacks: Vec<(String, String)> = found_job
                    .feedback
                    .iter()
                    .map(|f| (f.stakeholder.clone(), f.content.clone()))
                    .collect();
                Some((id.clone(), feedbacks))
            })
            .collect();

        let prompt_text =
            prompt::leader_decision_prompt(&state, &job_feedback, project_md.as_deref());

        let mcp_config = self.mcp_config_path.clone();
        let session = self.ensure_session(SessionKey::Leader);
        session.enqueue(QueueItem {
            prompt: prompt_text,
            system_prompt: Some(prompt::leader_system_prompt()),
            job_id: None,
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

    fn fetch_job_commits(&self, job_id: &str, session_dir: &Path) {
        let dir = self.project_dir.lock().unwrap();
        let repos = &dir.state().project.repos;
        let multi_repo = repos.len() > 1;
        let pool_dir = dir.root().join("pool");
        for repo in repos {
            let clone_path = session_dir.join(&repo.name);
            if !clone_path.exists() {
                continue;
            }
            // Ensure leader's repo exists by cloning from pool
            let target_path = dir.root().join("repos").join(&repo.name);
            if !target_path.exists() {
                let pool_repo = pool_dir.join(&repo.name);
                if pool_repo.exists() {
                    let _ = git::clone_from_pool(&pool_repo, &target_path, None);
                }
            }
            if target_path.exists() {
                let branch_name = if multi_repo {
                    format!("{}/{}", job_id, repo.name)
                } else {
                    job_id.to_string()
                };
                let _ = git::fetch_branch(&target_path, &clone_path, &branch_name);
            }
        }
    }
}
