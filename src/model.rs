use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: i64,
    pub name: String,
    pub parent: Option<String>,
    pub description: String,
    pub done: bool,
    pub assignee: Option<String>,
    pub assigned_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Task {
    /// Returns display icon: x=done, *=assigned (active), .=open (unassigned)
    pub fn icon(&self) -> &'static str {
        if self.done {
            "x"
        } else if self.assignee.is_some() {
            "*"
        } else {
            "."
        }
    }

    /// Returns status string for display
    pub fn status_str(&self) -> &'static str {
        if self.done {
            "done"
        } else if self.assignee.is_some() {
            "active"
        } else {
            "open"
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
    pub task: String,
    pub content: String,
    pub created_at: String,
}
