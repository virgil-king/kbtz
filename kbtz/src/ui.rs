//! Shared UI primitives for kbtz tree rendering.
//!
//! Used by both `kbtz watch` (the CLI TUI) and `kbtz-mux` (the multiplexer).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use ratatui::prelude::*;
use rusqlite::Connection;

use crate::model::Task;
use crate::ops;

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
        "done" => "\u{2705} ",    // ✅
        "active" => "\u{1f7e2} ", // 🟢
        "paused" => "\u{23f8}\u{fe0f} ", // ⏸️
        "blocked" => "\u{1f6a7} ", // 🚧
        _ => "\u{26aa} ",          // ⚪
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
        let row = TreeRow {
            name: "t".into(),
            status: "open".into(),
            description: String::new(),
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec!["other".into()],
        };
        assert_eq!(icon_for_task(&row), icon_for_status("blocked"));
    }

    #[test]
    fn icon_for_task_uses_status_when_no_blockers() {
        let row = TreeRow {
            name: "t".into(),
            status: "active".into(),
            description: String::new(),
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        };
        assert_eq!(icon_for_task(&row), icon_for_status("active"));
    }

    // ── tree_prefix ──

    #[test]
    fn tree_prefix_root_is_empty() {
        let row = TreeRow {
            name: "root".into(),
            status: "open".into(),
            description: String::new(),
            depth: 0,
            has_children: false,
            is_last_at_depth: vec![true],
            blocked_by: vec![],
        };
        assert_eq!(tree_prefix(&row), "");
    }

    #[test]
    fn tree_prefix_last_child() {
        let row = TreeRow {
            name: "child".into(),
            status: "open".into(),
            description: String::new(),
            depth: 1,
            has_children: false,
            is_last_at_depth: vec![false, true],
            blocked_by: vec![],
        };
        assert_eq!(tree_prefix(&row), "└── ");
    }

    #[test]
    fn tree_prefix_middle_child() {
        let row = TreeRow {
            name: "child".into(),
            status: "open".into(),
            description: String::new(),
            depth: 1,
            has_children: false,
            is_last_at_depth: vec![false, false],
            blocked_by: vec![],
        };
        assert_eq!(tree_prefix(&row), "├── ");
    }

    #[test]
    fn tree_prefix_nested_depth_2() {
        // Parent is not last, grandchild is last
        let row = TreeRow {
            name: "gc".into(),
            status: "open".into(),
            description: String::new(),
            depth: 2,
            has_children: false,
            is_last_at_depth: vec![false, false, true],
            blocked_by: vec![],
        };
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
}
