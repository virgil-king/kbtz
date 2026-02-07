use std::collections::HashMap;

use serde::Serialize;

use crate::model::{Note, Task};

#[derive(Serialize)]
pub struct TaskDetail<'a> {
    #[serde(flatten)]
    pub task: &'a Task,
    pub notes: &'a [Note],
    pub blocked_by: &'a [String],
    pub blocks: &'a [String],
}

pub fn format_task_detail(task: &Task, notes: &[Note], blockers: &[String], dependents: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Name:        {}\n", task.name));
    out.push_str(&format!("Status:      {}\n", task.status_str()));
    if let Some(ref p) = task.parent {
        out.push_str(&format!("Parent:      {}\n", p));
    }
    if !task.description.is_empty() {
        out.push_str(&format!("Description: {}\n", task.description));
    }
    if let Some(ref assignee) = task.assignee {
        out.push_str(&format!("Assignee:    {}\n", assignee));
    }
    if let Some(ref assigned_at) = task.assigned_at {
        out.push_str(&format!("Assigned at: {}\n", assigned_at));
    }
    out.push_str(&format!("Created:     {}\n", task.created_at));
    out.push_str(&format!("Updated:     {}\n", task.updated_at));

    if !blockers.is_empty() {
        out.push_str(&format!("Blocked by:  {}\n", blockers.join(", ")));
    }
    if !dependents.is_empty() {
        out.push_str(&format!("Blocks:      {}\n", dependents.join(", ")));
    }

    if !notes.is_empty() {
        out.push('\n');
        out.push_str("Notes:\n");
        for note in notes {
            out.push_str(&format!("  [{}] {}\n", note.created_at, note.content));
        }
    }

    out
}

pub fn format_task_list(tasks: &[Task]) -> String {
    let mut out = String::new();
    for task in tasks {
        let parent_info = task
            .parent
            .as_ref()
            .map(|p| format!(" (parent: {p})"))
            .unwrap_or_default();
        let desc = if task.description.is_empty() {
            String::new()
        } else {
            format!("  {}", task.description)
        };
        out.push_str(&format!(
            "{} {}{}{}\n",
            task.icon(),
            task.name,
            parent_info,
            desc
        ));
    }
    out
}

pub fn format_task_tree(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return String::new();
    }

    // Build parent -> children map
    let mut children_map: HashMap<Option<&str>, Vec<&Task>> = HashMap::new();
    let task_names: std::collections::HashSet<&str> = tasks.iter().map(|t| t.name.as_str()).collect();

    for task in tasks {
        let parent_key = match task.parent.as_deref() {
            Some(p) if task_names.contains(p) => Some(p),
            _ => None,
        };
        children_map.entry(parent_key).or_default().push(task);
    }

    let mut out = String::new();
    let roots = children_map.get(&None).cloned().unwrap_or_default();
    for root in &roots {
        write_tree(&mut out, root, &children_map, "", "");
    }
    out
}

/// Write a task line and recurse into children.
/// `line_prefix` is what goes before the status icon on this task's line.
/// `child_prefix` is the base prefix for this task's children's tree connectors.
fn write_tree(
    out: &mut String,
    task: &Task,
    children_map: &HashMap<Option<&str>, Vec<&Task>>,
    line_prefix: &str,
    child_prefix: &str,
) {
    let desc = if task.description.is_empty() {
        String::new()
    } else {
        format!("  {}", task.description)
    };

    out.push_str(&format!(
        "{}{} {}{}\n",
        line_prefix,
        task.icon(),
        task.name,
        desc
    ));

    let children = children_map
        .get(&Some(task.name.as_str()))
        .cloned()
        .unwrap_or_default();

    for (i, child) in children.iter().enumerate() {
        let is_last = i == children.len() - 1;
        let (connector, extension) = if is_last {
            ("└── ", "    ")
        } else {
            ("├── ", "│   ")
        };
        write_tree(
            out,
            child,
            children_map,
            &format!("{child_prefix}{connector}"),
            &format!("{child_prefix}{extension}"),
        );
    }
}

pub fn format_notes(notes: &[Note]) -> String {
    let mut out = String::new();
    for note in notes {
        out.push_str(&format!("[{}] {}\n", note.created_at, note.content));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(name: &str, parent: Option<&str>, done: bool, assignee: Option<&str>, desc: &str) -> Task {
        Task {
            id: 0,
            name: name.to_string(),
            parent: parent.map(|s| s.to_string()),
            description: desc.to_string(),
            done,
            assignee: assignee.map(|s| s.to_string()),
            assigned_at: assignee.map(|_| "2025-01-01T00:00:00Z".to_string()),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn tree_single_root() {
        let tasks = vec![make_task("root", None, false, Some("agent"), "Root task")];
        let out = format_task_tree(&tasks);
        assert_eq!(out, "* root  Root task\n");
    }

    #[test]
    fn tree_with_children() {
        let tasks = vec![
            make_task("root", None, false, Some("agent"), ""),
            make_task("child1", Some("root"), false, Some("agent"), ""),
            make_task("child2", Some("root"), false, None, ""),
        ];
        let out = format_task_tree(&tasks);
        assert!(out.contains("root"));
        assert!(out.contains("child1"));
        assert!(out.contains("child2"));
        assert!(out.contains("├──"));
        assert!(out.contains("└──"));
    }

    #[test]
    fn flat_list() {
        let tasks = vec![
            make_task("a", None, false, Some("agent"), "desc A"),
            make_task("b", None, false, None, ""),
        ];
        let out = format_task_list(&tasks);
        assert!(out.contains("* a  desc A")); // assigned = active = *
        assert!(out.contains(". b")); // unassigned = open = .
    }
}
