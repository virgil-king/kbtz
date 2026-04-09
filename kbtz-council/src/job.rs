use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    Dispatched,
    Running,
    Completed,
    Reviewing,
    Reviewed,
    Merged,
    Rework,
}

/// A reference to a repo + optional branch for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dispatch {
    pub prompt: String,
    pub repos: Vec<RepoRef>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feedback {
    pub stakeholder: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Merge,
    Rework { feedback: String },
    Abandon,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub phase: JobPhase,
    pub dispatch: Dispatch,
    pub summary: Option<String>,
    pub feedback: Vec<Feedback>,
    pub decision: Option<Decision>,
}
