use crate::session::{AgentSessionId, SessionKey};
use crate::step::{Dispatch, Step, StepPhase};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stakeholder {
    pub name: String,
    pub persona: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub repos: Vec<RepoConfig>,
    pub stakeholders: Vec<Stakeholder>,
    pub goal_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorState {
    pub project: Project,
    pub steps: Vec<Step>,
    pub next_step_id: u32,
    #[serde(default)]
    pub session_ids: HashMap<SessionKey, AgentSessionId>,
}

pub struct ProjectDir {
    root: PathBuf,
    state: OrchestratorState,
}

impl ProjectDir {
    pub fn init(root: &Path, project: &Project) -> std::io::Result<Self> {
        fs::create_dir_all(root.join("repos"))?;
        fs::create_dir_all(root.join("steps"))?;
        fs::create_dir_all(root.join("sessions"))?;
        fs::create_dir_all(root.join("claude-sessions"))?;

        let state = OrchestratorState {
            project: project.clone(),
            steps: vec![],
            next_step_id: 1,
            session_ids: HashMap::new(),
        };

        let dir = Self {
            root: root.to_path_buf(),
            state,
        };
        dir.save()?;
        Ok(dir)
    }

    pub fn load(root: &Path) -> std::io::Result<Self> {
        let data = fs::read_to_string(root.join("state.json"))?;
        let state: OrchestratorState = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self {
            root: root.to_path_buf(),
            state,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state(&self) -> &OrchestratorState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut OrchestratorState {
        &mut self.state
    }

    pub fn add_step(&mut self, dispatch: Dispatch) -> std::io::Result<String> {
        let id = format!("step-{:03}", self.state.next_step_id);
        self.state.next_step_id += 1;

        let step = Step {
            id: id.clone(),
            phase: StepPhase::Dispatched,
            dispatch,
            summary: None,
            feedback: vec![],
            decision: None,
        };

        let step_dir = self.root.join("steps").join(&id);
        fs::create_dir_all(step_dir.join("feedback"))?;

        let dispatch_json = serde_json::to_string_pretty(&step.dispatch)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write(step_dir.join("dispatch.json"), dispatch_json)?;

        self.state.steps.push(step);
        self.save()?;
        Ok(id)
    }

    fn save(&self) -> std::io::Result<()> {
        let data = serde_json::to_string_pretty(&self.state)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write(self.root.join("state.json"), data)?;
        Ok(())
    }

    pub fn persist(&self) -> std::io::Result<()> {
        self.save()
    }
}
