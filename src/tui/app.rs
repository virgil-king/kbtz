use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::Connection;

use crate::model::{Note, Task};
use crate::ops;
use crate::validate::validate_name;

/// A flattened tree row for display.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub name: String,
    pub status: String,
    pub description: String,
    pub depth: usize,
    pub has_children: bool,
    pub is_last_at_depth: Vec<bool>,
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    AddTask,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddField {
    Name,
    Description,
    Note,
}

pub struct AddForm {
    pub name: String,
    pub description: String,
    pub note: String,
    pub parent: Option<String>,
    pub focused: AddField,
    pub error: Option<String>,
}

impl AddForm {
    pub fn new(parent: Option<String>) -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            note: String::new(),
            parent,
            focused: AddField::Name,
            error: None,
        }
    }

    pub fn focused_buf_mut(&mut self) -> &mut String {
        match self.focused {
            AddField::Name => &mut self.name,
            AddField::Description => &mut self.description,
            AddField::Note => &mut self.note,
        }
    }

    pub fn validate(&mut self) -> bool {
        if self.name.is_empty() {
            self.error = Some("Name must not be empty".into());
            return false;
        }
        if let Err(e) = validate_name(&self.name) {
            self.error = Some(e.to_string());
            return false;
        }
        self.error = None;
        true
    }

    pub fn next_field(&mut self) {
        self.focused = match self.focused {
            AddField::Name => AddField::Description,
            AddField::Description => AddField::Note,
            AddField::Note => AddField::Name,
        };
    }

    pub fn prev_field(&mut self) {
        self.focused = match self.focused {
            AddField::Name => AddField::Note,
            AddField::Description => AddField::Name,
            AddField::Note => AddField::Description,
        };
    }
}

pub struct App {
    pub rows: Vec<TreeRow>,
    pub cursor: usize,
    pub collapsed: HashSet<String>,
    pub show_notes: bool,
    pub notes: Vec<Note>,
    pub mode: Mode,
    pub add_form: Option<AddForm>,
    pub error: Option<String>,
}

impl App {
    pub fn new(conn: &Connection, root: Option<&str>) -> Result<Self> {
        let mut app = App {
            rows: Vec::new(),
            cursor: 0,
            collapsed: HashSet::new(),
            show_notes: false,
            notes: Vec::new(),
            mode: Mode::Normal,
            add_form: None,
            error: None,
        };
        app.refresh(conn, root)?;
        Ok(app)
    }

    pub fn refresh(&mut self, conn: &Connection, root: Option<&str>) -> Result<()> {
        let mut tasks = ops::list_tasks(conn, None, true, root)?;
        tasks.retain(|t| t.status != "done");
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

    pub fn enter_add_mode(&mut self, with_parent: bool) {
        let parent = if with_parent {
            self.selected_name().map(|s| s.to_string())
        } else {
            None
        };
        self.add_form = Some(AddForm::new(parent));
        self.mode = Mode::AddTask;
    }

    pub fn cancel_add_mode(&mut self) {
        self.add_form = None;
        self.mode = Mode::Normal;
    }

    pub fn submit_add(&mut self, conn: &Connection, root: Option<&str>) -> Result<()> {
        let form = self.add_form.as_mut().unwrap();
        if !form.validate() {
            return Ok(());
        }
        let name = form.name.clone();
        let description = form.description.clone();
        let note = if form.note.is_empty() {
            None
        } else {
            Some(form.note.clone())
        };
        let parent = form.parent.clone();
        match ops::add_task(
            conn,
            &name,
            parent.as_deref(),
            &description,
            note.as_deref(),
            None,
        ) {
            Ok(()) => {
                self.add_form = None;
                self.mode = Mode::Normal;
                self.refresh(conn, root)?;
            }
            Err(e) => {
                self.add_form.as_mut().unwrap().error = Some(e.to_string());
            }
        }
        Ok(())
    }

    pub fn toggle_help(&mut self) {
        self.mode = match self.mode {
            Mode::Help => Mode::Normal,
            _ => Mode::Help,
        };
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
        status: task.status.clone(),
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
