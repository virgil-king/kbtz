use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: i64,
    pub name: String,
    pub parent: Option<String>,
    pub description: String,
    pub status: String,
    pub assignee: Option<String>,
    pub status_changed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Task {
    /// Returns display icon: x=done, *=active, ~=paused, .=open
    pub fn icon(&self) -> &'static str {
        match self.status.as_str() {
            "done" => "x",
            "active" => "*",
            "paused" => "~",
            _ => ".",
        }
    }

    /// Returns status string for display
    pub fn status_str(&self) -> &str {
        &self.status
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
    pub task: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    #[serde(flatten)]
    pub task: Task,
    pub matched_in: Vec<String>,
}
