use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Idle,
    Done,
}

impl Status {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "active" => Ok(Self::Active),
            "idle" => Ok(Self::Idle),
            "done" => Ok(Self::Done),
            _ => anyhow::bail!("invalid status '{s}': must be active, idle, or done"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Done => "done",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Active => "*",
            Self::Idle => ".",
            Self::Done => "x",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: i64,
    pub name: String,
    pub parent: Option<String>,
    pub description: String,
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
    pub task: String,
    pub content: String,
    pub created_at: String,
}
