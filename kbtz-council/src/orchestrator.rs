use crate::git;
use crate::global::GlobalState;
use crate::lifecycle::{self, Action, SessionSnapshot, JobSnapshot, WorldSnapshot};
use crate::project::ProjectDir;
use crate::prompt;
use crate::session::{ManagedSession, QueueItem, SessionKey, SessionMessage};
use crate::job::JobPhase;
use crate::stream::StreamEvent;
use crate::tui::AppState;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Per-project state: owns the project directory, sessions, MCP config,
/// and trace files for one project.
pub struct ProjectState {
    pub name: String,
    pub project_dir: Arc<Mutex<ProjectDir>>,
    pub sessions: HashMap<SessionKey, ManagedSession>,
    pub mcp_config_path: PathBuf,
    trace_dir: PathBuf,
    trace_files: HashMap<String, fs::File>,
}

impl ProjectState {
    pub fn new(
        name: String,
        project_dir: Arc<Mutex<ProjectDir>>,
        mcp_config_path: PathBuf,
    ) -> Self {
        let trace_dir = {
            let dir = project_dir.lock().unwrap();
            let td = dir.root().join("traces");
            let _ = fs::create_dir_all(&td);
            td
        };
        Self {
            name,
            project_dir,
            sessions: HashMap::new(),
            mcp_config_path,
            trace_dir,
            trace_files: HashMap::new(),
        }
    }

    /// Recover from persisted state on startup.
    pub fn recover_from_state(&mut self) {
        let mut dir = self.project_dir.lock().unwrap();

        for job in &mut dir.state_mut().jobs {
            match job.phase {
                JobPhase::Running => {
                    job.phase = JobPhase::Dispatched;
                }
                JobPhase::Reviewing => {
                    job.phase = JobPhase::Completed;
                }
                _ => {}
            }
        }

        for (key, agent_id) in &dir.state().session_ids {
            if !self.sessions.contains_key(key) {
                self.sessions.insert(
                    key.clone(),
                    ManagedSession::with_agent_session_id(key.clone(), agent_id.clone(), 1),
                );
            }
        }

        let _ = dir.persist();
    }

    fn ensure_session(&mut self, key: SessionKey) -> &mut ManagedSession {
        self.sessions
            .entry(key.clone())
            .or_insert_with(|| ManagedSession::new(key))
    }

    /// Count of currently running sessions in this project.
    pub fn running_count(&self) -> usize {
        self.sessions.values().filter(|ms| ms.is_running()).count()
    }

    fn write_trace(&mut self, session_key: &str, line: &str) {
        let trace_dir = &self.trace_dir;
        let file = self
            .trace_files
            .entry(session_key.to_string())
            .or_insert_with(|| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(trace_dir.join(format!("{}.jsonl", session_key)))
                    .expect("failed to open trace file")
            });
        let _ = writeln!(file, "{}", line);
    }

    fn fetch_job_commits(&self, job_id: &str, session_dir: &Path, pool_dir: &Path) {
        let dir = self.project_dir.lock().unwrap();
        let repos = &dir.state().project.repos;
        let multi_repo = repos.len() > 1;
        for repo in repos {
            let clone_path = session_dir.join(&repo.name);
            if !clone_path.exists() {
                continue;
            }
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

pub struct Orchestrator {
    pub projects: HashMap<String, ProjectState>,
    pub concierge: ManagedSession,
    pub concierge_mcp_config: PathBuf,
    pub global_state: Arc<Mutex<GlobalState>>,
    pub app: AppState,
    pub max_running_sessions: usize,
    pub pool_dir: PathBuf,
}

/// A resolved reference to a session — either the global concierge or a
/// project-scoped session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SelectedSession {
    Concierge,
    Project { project: String, key: SessionKey },
}

impl std::fmt::Display for SelectedSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Concierge => write!(f, "concierge"),
            Self::Project { project, key } => write!(f, "{}/{}", project, key),
        }
    }
}

impl Orchestrator {
    pub fn new(
        max_running_sessions: usize,
        pool_dir: PathBuf,
        concierge: ManagedSession,
        concierge_mcp_config: PathBuf,
        global_state: Arc<Mutex<GlobalState>>,
    ) -> Self {
        Self {
            projects: HashMap::new(),
            concierge,
            concierge_mcp_config,
            global_state,
            app: AppState::new(),
            max_running_sessions,
            pool_dir,
        }
    }

    /// Register a project. Recovers persisted state and starts its MCP server.
    pub fn add_project(&mut self, state: ProjectState) {
        let name = state.name.clone();
        self.projects.insert(name, state);
    }

    /// Total running sessions across all projects plus concierge.
    pub fn total_running(&self) -> usize {
        let project_running: usize = self.projects.values().map(|p| p.running_count()).sum();
        let concierge_running = if self.concierge.is_running() { 1 } else { 0 };
        project_running + concierge_running
    }

    /// Check whether a session is currently running.
    pub fn is_session_running(&self, session: Option<&SelectedSession>) -> bool {
        match session {
            Some(SelectedSession::Concierge) => self.concierge.is_running(),
            Some(SelectedSession::Project { project, key }) => self
                .projects
                .get(project.as_str())
                .and_then(|ps| ps.sessions.get(key))
                .map(|ms| ms.is_running())
                .unwrap_or(false),
            None => false,
        }
    }

    /// Enqueue a user message for any session (concierge or project-scoped).
    pub fn send_message(&mut self, target: &SelectedSession, message: String) {
        self.app
            .push_event(target, StreamEvent::UserMessage(message.clone()));

        match target {
            SelectedSession::Concierge => {
                let working_dir = {
                    let g = self.global_state.lock().unwrap();
                    g.root().to_path_buf()
                };
                self.concierge.enqueue(QueueItem {
                    prompt: message,
                    system_prompt: Some(prompt::concierge_system_prompt()),
                    job_id: None,
                    working_dir,
                    mcp_config: Some(self.concierge_mcp_config.clone()),
                });
                return;
            }
            SelectedSession::Project { project, key } => {
                self.send_project_message(project, key, message);
            }
        }
    }

    fn send_project_message(&mut self, project_name: &str, key: &SessionKey, message: String) {
        let ps = self.projects.get_mut(project_name)
            .unwrap_or_else(|| panic!("send_message: unknown project '{}'", project_name));

        let working_dir = {
            let dir = ps.project_dir.lock().unwrap();
            dir.root().to_path_buf()
        };
        let mcp_config = if matches!(key, SessionKey::Leader) {
            Some(ps.mcp_config_path.clone())
        } else {
            None
        };
        let system_prompt = match key {
            SessionKey::Leader => Some(prompt::leader_system_prompt()),
            _ => None,
        };
        let session = ps.ensure_session(key.clone());
        session.enqueue(QueueItem {
            prompt: message,
            system_prompt,
            job_id: None,
            working_dir,
            mcp_config,
        });
    }

    fn build_world(ps: &ProjectState) -> WorldSnapshot {
        let dir = ps.project_dir.lock().unwrap();
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

        let sessions = ps
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

        let leader_busy = ps
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

    /// Poll all sessions across all projects plus the concierge.
    pub fn poll_sessions(&mut self) {
        self.poll_concierge();
        let project_names: Vec<String> = self.projects.keys().cloned().collect();
        for name in project_names {
            self.poll_project_sessions(&name);
        }
    }

    fn poll_concierge(&mut self) {
        if let Some(ref mut active) = self.concierge.active {
            while let Ok(msg) = active.rx.try_recv() {
                match msg {
                    SessionMessage::Event(event) => {
                        self.app.push_event(&SelectedSession::Concierge, event);
                    }
                    SessionMessage::RawLine(_) => {}
                }
            }

            if !active.exited {
                if let Ok(Some(_)) = active.try_wait() {
                    active.exited = true;
                    let g = self.global_state.lock().unwrap();
                    let _ = g.save_concierge_session_id(&self.concierge.agent_session_id);
                }
            }
        }
    }

    fn poll_project_sessions(&mut self, project_name: &str) {
        let pool_dir = self.pool_dir.clone();
        let ps = match self.projects.get_mut(project_name) {
            Some(ps) => ps,
            None => return,
        };

        let mut trace_lines: Vec<(String, String)> = Vec::new();
        let mut newly_exited: Vec<(SessionKey, Option<String>)> = Vec::new();

        for (key, ms) in &mut ps.sessions {
            if let Some(ref mut active) = ms.active {
                while let Ok(msg) = active.rx.try_recv() {
                    match msg {
                        SessionMessage::Event(event) => {
                            let sel = SelectedSession::Project {
                                project: project_name.to_string(),
                                key: key.clone(),
                            };
                            self.app.push_event(&sel, event);
                        }
                        SessionMessage::RawLine(line) => {
                            trace_lines.push((key.to_string(), line));
                        }
                    }
                }

                if !active.exited {
                    if let Ok(Some(_)) = active.try_wait() {
                        active.exited = true;
                        newly_exited.push((key.clone(), active.job_id.clone()));
                    }
                }
            }
        }

        for (key, line) in trace_lines {
            ps.write_trace(&key, &line);
        }

        for (key, job_id) in &newly_exited {
            // Persist agent session ID
            {
                let ms = ps.sessions.get(key).unwrap();
                let mut dir = ps.project_dir.lock().unwrap();
                dir.state_mut()
                    .session_ids
                    .retain(|(k, _)| k != key);
                dir.state_mut()
                    .session_ids
                    .push((key.clone(), ms.agent_session_id.clone()));
                let _ = dir.persist();
            }

            if let Some(job_id) = job_id {
                let sel = SelectedSession::Project {
                    project: project_name.to_string(),
                    key: key.clone(),
                };
                match key {
                    SessionKey::Implementation { .. } => {
                        let summary = extract_summary(&self.app, &sel);
                        let session_dir = {
                            let dir = ps.project_dir.lock().unwrap();
                            dir.root()
                                .join("sessions")
                                .join(format!("{}-impl", job_id))
                        };
                        ps.fetch_job_commits(job_id, &session_dir, &pool_dir);

                        let mut dir = ps.project_dir.lock().unwrap();
                        dir.create_artifact(job_id, summary);
                        let _ = dir.persist();
                    }
                    SessionKey::Stakeholder { name, .. } => {
                        let feedback_content = extract_summary(&self.app, &sel);
                        let agent_uuid = ps.sessions.get(key)
                            .map(|ms| ms.agent_session_id.0.to_string());

                        let mut dir = ps.project_dir.lock().unwrap();
                        if let Some(artifact) = dir.latest_artifact_mut(job_id) {
                            artifact.feedback.push(crate::job::Feedback {
                                stakeholder: name.clone(),
                                content: feedback_content,
                                agent_id: agent_uuid,
                            });
                        }
                        let _ = dir.persist();
                    }
                    SessionKey::Leader | SessionKey::Concierge => {}
                }
            }
        }
    }

    /// Reap exited sessions and dispatch queued items across all projects
    /// and the concierge, respecting the global concurrency limit.
    pub fn reap_and_dispatch(&mut self) -> io::Result<()> {
        // Reap concierge
        if self.concierge.has_exited() {
            self.concierge.reap();
        }

        for ps in self.projects.values_mut() {
            for ms in ps.sessions.values_mut() {
                if ms.has_exited() {
                    ms.reap();
                }
            }
        }

        let budget = self.max_running_sessions.saturating_sub(self.total_running());
        let mut dispatched = 0usize;

        // Dispatch concierge first (user-facing, high priority)
        if dispatched < budget
            && self.concierge.active.is_none()
            && !self.concierge.queue.is_empty()
        {
            if self.concierge.try_dispatch()? {
                dispatched += 1;
            }
        }

        // Sort project names for deterministic, fair dispatch order
        let mut names: Vec<&String> = self.projects.keys().collect();
        names.sort();
        let names: Vec<String> = names.into_iter().cloned().collect();

        for name in &names {
            let ps = self.projects.get_mut(name).unwrap();
            for ms in ps.sessions.values_mut() {
                if dispatched >= budget {
                    return Ok(());
                }
                if ms.active.is_none() && !ms.queue.is_empty() {
                    if ms.try_dispatch()? {
                        dispatched += 1;
                    }
                }
            }
        }

        Ok(())
    }

    /// Process lifecycle tick for all projects.
    pub fn process_tick(&mut self) -> io::Result<()> {
        let project_names: Vec<String> = self.projects.keys().cloned().collect();
        for name in project_names {
            self.process_project_tick(&name)?;
        }
        Ok(())
    }

    fn process_project_tick(&mut self, project_name: &str) -> io::Result<()> {
        let ps = match self.projects.get(project_name) {
            Some(ps) => ps,
            None => return Ok(()),
        };
        let world = Self::build_world(ps);
        let actions = lifecycle::tick(&world);

        for action in actions {
            match action {
                Action::SpawnImplementation { job_id, .. } => {
                    self.enqueue_implementation(project_name, &job_id)?;
                }
                Action::SpawnStakeholders { job_id } => {
                    self.enqueue_stakeholders(project_name, &job_id)?;
                }
                Action::InvokeLeader { job_ids } => {
                    self.enqueue_leader_decision(project_name, &job_ids)?;
                }
                Action::TransitionJob { job_id, to } => {
                    let ps = self.projects.get_mut(project_name).unwrap();
                    let mut dir = ps.project_dir.lock().unwrap();
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

    fn enqueue_implementation(&mut self, project_name: &str, job_id: &str) -> io::Result<()> {
        let ps = self.projects.get(project_name).unwrap();
        let (session_dir, prompt_text, repo_refs) = {
            let dir = ps.project_dir.lock().unwrap();
            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", job_id));
            let job = dir.state().jobs.iter().find(|j| j.id == job_id);

            let latest_decision = job
                .and_then(|j| j.artifacts.last())
                .and_then(|art_id| dir.state().artifacts.iter().find(|a| a.id == *art_id))
                .and_then(|a| a.decision.clone());

            let prompt_text = match latest_decision {
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

        let pool_dir = self.pool_dir.clone();
        std::fs::create_dir_all(&pool_dir)?;

        let project_repos = {
            let dir = ps.project_dir.lock().unwrap();
            dir.state().project.repos.clone()
        };

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

        if !session_dir.exists() {
            let ref_tuples: Vec<(&str, &Path, Option<&str>)> = session_repos
                .iter()
                .map(|(n, p, b)| (n.as_str(), p.as_path(), b.as_deref()))
                .collect();
            git::setup_session_dir(&session_dir, &ref_tuples)?;
        }

        let ps = self.projects.get_mut(project_name).unwrap();
        let key = SessionKey::Implementation {
            job_id: job_id.to_string(),
        };
        let session = ps.ensure_session(key);
        session.enqueue(QueueItem {
            prompt: prompt::implementation_prompt(Some(&session_dir), &prompt_text),
            system_prompt: None,
            job_id: Some(job_id.to_string()),
            working_dir: session_dir,
            mcp_config: None,
        });

        {
            let mut dir = ps.project_dir.lock().unwrap();
            if let Some(job) =
                dir.state_mut().jobs.iter_mut().find(|s| s.id == job_id)
            {
                job.phase = JobPhase::Running;
            }
            dir.persist()?;
        }

        Ok(())
    }

    fn enqueue_stakeholders(&mut self, project_name: &str, job_id: &str) -> io::Result<()> {
        let ps = self.projects.get(project_name).unwrap();
        let (dispatch_prompt, artifact_summary, stakeholders, session_dir) = {
            let dir = ps.project_dir.lock().unwrap();
            let job = dir.state().jobs.iter().find(|s| s.id == job_id);
            let dispatch_prompt = job.map(|j| j.dispatch.prompt.clone()).unwrap_or_default();
            let artifact_summary = dir.latest_artifact(job_id)
                .map(|a| a.summary.clone())
                .unwrap_or_else(|| "No summary".to_string());
            let stakeholders = dir.state().project.stakeholders.clone();
            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", job_id));
            (dispatch_prompt, artifact_summary, stakeholders, session_dir)
        };

        let ps = self.projects.get_mut(project_name).unwrap();
        for stakeholder in &stakeholders {
            let prompt_text = prompt::stakeholder_prompt(
                Some(&session_dir),
                &stakeholder.persona,
                &dispatch_prompt,
                &artifact_summary,
            );

            let key = SessionKey::Stakeholder {
                job_id: job_id.to_string(),
                name: stakeholder.name.clone(),
            };
            let session = ps.ensure_session(key);
            session.enqueue(QueueItem {
                prompt: prompt_text,
                system_prompt: None,
                job_id: Some(job_id.to_string()),
                working_dir: session_dir.clone(),
                mcp_config: None,
            });
        }

        {
            let mut dir = ps.project_dir.lock().unwrap();
            if let Some(job) =
                dir.state_mut().jobs.iter_mut().find(|s| s.id == job_id)
            {
                job.phase = JobPhase::Reviewing;
            }
            dir.persist()?;
        }

        Ok(())
    }

    fn enqueue_leader_decision(&mut self, project_name: &str, job_ids: &[String]) -> io::Result<()> {
        let ps = self.projects.get(project_name).unwrap();
        let (state, project_md, working_dir) = {
            let dir = ps.project_dir.lock().unwrap();
            let state = dir.state().clone();
            let project_md_path = dir.root().join("project.md");
            let project_md = std::fs::read_to_string(&project_md_path).ok();
            let working_dir = dir.root().to_path_buf();
            (state, project_md, working_dir)
        };

        let job_feedback: Vec<(String, Vec<(String, String)>)> = job_ids
            .iter()
            .filter_map(|id| {
                let job = state.jobs.iter().find(|s| &s.id == id)?;
                let artifact = job.artifacts.last()
                    .and_then(|art_id| state.artifacts.iter().find(|a| a.id == *art_id));
                let feedbacks: Vec<(String, String)> = artifact
                    .map(|a| a.feedback.iter()
                        .map(|f| (f.stakeholder.clone(), f.content.clone()))
                        .collect())
                    .unwrap_or_default();
                Some((id.clone(), feedbacks))
            })
            .collect();

        let prompt_text =
            prompt::leader_decision_prompt(&state, &job_feedback, project_md.as_deref());

        let ps = self.projects.get_mut(project_name).unwrap();
        let mcp_config = ps.mcp_config_path.clone();
        let session = ps.ensure_session(SessionKey::Leader);
        session.enqueue(QueueItem {
            prompt: prompt_text,
            system_prompt: Some(prompt::leader_system_prompt()),
            job_id: None,
            working_dir,
            mcp_config: Some(mcp_config),
        });

        Ok(())
    }
}

fn extract_summary(app: &AppState, session: &SelectedSession) -> String {
    if let Some((_, events)) = app
        .session_events
        .iter()
        .find(|(id, _)| id == session)
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
