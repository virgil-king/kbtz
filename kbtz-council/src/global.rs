use crate::project::{Project, ProjectDir};
use crate::util::iso_now;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Paused,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub name: String,
    pub status: ProjectStatus,
    pub goal: String,
    pub created_at: String,
    /// Relative path within the global directory (e.g. "projects/foo" or "archive/foo").
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub projects: Vec<IndexEntry>,
}

pub struct GlobalState {
    root: PathBuf,
    index: Index,
}

impl GlobalState {
    /// Initialize or load the global directory at `~/.kbtz-council/`.
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root.join("projects"))?;
        fs::create_dir_all(root.join("archive"))?;
        fs::create_dir_all(root.join("pool"))?;

        let index_path = root.join("index.json");
        let index = if index_path.exists() {
            let data = fs::read_to_string(&index_path)?;
            serde_json::from_str(&data)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else {
            let idx = Index { projects: vec![] };
            let data = serde_json::to_string_pretty(&idx)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            fs::write(&index_path, data)?;
            idx
        };

        Ok(Self {
            root: root.to_path_buf(),
            index,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn pool_dir(&self) -> PathBuf {
        self.root.join("pool")
    }

    /// Create a new project and register it in the index.
    pub fn create_project(
        &mut self,
        name: &str,
        goal: &str,
        project: &Project,
    ) -> io::Result<ProjectDir> {
        if self.index.projects.iter().any(|e| e.name == name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("project '{}' already exists", name),
            ));
        }

        let rel_path = format!("projects/{}", name);
        let project_path = self.root.join(&rel_path);
        let dir = ProjectDir::init(&project_path, project)?;

        self.index.projects.push(IndexEntry {
            name: name.to_string(),
            status: ProjectStatus::Active,
            goal: goal.to_string(),
            created_at: iso_now(),
            path: rel_path,
        });
        self.save_index()?;

        Ok(dir)
    }

    /// Load an existing project by name.
    pub fn load_project(&self, name: &str) -> io::Result<ProjectDir> {
        let entry = self
            .index
            .projects
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("project '{}' not found", name),
                )
            })?;

        ProjectDir::load(&self.root.join(&entry.path))
    }

    /// List projects, optionally filtered by status.
    pub fn list_projects(&self, status: Option<ProjectStatus>) -> Vec<&IndexEntry> {
        self.index
            .projects
            .iter()
            .filter(|e| status.map_or(true, |s| e.status == s))
            .collect()
    }

    /// Change a project's status. Moves the directory between `projects/` and `archive/`.
    pub fn set_status(&mut self, name: &str, new_status: ProjectStatus) -> io::Result<()> {
        let entry = self
            .index
            .projects
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("project '{}' not found", name),
                )
            })?;

        let old_status = entry.status;
        if old_status == new_status {
            return Ok(());
        }

        let old_path = self.root.join(&entry.path);
        let new_rel = relative_path(name, new_status);
        let new_path = self.root.join(&new_rel);

        if !old_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "project directory missing from disk: {}",
                    old_path.display()
                ),
            ));
        }
        fs::create_dir_all(new_path.parent().unwrap())?;
        fs::rename(&old_path, &new_path)?;

        let entry = self
            .index
            .projects
            .iter_mut()
            .find(|e| e.name == name)
            .unwrap();
        entry.status = new_status;
        entry.path = new_rel;
        self.save_index()?;

        Ok(())
    }

    /// Resolve a project's absolute path from its index entry.
    pub fn project_path(&self, name: &str) -> io::Result<PathBuf> {
        let entry = self
            .index
            .projects
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("project '{}' not found", name),
                )
            })?;
        Ok(self.root.join(&entry.path))
    }

    fn save_index(&self) -> io::Result<()> {
        let data = serde_json::to_string_pretty(&self.index)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        fs::write(self.root.join("index.json"), data)?;
        Ok(())
    }
}

fn relative_path(name: &str, status: ProjectStatus) -> String {
    match status {
        ProjectStatus::Active | ProjectStatus::Paused => format!("projects/{}", name),
        ProjectStatus::Archived => format!("archive/{}", name),
    }
}

