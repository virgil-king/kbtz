use crate::git;
use crate::lifecycle::{self, Action, SessionSnapshot, StepSnapshot, WorldSnapshot};
use crate::mcp::LeaderRequest;
use crate::project::ProjectDir;
use crate::prompt;
use crate::session::{HeadlessSession, SessionMessage, SessionRole};
use crate::step::{Dispatch, StepPhase};
use crate::stream::StreamEvent;
use crate::tui::AppState;
use serde_json::Value;
use std::io;
use std::path::Path;
use std::sync::mpsc;

pub struct Orchestrator {
    pub project_dir: ProjectDir,
    pub sessions: Vec<HeadlessSession>,
    pub app: AppState,
    pub leader_busy: bool,
    mcp_rx: mpsc::Receiver<LeaderRequest>,
    mcp_resp_tx: mpsc::Sender<Value>,
}

impl Orchestrator {
    pub fn new(
        project_dir: ProjectDir,
        mcp_rx: mpsc::Receiver<LeaderRequest>,
        mcp_resp_tx: mpsc::Sender<Value>,
    ) -> Self {
        Self {
            project_dir,
            sessions: vec![],
            app: AppState::new(),
            leader_busy: false,
            mcp_rx,
            mcp_resp_tx,
        }
    }

    /// Build a world snapshot from current state for the lifecycle tick function.
    fn build_world(&self) -> WorldSnapshot {
        let steps = self
            .project_dir
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
                exited: s.rx.try_recv().is_err(), // rough check
            })
            .collect();

        WorldSnapshot {
            steps,
            sessions,
            leader_busy: self.leader_busy,
        }
    }

    /// Poll all sessions for new events and detect exits.
    pub fn poll_sessions(&mut self) {
        let mut exited_indices = vec![];

        for (i, session) in self.sessions.iter_mut().enumerate() {
            // Drain events
            while let Ok(msg) = session.rx.try_recv() {
                match msg {
                    SessionMessage::Event(event) => {
                        self.app.push_event(&session.id.0, event);
                    }
                    SessionMessage::RawLine(_) => {}
                    SessionMessage::Exited { .. } => {}
                }
            }

            // Check if process exited
            if let Ok(Some(_code)) = session.try_wait() {
                exited_indices.push(i);
            }
        }

        // Process exited sessions
        for &i in exited_indices.iter().rev() {
            let session = &self.sessions[i];
            let step_id = session.step_id.clone();
            let role = session.role.clone();

            match role {
                SessionRole::Implementation => {
                    // Extract summary from events
                    let summary = self.extract_summary(&session.id.0);

                    // Fetch commits into leader's repos
                    let session_dir = self
                        .project_dir
                        .root()
                        .join("sessions")
                        .join(format!("{}-impl", step_id));
                    self.fetch_step_commits(&step_id, &session_dir);

                    // Update step
                    if let Some(step) = self
                        .project_dir
                        .state_mut()
                        .steps
                        .iter_mut()
                        .find(|s| s.id == step_id)
                    {
                        step.summary = Some(summary);
                        step.phase = StepPhase::Completed;
                    }
                    let _ = self.project_dir.persist();
                }
                SessionRole::Stakeholder { ref name } => {
                    let feedback = self.extract_summary(&session.id.0);
                    let expected = self.project_dir.state().project.stakeholders.len();
                    if let Some(step) = self
                        .project_dir
                        .state_mut()
                        .steps
                        .iter_mut()
                        .find(|s| s.id == step_id)
                    {
                        step.feedback.push(crate::step::Feedback {
                            stakeholder: name.clone(),
                            content: feedback,
                        });

                        if step.feedback.len() >= expected {
                            step.phase = StepPhase::Reviewed;
                        }
                    }
                    let _ = self.project_dir.persist();
                }
                SessionRole::LeaderDecision => {
                    self.leader_busy = false;
                }
            }

            self.sessions.remove(i);
        }
    }

    /// Process lifecycle tick — execute actions from the state machine.
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
                    if let Some(step) = self
                        .project_dir
                        .state_mut()
                        .steps
                        .iter_mut()
                        .find(|s| s.id == step_id)
                    {
                        step.phase = to;
                    }
                    self.project_dir.persist()?;
                }
            }
        }

        Ok(())
    }

    /// Handle MCP requests from the leader.
    pub fn handle_mcp_requests(&mut self) -> io::Result<()> {
        while let Ok(request) = self.mcp_rx.try_recv() {
            match request {
                LeaderRequest::DefineProject {
                    id: _,
                    repos,
                    stakeholders,
                    goal_summary,
                } => {
                    // Clone repos into project/repos/
                    for repo in &repos {
                        let dest = self.project_dir.root().join("repos").join(&repo.name);
                        if !dest.exists() {
                            git::shallow_clone(Path::new(&repo.url), &dest)?;
                        }
                    }

                    let state = self.project_dir.state_mut();
                    state.project.repos = repos
                        .iter()
                        .map(|r| crate::project::RepoConfig {
                            name: r.name.clone(),
                            url: r.url.clone(),
                        })
                        .collect();
                    state.project.stakeholders = stakeholders
                        .iter()
                        .map(|s| crate::project::Stakeholder {
                            name: s.name.clone(),
                            persona: s.persona.clone(),
                        })
                        .collect();
                    state.project.goal_summary = goal_summary;
                    self.project_dir.persist()?;

                    let _ = self.mcp_resp_tx.send(serde_json::json!({
                        "content": [{"type": "text", "text": "Project defined successfully."}]
                    }));
                }
                LeaderRequest::DispatchStep {
                    id: _,
                    prompt,
                    repos,
                    files,
                } => {
                    let step_id = self.project_dir.add_step(Dispatch {
                        prompt,
                        repos,
                        files,
                    })?;
                    let _ = self.mcp_resp_tx.send(serde_json::json!({
                        "content": [{"type": "text", "text": format!("Step {} dispatched.", step_id)}]
                    }));
                }
                LeaderRequest::ReworkStep {
                    id: _,
                    step_id,
                    feedback,
                } => {
                    if let Some(step) = self
                        .project_dir
                        .state_mut()
                        .steps
                        .iter_mut()
                        .find(|s| s.id == step_id)
                    {
                        step.phase = StepPhase::Rework;
                    }
                    self.project_dir.persist()?;

                    // Resume implementation session with feedback
                    let session_dir = self
                        .project_dir
                        .root()
                        .join("sessions")
                        .join(format!("{}-impl", step_id));
                    if session_dir.exists() {
                        let session = HeadlessSession::spawn(
                            &step_id,
                            SessionRole::Implementation,
                            &feedback,
                            &session_dir,
                            None, // TODO: resume with stored session ID
                        )?;
                        self.sessions.push(session);
                    }

                    let _ = self.mcp_resp_tx.send(serde_json::json!({
                        "content": [{"type": "text", "text": format!("Step {} sent back for rework.", step_id)}]
                    }));
                }
                LeaderRequest::CloseStep { id: _, step_id } => {
                    if let Some(step) = self
                        .project_dir
                        .state_mut()
                        .steps
                        .iter_mut()
                        .find(|s| s.id == step_id)
                    {
                        step.phase = StepPhase::Merged;
                    }
                    self.project_dir.persist()?;

                    // Clean up session directory
                    let session_dir = self
                        .project_dir
                        .root()
                        .join("sessions")
                        .join(format!("{}-impl", step_id));
                    if session_dir.exists() {
                        let _ = git::cleanup_session_dir(&session_dir);
                    }

                    let _ = self.mcp_resp_tx.send(serde_json::json!({
                        "content": [{"type": "text", "text": format!("Step {} closed.", step_id)}]
                    }));
                }
            }
        }

        Ok(())
    }

    fn spawn_implementation(&mut self, step_id: &str, repos: &[String]) -> io::Result<()> {
        let session_dir = self
            .project_dir
            .root()
            .join("sessions")
            .join(format!("{}-impl", step_id));

        let owned_pairs: Vec<(String, std::path::PathBuf)> = repos
            .iter()
            .filter_map(|name| {
                let source = self.project_dir.root().join("repos").join(name);
                if source.exists() {
                    Some((name.clone(), source))
                } else {
                    None
                }
            })
            .collect();

        let ref_pairs: Vec<(&str, &Path)> = owned_pairs
            .iter()
            .map(|(n, p)| (n.as_str(), p.as_path()))
            .collect();

        git::setup_session_dir(&session_dir, &ref_pairs)?;

        // Get step prompt
        let step_prompt = self
            .project_dir
            .state()
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .map(|s| s.dispatch.prompt.clone())
            .unwrap_or_default();

        let prompt_text = prompt::implementation_prompt(&step_prompt);
        let session = HeadlessSession::spawn(
            step_id,
            SessionRole::Implementation,
            &prompt_text,
            &session_dir,
            None,
        )?;

        // Transition to running
        if let Some(step) = self
            .project_dir
            .state_mut()
            .steps
            .iter_mut()
            .find(|s| s.id == step_id)
        {
            step.phase = StepPhase::Running;
        }
        self.project_dir.persist()?;

        self.sessions.push(session);
        Ok(())
    }

    fn spawn_stakeholders(&mut self, step_id: &str) -> io::Result<()> {
        let step = self
            .project_dir
            .state()
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .cloned();

        let stakeholders = self.project_dir.state().project.stakeholders.clone();
        let session_dir = self
            .project_dir
            .root()
            .join("sessions")
            .join(format!("{}-impl", step_id));

        for stakeholder in &stakeholders {
            let prompt_text = prompt::stakeholder_prompt(
                &stakeholder.persona,
                &step.as_ref().map(|s| s.dispatch.prompt.as_str()).unwrap_or(""),
                &step.as_ref().and_then(|s| s.summary.as_deref()).unwrap_or("No summary"),
            );

            let session = HeadlessSession::spawn(
                step_id,
                SessionRole::Stakeholder {
                    name: stakeholder.name.clone(),
                },
                &prompt_text,
                &session_dir,
                None,
            )?;
            self.sessions.push(session);
        }

        // Transition to reviewing
        if let Some(step) = self
            .project_dir
            .state_mut()
            .steps
            .iter_mut()
            .find(|s| s.id == step_id)
        {
            step.phase = StepPhase::Reviewing;
        }
        self.project_dir.persist()?;

        Ok(())
    }

    fn invoke_leader(&mut self, step_ids: &[String]) -> io::Result<()> {
        let state = self.project_dir.state().clone();
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

        let prompt_text = prompt::leader_decision_prompt(&state, &step_feedback);
        let working_dir = self.project_dir.root().to_path_buf();

        let session = HeadlessSession::spawn(
            "leader",
            SessionRole::LeaderDecision,
            &prompt_text,
            &working_dir,
            None, // TODO: resume with stored leader session ID
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
            // Look for Result event first
            for event in events.iter().rev() {
                if let StreamEvent::Result { result } = event {
                    return result.clone();
                }
            }
            // Fall back to last AssistantText
            for event in events.iter().rev() {
                if let StreamEvent::AssistantText(text) = event {
                    return text.clone();
                }
            }
        }
        "No summary available".to_string()
    }

    fn fetch_step_commits(&self, step_id: &str, session_dir: &Path) {
        for repo in &self.project_dir.state().project.repos {
            let clone_path = session_dir.join(&repo.name);
            let target_path = self.project_dir.root().join("repos").join(&repo.name);
            if clone_path.exists() && target_path.exists() {
                let _ = git::fetch_branch(&target_path, &clone_path, step_id);
            }
        }
    }
}
