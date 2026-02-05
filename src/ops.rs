use anyhow::{bail, Result};
use rusqlite::Connection;

use crate::model::{Note, Status, Task};
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
    status: Status,
) -> Result<()> {
    validate_name(name)?;
    if task_exists(conn, name)? {
        bail!("task '{name}' already exists");
    }
    if let Some(p) = parent {
        require_task(conn, p)?;
    }
    conn.execute(
        "INSERT INTO tasks (name, parent, description, status) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![name, parent, description, status.as_str()],
    )?;
    Ok(())
}

pub fn edit_task(
    conn: &Connection,
    name: &str,
    desc: Option<&str>,
    status: Option<Status>,
    parent: Option<Option<&str>>,
    rename: Option<&str>,
) -> Result<()> {
    require_task(conn, name)?;

    if let Some(new_name) = rename {
        validate_name(new_name)?;
        if new_name != name && task_exists(conn, new_name)? {
            bail!("task '{new_name}' already exists");
        }
    }

    if let Some(Some(new_parent)) = parent {
        require_task(conn, new_parent)?;
        let check_name = rename.unwrap_or(name);
        if detect_parent_cycle(conn, check_name, new_parent)? {
            bail!("setting parent to '{new_parent}' would create a cycle");
        }
    }

    if let Some(d) = desc {
        conn.execute(
            "UPDATE tasks SET description = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?2",
            rusqlite::params![d, name],
        )?;
    }

    if let Some(s) = status {
        conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?2",
            rusqlite::params![s.as_str(), name],
        )?;
    }

    if let Some(p) = parent {
        conn.execute(
            "UPDATE tasks SET parent = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?2",
            rusqlite::params![p, name],
        )?;
    }

    if let Some(new_name) = rename {
        if new_name != name {
            conn.execute(
                "UPDATE tasks SET name = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?2",
                rusqlite::params![new_name, name],
            )?;
        }
    }

    Ok(())
}

pub fn remove_task(conn: &Connection, name: &str, recursive: bool) -> Result<()> {
    require_task(conn, name)?;

    if recursive {
        // Collect all descendants depth-first, delete from leaves up
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
        "SELECT id, name, parent, description, status, created_at, updated_at FROM tasks WHERE name = ?1",
        [name],
        |row| {
            let status_str: String = row.get(4)?;
            Ok(Task {
                id: row.get(0)?,
                name: row.get(1)?,
                parent: row.get(2)?,
                description: row.get(3)?,
                status: Status::parse(&status_str).unwrap(),
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        },
    )?;
    Ok(task)
}

pub fn list_tasks(
    conn: &Connection,
    status: Option<Status>,
    all: bool,
    root: Option<&str>,
) -> Result<Vec<Task>> {
    if let Some(r) = root {
        require_task(conn, r)?;
    }

    let mut tasks: Vec<Task> = Vec::new();

    if let Some(root_name) = root {
        // Get the root task itself + all descendants
        let root_task = get_task(conn, root_name)?;
        tasks.push(root_task);
        let descendants = collect_descendants(conn, root_name)?;
        for d in &descendants {
            tasks.push(get_task(conn, d)?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, name, parent, description, status, created_at, updated_at FROM tasks ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            let status_str: String = row.get(4)?;
            Ok(Task {
                id: row.get(0)?,
                name: row.get(1)?,
                parent: row.get(2)?,
                description: row.get(3)?,
                status: Status::parse(&status_str).unwrap(),
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        for row in rows {
            tasks.push(row?);
        }
    }

    // Apply status filter
    if !all {
        if let Some(s) = status {
            tasks.retain(|t| t.status == s);
        } else {
            // Default: exclude done tasks
            tasks.retain(|t| t.status != Status::Done);
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
        add_task(&conn, "test-task", None, "A test", Status::Active).unwrap();
        let task = get_task(&conn, "test-task").unwrap();
        assert_eq!(task.name, "test-task");
        assert_eq!(task.description, "A test");
        assert_eq!(task.status, Status::Active);
        assert!(task.parent.is_none());
    }

    #[test]
    fn add_duplicate_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "dup", None, "", Status::Active).unwrap();
        assert!(add_task(&conn, "dup", None, "", Status::Active).is_err());
    }

    #[test]
    fn add_with_parent() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", Status::Active).unwrap();
        add_task(&conn, "child", Some("parent"), "", Status::Active).unwrap();
        let child = get_task(&conn, "child").unwrap();
        assert_eq!(child.parent.as_deref(), Some("parent"));
    }

    #[test]
    fn add_with_missing_parent_fails() {
        let conn = db::open_memory().unwrap();
        assert!(add_task(&conn, "child", Some("nonexistent"), "", Status::Active).is_err());
    }

    #[test]
    fn edit_description() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "old", Status::Active).unwrap();
        edit_task(&conn, "t", Some("new"), None, None, None).unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.description, "new");
    }

    #[test]
    fn edit_status() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", Status::Active).unwrap();
        edit_task(&conn, "t", None, Some(Status::Done), None, None).unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn rename_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "old-name", None, "desc", Status::Active).unwrap();
        add_task(&conn, "child", Some("old-name"), "", Status::Active).unwrap();
        edit_task(&conn, "old-name", None, None, None, Some("new-name")).unwrap();
        // Old name gone
        assert!(get_task(&conn, "old-name").is_err());
        // New name exists with same description
        let task = get_task(&conn, "new-name").unwrap();
        assert_eq!(task.description, "desc");
        // Child's parent updated via CASCADE
        let child = get_task(&conn, "child").unwrap();
        assert_eq!(child.parent.as_deref(), Some("new-name"));
    }

    #[test]
    fn remove_leaf_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", Status::Active).unwrap();
        remove_task(&conn, "t", false).unwrap();
        assert!(get_task(&conn, "t").is_err());
    }

    #[test]
    fn remove_parent_without_recursive_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", Status::Active).unwrap();
        add_task(&conn, "child", Some("parent"), "", Status::Active).unwrap();
        assert!(remove_task(&conn, "parent", false).is_err());
    }

    #[test]
    fn remove_parent_recursive() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", Status::Active).unwrap();
        add_task(&conn, "child", Some("parent"), "", Status::Active).unwrap();
        add_task(&conn, "grandchild", Some("child"), "", Status::Active).unwrap();
        remove_task(&conn, "parent", true).unwrap();
        assert!(get_task(&conn, "parent").is_err());
        assert!(get_task(&conn, "child").is_err());
        assert!(get_task(&conn, "grandchild").is_err());
    }

    #[test]
    fn list_excludes_done_by_default() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "active", None, "", Status::Active).unwrap();
        add_task(&conn, "done", None, "", Status::Done).unwrap();
        let tasks = list_tasks(&conn, None, false, None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "active");
    }

    #[test]
    fn list_all_includes_done() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "active", None, "", Status::Active).unwrap();
        add_task(&conn, "done", None, "", Status::Done).unwrap();
        let tasks = list_tasks(&conn, None, true, None).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn list_filter_status() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", Status::Active).unwrap();
        add_task(&conn, "i", None, "", Status::Idle).unwrap();
        let tasks = list_tasks(&conn, Some(Status::Idle), false, None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "i");
    }

    #[test]
    fn list_with_root() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "root", None, "", Status::Active).unwrap();
        add_task(&conn, "child", Some("root"), "", Status::Active).unwrap();
        add_task(&conn, "other", None, "", Status::Active).unwrap();
        let tasks = list_tasks(&conn, None, false, Some("root")).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn notes_crud() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", Status::Active).unwrap();
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
        add_task(&conn, "a", None, "", Status::Active).unwrap();
        add_task(&conn, "b", None, "", Status::Active).unwrap();
        add_block(&conn, "a", "b").unwrap();
        assert_eq!(get_blockers(&conn, "b").unwrap(), vec!["a"]);
        assert_eq!(get_dependents(&conn, "a").unwrap(), vec!["b"]);
        remove_block(&conn, "a", "b").unwrap();
        assert!(get_blockers(&conn, "b").unwrap().is_empty());
    }

    #[test]
    fn self_block_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", Status::Active).unwrap();
        assert!(add_block(&conn, "a", "a").is_err());
    }

    #[test]
    fn dep_cycle_detected() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", Status::Active).unwrap();
        add_task(&conn, "b", None, "", Status::Active).unwrap();
        add_task(&conn, "c", None, "", Status::Active).unwrap();
        add_block(&conn, "a", "b").unwrap();
        add_block(&conn, "b", "c").unwrap();
        // c -> a would create cycle a->b->c->a
        assert!(add_block(&conn, "c", "a").is_err());
    }

    #[test]
    fn parent_cycle_detected() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", Status::Active).unwrap();
        add_task(&conn, "b", Some("a"), "", Status::Active).unwrap();
        add_task(&conn, "c", Some("b"), "", Status::Active).unwrap();
        // Setting a's parent to c would create a->b->c->a
        let err = edit_task(&conn, "a", None, None, Some(Some("c")), None);
        assert!(err.is_err());
    }

    #[test]
    fn notes_cascade_on_delete() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", Status::Active).unwrap();
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
        add_task(&conn, "a", None, "", Status::Active).unwrap();
        add_task(&conn, "b", None, "", Status::Active).unwrap();
        add_block(&conn, "a", "b").unwrap();
        remove_task(&conn, "a", false).unwrap();
        assert!(get_blockers(&conn, "b").unwrap().is_empty());
    }

    #[test]
    fn rename_cascades_to_deps() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", Status::Active).unwrap();
        add_task(&conn, "b", None, "", Status::Active).unwrap();
        add_block(&conn, "a", "b").unwrap();
        edit_task(&conn, "a", None, None, None, Some("a2")).unwrap();
        assert_eq!(get_blockers(&conn, "b").unwrap(), vec!["a2"]);
    }

    #[test]
    fn rename_cascades_to_notes() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", Status::Active).unwrap();
        add_note(&conn, "t", "hello").unwrap();
        edit_task(&conn, "t", None, None, None, Some("t2")).unwrap();
        let notes = list_notes(&conn, "t2").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "hello");
    }
}
