use anyhow::{bail, Result};
use rusqlite::Connection;

use crate::model::{Note, Task};
use crate::validate::{detect_dep_cycle, detect_parent_cycle, validate_name};

fn task_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM tasks WHERE name = ?1", [name], |row| {
            row.get(0)
        })?;
    Ok(count > 0)
}

fn require_task(conn: &Connection, name: &str) -> Result<()> {
    if !task_exists(conn, name)? {
        bail!("task '{name}' not found");
    }
    Ok(())
}

pub fn add_task(
    conn: &Connection,
    name: &str,
    parent: Option<&str>,
    description: &str,
    note: Option<&str>,
) -> Result<()> {
    validate_name(name)?;
    if task_exists(conn, name)? {
        bail!("task '{name}' already exists");
    }
    if let Some(p) = parent {
        require_task(conn, p)?;
    }
    conn.execute(
        "INSERT INTO tasks (name, parent, description) VALUES (?1, ?2, ?3)",
        rusqlite::params![name, parent, description],
    )?;
    if let Some(content) = note {
        conn.execute(
            "INSERT INTO notes (task, content) VALUES (?1, ?2)",
            rusqlite::params![name, content],
        )?;
    }
    Ok(())
}

pub fn claim_task(conn: &Connection, name: &str, assignee: &str) -> Result<()> {
    require_task(conn, name)?;
    conn.execute(
        "UPDATE tasks SET assignee = ?1, assigned_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?2",
        rusqlite::params![assignee, name],
    )?;
    Ok(())
}

pub fn release_task(conn: &Connection, name: &str, assignee: &str) -> Result<()> {
    require_task(conn, name)?;
    let current_assignee: Option<String> = conn.query_row(
        "SELECT assignee FROM tasks WHERE name = ?1",
        [name],
        |row| row.get(0),
    )?;
    match current_assignee {
        Some(ref a) if a == assignee => {
            conn.execute(
                "UPDATE tasks SET assignee = NULL, assigned_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?1",
                [name],
            )?;
            Ok(())
        }
        Some(a) => bail!("task '{name}' is assigned to '{a}', not '{assignee}'"),
        None => bail!("task '{name}' is not assigned"),
    }
}

pub fn mark_done(conn: &Connection, name: &str) -> Result<()> {
    require_task(conn, name)?;
    conn.execute(
        "UPDATE tasks SET done = 1, assignee = NULL, assigned_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?1",
        [name],
    )?;
    Ok(())
}

pub fn reopen_task(conn: &Connection, name: &str) -> Result<()> {
    require_task(conn, name)?;
    conn.execute(
        "UPDATE tasks SET done = 0, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?1",
        [name],
    )?;
    Ok(())
}

pub fn update_description(conn: &Connection, name: &str, description: &str) -> Result<()> {
    require_task(conn, name)?;
    conn.execute(
        "UPDATE tasks SET description = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?2",
        rusqlite::params![description, name],
    )?;
    Ok(())
}

pub fn reparent_task(conn: &Connection, name: &str, parent: Option<&str>) -> Result<()> {
    require_task(conn, name)?;
    if let Some(new_parent) = parent {
        require_task(conn, new_parent)?;
        if detect_parent_cycle(conn, name, new_parent)? {
            bail!("setting parent to '{new_parent}' would create a cycle");
        }
    }
    conn.execute(
        "UPDATE tasks SET parent = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?2",
        rusqlite::params![parent, name],
    )?;
    Ok(())
}

pub fn remove_task(conn: &Connection, name: &str, recursive: bool) -> Result<()> {
    require_task(conn, name)?;

    if recursive {
        let descendants = collect_descendants(conn, name)?;
        for desc_name in descendants.iter().rev() {
            conn.execute("DELETE FROM tasks WHERE name = ?1", [desc_name])?;
        }
        conn.execute("DELETE FROM tasks WHERE name = ?1", [name])?;
    } else {
        let child_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE parent = ?1",
            [name],
            |row| row.get(0),
        )?;
        if child_count > 0 {
            bail!("task '{name}' has children; use --recursive to remove");
        }
        conn.execute("DELETE FROM tasks WHERE name = ?1", [name])?;
    }
    Ok(())
}

fn collect_descendants(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(name.to_string());
    while let Some(current) = queue.pop_front() {
        let mut stmt = conn.prepare_cached("SELECT name FROM tasks WHERE parent = ?1")?;
        let children: Vec<String> = stmt
            .query_map([&current], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for child in children {
            result.push(child.clone());
            queue.push_back(child);
        }
    }
    Ok(result)
}

pub fn get_task(conn: &Connection, name: &str) -> Result<Task> {
    require_task(conn, name)?;
    let task = conn.query_row(
        "SELECT id, name, parent, description, done, assignee, assigned_at, created_at, updated_at FROM tasks WHERE name = ?1",
        [name],
        |row| {
            Ok(Task {
                id: row.get(0)?,
                name: row.get(1)?,
                parent: row.get(2)?,
                description: row.get(3)?,
                done: row.get::<_, i64>(4)? != 0,
                assignee: row.get(5)?,
                assigned_at: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        },
    )?;
    Ok(task)
}

/// Status filter for list_tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Open,   // not done, no assignee
    Active, // not done, has assignee
    Done,   // done
}

impl StatusFilter {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "active" => Ok(Self::Active),
            "done" => Ok(Self::Done),
            _ => bail!("invalid status '{s}': must be open, active, or done"),
        }
    }

    fn matches(&self, task: &Task) -> bool {
        match self {
            Self::Open => !task.done && task.assignee.is_none(),
            Self::Active => !task.done && task.assignee.is_some(),
            Self::Done => task.done,
        }
    }
}

pub fn list_tasks(
    conn: &Connection,
    status: Option<StatusFilter>,
    all: bool,
    root: Option<&str>,
) -> Result<Vec<Task>> {
    if let Some(r) = root {
        require_task(conn, r)?;
    }

    let mut tasks: Vec<Task> = Vec::new();

    if let Some(root_name) = root {
        let root_task = get_task(conn, root_name)?;
        tasks.push(root_task);
        let descendants = collect_descendants(conn, root_name)?;
        for d in &descendants {
            tasks.push(get_task(conn, d)?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, name, parent, description, done, assignee, assigned_at, created_at, updated_at FROM tasks ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Task {
                id: row.get(0)?,
                name: row.get(1)?,
                parent: row.get(2)?,
                description: row.get(3)?,
                done: row.get::<_, i64>(4)? != 0,
                assignee: row.get(5)?,
                assigned_at: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })?;
        for row in rows {
            tasks.push(row?);
        }
    }

    // Apply status filter
    if !all {
        if let Some(s) = status {
            tasks.retain(|t| s.matches(t));
        } else {
            // Default: exclude done tasks
            tasks.retain(|t| !t.done);
        }
    }

    Ok(tasks)
}

pub fn add_note(conn: &Connection, task_name: &str, content: &str) -> Result<()> {
    require_task(conn, task_name)?;
    conn.execute(
        "INSERT INTO notes (task, content) VALUES (?1, ?2)",
        rusqlite::params![task_name, content],
    )?;
    Ok(())
}

pub fn list_notes(conn: &Connection, task_name: &str) -> Result<Vec<Note>> {
    require_task(conn, task_name)?;
    let mut stmt = conn.prepare(
        "SELECT id, task, content, created_at FROM notes WHERE task = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([task_name], |row| {
        Ok(Note {
            id: row.get(0)?,
            task: row.get(1)?,
            content: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn add_block(conn: &Connection, blocker: &str, blocked: &str) -> Result<()> {
    require_task(conn, blocker)?;
    require_task(conn, blocked)?;
    if blocker == blocked {
        bail!("a task cannot block itself");
    }
    if detect_dep_cycle(conn, blocker, blocked)? {
        bail!("adding this dependency would create a cycle");
    }
    conn.execute(
        "INSERT INTO task_deps (blocker, blocked) VALUES (?1, ?2)",
        rusqlite::params![blocker, blocked],
    )?;
    Ok(())
}

pub fn remove_block(conn: &Connection, blocker: &str, blocked: &str) -> Result<()> {
    require_task(conn, blocker)?;
    require_task(conn, blocked)?;
    let changed = conn.execute(
        "DELETE FROM task_deps WHERE blocker = ?1 AND blocked = ?2",
        rusqlite::params![blocker, blocked],
    )?;
    if changed == 0 {
        bail!("'{blocker}' is not blocking '{blocked}'");
    }
    Ok(())
}

pub fn get_blockers(conn: &Connection, task_name: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare_cached("SELECT blocker FROM task_deps WHERE blocked = ?1 ORDER BY blocker")?;
    let rows = stmt.query_map([task_name], |row| row.get(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get_dependents(conn: &Connection, task_name: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare_cached("SELECT blocked FROM task_deps WHERE blocker = ?1 ORDER BY blocked")?;
    let rows = stmt.query_map([task_name], |row| row.get(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn add_and_get_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "test-task", None, "A test", None).unwrap();
        let task = get_task(&conn, "test-task").unwrap();
        assert_eq!(task.name, "test-task");
        assert_eq!(task.description, "A test");
        assert!(!task.done);
        assert!(task.assignee.is_none());
        assert!(task.parent.is_none());
    }

    #[test]
    fn add_duplicate_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "dup", None, "", None).unwrap();
        assert!(add_task(&conn, "dup", None, "", None).is_err());
    }

    #[test]
    fn add_with_parent() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", None).unwrap();
        add_task(&conn, "child", Some("parent"), "", None).unwrap();
        let child = get_task(&conn, "child").unwrap();
        assert_eq!(child.parent.as_deref(), Some("parent"));
    }

    #[test]
    fn add_with_missing_parent_fails() {
        let conn = db::open_memory().unwrap();
        assert!(add_task(&conn, "child", Some("nonexistent"), "", None).is_err());
    }

    #[test]
    fn claim_and_release() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None).unwrap();

        // Initially unassigned
        let task = get_task(&conn, "t").unwrap();
        assert!(task.assignee.is_none());

        // Claim it
        claim_task(&conn, "t", "agent-123").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.assignee.as_deref(), Some("agent-123"));
        assert!(task.assigned_at.is_some());

        // Release with wrong assignee fails
        assert!(release_task(&conn, "t", "wrong-agent").is_err());

        // Release with correct assignee
        release_task(&conn, "t", "agent-123").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert!(task.assignee.is_none());
        assert!(task.assigned_at.is_none());
    }

    #[test]
    fn done_and_reopen() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None).unwrap();
        claim_task(&conn, "t", "agent").unwrap();

        // Mark done clears assignee
        mark_done(&conn, "t").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert!(task.done);
        assert!(task.assignee.is_none());

        // Reopen
        reopen_task(&conn, "t").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert!(!task.done);
    }

    #[test]
    fn update_description_works() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "old", None).unwrap();
        update_description(&conn, "t", "new").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.description, "new");
    }

    #[test]
    fn reparent_works() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None).unwrap();
        add_task(&conn, "b", None, "", None).unwrap();
        reparent_task(&conn, "b", Some("a")).unwrap();
        let task = get_task(&conn, "b").unwrap();
        assert_eq!(task.parent.as_deref(), Some("a"));

        // Clear parent
        reparent_task(&conn, "b", None).unwrap();
        let task = get_task(&conn, "b").unwrap();
        assert!(task.parent.is_none());
    }

    #[test]
    fn remove_leaf_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None).unwrap();
        remove_task(&conn, "t", false).unwrap();
        assert!(get_task(&conn, "t").is_err());
    }

    #[test]
    fn remove_parent_without_recursive_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", None).unwrap();
        add_task(&conn, "child", Some("parent"), "", None).unwrap();
        assert!(remove_task(&conn, "parent", false).is_err());
    }

    #[test]
    fn remove_parent_recursive() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", None).unwrap();
        add_task(&conn, "child", Some("parent"), "", None).unwrap();
        add_task(&conn, "grandchild", Some("child"), "", None).unwrap();
        remove_task(&conn, "parent", true).unwrap();
        assert!(get_task(&conn, "parent").is_err());
        assert!(get_task(&conn, "child").is_err());
        assert!(get_task(&conn, "grandchild").is_err());
    }

    #[test]
    fn list_excludes_done_by_default() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "open", None, "", None).unwrap();
        add_task(&conn, "done", None, "", None).unwrap();
        mark_done(&conn, "done").unwrap();
        let tasks = list_tasks(&conn, None, false, None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "open");
    }

    #[test]
    fn list_all_includes_done() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "open", None, "", None).unwrap();
        add_task(&conn, "done", None, "", None).unwrap();
        mark_done(&conn, "done").unwrap();
        let tasks = list_tasks(&conn, None, true, None).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn list_filter_status() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "open", None, "", None).unwrap();
        add_task(&conn, "active", None, "", None).unwrap();
        claim_task(&conn, "active", "agent").unwrap();

        let open_tasks = list_tasks(&conn, Some(StatusFilter::Open), false, None).unwrap();
        assert_eq!(open_tasks.len(), 1);
        assert_eq!(open_tasks[0].name, "open");

        let active_tasks = list_tasks(&conn, Some(StatusFilter::Active), false, None).unwrap();
        assert_eq!(active_tasks.len(), 1);
        assert_eq!(active_tasks[0].name, "active");
    }

    #[test]
    fn list_with_root() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "root", None, "", None).unwrap();
        add_task(&conn, "child", Some("root"), "", None).unwrap();
        add_task(&conn, "other", None, "", None).unwrap();
        let tasks = list_tasks(&conn, None, false, Some("root")).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn notes_crud() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None).unwrap();
        add_note(&conn, "t", "note 1").unwrap();
        add_note(&conn, "t", "note 2").unwrap();
        let notes = list_notes(&conn, "t").unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].content, "note 1");
        assert_eq!(notes[1].content, "note 2");
    }

    #[test]
    fn blocking_relationships() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None).unwrap();
        add_task(&conn, "b", None, "", None).unwrap();
        add_block(&conn, "a", "b").unwrap();
        assert_eq!(get_blockers(&conn, "b").unwrap(), vec!["a"]);
        assert_eq!(get_dependents(&conn, "a").unwrap(), vec!["b"]);
        remove_block(&conn, "a", "b").unwrap();
        assert!(get_blockers(&conn, "b").unwrap().is_empty());
    }

    #[test]
    fn self_block_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None).unwrap();
        assert!(add_block(&conn, "a", "a").is_err());
    }

    #[test]
    fn dep_cycle_detected() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None).unwrap();
        add_task(&conn, "b", None, "", None).unwrap();
        add_task(&conn, "c", None, "", None).unwrap();
        add_block(&conn, "a", "b").unwrap();
        add_block(&conn, "b", "c").unwrap();
        assert!(add_block(&conn, "c", "a").is_err());
    }

    #[test]
    fn parent_cycle_detected() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None).unwrap();
        add_task(&conn, "b", Some("a"), "", None).unwrap();
        add_task(&conn, "c", Some("b"), "", None).unwrap();
        assert!(reparent_task(&conn, "a", Some("c")).is_err());
    }

    #[test]
    fn notes_cascade_on_delete() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None).unwrap();
        add_note(&conn, "t", "a note").unwrap();
        remove_task(&conn, "t", false).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn deps_cascade_on_delete() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None).unwrap();
        add_task(&conn, "b", None, "", None).unwrap();
        add_block(&conn, "a", "b").unwrap();
        remove_task(&conn, "a", false).unwrap();
        assert!(get_blockers(&conn, "b").unwrap().is_empty());
    }
}
