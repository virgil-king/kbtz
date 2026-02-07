use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::Connection;

use crate::model::{Note, Task};
use crate::ops;

/// A flattened tree row for display.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub name: String,
    pub done: bool,
    pub has_assignee: bool,
    pub description: String,
    pub depth: usize,
    pub has_children: bool,
    pub is_last_at_depth: Vec<bool>,
    pub blocked_by: Vec<String>,
}

impl TreeRow {
    /// Returns display icon: x=done, *=assigned (active), .=open (unassigned)
    pub fn icon(&self) -> &'static str {
        if self.done {
            "x"
        } else if self.has_assignee {
            "*"
        } else {
            "."
        }
    }
}

pub struct App {
    pub rows: Vec<TreeRow>,
    pub cursor: usize,
    pub collapsed: HashSet<String>,
    pub show_notes: bool,
    pub notes: Vec<Note>,
}

impl App {
    pub fn new(conn: &Connection, root: Option<&str>) -> Result<Self> {
        let mut app = App {
            rows: Vec::new(),
            cursor: 0,
            collapsed: HashSet::new(),
            show_notes: false,
            notes: Vec::new(),
        };
        app.refresh(conn, root)?;
        Ok(app)
    }

    pub fn refresh(&mut self, conn: &Connection, root: Option<&str>) -> Result<()> {
        let tasks = ops::list_tasks(conn, None, false, root)?;
        self.rows = flatten_tree(&tasks, &self.collapsed, conn)?;
        // Clamp cursor
        if !self.rows.is_empty() {
            if self.cursor >= self.rows.len() {
                self.cursor = self.rows.len() - 1;
            }
        } else {
            self.cursor = 0;
        }
        // Refresh notes if panel is open
        if self.show_notes {
            self.load_notes(conn)?;
        }
        Ok(())
    }

    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if !self.rows.is_empty() && self.cursor < self.rows.len() - 1 {
            self.cursor += 1;
        }
    }

    pub fn toggle_collapse(&mut self) {
        if let Some(row) = self.rows.get(self.cursor) {
            if row.has_children {
                let name = row.name.clone();
                if !self.collapsed.remove(&name) {
                    self.collapsed.insert(name);
                }
            }
        }
    }

    pub fn toggle_notes(&mut self) {
        self.show_notes = !self.show_notes;
    }

    pub fn load_notes(&mut self, conn: &Connection) -> Result<()> {
        if let Some(row) = self.rows.get(self.cursor) {
            self.notes = ops::list_notes(conn, &row.name)?;
        } else {
            self.notes.clear();
        }
        Ok(())
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.rows.get(self.cursor).map(|r| r.name.as_str())
    }
}

fn flatten_tree(
    tasks: &[Task],
    collapsed: &HashSet<String>,
    conn: &Connection,
) -> Result<Vec<TreeRow>> {
    let task_names: HashSet<&str> = tasks.iter().map(|t| t.name.as_str()).collect();

    // Build parent -> children map
    let mut children_map: HashMap<Option<&str>, Vec<&Task>> = HashMap::new();
    for task in tasks {
        let parent_key = match task.parent.as_deref() {
            Some(p) if task_names.contains(p) => Some(p),
            _ => None,
        };
        children_map.entry(parent_key).or_default().push(task);
    }

    let mut rows = Vec::new();
    let roots = children_map.get(&None).cloned().unwrap_or_default();

    for (i, root) in roots.iter().enumerate() {
        let is_last = i == roots.len() - 1;
        flatten_node(
            &mut rows,
            root,
            &children_map,
            collapsed,
            conn,
            0,
            &mut vec![is_last],
        )?;
    }

    Ok(rows)
}

fn flatten_node(
    rows: &mut Vec<TreeRow>,
    task: &Task,
    children_map: &HashMap<Option<&str>, Vec<&Task>>,
    collapsed: &HashSet<String>,
    conn: &Connection,
    depth: usize,
    is_last_at_depth: &mut Vec<bool>,
) -> Result<()> {
    let children = children_map
        .get(&Some(task.name.as_str()))
        .cloned()
        .unwrap_or_default();
    let has_children = !children.is_empty();
    let blocked_by = ops::get_blockers(conn, &task.name)?;

    rows.push(TreeRow {
        name: task.name.clone(),
        done: task.done,
        has_assignee: task.assignee.is_some(),
        description: task.description.clone(),
        depth,
        has_children,
        is_last_at_depth: is_last_at_depth.clone(),
        blocked_by,
    });

    if has_children && !collapsed.contains(&task.name) {
        for (i, child) in children.iter().enumerate() {
            let child_is_last = i == children.len() - 1;
            is_last_at_depth.push(child_is_last);
            flatten_node(rows, child, children_map, collapsed, conn, depth + 1, is_last_at_depth)?;
            is_last_at_depth.pop();
        }
    }

    Ok(())
}
