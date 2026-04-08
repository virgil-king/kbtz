use serde::{Deserialize, Serialize};

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
