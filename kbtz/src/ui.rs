//! Shared UI primitives for kbtz tree rendering.
//!
//! Used by both `kbtz watch` (the CLI TUI) and `kbtz-workspace`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, ListItem, ListState, Paragraph, Wrap};
use rusqlite::Connection;

use crate::paths;

use crate::model::{Note, Task};
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

/// Emoji for a single state dimension (no trailing space).
fn state_emoji(state: &str) -> &'static str {
    match state {
        "done" => "\u{2705}",           // ✅
        "active" => "\u{1f7e2}",        // 🟢
        "paused" => "\u{23f8}\u{fe0f}", // ⏸️
        "blocked" => "\u{1f6a7}",       // 🚧
        _ => "\u{26aa}",                // ⚪
    }
}

/// Icon for a task, combining all non-default orthogonal states.
///
/// Dimensions: blocked (default: unblocked), status (default: open).
/// Each non-default dimension adds its emoji. If all dimensions are default,
/// the open (⚪) emoji is shown.
pub fn icon_for_task(row: &TreeRow) -> String {
    let blocked = !row.blocked_by.is_empty();
    let non_default_status = row.status != "open";
    let mut s = String::new();
    if blocked {
        s.push_str(state_emoji("blocked"));
    }
    if non_default_status {
        s.push_str(state_emoji(&row.status));
    }
    if s.is_empty() {
        s.push_str(state_emoji("open"));
    }
    s.push(' ');
    s
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
    Search(String),
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
    /// Toggle visibility of done/paused tasks; caller should refresh from DB.
    ToggleShowAll,
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
    pub filter: Option<String>,
    pub show_done: bool,
    pub show_paused: bool,
    /// Task name to select on the next tree refresh (e.g. after returning
    /// from a zoomed session). Consumed by `clamp_cursor`.
    pub pending_select: Option<String>,
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
            filter: None,
            show_done: false,
            show_paused: false,
            pending_select: None,
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

    /// Filter a task list according to the current show_done/show_paused flags.
    pub fn filter_tasks(&self, tasks: &mut Vec<Task>) {
        tasks.retain(|t| match t.status.as_str() {
            "done" => self.show_done,
            "paused" => self.show_paused,
            _ => true,
        });
    }

    /// Toggle visibility of done tasks.
    pub fn toggle_show_done(&mut self) {
        self.show_done = !self.show_done;
    }

    /// Toggle visibility of paused tasks.
    pub fn toggle_show_paused(&mut self) {
        self.show_paused = !self.show_paused;
    }

    /// Returns a label describing the current filter state, or `None` if
    /// using default filtering (hiding done and paused).
    pub fn filter_label(&self) -> Option<&'static str> {
        match (self.show_done, self.show_paused) {
            (false, false) => None,
            (false, true) => Some("+paused"),
            (true, false) => Some("+done"),
            (true, true) => Some("all"),
        }
    }

    /// Clamp cursor after rows change (e.g. after refresh from DB).
    ///
    /// If `pending_select` is set, moves the cursor to that task (if found
    /// in the current rows) and clears the pending selection. Otherwise
    /// clamps the current cursor index to remain within bounds.
    pub fn clamp_cursor(&mut self) {
        if self.rows.is_empty() {
            self.cursor = 0;
            self.list_state.select(None);
            self.pending_select = None;
            return;
        }
        if let Some(name) = self.pending_select.take() {
            if let Some(idx) = self.rows.iter().position(|r| r.name == name) {
                self.cursor = idx;
            }
        }
        self.cursor = self.cursor.min(self.rows.len() - 1);
        self.list_state.select(Some(self.cursor));
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
            TreeMode::Search(query) => {
                let mut query = query.clone();
                match key.code {
                    KeyCode::Esc => {
                        self.filter = None;
                        self.mode = TreeMode::Normal;
                        TreeKeyAction::Refresh
                    }
                    KeyCode::Enter => {
                        self.filter = if query.is_empty() { None } else { Some(query) };
                        self.mode = TreeMode::Normal;
                        // Return Unhandled so the caller can process Enter
                        // (e.g. as RunAction to select the highlighted task).
                        TreeKeyAction::Unhandled
                    }
                    KeyCode::Down => {
                        self.move_down();
                        TreeKeyAction::Continue
                    }
                    KeyCode::Up => {
                        self.move_up();
                        TreeKeyAction::Continue
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.filter = if query.is_empty() {
                            None
                        } else {
                            Some(query.clone())
                        };
                        self.mode = TreeMode::Search(query);
                        TreeKeyAction::Refresh
                    }
                    KeyCode::Char(c) => {
                        query.push(c);
                        self.filter = Some(query.clone());
                        self.mode = TreeMode::Search(query);
                        TreeKeyAction::Refresh
                    }
                    _ => TreeKeyAction::Continue,
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
                    KeyCode::Char('D') => {
                        self.toggle_show_done();
                        TreeKeyAction::ToggleShowAll
                    }
                    KeyCode::Char('P') => {
                        self.toggle_show_paused();
                        TreeKeyAction::ToggleShowAll
                    }
                    KeyCode::Char('?') => {
                        self.mode = TreeMode::Help;
                        TreeKeyAction::Continue
                    }
                    KeyCode::Char('/') => {
                        let initial = self.filter.clone().unwrap_or_default();
                        self.mode = TreeMode::Search(initial);
                        TreeKeyAction::Continue
                    }
                    KeyCode::Esc if self.filter.is_some() => {
                        self.filter = None;
                        TreeKeyAction::Refresh
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

/// Extension point for per-row tree customization.
pub trait TreeDecorator {
    fn decorate(&self, row: &TreeRow) -> RowDecoration;
}

/// No-op decorator — returns default decoration for every row.
pub struct DefaultDecorator;

impl TreeDecorator for DefaultDecorator {
    fn decorate(&self, _row: &TreeRow) -> RowDecoration {
        RowDecoration::default()
    }
}

/// Map a session status string to its indicator emoji.
pub fn session_indicator(status: &str) -> &'static str {
    match status.trim() {
        "active" => "\u{1f7e2}",      // 🟢
        "idle" => "\u{1f7e1}",        // 🟡
        "needs_input" => "\u{1f514}", // 🔔
        _ => "\u{23f3}",              // ⏳
    }
}

/// Decorator that reads session status from workspace status files.
/// Replaces the task status icon with a session indicator for tasks
/// that have an active session with a status file.
pub struct FileStatusDecorator {
    /// assignee string → status file content (e.g. "active", "idle")
    pub statuses: HashMap<String, String>,
}

impl FileStatusDecorator {
    /// Read status files from the workspace directory, keyed by assignee strings
    /// found in the given tree rows.
    pub fn from_dir(dir: &Path, rows: &[TreeRow]) -> Self {
        let mut statuses = HashMap::new();
        for row in rows {
            let Some(ref assignee) = row.assignee else {
                continue;
            };
            if statuses.contains_key(assignee) {
                continue;
            }
            let filename = paths::session_id_to_filename(assignee);
            let path = dir.join(&filename);
            if let Ok(content) = std::fs::read_to_string(&path) {
                statuses.insert(assignee.clone(), content);
            }
        }
        Self { statuses }
    }
}

impl TreeDecorator for FileStatusDecorator {
    fn decorate(&self, row: &TreeRow) -> RowDecoration {
        if let Some(ref assignee) = row.assignee {
            // Known session with status file: 🤖🟢 task-name
            if let Some(status) = self.statuses.get(assignee) {
                return RowDecoration {
                    icon_override: Some((
                        format!("\u{1f916}{} ", session_indicator(status)),
                        status_style(&row.status),
                    )),
                    after_name: vec![],
                };
            }
            // Assigned but no status file (external/stale): 👽⭕  task-name
            if row.status == "active" {
                return RowDecoration {
                    icon_override: Some((
                        format!("\u{1f47d}{}  ", icon_for_task(row)),
                        status_style(&row.status),
                    )),
                    after_name: vec![],
                };
            }
        }
        RowDecoration::default()
    }
}

/// Filter tree rows to only those matching the query (case-insensitive substring
/// of name or description). Ancestor rows are retained to preserve tree structure.
pub fn filter_rows(rows: &[TreeRow], query: &str) -> Vec<TreeRow> {
    let query_lower = query.to_lowercase();
    // First pass: find which rows match directly.
    let matches: HashSet<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.name.to_lowercase().contains(&query_lower)
                || row.description.to_lowercase().contains(&query_lower)
        })
        .map(|(i, _)| i)
        .collect();

    // Second pass: for each matching row, include its ancestors.
    // Ancestors are rows at shallower depths that precede it in the flat list.
    let mut keep: Vec<bool> = vec![false; rows.len()];
    for &idx in &matches {
        keep[idx] = true;
        // Walk backwards to find ancestors at each depth level.
        let mut need_depth = rows[idx].depth;
        if need_depth > 0 {
            for j in (0..idx).rev() {
                if rows[j].depth < need_depth {
                    keep[j] = true;
                    need_depth = rows[j].depth;
                    if need_depth == 0 {
                        break;
                    }
                }
            }
        }
    }

    // Third pass: rebuild with corrected is_last_at_depth.
    let kept: Vec<&TreeRow> = rows
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(r, _)| r)
        .collect();
    let mut result = Vec::with_capacity(kept.len());
    for (i, row) in kept.iter().enumerate() {
        let mut new_row = (*row).clone();
        // Recalculate is_last_at_depth: a row is last at its depth if no later
        // sibling exists at the same depth (before the next row at a shallower depth).
        let is_last = kept[i + 1..]
            .iter()
            .take_while(|r| r.depth >= new_row.depth)
            .all(|r| r.depth != new_row.depth);
        if new_row.depth < new_row.is_last_at_depth.len() {
            new_row.is_last_at_depth[new_row.depth] = is_last;
        }
        // Check if any children survived filtering.
        let has_visible_children = kept[i + 1..]
            .first()
            .is_some_and(|next| next.depth > new_row.depth);
        new_row.has_children = has_visible_children;
        result.push(new_row);
    }
    result
}

/// Render the search input footer line.
pub fn search_footer_line(query: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("/", Style::default().fg(Color::Cyan)),
        Span::raw(query.to_string()),
        Span::styled("_", Style::default().fg(Color::DarkGray)),
    ])
}

/// Render a filter indicator for the footer when a filter is active but not in search mode.
pub fn filter_footer_spans(filter: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled("/", Style::default().fg(Color::Cyan)),
        Span::styled(filter.to_string(), Style::default().fg(Color::Yellow)),
        Span::raw("  "),
    ]
}

/// Build ListItems for all tree rows.
///
/// The decorator is called for each row to provide optional
/// per-row customization (e.g. session indicators).
pub fn build_tree_items(
    rows: &[TreeRow],
    collapsed: &HashSet<String>,
    decorator: &dyn TreeDecorator,
) -> Vec<ListItem<'static>> {
    rows.iter()
        .map(|row| {
            let decoration = decorator.decorate(row);
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
                let icon = icon_for_task(row);
                let style = status_style(&row.status);
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
                Span::raw(collapse_indicator),
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

// ── Notes panel ────────────────────────────────────────────────────

/// Action returned by `NotesPanel::handle_key`.
pub enum NotesKeyAction {
    /// Stay in notes mode; caller should redraw.
    Continue,
    /// User dismissed the notes panel.
    Close,
}

/// Shared notes-panel state used by both `kbtz watch` and `kbtz-workspace`.
#[derive(Default)]
pub struct NotesPanel {
    pub notes: Vec<Note>,
    pub scroll: u16,
}

impl NotesPanel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load notes for `task_name` from the database.
    pub fn load(&mut self, conn: &Connection, task_name: &str) -> Result<()> {
        self.notes = ops::list_notes(conn, task_name)?;
        Ok(())
    }

    /// Handle a key press while the notes panel is visible.
    pub fn handle_key(&mut self, key: KeyEvent) -> NotesKeyAction {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n') | KeyCode::Char('q') => {
                NotesKeyAction::Close
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                NotesKeyAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                NotesKeyAction::Continue
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(20);
                NotesKeyAction::Continue
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(20);
                NotesKeyAction::Continue
            }
            KeyCode::Char('G') => {
                let lines: u16 = self
                    .notes
                    .iter()
                    .map(|n| n.content.lines().count() as u16 + 2)
                    .sum();
                self.scroll = lines.saturating_sub(1);
                NotesKeyAction::Continue
            }
            KeyCode::Char('g') => {
                self.scroll = 0;
                NotesKeyAction::Continue
            }
            _ => NotesKeyAction::Continue,
        }
    }

    /// Render the notes panel as a full-screen overlay.
    pub fn render(&self, frame: &mut Frame, area: Rect, task_name: Option<&str>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        let notes_area = chunks[0];
        let hint_area = chunks[1];

        let title = task_name
            .map(|n| format!(" Notes: {n} "))
            .unwrap_or_else(|| " Notes ".to_string());

        let text = if self.notes.is_empty() {
            "No notes.".to_string()
        } else {
            self.notes
                .iter()
                .map(|n| format!("[{}]\n{}\n", n.created_at, n.content))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));

        frame.render_widget(paragraph, notes_area);

        frame.render_widget(
            Paragraph::new("Esc/q/n: back  j/k: scroll  g/G: top/bottom")
                .style(Style::default().fg(Color::DarkGray)),
            hint_area,
        );
    }
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
            agent: None,
            directory: None,
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

    // ── state_emoji ──

    #[test]
    fn state_emoji_for_each_status() {
        assert!(state_emoji("done").contains('\u{2705}'));
        assert!(state_emoji("active").contains('\u{1f7e2}'));
        assert!(state_emoji("paused").contains('\u{23f8}'));
        assert!(state_emoji("blocked").contains('\u{1f6a7}'));
        assert!(state_emoji("open").contains('\u{26aa}'));
        // Unknown status gets the default
        assert!(state_emoji("whatever").contains('\u{26aa}'));
    }

    // ── icon_for_task ──

    #[test]
    fn icon_for_task_blocked_and_open_shows_blocked_only() {
        let mut row = make_row("t", "open", None);
        row.blocked_by = vec!["other".into()];
        assert!(icon_for_task(&row).contains('\u{1f6a7}'));
        assert!(!icon_for_task(&row).contains('\u{26aa}'));
    }

    #[test]
    fn icon_for_task_blocked_and_paused_shows_both() {
        let mut row = make_row("t", "paused", None);
        row.blocked_by = vec!["other".into()];
        let icon = icon_for_task(&row);
        assert!(icon.contains('\u{1f6a7}'), "should contain blocked emoji");
        assert!(icon.contains('\u{23f8}'), "should contain paused emoji");
    }

    #[test]
    fn icon_for_task_unblocked_active_shows_active() {
        let row = make_row("t", "active", None);
        let icon = icon_for_task(&row);
        assert!(icon.contains('\u{1f7e2}'));
        assert!(!icon.contains('\u{1f6a7}'));
    }

    #[test]
    fn icon_for_task_blocked_and_done_shows_both() {
        let mut row = make_row("t", "done", None);
        row.blocked_by = vec!["other".into()];
        let icon = icon_for_task(&row);
        assert!(icon.contains('\u{1f6a7}'), "should contain blocked emoji");
        assert!(icon.contains('\u{2705}'), "should contain done emoji");
    }

    #[test]
    fn icon_for_task_open_unblocked_shows_default() {
        let row = make_row("t", "open", None);
        assert!(icon_for_task(&row).contains('\u{26aa}'));
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
        ops::add_task(
            &conn,
            ops::AddTaskParams {
                name: "blocker",
                ..Default::default()
            },
        )
        .unwrap();
        ops::add_task(
            &conn,
            ops::AddTaskParams {
                name: "blocked",
                ..Default::default()
            },
        )
        .unwrap();
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
    fn pending_select_moves_cursor_to_matching_row() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        let row = |name: &str| TreeRow {
            name: name.into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        };
        tv.rows = vec![row("a"), row("b"), row("c")];
        tv.cursor = 0;
        tv.pending_select = Some("c".into());
        tv.clamp_cursor();
        assert_eq!(tv.cursor, 2);
        assert!(tv.pending_select.is_none());
    }

    #[test]
    fn pending_select_not_found_keeps_cursor() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        let row = |name: &str| TreeRow {
            name: name.into(),
            status: "open".into(),
            description: String::new(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        };
        tv.rows = vec![row("a"), row("b")];
        tv.cursor = 1;
        tv.pending_select = Some("missing".into());
        tv.clamp_cursor();
        assert_eq!(tv.cursor, 1);
        assert!(tv.pending_select.is_none());
    }

    #[test]
    fn pending_select_cleared_on_empty_tree() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.pending_select = Some("task".into());
        tv.clamp_cursor();
        assert_eq!(tv.cursor, 0);
        assert!(tv.pending_select.is_none());
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

    // ── filter ──

    #[test]
    fn handle_key_d_toggles_show_done() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        assert!(!tv.show_done);

        let key = KeyEvent::from(KeyCode::Char('D'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::ToggleShowAll));
        assert!(tv.show_done);

        let key = KeyEvent::from(KeyCode::Char('D'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::ToggleShowAll));
        assert!(!tv.show_done);
    }

    #[test]
    fn handle_key_p_toggles_show_paused() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        assert!(!tv.show_paused);

        let key = KeyEvent::from(KeyCode::Char('P'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::ToggleShowAll));
        assert!(tv.show_paused);

        let key = KeyEvent::from(KeyCode::Char('P'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::ToggleShowAll));
        assert!(!tv.show_paused);
    }

    #[test]
    fn filter_tasks_hides_done_and_paused_by_default() {
        let tv = TreeView::new(ActiveTaskPolicy::Refuse);
        let mut tasks = vec![
            make_task("open-task", None, "open"),
            make_task("done-task", None, "done"),
            make_task("paused-task", None, "paused"),
            make_task("active-task", None, "active"),
        ];
        tv.filter_tasks(&mut tasks);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["open-task", "active-task"]);
    }

    #[test]
    fn filter_tasks_shows_all_when_enabled() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.show_done = true;
        tv.show_paused = true;
        let mut tasks = vec![
            make_task("open-task", None, "open"),
            make_task("done-task", None, "done"),
            make_task("paused-task", None, "paused"),
        ];
        tv.filter_tasks(&mut tasks);
        assert_eq!(tasks.len(), 3);
    }

    #[test]
    fn filter_label_all_states() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        assert_eq!(tv.filter_label(), None);

        tv.show_paused = true;
        assert_eq!(tv.filter_label(), Some("+paused"));

        tv.show_paused = false;
        tv.show_done = true;
        assert_eq!(tv.filter_label(), Some("+done"));

        tv.show_paused = true;
        assert_eq!(tv.filter_label(), Some("all"));
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
        let items = build_tree_items(&rows, &collapsed, &DefaultDecorator);
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
        struct TestDecorator;
        impl TreeDecorator for TestDecorator {
            fn decorate(&self, _row: &TreeRow) -> RowDecoration {
                RowDecoration {
                    icon_override: Some(("X ".into(), Style::default())),
                    after_name: vec![Span::raw(" extra")],
                }
            }
        }
        let items = build_tree_items(&rows, &collapsed, &TestDecorator);
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
        let items = build_tree_items(&rows, &collapsed, &DefaultDecorator);
        assert_eq!(items.len(), 2);
    }

    // ── session_indicator ──

    #[test]
    fn session_indicator_known_statuses() {
        assert_eq!(session_indicator("active"), "\u{1f7e2}");
        assert_eq!(session_indicator("idle"), "\u{1f7e1}");
        assert_eq!(session_indicator("needs_input"), "\u{1f514}");
    }

    #[test]
    fn session_indicator_trims_whitespace() {
        assert_eq!(session_indicator("active\n"), "\u{1f7e2}");
        assert_eq!(session_indicator("  idle  "), "\u{1f7e1}");
    }

    #[test]
    fn session_indicator_unknown_is_hourglass() {
        assert_eq!(session_indicator("starting"), "\u{23f3}");
        assert_eq!(session_indicator(""), "\u{23f3}");
    }

    // ── FileStatusDecorator ──

    #[test]
    fn file_status_decorator_overrides_icon() {
        let mut statuses = HashMap::new();
        statuses.insert("ws/1".into(), "active".into());
        let decorator = FileStatusDecorator { statuses };

        let row = make_row("task", "active", Some("ws/1"));
        let dec = decorator.decorate(&row);
        assert!(dec.icon_override.is_some());
        let (icon, _) = dec.icon_override.unwrap();
        assert!(icon.contains('\u{1f916}'));
        assert!(icon.contains('\u{1f7e2}'));
    }

    #[test]
    fn file_status_decorator_no_match_returns_default() {
        let decorator = FileStatusDecorator {
            statuses: HashMap::new(),
        };
        let row = make_row("task", "open", None);
        let dec = decorator.decorate(&row);
        assert!(dec.icon_override.is_none());
        assert!(dec.after_name.is_empty());
    }

    #[test]
    fn file_status_decorator_from_dir() {
        let dir = std::env::temp_dir().join("kbtz-test-file-status");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ws-1"), "active\n").unwrap();

        let rows = vec![make_row("task", "active", Some("ws/1"))];
        let decorator = FileStatusDecorator::from_dir(&dir, &rows);
        assert_eq!(decorator.statuses.get("ws/1").unwrap(), "active\n");

        let dec = decorator.decorate(&rows[0]);
        assert!(dec.icon_override.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_decorator_returns_default() {
        let row = make_row("t", "open", None);
        let dec = DefaultDecorator.decorate(&row);
        assert!(dec.icon_override.is_none());
        assert!(dec.after_name.is_empty());
    }

    // ── filter_rows ──

    fn make_row_with_desc(name: &str, desc: &str) -> TreeRow {
        TreeRow {
            name: name.into(),
            status: "open".into(),
            description: desc.into(),
            assignee: None,
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        }
    }

    #[test]
    fn filter_rows_matches_name() {
        let rows = vec![
            make_row_with_desc("auth-login", "Login feature"),
            make_row_with_desc("db-migrate", "Run migrations"),
        ];
        let filtered = filter_rows(&rows, "auth");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "auth-login");
    }

    #[test]
    fn filter_rows_matches_description() {
        let rows = vec![
            make_row_with_desc("task-a", "Fix login bug"),
            make_row_with_desc("task-b", "Add signup page"),
        ];
        let filtered = filter_rows(&rows, "login");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "task-a");
    }

    #[test]
    fn filter_rows_case_insensitive() {
        let rows = vec![make_row_with_desc("MyTask", "Description")];
        let filtered = filter_rows(&rows, "mytask");
        assert_eq!(filtered.len(), 1);
        let filtered = filter_rows(&rows, "MYTASK");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_rows_preserves_ancestors() {
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
                name: "child-match".into(),
                status: "open".into(),
                description: String::new(),
                assignee: None,
                depth: 1,
                has_children: false,
                is_last_at_depth: vec![true, true],
                blocked_by: vec![],
            },
            TreeRow {
                name: "child-no".into(),
                status: "open".into(),
                description: String::new(),
                assignee: None,
                depth: 1,
                has_children: false,
                is_last_at_depth: vec![true, true],
                blocked_by: vec![],
            },
        ];
        let filtered = filter_rows(&rows, "match");
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name, "parent");
        assert_eq!(filtered[1].name, "child-match");
    }

    #[test]
    fn filter_rows_empty_query_returns_all() {
        let rows = vec![make_row_with_desc("a", ""), make_row_with_desc("b", "")];
        let filtered = filter_rows(&rows, "");
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_rows_no_match_returns_empty() {
        let rows = vec![make_row_with_desc("task", "desc")];
        let filtered = filter_rows(&rows, "zzz");
        assert!(filtered.is_empty());
    }

    // ── Search mode key handling ──

    #[test]
    fn handle_key_slash_enters_search() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        let key = KeyEvent::from(KeyCode::Char('/'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
        assert!(matches!(tv.mode, TreeMode::Search(ref q) if q.is_empty()));
    }

    #[test]
    fn search_mode_typing_updates_filter() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.mode = TreeMode::Search(String::new());
        let key = KeyEvent::from(KeyCode::Char('a'));
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Refresh));
        assert_eq!(tv.filter.as_deref(), Some("a"));
        assert!(matches!(tv.mode, TreeMode::Search(ref q) if q == "a"));
    }

    #[test]
    fn search_mode_enter_confirms_filter() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.mode = TreeMode::Search("test".into());
        tv.filter = Some("test".into());
        let key = KeyEvent::from(KeyCode::Enter);
        // Enter returns Unhandled so the caller can process it (e.g. RunAction).
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Unhandled));
        assert_eq!(tv.filter.as_deref(), Some("test"));
        assert!(matches!(tv.mode, TreeMode::Normal));
    }

    #[test]
    fn search_mode_arrow_keys_navigate() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.rows = vec![
            make_row("a", "open", None),
            make_row("b", "open", None),
            make_row("c", "open", None),
        ];
        tv.cursor = 0;
        tv.list_state.select(Some(0));
        tv.mode = TreeMode::Search("test".into());

        // Down moves cursor
        let key = KeyEvent::from(KeyCode::Down);
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
        assert_eq!(tv.cursor, 1);
        // Still in search mode
        assert!(matches!(tv.mode, TreeMode::Search(_)));

        // Up moves cursor back
        let key = KeyEvent::from(KeyCode::Up);
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
        assert_eq!(tv.cursor, 0);
        assert!(matches!(tv.mode, TreeMode::Search(_)));
    }

    #[test]
    fn search_mode_esc_clears_filter() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.mode = TreeMode::Search("test".into());
        tv.filter = Some("test".into());
        let key = KeyEvent::from(KeyCode::Esc);
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Refresh));
        assert!(tv.filter.is_none());
        assert!(matches!(tv.mode, TreeMode::Normal));
    }

    #[test]
    fn search_mode_backspace_shrinks_query() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.mode = TreeMode::Search("ab".into());
        tv.filter = Some("ab".into());
        let key = KeyEvent::from(KeyCode::Backspace);
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Refresh));
        assert_eq!(tv.filter.as_deref(), Some("a"));
        assert!(matches!(tv.mode, TreeMode::Search(ref q) if q == "a"));
    }

    #[test]
    fn search_mode_backspace_empty_clears_filter() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.mode = TreeMode::Search("a".into());
        tv.filter = Some("a".into());
        let key = KeyEvent::from(KeyCode::Backspace);
        tv.handle_key(key);
        assert!(tv.filter.is_none());
    }

    #[test]
    fn esc_in_normal_clears_active_filter() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.filter = Some("test".into());
        let key = KeyEvent::from(KeyCode::Esc);
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Refresh));
        assert!(tv.filter.is_none());
    }

    #[test]
    fn esc_in_normal_without_filter_is_unhandled() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        let key = KeyEvent::from(KeyCode::Esc);
        assert!(matches!(tv.handle_key(key), TreeKeyAction::Unhandled));
    }

    #[test]
    fn slash_with_existing_filter_reopens_search() {
        let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
        tv.filter = Some("old".into());
        let key = KeyEvent::from(KeyCode::Char('/'));
        tv.handle_key(key);
        assert!(matches!(tv.mode, TreeMode::Search(ref q) if q == "old"));
    }
}
