use crate::session::{AgentSessionId, SessionKey};
use crate::job::{Artifact, Dispatch, Job, JobPhase};
use crate::util::iso_now;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
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
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    pub next_job_id: u32,
    #[serde(default)]
    pub next_artifact_id: u32,
    #[serde(default)]
    pub session_ids: Vec<(SessionKey, AgentSessionId)>,
}

#[derive(Debug)]
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
            jobs: vec![],
            artifacts: vec![],
            next_job_id: 1,
            next_artifact_id: 1,
            session_ids: Vec::new(),
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

    /// Reload state from disk (picks up changes from MCP subprocess).
    pub fn reload(&mut self) -> std::io::Result<()> {
        let data = fs::read_to_string(self.root.join("state.json"))?;
        self.state = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(())
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

    pub fn add_job(&mut self, dispatch: Dispatch) -> std::io::Result<String> {
        let id = format!("job-{:03}", self.state.next_job_id);
        self.state.next_job_id += 1;

        let job = Job {
            id: id.clone(),
            phase: JobPhase::Dispatched,
            dispatch,
            implementor: Some("agent".to_string()),
            agent_id: None,
            artifacts: vec![],
        };

        self.state.jobs.push(job);
        self.save()?;
        Ok(id)
    }

    /// Create a job that starts at `Completed` phase with an artifact already
    /// attached. Used when the leader submits work directly without dispatching
    /// an implementation agent.
    pub fn add_completed_job(&mut self, description: String) -> std::io::Result<String> {
        let id = format!("job-{:03}", self.state.next_job_id);
        self.state.next_job_id += 1;

        let job = Job {
            id: id.clone(),
            phase: JobPhase::Completed,
            dispatch: Dispatch {
                prompt: description.clone(),
                repos: vec![],
                files: vec![],
            },
            implementor: Some("leader".to_string()),
            agent_id: None,
            artifacts: vec![],
        };

        self.state.jobs.push(job);
        self.create_artifact(&id, description);
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

    /// Create an artifact for a job and link it.
    pub fn create_artifact(&mut self, job_id: &str, summary: String) -> String {
        let id = format!("art-{:03}", self.state.next_artifact_id);
        self.state.next_artifact_id += 1;

        let artifact = Artifact {
            id: id.clone(),
            job_id: job_id.to_string(),
            ts: iso_now(),
            summary,
            commits: vec![],
            feedback: vec![],
            decision: None,
        };

        self.state.artifacts.push(artifact);

        if let Some(job) = self.state.jobs.iter_mut().find(|j| j.id == job_id) {
            job.artifacts.push(id.clone());
        }

        id
    }

    /// Get the latest artifact for a job.
    pub fn latest_artifact(&self, job_id: &str) -> Option<&Artifact> {
        self.state.jobs.iter()
            .find(|j| j.id == job_id)
            .and_then(|j| j.artifacts.last())
            .and_then(|art_id| self.state.artifacts.iter().find(|a| a.id == *art_id))
    }

    /// Get a mutable reference to the latest artifact for a job.
    pub fn latest_artifact_mut(&mut self, job_id: &str) -> Option<&mut Artifact> {
        let art_id = self.state.jobs.iter()
            .find(|j| j.id == job_id)
            .and_then(|j| j.artifacts.last())
            .cloned();
        if let Some(art_id) = art_id {
            self.state.artifacts.iter_mut().find(|a| a.id == art_id)
        } else {
            None
        }
    }
}


