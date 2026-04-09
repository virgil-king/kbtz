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

/// Stakeholder feedback on an artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feedback {
    pub stakeholder: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Leader's decision on an artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Merge,
    Rework { feedback: String },
    Abandon,
}

/// An immutable snapshot of one revision's review round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub job_id: String,
    pub ts: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    #[serde(default)]
    pub feedback: Vec<Feedback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
}

/// A job is the durable identity across revisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub phase: JobPhase,
    pub dispatch: Dispatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub artifacts: Vec<String>,
}
