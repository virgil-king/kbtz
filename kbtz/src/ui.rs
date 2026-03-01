//! Shared UI primitives for kbtz tree rendering.
//!
//! Used by both `kbtz watch` (the CLI TUI) and `kbtz-workspace`.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::{Clear, ListItem, ListState, Paragraph};
use rusqlite::Connection;

use crate::model::Task;
use crate::ops;

/// A flattened tree row for display.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub name: String,
    pub status: String,
    pub description: String,
    pub assignee: Option<String>,
    pub depth: usize,
    pub has_children: bool,
    pub is_last_at_depth: Vec<bool>,
    pub blocked_by: Vec<String>,
}

/// Flatten a list of tasks into a displayable tree.
///
/// Tasks whose parents aren't in the list are promoted to root level.
/// Collapsed nodes have their children hidden.
pub fn flatten_tree(
    tasks: &[Task],
    collapsed: &HashSet<String>,
    conn: &Connection,
) -> Result<Vec<TreeRow>> {
    let task_names: HashSet<&str> = tasks.iter().map(|t| t.name.as_str()).collect();

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
        assignee: task.assignee.clone(),
        depth,
        has_children,
        is_last_at_depth: is_last_at_depth.clone(),
        blocked_by,
    });

    if has_children && !collapsed.contains(&task.name) {
        for (i, child) in children.iter().enumerate() {
            let child_is_last = i == children.len() - 1;
            is_last_at_depth.push(child_is_last);
            flatten_node(
                rows,
                child,
                children_map,
                collapsed,
                conn,
                depth + 1,
                is_last_at_depth,
            )?;
            is_last_at_depth.pop();
        }
    }

    Ok(())
}

// ── Rendering helpers ──────────────────────────────────────────────────

/// Style for a task status.
pub fn status_style(status: &str) -> Style {
    match status {
        "done" => Style::default().dim(),
        "active" => Style::default().fg(Color::Green),
        "paused" => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::Yellow),
    }
}

/// Icon for a raw status string.
pub fn icon_for_status(status: &str) -> &'static str {
    match status {
        "done" => "\u{2705} ",           // ✅
        "active" => "\u{1f7e2} ",        // 🟢
        "paused" => "\u{23f8}\u{fe0f} ", // ⏸️
        "blocked" => "\u{1f6a7} ",       // 🚧
        _ => "\u{26aa} ",                // ⚪
    }
}

/// Icon for a task, considering blocking relationships.
pub fn icon_for_task(row: &TreeRow) -> &'static str {
    if !row.blocked_by.is_empty() {
        icon_for_status("blocked")
    } else {
        icon_for_status(&row.status)
    }
}

/// Build the tree connector prefix string for a row.
pub fn tree_prefix(row: &TreeRow) -> String {
    let mut prefix = String::new();
    for d in 1..row.depth + 1 {
        if d == row.depth {
            if row.is_last_at_depth[d] {
                prefix.push_str("└── ");
            } else {
                prefix.push_str("├── ");
            }
        } else if row.is_last_at_depth[d] {
            prefix.push_str("    ");
        } else {
            prefix.push_str("│   ");
        }
    }
    prefix
}

/// Center a rectangle within an area.
pub fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

/// What to do when the user tries to act on an active (claimed) task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTaskPolicy {
    /// Refuse the action with an error message (standalone mode).
    Refuse,
    /// Show a confirmation dialog (session-managed mode).
    Confirm,
}

/// Modal state for the tree view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeMode {
    Normal,
    Help,
    ConfirmDone(String),
    ConfirmPause(String),
}

/// Action returned by `TreeView::handle_key()`.
pub enum TreeKeyAction {
    /// Quit the application.
    Quit,
    /// Tree structure changed (collapse toggled), caller should refresh from DB.
    Refresh,
    /// Pause this task.
    Pause(String),
    /// Unpause this task.
    Unpause(String),
    /// Mark this task done.
    MarkDone(String),
    /// Force-unassign this task.
    ForceUnassign(String),
    /// Key was not handled; caller should check app-specific bindings.
    Unhandled,
    /// Handled, no further action needed.
    Continue,
}

/// Shared tree view state used by both `kbtz watch` and `kbtz-workspace`.
pub struct TreeView {
    pub rows: Vec<TreeRow>,
    pub cursor: usize,
    pub list_state: ListState,
    pub collapsed: HashSet<String>,
    pub error: Option<String>,
    pub mode: TreeMode,
    pub active_policy: ActiveTaskPolicy,
}

impl TreeView {
    pub fn new(active_policy: ActiveTaskPolicy) -> Self {
        Self {
            rows: Vec::new(),
            cursor: 0,
            list_state: ListState::default(),
            collapsed: HashSet::new(),
            error: None,
            mode: TreeMode::Normal,
            active_policy,
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.list_state.select(Some(self.cursor));
        }
    }

    pub fn move_down(&mut self) {
        if !self.rows.is_empty() && self.cursor < self.rows.len() - 1 {
            self.cursor += 1;
            self.list_state.select(Some(self.cursor));
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

    pub fn selected_name(&self) -> Option<&str> {
        self.rows.get(self.cursor).map(|r| r.name.as_str())
    }

    /// Clamp cursor after rows change (e.g. after refresh from DB).
    pub fn clamp_cursor(&mut self) {
        if self.rows.is_empty() {
            self.cursor = 0;
            self.list_state.select(None);
        } else {
            if self.cursor >= self.rows.len() {
                self.cursor = self.rows.len() - 1;
            }
            self.list_state.select(Some(self.cursor));
        }
    }

    /// Handle a key press. Returns an action for the caller.
    ///
    /// Handles shared keys (navigation, collapse, pause, done, unassign,
    /// help, quit) and confirm/help mode dismissal. Returns `Unhandled`
    /// for keys the caller should process (app-specific bindings).
    pub fn handle_key(&mut self, key: KeyEvent) -> TreeKeyAction {
        match &self.mode {
            TreeMode::Help => {
                match key.code {
                    KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                        self.mode = TreeMode::Normal;
                    }
                    _ => {}
                }
                TreeKeyAction::Continue
            }
            TreeMode::ConfirmDone(name) => {
                let name = name.clone();
                self.mode = TreeMode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                    TreeKeyAction::MarkDone(name)
                } else {
                    TreeKeyAction::Continue
                }
            }
            TreeMode::ConfirmPause(name) => {
                let name = name.clone();
                self.mode = TreeMode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                    TreeKeyAction::Pause(name)
                } else {
                    TreeKeyAction::Continue
                }
            }
            TreeMode::Normal => {
                self.error = None;
                match key.code {
                    KeyCode::Char('q') => TreeKeyAction::Quit,
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.move_down();
                        TreeKeyAction::Continue
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.move_up();
                        TreeKeyAction::Continue
                    }
                    KeyCode::Char(' ') => {
                        self.toggle_collapse();
                        TreeKeyAction::Refresh
                    }
                    KeyCode::Char('p') => self.handle_pause(),
                    KeyCode::Char('d') => self.handle_done(),
                    KeyCode::Char('U') => {
                        if let Some(name) = self.selected_name() {
                            TreeKeyAction::ForceUnassign(name.to_string())
                        } else {
                            TreeKeyAction::Continue
                        }
                    }
                    KeyCode::Char('?') => {
                        self.mode = TreeMode::Help;
                        TreeKeyAction::Continue
                    }
                    _ => TreeKeyAction::Unhandled,
                }
            }
        }
    }

    fn handle_pause(&mut self) -> TreeKeyAction {
        let Some(row) = self.rows.get(self.cursor) else {
            return TreeKeyAction::Continue;
        };
        let name = row.name.clone();
        match row.status.as_str() {
            "paused" => TreeKeyAction::Unpause(name),
            "open" => TreeKeyAction::Pause(name),
            "active" => match self.active_policy {
                ActiveTaskPolicy::Confirm => {
                    self.mode = TreeMode::ConfirmPause(name);
                    TreeKeyAction::Continue
                }
                ActiveTaskPolicy::Refuse => {
                    self.error = Some("cannot pause active task".into());
                    TreeKeyAction::Continue
                }
            },
            status => {
                self.error = Some(format!("cannot pause {status} task"));
                TreeKeyAction::Continue
            }
        }
    }

    fn handle_done(&mut self) -> TreeKeyAction {
        let Some(row) = self.rows.get(self.cursor) else {
            return TreeKeyAction::Continue;
        };
        let name = row.name.clone();
        match row.status.as_str() {
            "done" => {
                self.error = Some("task is already done".into());
                TreeKeyAction::Continue
            }
            "active" => match self.active_policy {
                ActiveTaskPolicy::Confirm => {
                    self.mode = TreeMode::ConfirmDone(name);
                    TreeKeyAction::Continue
                }
                ActiveTaskPolicy::Refuse => {
                    self.error = Some("cannot close active task".into());
                    TreeKeyAction::Continue
                }
            },
            _ => TreeKeyAction::MarkDone(name),
        }
    }
}

/// Per-row customization for tree item rendering.
#[derive(Default)]
pub struct RowDecoration {
    /// If set, replaces the default status icon and style.
    pub icon_override: Option<(String, Style)>,
    /// Extra spans inserted after the task name.
    pub after_name: Vec<Span<'static>>,
}

/// Build ListItems for all tree rows.
///
/// The `decorate` closure is called for each row to provide optional
/// per-row customization (e.g. session indicators in kbtz-workspace).
pub fn build_tree_items<F>(
    rows: &[TreeRow],
    collapsed: &HashSet<String>,
    decorate: F,
) -> Vec<ListItem<'static>>
where
    F: Fn(&TreeRow) -> RowDecoration,
{
    rows.iter()
        .map(|row| {
            let decoration = decorate(row);
            let prefix = tree_prefix(row);

            let collapse_indicator = if row.has_children {
                if collapsed.contains(&row.name) {
                    "> "
                } else {
                    "v "
                }
            } else {
                "  "
            };

            let (icon, icon_style) = if let Some((icon, style)) = decoration.icon_override {
                (icon, style)
            } else {
                let icon = icon_for_task(row).to_string();
                let style = if !row.blocked_by.is_empty() {
                    status_style("blocked")
                } else {
                    status_style(&row.status)
                };
                (icon, style)
            };

            let blocked_info = if row.blocked_by.is_empty() {
                String::new()
            } else {
                format!(" [blocked by: {}]", row.blocked_by.join(", "))
            };

            let desc = if row.description.is_empty() {
                String::new()
            } else {
                format!("  {}", row.description)
            };

            let mut spans = vec![
                Span::raw(prefix),
                Span::raw(collapse_indicator.to_string()),
                Span::styled(icon, icon_style),
                Span::styled(row.name.clone(), Style::default().bold()),
            ];
            spans.extend(decoration.after_name);
            spans.push(Span::styled(blocked_info, Style::default().fg(Color::Red)));
            spans.push(Span::raw(desc));

            ListItem::new(Line::from(spans))
        })
        .collect()
}

/// Render a confirmation dialog overlay.
pub fn render_confirm(frame: &mut Frame, action: &str, task_name: &str, message: &str) {
    let term = frame.area();
    let width = 50.min(term.width.saturating_sub(4));
    let height = 5.min(term.height.saturating_sub(2));
    let area = centered_rect(width, height, term);
    frame.render_widget(Clear, area);

    let title = format!(" {action} ");
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = vec![
        Line::from(vec![
            Span::raw("Task "),
            Span::styled(task_name, Style::default().bold()),
            Span::raw(format!(" {message}")),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Proceed? "),
            Span::styled("y", Style::default().fg(Color::Green).bold()),
            Span::raw("/"),
            Span::styled("n", Style::default().fg(Color::Red).bold()),
        ]),
    ];

    frame.render_widget(Paragraph::new(text), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn make_task(name: &str, parent: Option<&str>, status: &str) -> Task {
        Task {
            id: 0,
            name: name.to_string(),
            parent: parent.map(|s| s.to_string()),
            description: String::new(),
            status: status.to_string(),
            assignee: None,
            status_changed_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn make_row(name: &str, status: &str, assignee: Option<&str>) -> TreeRow {
        TreeRow {
            name: name.into(),
            status: status.into(),
            description: String::new(),
            assignee: assignee.map(|s| s.into()),
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }
    }

    // ── icon_for_status ──

    #[test]
    fn icon_for_each_status() {
        assert!(icon_for_status("done").contains('\u{2705}'));
        assert!(icon_for_status("active").contains('\u{1f7e2}'));
        assert!(icon_for_status("paused").contains('\u{23f8}'));
        assert!(icon_for_status("blocked").contains('\u{1f6a7}'));
        assert!(icon_for_status("open").contains('\u{26aa}'));
        // Unknown status gets the default
        assert!(icon_for_status("whatever").contains('\u{26aa}'));
    }

    // ── icon_for_task ──

    #[test]
    fn icon_for_task_uses_blocked_when_blockers_exist() {
        let mut row = make_row("t", "open", None);
        row.blocked_by = vec!["other".into()];
        assert_eq!(icon_for_task(&row), icon_for_status("blocked"));
    }

    #[test]
    fn icon_for_task_uses_status_when_no_blockers() {
        let row = make_row("t", "active", None);
        assert_eq!(icon_for_task(&row), icon_for_status("active"));
    }

    // ── tree_prefix ──

    #[test]
    fn tree_prefix_root_is_empty() {
        let row = make_row("root", "open", None);
        assert_eq!(tree_prefix(&row), "");
    }

    #[test]
    fn tree_prefix_last_child() {
        let mut row = make_row("child", "open", None);
        row.depth = 1;
        row.is_last_at_depth = vec![false, true];
        assert_eq!(tree_prefix(&row), "└── ");
    }

    #[test]
    fn tree_prefix_middle_child() {
        let mut row = make_row("child", "open", None);
        row.depth = 1;
        row.is_last_at_depth = vec![false, false];
        assert_eq!(tree_prefix(&row), "├── ");
    }

    #[test]
    fn tree_prefix_nested_depth_2() {
        // Parent is not last, grandchild is last
        let mut row = make_row("gc", "open", None);
        row.depth = 2;
        row.is_last_at_depth = vec![false, false, true];
        assert_eq!(tree_prefix(&row), "│   └── ");
    }

    // ── flatten_tree ──

    #[test]
    fn flatten_tree_single_root() {
        let conn = db::open_memory().unwrap();
        let tasks = vec![make_task("root", None, "open")];
        let rows = flatten_tree(&tasks, &HashSet::new(), &conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "root");
        assert_eq!(rows[0].depth, 0);
        assert!(!rows[0].has_children);
    }

    #[test]
    fn flatten_tree_parent_child() {
        let conn = db::open_memory().unwrap();
        let tasks = vec![
            make_task("parent", None, "open"),
            make_task("child", Some("parent"), "open"),
        ];
        let rows = flatten_tree(&tasks, &HashSet::new(), &conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "parent");
        assert!(rows[0].has_children);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].name, "child");
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn flatten_tree_collapsed_hides_children() {
        let conn = db::open_memory().unwrap();
        let tasks = vec![
            make_task("parent", None, "open"),
            make_task("child", Some("parent"), "open"),
        ];
        let mut collapsed = HashSet::new();
        collapsed.insert("parent".to_string());
        let rows = flatten_tree(&tasks, &collapsed, &conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "parent");
        assert!(rows[0].has_children);
    }

    #[test]
    fn flatten_tree_orphan_promoted_to_root() {
        let conn = db::open_memory().unwrap();
        // Child references a parent not in the task list
        let tasks = vec![make_task("orphan", Some("missing"), "open")];
        let rows = flatten_tree(&tasks, &HashSet::new(), &conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "orphan");
        assert_eq!(rows[0].depth, 0);
    }

    #[test]
    fn flatten_tree_with_blockers() {
        let conn = db::open_memory().unwrap();
        ops::add_task(&conn, "blocker", None, "", None, None, false).unwrap();
        ops::add_task(&conn, "blocked", None, "", None, None, false).unwrap();
        ops::add_block(&conn, "blocker", "blocked").unwrap();

        let tasks = vec![
            make_task("blocker", None, "open"),
            make_task("blocked", None, "open"),
        ];
        let rows = flatten_tree(&tasks, &HashSet::new(), &conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].blocked_by.is_empty());
        assert_eq!(rows[1].blocked_by, vec!["blocker"]);
    }

    // ── centered_rect ──

    #[test]
    fn centered_rect_centers_within_area() {
        let area = Rect::new(0, 0, 80, 24);
        let r = centered_rect(40, 10, area);
        assert_eq!(r.x, 20);
        assert_eq!(r.y, 7);
        assert_eq!(r.width, 40);
        assert_eq!(r.height, 10);
    }

    #[test]
    fn centered_rect_clamps_to_area() {
        let area = Rect::new(0, 0, 20, 10);
        let r = centered_rect(40, 20, area);
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 10);
    }

    // ── TreeView ──

    #[test]
    fn tree_view_move_down_clamps() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![
            TreeRow {
                name: "a".into(),
                status: "open".into(),
                description: String::new(),
                assignee: None,
                depth: 0,
                has_children: false,
                is_last_at_depth: vec![false],
                blocked_by: vec![],
            },
            TreeRow {
                name: "b".into(),
                status: "open".into(),
                description: String::new(),
                assignee: None,
                depth: 0,
                has_children: false,
                is_last_at_depth: vec![true],
                blocked_by: vec![],
            },
        ];
        tv.move_down();
        assert_eq!(tv.cursor, 1);
        tv.move_down(); // should clamp
        assert_eq!(tv.cursor, 1);
    }

    #[test]
    fn tree_view_move_up_clamps() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "a".into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        tv.move_up(); // already at 0
        assert_eq!(tv.cursor, 0);
    }

    #[test]
    fn tree_view_toggle_collapse() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "parent".into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: true,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        assert!(!tv.collapsed.contains("parent"));
        tv.toggle_collapse();
        assert!(tv.collapsed.contains("parent"));
        tv.toggle_collapse();
        assert!(!tv.collapsed.contains("parent"));
    }

    #[test]
    fn tree_view_clamp_cursor_empty() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.cursor = 5;
        tv.clamp_cursor();
        assert_eq!(tv.cursor, 0);
    }

    #[test]
    fn tree_view_clamp_cursor_shrunk() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "a".into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        tv.cursor = 5;
        tv.clamp_cursor();
        assert_eq!(tv.cursor, 0);
    }

    #[test]
    fn handle_key_quit() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        let key = KeyEvent::from(KeyCode::Char('q'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Quit));
    }

    #[test]
    fn handle_key_space_returns_refresh() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "a".into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: true,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let key = KeyEvent::from(KeyCode::Char(' '));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Refresh));
        assert!(tv.collapsed.contains("a"));
    }

    #[test]
    fn handle_key_done_refuse_active() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "t".into(),
            status: "active".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let key = KeyEvent::from(KeyCode::Char('d'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
        assert!(tv.error.is_some());
    }

    #[test]
    fn handle_key_done_confirm_active() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Confirm);
        tv.rows = vec![TreeRow {
            name: "t".into(),
            status: "active".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let key = KeyEvent::from(KeyCode::Char('d'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
        assert!(matches!(tv.mode, TreeMode::ConfirmDone(_)));

        // Confirm with y
        let key = KeyEvent::from(KeyCode::Char('y'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::MarkDone(_)));
        assert!(matches!(tv.mode, TreeMode::Normal));
    }

    #[test]
    fn handle_key_done_open_task() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "t".into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let key = KeyEvent::from(KeyCode::Char('d'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::MarkDone(_)));
    }

    #[test]
    fn handle_key_pause_open() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "t".into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let key = KeyEvent::from(KeyCode::Char('p'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Pause(_)));
    }

    #[test]
    fn handle_key_unpause() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "t".into(),
            status: "paused".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let key = KeyEvent::from(KeyCode::Char('p'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Unpause(_)));
    }

    #[test]
    fn handle_key_pause_confirm_active() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Confirm);
        tv.rows = vec![TreeRow {
            name: "t".into(),
            status: "active".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let key = KeyEvent::from(KeyCode::Char('p'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
        assert!(matches!(tv.mode, TreeMode::ConfirmPause(_)));
    }

    #[test]
    fn handle_key_force_unassign() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![TreeRow {
            name: "t".into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let key = KeyEvent::from(KeyCode::Char('U'));
        assert!(matches!(
            tv.handle_key(key),
            TreeKeyAction::ForceUnassign(_)
        ));
    }

    #[test]
    fn handle_key_help_toggle() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        let key = KeyEvent::from(KeyCode::Char('?'));
        tv.handle_key(key);
        assert!(matches!(tv.mode, TreeMode::Help));

        let key = KeyEvent::from(KeyCode::Char('?'));
        tv.handle_key(key);
        assert!(matches!(tv.mode, TreeMode::Normal));
    }

    #[test]
    fn handle_key_help_esc_dismisses() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.mode = TreeMode::Help;
        let key = KeyEvent::from(KeyCode::Esc);
        tv.handle_key(key);
        assert!(matches!(tv.mode, TreeMode::Normal));
    }

    #[test]
    fn handle_key_unhandled_for_unknown() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        let key = KeyEvent::from(KeyCode::Enter);
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Unhandled));
    }

    #[test]
    fn handle_key_confirm_cancel() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Confirm);
        tv.mode = TreeMode::ConfirmDone("t".into());
        let key = KeyEvent::from(KeyCode::Char('n'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
        assert!(matches!(tv.mode, TreeMode::Normal));
    }

    // ── build_tree_items ──

    #[test]
    fn build_tree_items_default_decoration() {
        let collapsed = HashSet::new();
        let rows = vec![TreeRow {
            name: "task".into(),
            status: "open".into(),
            description: "desc".into(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let items = build_tree_items(&rows, &collapsed, |_| RowDecoration::default());
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn build_tree_items_with_decoration() {
        let collapsed = HashSet::new();
        let rows = vec![TreeRow {
            name: "task".into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }];
        let items = build_tree_items(&rows, &collapsed, |_| RowDecoration {
            icon_override: Some(("X ".into(), Style::default())),
            after_name: vec![Span::raw(" extra")],
        });
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn build_tree_items_collapse_indicators() {
        let mut collapsed = HashSet::new();
        collapsed.insert("parent".to_string());
        let rows = vec![
            TreeRow {
                name: "parent".into(),
                status: "open".into(),
                description: String::new(),
                assignee: None,
                depth: 0,
                has_children: true,
                is_last_at_depth: vec![true],
                blocked_by: vec![],
            },
            TreeRow {
                name: "leaf".into(),
                status: "open".into(),
                description: String::new(),
                assignee: None,
                depth: 0,
                has_children: false,
                is_last_at_depth: vec![true],
                blocked_by: vec![],
            },
        ];
        let items = build_tree_items(&rows, &collapsed, |_| RowDecoration::default());
        assert_eq!(items.len(), 2);
    }
}
