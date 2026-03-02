# Unify Tree View Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Extract shared tree rendering and keyboard handling from `kbtz watch` and `kbtz-workspace` into a single implementation in `kbtz::ui`, eliminating ~150 lines of duplication.

**Architecture:** Add `TreeView` struct, `TreeMode`/`ActiveTaskPolicy`/`TreeKeyAction` enums, `RowDecoration`/`build_tree_items` rendering, and `render_confirm` dialog to `kbtz::ui`. Both apps embed `TreeView` and call `handle_key()`, handling `Unhandled` for app-specific keys. Watch gains confirm dialogs for managed mode; workspace switches to shared handler.

**Tech Stack:** Rust, ratatui 0.30, crossterm 0.28

**Worktree:** `/home/virgil/kbtz/.worktrees/unify-tree-view` (branch `unify-tree-view`)

---

### Task 1: Add shared types and TreeView to kbtz::ui

**Files:**
- Modify: `kbtz/src/ui.rs`

**Step 1: Add the new types and TreeView struct after the existing rendering helpers (after line 167)**

Add these types and the `TreeView` struct with navigation methods:

```rust
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::{Clear, ListItem, ListState, Paragraph};

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
```

**Step 2: Add RowDecoration and build_tree_items after the TreeView impl**

```rust
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
pub fn render_confirm(frame: &mut Frame, action: &str, task_name: &str) {
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
            Span::raw(" has an active session."),
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
```

**Step 3: Add tests for the new code**

Add to the existing `#[cfg(test)] mod tests` block:

```rust
// ── TreeView ──

#[test]
fn tree_view_move_down_clamps() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
    tv.rows = vec![
        TreeRow { name: "a".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![false], blocked_by: vec![] },
        TreeRow { name: "b".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    tv.move_down();
    assert_eq!(tv.cursor, 1);
    tv.move_down(); // should clamp
    assert_eq!(tv.cursor, 1);
}

#[test]
fn tree_view_move_up_clamps() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
    tv.rows = vec![
        TreeRow { name: "a".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    tv.move_up(); // already at 0
    assert_eq!(tv.cursor, 0);
}

#[test]
fn tree_view_toggle_collapse() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
    tv.rows = vec![
        TreeRow { name: "parent".into(), status: "open".into(), description: String::new(), depth: 0, has_children: true, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
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
    tv.rows = vec![
        TreeRow { name: "a".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
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
    tv.rows = vec![
        TreeRow { name: "a".into(), status: "open".into(), description: String::new(), depth: 0, has_children: true, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let key = KeyEvent::from(KeyCode::Char(' '));
    assert!(matches!(tv.handle_key(key), TreeKeyAction::Refresh));
    assert!(tv.collapsed.contains("a"));
}

#[test]
fn handle_key_done_refuse_active() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
    tv.rows = vec![
        TreeRow { name: "t".into(), status: "active".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let key = KeyEvent::from(KeyCode::Char('d'));
    assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
    assert!(tv.error.is_some());
}

#[test]
fn handle_key_done_confirm_active() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Confirm);
    tv.rows = vec![
        TreeRow { name: "t".into(), status: "active".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
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
    tv.rows = vec![
        TreeRow { name: "t".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let key = KeyEvent::from(KeyCode::Char('d'));
    assert!(matches!(tv.handle_key(key), TreeKeyAction::MarkDone(_)));
}

#[test]
fn handle_key_pause_open() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
    tv.rows = vec![
        TreeRow { name: "t".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let key = KeyEvent::from(KeyCode::Char('p'));
    assert!(matches!(tv.handle_key(key), TreeKeyAction::Pause(_)));
}

#[test]
fn handle_key_unpause() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
    tv.rows = vec![
        TreeRow { name: "t".into(), status: "paused".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let key = KeyEvent::from(KeyCode::Char('p'));
    assert!(matches!(tv.handle_key(key), TreeKeyAction::Unpause(_)));
}

#[test]
fn handle_key_pause_confirm_active() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Confirm);
    tv.rows = vec![
        TreeRow { name: "t".into(), status: "active".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let key = KeyEvent::from(KeyCode::Char('p'));
    assert!(matches!(tv.handle_key(key), TreeKeyAction::Continue));
    assert!(matches!(tv.mode, TreeMode::ConfirmPause(_)));
}

#[test]
fn handle_key_force_unassign() {
    let mut tv = TreeView::new(ActiveTaskPolicy::Refuse);
    tv.rows = vec![
        TreeRow { name: "t".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let key = KeyEvent::from(KeyCode::Char('U'));
    assert!(matches!(tv.handle_key(key), TreeKeyAction::ForceUnassign(_)));
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
    let rows = vec![
        TreeRow { name: "task".into(), status: "open".into(), description: "desc".into(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let items = build_tree_items(&rows, &collapsed, |_| RowDecoration::default());
    assert_eq!(items.len(), 1);
}

#[test]
fn build_tree_items_with_decoration() {
    let collapsed = HashSet::new();
    let rows = vec![
        TreeRow { name: "task".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
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
        TreeRow { name: "parent".into(), status: "open".into(), description: String::new(), depth: 0, has_children: true, is_last_at_depth: vec![true], blocked_by: vec![] },
        TreeRow { name: "leaf".into(), status: "open".into(), description: String::new(), depth: 0, has_children: false, is_last_at_depth: vec![true], blocked_by: vec![] },
    ];
    let items = build_tree_items(&rows, &collapsed, |_| RowDecoration::default());
    assert_eq!(items.len(), 2);
}
```

**Step 4: Run tests**

Run: `cd /home/virgil/kbtz/.worktrees/unify-tree-view && cargo test -p kbtz ui::tests`
Expected: All new tests pass.

**Step 5: Commit**

```bash
git add kbtz/src/ui.rs
git commit -m "feat: add shared TreeView, key handler, and build_tree_items to kbtz::ui"
```

---

### Task 2: Update kbtz watch to use shared TreeView

**Files:**
- Modify: `kbtz/src/tui/app.rs` — replace flat tree state with embedded `TreeView`
- Modify: `kbtz/src/tui/tree.rs` — use `build_tree_items` and `render_confirm`
- Modify: `kbtz/src/tui/event.rs` — delegate to `TreeView::handle_key`, handle watch-specific keys
- Modify: `kbtz/src/tui/mod.rs` — adapt event loop to new action types and tree modes

**Step 1: Rewrite `kbtz/src/tui/app.rs`**

Replace the `App` struct to embed `TreeView` instead of flat fields. Keep `show_notes`, `notes`, `add_form` as watch-specific. The `Mode` enum shrinks to just `AddTask` (tree modes are in `TreeView`). Migrate `move_up`, `move_down`, `toggle_collapse`, `selected_name` to delegate to `self.tree`.

Key changes:
- `App.rows` → `App.tree.rows`
- `App.cursor` → `App.tree.cursor`
- `App.collapsed` → `App.tree.collapsed`
- `App.error` → `App.tree.error`
- `App.mode` only holds `None` or `Some(AddTask)` — the `Mode::Normal` and `Mode::Help` states move to `TreeView.mode`
- `App.refresh()` calls `self.tree.clamp_cursor()` instead of manual clamping
- `App.selected_name()` delegates to `self.tree.selected_name()`

**Step 2: Rewrite `kbtz/src/tui/event.rs`**

Replace the current `KeyAction` enum and `handle_key` function. The new flow:
1. If in AddTask mode, handle locally (unchanged)
2. Otherwise, call `app.tree.handle_key(key)`
3. If result is `Unhandled`, check watch-specific keys: `Esc` (quit), `Enter`/`n` (toggle notes), `a` (add child), `A` (add root), `N` (add note)

The `KeyAction` enum becomes simpler since tree actions are now handled by `TreeKeyAction`:
```rust
pub enum KeyAction {
    Quit,
    Submit,        // add-task form submit
    OpenEditor,    // Ctrl-E in add-task note field
    AddNote,       // N key
    Refresh,       // tree changed, refresh from DB
    TogglePause(String),
    Unpause(String),
    MarkDone(String),
    ForceUnassign(String),
    Continue,
}
```

**Step 3: Rewrite `kbtz/src/tui/tree.rs`**

Replace `render_tree`'s item-building loop with `ui::build_tree_items`. Use `render_stateful_widget` with `highlight_style` instead of manual cursor index comparison. Add `render_confirm` overlay rendering in the `render` function based on `app.tree.mode`.

Key changes to `render()`:
```rust
pub fn render(frame: &mut Frame, app: &mut App) {
    // ... notes panel split unchanged ...

    match &app.tree.mode {
        TreeMode::ConfirmDone(name) => ui::render_confirm(frame, "Done", name),
        TreeMode::ConfirmPause(name) => ui::render_confirm(frame, "Pause", name),
        TreeMode::Help => render_help(frame),
        TreeMode::Normal => {}
    }

    if let Some(_) = &app.add_form {
        render_add_dialog(frame, app);
    }
}
```

Key changes to `render_tree()`:
```rust
fn render_tree(frame: &mut Frame, app: &mut App, area: Rect) {
    // ... error area handling unchanged ...

    let items = ui::build_tree_items(&app.tree.rows, &app.tree.collapsed, |_row| {
        ui::RowDecoration::default()
    });

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Tasks "))
        .highlight_style(Style::default().bg(Color::DarkGray));

    frame.render_stateful_widget(list, area, &mut app.tree.list_state);
}
```

**Step 4: Update `kbtz/src/tui/mod.rs`**

Adapt the event loop to handle `TreeKeyAction` variants mapped through `event::handle_key`. The pause/unpause/done/unassign handlers already exist, just need to match the new action names.

**Step 5: Run tests**

Run: `cd /home/virgil/kbtz/.worktrees/unify-tree-view && cargo test -p kbtz`
Expected: All tests pass, including the new ui::tests.

Run: `cd /home/virgil/kbtz/.worktrees/unify-tree-view && cargo build -p kbtz`
Expected: Clean build.

**Step 6: Commit**

```bash
git add kbtz/src/tui/
git commit -m "refactor: kbtz watch uses shared TreeView and build_tree_items"
```

---

### Task 3: Update kbtz-workspace to use shared TreeView

**Files:**
- Modify: `kbtz-workspace/src/app.rs` — replace `TreeView` struct with `kbtz::ui::TreeView`, remove duplicated navigation methods
- Modify: `kbtz-workspace/src/tree.rs` — use `build_tree_items` and shared `render_confirm`
- Modify: `kbtz-workspace/src/main.rs` — delegate to `TreeView::handle_key`, handle workspace-specific keys

**Step 1: Update `kbtz-workspace/src/app.rs`**

- Delete the local `TreeView` struct (lines 27-33)
- Replace with `use kbtz::ui::TreeView;` (already re-exported `TreeRow`)
- Update `App::new()` to construct `TreeView::new(ActiveTaskPolicy::Confirm)`
- Delete `move_up`, `move_down`, `toggle_collapse`, `selected_name` from `App` — these now live on `TreeView`. Call sites change from `app.move_down()` to `app.tree.move_down()`.
- Update `refresh_tree()` to use `self.tree.clamp_cursor()` instead of manual clamping.

**Step 2: Update `kbtz-workspace/src/tree.rs`**

Replace `render_tree`'s item-building loop with `ui::build_tree_items`. The decoration closure provides session indicators:

```rust
fn render_tree(frame: &mut Frame, app: &mut App, area: Rect) {
    // ... empty check unchanged ...

    let task_to_session = &app.task_to_session;
    let sessions = &app.sessions;
    let items = ui::build_tree_items(&app.tree.rows, &app.tree.collapsed, |row| {
        if let Some(sid) = task_to_session.get(&row.name) {
            if let Some(session) = sessions.get(sid) {
                return ui::RowDecoration {
                    icon_override: Some((
                        format!("\u{1f916}{} ", session.status().indicator()),
                        ui::status_style(&row.status),
                    )),
                    after_name: vec![
                        Span::styled(format!(" {sid}"), Style::default().fg(Color::Cyan)),
                    ],
                };
            }
        }
        ui::RowDecoration::default()
    });

    // ... list construction with title unchanged ...
    // ... render_stateful_widget unchanged ...
}
```

Delete the local `render_confirm` function — use `ui::render_confirm` instead.

**Step 3: Update `kbtz-workspace/src/main.rs`**

Replace the `TreeMode` enum and inline keyboard handling in `tree_loop` with delegation to `app.tree.handle_key()`:

- Delete the local `TreeMode` enum
- In `tree_loop`, the drawing closure checks `app.tree.mode` for overlays:
  ```rust
  terminal.draw(|frame| {
      tree::render(frame, app);
      match &app.tree.mode {
          kbtz::ui::TreeMode::Help => tree::render_help(frame),
          kbtz::ui::TreeMode::ConfirmDone(name) => ui::render_confirm(frame, "Done", name),
          kbtz::ui::TreeMode::ConfirmPause(name) => ui::render_confirm(frame, "Pause", name),
          kbtz::ui::TreeMode::Normal => {}
      }
  })?;
  ```
- Key handling becomes:
  ```rust
  match app.tree.handle_key(key) {
      TreeKeyAction::Quit => return Ok(Action::Quit),
      TreeKeyAction::Refresh => app.refresh_tree()?,
      TreeKeyAction::Pause(name) => { /* ops::pause_task */ },
      TreeKeyAction::Unpause(name) => { /* ops::unpause_task */ },
      TreeKeyAction::MarkDone(name) => { /* ops::mark_done */ },
      TreeKeyAction::ForceUnassign(name) => { /* ops::force_unassign_task */ },
      TreeKeyAction::Unhandled => {
          // Workspace-specific keys
          match key.code {
              KeyCode::Enter => { /* zoom */ },
              KeyCode::Char('s') => { /* spawn */ },
              KeyCode::Char('r') => { /* restart */ },
              KeyCode::Tab => { /* needs-input */ },
              KeyCode::Char('c') => { /* toplevel */ },
              _ => {}
          }
      }
      TreeKeyAction::Continue => {}
  }
  ```

**Step 4: Run tests**

Run: `cd /home/virgil/kbtz/.worktrees/unify-tree-view && cargo test -p kbtz-workspace`
Expected: All tests pass.

Run: `cd /home/virgil/kbtz/.worktrees/unify-tree-view && cargo build`
Expected: Clean build for entire workspace.

**Step 5: Commit**

```bash
git add kbtz-workspace/src/
git commit -m "refactor: kbtz-workspace uses shared TreeView and build_tree_items"
```

---

### Task 4: Delete dead code and verify

**Files:**
- Modify: `kbtz/src/tui/tree.rs` — confirm no leftover dead rendering code
- Modify: `kbtz-workspace/src/tree.rs` — confirm no leftover dead rendering code

**Step 1: Check for dead code**

Run: `cd /home/virgil/kbtz/.worktrees/unify-tree-view && cargo build 2>&1 | grep warning`
Expected: No dead code warnings.

**Step 2: Run full test suite**

Run: `cd /home/virgil/kbtz/.worktrees/unify-tree-view && cargo test`
Expected: All tests pass across both crates.

**Step 3: Check line count reduction**

Run: `wc -l kbtz/src/ui.rs kbtz/src/tui/tree.rs kbtz/src/tui/event.rs kbtz/src/tui/app.rs kbtz/src/tui/mod.rs kbtz-workspace/src/tree.rs kbtz-workspace/src/app.rs`
Expected: Net reduction in total line count (shared code in ui.rs, less code in each app).

**Step 4: Commit any cleanup**

```bash
git add -A
git commit -m "chore: remove dead code after tree view unification"
```

---

### Task 5: Open PR

**Step 1: Push branch**

```bash
cd /home/virgil/kbtz/.worktrees/unify-tree-view
git push -u origin unify-tree-view
```

**Step 2: Create PR**

```bash
gh pr create --title "Unify tree view between kbtz watch and kbtz-workspace" --body "$(cat <<'EOF'
## Summary

- Extract shared `TreeView` struct, `TreeMode`/`ActiveTaskPolicy`/`TreeKeyAction` enums, `RowDecoration`/`build_tree_items` rendering, and `render_confirm` dialog into `kbtz::ui`
- Both `kbtz watch` and `kbtz-workspace` now use the shared implementation
- Watch gains `ActiveTaskPolicy::Confirm` support for managed mode
- Consistent blocked-task styling (yellow icon) across both apps
- Net reduction in total code size

## Test plan
- [ ] `cargo test` passes for both crates
- [ ] `kbtz watch` renders tree correctly, navigation works
- [ ] `kbtz-workspace` tree view renders with session indicators
- [ ] Confirm dialogs work in kbtz-workspace (d/p on active task)
- [ ] Help overlay works in both apps

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
