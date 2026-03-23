use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use kbtz::model::Task;
use kbtz::ops;

/// A task node in the tree, ready for JSON serialization.
#[derive(Debug, Clone, Serialize)]
pub struct TaskNode {
    pub name: String,
    pub description: String,
    pub status: String,
    pub assignee: Option<String>,
    pub blocked_by: Vec<String>,
    pub children: Vec<TaskNode>,
}

/// Build a full task tree from the database.
/// Returns root-level nodes with children nested recursively.
pub fn build_task_tree(conn: &Connection, include_done: bool) -> Result<Vec<TaskNode>> {
    let tasks = ops::list_tasks(conn, None, include_done, None, None, None)?;
    let blocked = build_blockers_map(conn, &tasks)?;
    let tree = assemble_tree(&tasks, &blocked);
    Ok(tree)
}

fn build_blockers_map(conn: &Connection, tasks: &[Task]) -> Result<HashMap<String, Vec<String>>> {
    let mut map = HashMap::new();
    for task in tasks {
        let blockers = ops::get_blockers(conn, &task.name)?;
        if !blockers.is_empty() {
            map.insert(task.name.clone(), blockers);
        }
    }
    Ok(map)
}

fn assemble_tree(tasks: &[Task], blocked: &HashMap<String, Vec<String>>) -> Vec<TaskNode> {
    let task_names: std::collections::HashSet<&str> =
        tasks.iter().map(|t| t.name.as_str()).collect();

    let mut children_map: HashMap<Option<&str>, Vec<&Task>> = HashMap::new();
    for task in tasks {
        let parent_key = match task.parent.as_deref() {
            Some(p) if task_names.contains(p) => Some(p),
            _ => None,
        };
        children_map.entry(parent_key).or_default().push(task);
    }

    let roots = children_map.get(&None).cloned().unwrap_or_default();
    roots
        .iter()
        .map(|t| build_node(t, &children_map, blocked))
        .collect()
}

fn build_node(
    task: &Task,
    children_map: &HashMap<Option<&str>, Vec<&Task>>,
    blocked: &HashMap<String, Vec<String>>,
) -> TaskNode {
    let children = children_map
        .get(&Some(task.name.as_str()))
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|t| build_node(t, children_map, blocked))
        .collect();

    TaskNode {
        name: task.name.clone(),
        description: task.description.clone(),
        status: task.status.clone(),
        assignee: task.assignee.clone(),
        blocked_by: blocked.get(&task.name).cloned().unwrap_or_default(),
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_tree_roots_only() {
        let tasks = vec![
            Task {
                id: 1,
                name: "a".into(),
                parent: None,
                description: "Task A".into(),
                status: "open".into(),
                assignee: None,
                agent: None,
                directory: None,
                status_changed_at: None,
                created_at: "2026-01-01".into(),
                updated_at: "2026-01-01".into(),
            },
            Task {
                id: 2,
                name: "b".into(),
                parent: None,
                description: "Task B".into(),
                status: "active".into(),
                assignee: Some("ws/1".into()),
                agent: None,
                directory: None,
                status_changed_at: None,
                created_at: "2026-01-01".into(),
                updated_at: "2026-01-01".into(),
            },
        ];
        let blocked = HashMap::new();
        let tree = assemble_tree(&tasks, &blocked);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "a");
        assert_eq!(tree[1].name, "b");
        assert!(tree[0].children.is_empty());
    }

    #[test]
    fn assemble_tree_nesting() {
        let tasks = vec![
            Task {
                id: 1,
                name: "parent".into(),
                parent: None,
                description: "Parent".into(),
                status: "open".into(),
                assignee: None,
                agent: None,
                directory: None,
                status_changed_at: None,
                created_at: "2026-01-01".into(),
                updated_at: "2026-01-01".into(),
            },
            Task {
                id: 2,
                name: "child".into(),
                parent: Some("parent".into()),
                description: "Child".into(),
                status: "open".into(),
                assignee: None,
                agent: None,
                directory: None,
                status_changed_at: None,
                created_at: "2026-01-01".into(),
                updated_at: "2026-01-01".into(),
            },
        ];
        let blocked = HashMap::new();
        let tree = assemble_tree(&tasks, &blocked);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].name, "child");
    }

    #[test]
    fn assemble_tree_blocked_by() {
        let tasks = vec![Task {
            id: 1,
            name: "blocked-task".into(),
            parent: None,
            description: "Blocked".into(),
            status: "open".into(),
            assignee: None,
            agent: None,
            directory: None,
            status_changed_at: None,
            created_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
        }];
        let mut blocked = HashMap::new();
        blocked.insert("blocked-task".to_string(), vec!["blocker".to_string()]);
        let tree = assemble_tree(&tasks, &blocked);
        assert_eq!(tree[0].blocked_by, vec!["blocker"]);
    }
}
