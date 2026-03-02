use anyhow::Result;
use rusqlite::Connection;

use crate::model::Note;
use crate::ops;
use crate::ui::{self, ActiveTaskPolicy, TreeView};
use crate::validate::validate_name;

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
    pub tree: TreeView,
    pub show_notes: bool,
    pub notes: Vec<Note>,
    pub add_form: Option<AddForm>,
}

impl App {
    pub fn new(conn: &Connection, root: Option<&str>) -> Result<Self> {
        let mut app = App {
            tree: TreeView::new(ActiveTaskPolicy::Refuse),
            show_notes: false,
            notes: Vec::new(),
            add_form: None,
        };
        app.refresh(conn, root)?;
        Ok(app)
    }

    pub fn refresh(&mut self, conn: &Connection, root: Option<&str>) -> Result<()> {
        let mut tasks = ops::list_tasks(conn, None, true, root, None, None)?;
        tasks.retain(|t| t.status != "done");
        self.tree.rows = ui::flatten_tree(&tasks, &self.tree.collapsed, conn)?;
        self.tree.clamp_cursor();
        // Refresh notes if panel is open
        if self.show_notes {
            self.load_notes(conn)?;
        }
        Ok(())
    }

    pub fn toggle_notes(&mut self) {
        self.show_notes = !self.show_notes;
    }

    pub fn load_notes(&mut self, conn: &Connection) -> Result<()> {
        if let Some(name) = self.tree.selected_name() {
            self.notes = ops::list_notes(conn, name)?;
        } else {
            self.notes.clear();
        }
        Ok(())
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.tree.selected_name()
    }

    pub fn enter_add_mode(&mut self, with_parent: bool) {
        let parent = if with_parent {
            self.tree.selected_name().map(|s| s.to_string())
        } else {
            None
        };
        self.add_form = Some(AddForm::new(parent));
    }

    pub fn cancel_add_mode(&mut self) {
        self.add_form = None;
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
            false,
        ) {
            Ok(()) => {
                self.add_form = None;
                self.refresh(conn, root)?;
            }
            Err(e) => {
                self.add_form.as_mut().unwrap().error = Some(e.to_string());
            }
        }
        Ok(())
    }
}
