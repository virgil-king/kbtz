use std::collections::HashMap;

use anyhow::{bail, Result};
use rusqlite::Connection;
use rusqlite::OptionalExtension;

use crate::model::{Note, SearchResult, Task};
use crate::validate::{detect_dep_cycle, detect_parent_cycle, validate_name};

fn task_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn require_task(conn: &Connection, name: &str) -> Result<()> {
    if !task_exists(conn, name)? {
        bail!("task '{name}' not found");
    }
    Ok(())
}

fn read_task_row(row: &rusqlite::Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        name: row.get(1)?,
        parent: row.get(2)?,
        description: row.get(3)?,
        status: row.get(4)?,
        assignee: row.get(5)?,
        status_changed_at: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

const TASK_COLUMNS: &str =
    "id, name, parent, description, status, assignee, status_changed_at, created_at, updated_at";

const INSERT_TASK: &str = "
INSERT INTO tasks (name, parent, description, status, assignee, status_changed_at)
VALUES (?1, ?2, ?3, ?4, ?5,
    CASE WHEN ?4 != 'open' THEN strftime('%Y-%m-%dT%H:%M:%SZ', 'now') END)
";

const CLAIM_OPEN: &str = "
UPDATE tasks
SET status = 'active', assignee = ?1,
    status_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?2 AND status = 'open'
";

const RECLAIM_ACTIVE: &str = "
UPDATE tasks
SET status_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?2 AND status = 'active' AND assignee = ?1
";

const REASSIGN_ACTIVE: &str = "
UPDATE tasks
SET assignee = ?1,
    status_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?2 AND status = 'active'
";

const RELEASE_TO_OPEN: &str = "
UPDATE tasks
SET status = 'open', assignee = NULL,
    status_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?1
";

const SET_DONE: &str = "
UPDATE tasks
SET status = 'done', assignee = NULL,
    status_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?1
";

const SET_PAUSED: &str = "
UPDATE tasks
SET status = 'paused', assignee = NULL,
    status_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?1
";

const SET_OPEN: &str = "
UPDATE tasks
SET status = 'open',
    status_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?1
";

const SET_DESCRIPTION: &str = "
UPDATE tasks
SET description = ?1,
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?2
";

const SET_PARENT: &str = "
UPDATE tasks
SET parent = ?1,
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE name = ?2
";

pub fn add_task(
    conn: &Connection,
    name: &str,
    parent: Option<&str>,
    description: &str,
    note: Option<&str>,
    claim: Option<&str>,
    paused: bool,
) -> Result<()> {
    validate_name(name)?;
    if paused && claim.is_some() {
        bail!("--paused and --claim are mutually exclusive");
    }
    if task_exists(conn, name)? {
        bail!("task '{name}' already exists");
    }
    if let Some(p) = parent {
        require_task(conn, p)?;
    }
    let status = if paused {
        "paused"
    } else if claim.is_some() {
        "active"
    } else {
        "open"
    };
    conn.execute(
        INSERT_TASK,
        rusqlite::params![name, parent, description, status, claim],
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
    let rows = conn.execute(CLAIM_OPEN, rusqlite::params![assignee, name])?;
    if rows == 0 {
        // Also allow idempotent re-claim by same assignee
        let rows = conn.execute(RECLAIM_ACTIVE, rusqlite::params![assignee, name])?;
        if rows > 0 {
            return Ok(());
        }

        let (status, current_assignee): (String, Option<String>) = conn.query_row(
            "SELECT status, assignee FROM tasks WHERE name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        match status.as_str() {
            "active" => {
                let a = current_assignee.unwrap();
                bail!("task '{name}' is already claimed by '{a}'");
            }
            "paused" => bail!("task '{name}' is paused"),
            "done" => bail!("task '{name}' is done"),
            _ => bail!("task '{name}' could not be claimed"),
        }
    }
    Ok(())
}

/// Sanitize free-form text into an FTS5 query: split on whitespace, quote each
/// word, join with OR. Returns None if no words remain after filtering.
fn sanitize_fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| {
            let cleaned: String = w.chars().filter(|c| *c != '"').collect();
            format!("\"{}\"", cleaned)
        })
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(words.join(" OR "))
}

const CLAIM_NEXT_WITH_PREFER: &str = "
SELECT t.name FROM tasks t
LEFT JOIN (
    SELECT rowid, rank FROM tasks_fts
    WHERE tasks_fts MATCH ?1
      AND rowid IN (SELECT id FROM tasks WHERE status = 'open')
) tfts ON tfts.rowid = t.id
LEFT JOIN (
    SELECT n.task, MIN(nfts.rank) as best_rank
    FROM notes_fts nfts
    JOIN notes n ON n.id = nfts.rowid
    JOIN tasks t2 ON t2.name = n.task AND t2.status = 'open'
    WHERE notes_fts MATCH ?1
    GROUP BY n.task
) nfts ON nfts.task = t.name
LEFT JOIN (
    SELECT td.blocker, COUNT(*) as cnt FROM task_deps td
    INNER JOIN tasks bt ON bt.name = td.blocked AND bt.status NOT IN ('done')
    GROUP BY td.blocker
) uc ON uc.blocker = t.name
WHERE t.status = 'open'
  AND NOT EXISTS (
      SELECT 1 FROM task_deps td2
      INNER JOIN tasks bt2 ON bt2.name = td2.blocker AND bt2.status NOT IN ('done')
      WHERE td2.blocked = t.name
  )
ORDER BY
    CASE WHEN tfts.rank IS NOT NULL OR nfts.best_rank IS NOT NULL THEN 0 ELSE 1 END,
    MIN(COALESCE(tfts.rank, 0), COALESCE(nfts.best_rank, 0)),
    COALESCE(uc.cnt, 0) DESC,
    t.id ASC
LIMIT 1
";

const CLAIM_NEXT_NO_PREFER: &str = "
SELECT t.name FROM tasks t
LEFT JOIN (
    SELECT td.blocker, COUNT(*) as cnt FROM task_deps td
    INNER JOIN tasks bt ON bt.name = td.blocked AND bt.status NOT IN ('done')
    GROUP BY td.blocker
) uc ON uc.blocker = t.name
WHERE t.status = 'open'
  AND NOT EXISTS (
      SELECT 1 FROM task_deps td2
      INNER JOIN tasks bt2 ON bt2.name = td2.blocker AND bt2.status NOT IN ('done')
      WHERE td2.blocked = t.name
  )
ORDER BY
    COALESCE(uc.cnt, 0) DESC,
    t.id ASC
LIMIT 1
";

pub fn claim_next_task(
    conn: &Connection,
    assignee: &str,
    prefer: Option<&str>,
) -> Result<Option<String>> {
    // Use SAVEPOINT instead of BEGIN IMMEDIATE so this works both standalone
    // and nested inside an existing transaction (e.g. `exec` batch).
    conn.execute_batch("SAVEPOINT claim_next")?;

    let result = (|| -> Result<Option<String>> {
        let fts_query = prefer.and_then(sanitize_fts_query);

        let task_name: Option<String> = match fts_query {
            Some(ref q) => conn
                .query_row(CLAIM_NEXT_WITH_PREFER, [q], |row| row.get(0))
                .optional()?,
            None => conn
                .query_row(CLAIM_NEXT_NO_PREFER, [], |row| row.get(0))
                .optional()?,
        };

        let Some(name) = task_name else {
            return Ok(None);
        };

        let rows = conn.execute(CLAIM_OPEN, rusqlite::params![assignee, name])?;

        if rows == 0 {
            // Another writer claimed it between our SELECT and UPDATE
            return Ok(None);
        }

        Ok(Some(name))
    })();

    match result {
        Ok(v) => {
            conn.execute_batch("RELEASE claim_next")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO claim_next");
            let _ = conn.execute_batch("RELEASE claim_next");
            Err(e)
        }
    }
}

pub fn steal_task(conn: &Connection, name: &str, new_assignee: &str) -> Result<String> {
    require_task(conn, name)?;
    let (status, current_assignee): (String, Option<String>) = conn.query_row(
        "SELECT status, assignee FROM tasks WHERE name = ?1",
        [name],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if status != "active" {
        bail!("task '{name}' is not active (status: {status})");
    }
    let prev = current_assignee.unwrap();
    conn.execute(REASSIGN_ACTIVE, rusqlite::params![new_assignee, name])?;
    Ok(prev)
}

pub fn release_task(conn: &Connection, name: &str, assignee: &str) -> Result<()> {
    require_task(conn, name)?;
    let (status, current_assignee): (String, Option<String>) = conn.query_row(
        "SELECT status, assignee FROM tasks WHERE name = ?1",
        [name],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    match (status.as_str(), current_assignee) {
        ("active", Some(ref a)) if a == assignee => {
            conn.execute(RELEASE_TO_OPEN, [name])?;
            Ok(())
        }
        ("active", Some(a)) => bail!("task '{name}' is assigned to '{a}', not '{assignee}'"),
        _ => bail!("task '{name}' is not assigned"),
    }
}

pub fn mark_done(conn: &Connection, name: &str) -> Result<()> {
    require_task(conn, name)?;
    conn.execute(SET_DONE, [name])?;
    Ok(())
}

pub fn force_unassign_task(conn: &Connection, name: &str) -> Result<()> {
    require_task(conn, name)?;
    let status: String =
        conn.query_row("SELECT status FROM tasks WHERE name = ?1", [name], |row| {
            row.get(0)
        })?;
    if status != "active" {
        bail!("task '{name}' is not active (status: {status})");
    }
    conn.execute(RELEASE_TO_OPEN, [name])?;
    Ok(())
}

pub fn reopen_task(conn: &Connection, name: &str) -> Result<()> {
    require_task(conn, name)?;
    let status: String =
        conn.query_row("SELECT status FROM tasks WHERE name = ?1", [name], |row| {
            row.get(0)
        })?;
    if status != "done" {
        bail!("task '{name}' is not done (status: {status})");
    }
    conn.execute(RELEASE_TO_OPEN, [name])?;
    Ok(())
}

pub fn pause_task(conn: &Connection, name: &str) -> Result<()> {
    require_task(conn, name)?;
    let status: String =
        conn.query_row("SELECT status FROM tasks WHERE name = ?1", [name], |row| {
            row.get(0)
        })?;
    if status == "done" {
        bail!("task '{name}' is done");
    }
    if status == "paused" {
        bail!("task '{name}' is already paused");
    }
    conn.execute(SET_PAUSED, [name])?;
    Ok(())
}

pub fn unpause_task(conn: &Connection, name: &str) -> Result<()> {
    require_task(conn, name)?;
    let status: String =
        conn.query_row("SELECT status FROM tasks WHERE name = ?1", [name], |row| {
            row.get(0)
        })?;
    if status != "paused" {
        bail!("task '{name}' is not paused");
    }
    conn.execute(SET_OPEN, [name])?;
    Ok(())
}

pub fn update_description(conn: &Connection, name: &str, description: &str) -> Result<()> {
    require_task(conn, name)?;
    conn.execute(SET_DESCRIPTION, rusqlite::params![description, name])?;
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
    conn.execute(SET_PARENT, rusqlite::params![parent, name])?;
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
    let query = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE name = ?1");
    let task = conn.query_row(&query, [name], read_task_row)?;
    Ok(task)
}

/// Status filter for list_tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Open,
    Active,
    Paused,
    Done,
}

impl StatusFilter {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            _ => bail!("invalid status '{s}': must be open, active, paused, or done"),
        }
    }

    fn matches(&self, task: &Task) -> bool {
        match self {
            Self::Open => task.status == "open",
            Self::Active => task.status == "active",
            Self::Paused => task.status == "paused",
            Self::Done => task.status == "done",
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
        let query = format!("SELECT {TASK_COLUMNS} FROM tasks ORDER BY id");
        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map([], read_task_row)?;
        for row in rows {
            tasks.push(row?);
        }
    }

    // Apply status filter
    if !all {
        if let Some(s) = status {
            tasks.retain(|t| s.matches(t));
        } else {
            // Default: exclude done and paused tasks
            tasks.retain(|t| t.status != "done" && t.status != "paused");
        }
    }

    Ok(tasks)
}

pub fn list_children(
    conn: &Connection,
    parent: &str,
    status: Option<StatusFilter>,
    all: bool,
) -> Result<Vec<Task>> {
    require_task(conn, parent)?;
    let query = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE parent = ?1 ORDER BY id");
    let mut stmt = conn.prepare(&query)?;
    let rows = stmt.query_map([parent], read_task_row)?;
    let mut tasks: Vec<Task> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    if !all {
        if let Some(s) = status {
            tasks.retain(|t| s.matches(t));
        } else {
            tasks.retain(|t| t.status != "done" && t.status != "paused");
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
    let mut stmt = conn
        .prepare("SELECT id, task, content, created_at FROM notes WHERE task = ?1 ORDER BY id")?;
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

const SEARCH_TASKS: &str = "
SELECT DISTINCT t.id, t.name, t.parent, t.description, t.status,
       t.assignee, t.status_changed_at, t.created_at, t.updated_at,
       CASE WHEN tfts.rowid IS NOT NULL THEN 1 ELSE 0 END as task_match,
       CASE WHEN nfts.task IS NOT NULL THEN 1 ELSE 0 END as note_match,
       COALESCE(MIN(COALESCE(tfts.rank, 0), COALESCE(nfts.best_rank, 0)), 0) as best_rank
FROM tasks t
LEFT JOIN (
    SELECT rowid, rank FROM tasks_fts WHERE tasks_fts MATCH ?1
) tfts ON tfts.rowid = t.id
LEFT JOIN (
    SELECT n.task, MIN(nfts2.rank) as best_rank
    FROM notes_fts nfts2
    JOIN notes n ON n.id = nfts2.rowid
    WHERE notes_fts MATCH ?1
    GROUP BY n.task
) nfts ON nfts.task = t.name
WHERE tfts.rowid IS NOT NULL OR nfts.task IS NOT NULL
ORDER BY best_rank ASC, t.id ASC
";

pub fn search_tasks(conn: &Connection, query: &str) -> Result<Vec<SearchResult>> {
    let fts_query = sanitize_fts_query(query);
    let Some(fts_query) = fts_query else {
        bail!("empty search query");
    };

    let mut stmt = conn.prepare(SEARCH_TASKS)?;
    let rows = stmt.query_map([&fts_query], |row| {
        let task = read_task_row(row)?;
        let task_match: bool = row.get(9)?;
        let note_match: bool = row.get(10)?;
        let mut matched_in = Vec::new();
        if task_match {
            matched_in.push("task".to_string());
        }
        if note_match {
            matched_in.push("notes".to_string());
        }
        Ok(SearchResult { task, matched_in })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get_blockers(conn: &Connection, task_name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT td.blocker FROM task_deps td \
         INNER JOIN tasks t ON t.name = td.blocker AND t.status != 'done' \
         WHERE td.blocked = ?1 ORDER BY td.blocker",
    )?;
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

/// (blocked_by, blocks) for a single task.
pub type TaskDeps = (Vec<String>, Vec<String>);

/// Batch-fetch blocked_by and blocks for all tasks in two queries.
/// Returns a map from task name to (blocked_by, blocks).
pub fn get_all_deps(conn: &Connection) -> Result<HashMap<String, TaskDeps>> {
    let mut map: HashMap<String, TaskDeps> = HashMap::new();

    // blocked_by: for each blocked task, which non-done tasks block it
    let mut stmt = conn.prepare(
        "SELECT td.blocked, td.blocker FROM task_deps td \
         INNER JOIN tasks t ON t.name = td.blocker AND t.status != 'done' \
         ORDER BY td.blocked, td.blocker",
    )?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (blocked, blocker) = row?;
        map.entry(blocked).or_default().0.push(blocker);
    }

    // blocks: for each blocker task, which tasks does it block
    let mut stmt =
        conn.prepare("SELECT blocker, blocked FROM task_deps ORDER BY blocker, blocked")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (blocker, blocked) = row?;
        map.entry(blocker).or_default().1.push(blocked);
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn add_and_get_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "test-task", None, "A test", None, None, false).unwrap();
        let task = get_task(&conn, "test-task").unwrap();
        assert_eq!(task.name, "test-task");
        assert_eq!(task.description, "A test");
        assert_eq!(task.status, "open");
        assert!(task.assignee.is_none());
        assert!(task.parent.is_none());
    }

    #[test]
    fn add_duplicate_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "dup", None, "", None, None, false).unwrap();
        assert!(add_task(&conn, "dup", None, "", None, None, false).is_err());
    }

    #[test]
    fn add_with_parent() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", None, None, false).unwrap();
        add_task(&conn, "child", Some("parent"), "", None, None, false).unwrap();
        let child = get_task(&conn, "child").unwrap();
        assert_eq!(child.parent.as_deref(), Some("parent"));
    }

    #[test]
    fn add_with_missing_parent_fails() {
        let conn = db::open_memory().unwrap();
        assert!(add_task(&conn, "child", Some("nonexistent"), "", None, None, false).is_err());
    }

    #[test]
    fn claim_and_release() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();

        // Initially unassigned
        let task = get_task(&conn, "t").unwrap();
        assert!(task.assignee.is_none());

        // Claim it
        claim_task(&conn, "t", "agent-123").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.assignee.as_deref(), Some("agent-123"));
        assert_eq!(task.status, "active");
        assert!(task.status_changed_at.is_some());

        // Release with wrong assignee fails
        assert!(release_task(&conn, "t", "wrong-agent").is_err());

        // Release with correct assignee
        release_task(&conn, "t", "agent-123").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert!(task.assignee.is_none());
        assert_eq!(task.status, "open");
    }

    #[test]
    fn done_and_reopen() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        claim_task(&conn, "t", "agent").unwrap();

        // Mark done clears assignee
        mark_done(&conn, "t").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.status, "done");
        assert!(task.assignee.is_none());

        // Reopen
        reopen_task(&conn, "t").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.status, "open");
    }

    #[test]
    fn reopen_open_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        let err = reopen_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("not done"),
            "expected 'not done' error: {err}"
        );
    }

    #[test]
    fn reopen_active_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        claim_task(&conn, "t", "agent").unwrap();
        let err = reopen_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("not done"),
            "expected 'not done' error: {err}"
        );
    }

    #[test]
    fn reopen_paused_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        pause_task(&conn, "t").unwrap();
        let err = reopen_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("not done"),
            "expected 'not done' error: {err}"
        );
    }

    #[test]
    fn update_description_works() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "old", None, None, false).unwrap();
        update_description(&conn, "t", "new").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.description, "new");
    }

    #[test]
    fn reparent_works() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        add_task(&conn, "b", None, "", None, None, false).unwrap();
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
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        remove_task(&conn, "t", false).unwrap();
        assert!(get_task(&conn, "t").is_err());
    }

    #[test]
    fn remove_parent_without_recursive_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", None, None, false).unwrap();
        add_task(&conn, "child", Some("parent"), "", None, None, false).unwrap();
        assert!(remove_task(&conn, "parent", false).is_err());
    }

    #[test]
    fn remove_parent_recursive() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "parent", None, "", None, None, false).unwrap();
        add_task(&conn, "child", Some("parent"), "", None, None, false).unwrap();
        add_task(&conn, "grandchild", Some("child"), "", None, None, false).unwrap();
        remove_task(&conn, "parent", true).unwrap();
        assert!(get_task(&conn, "parent").is_err());
        assert!(get_task(&conn, "child").is_err());
        assert!(get_task(&conn, "grandchild").is_err());
    }

    #[test]
    fn list_excludes_done_by_default() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "open", None, "", None, None, false).unwrap();
        add_task(&conn, "done", None, "", None, None, false).unwrap();
        mark_done(&conn, "done").unwrap();
        let tasks = list_tasks(&conn, None, false, None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "open");
    }

    #[test]
    fn list_all_includes_done() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "open", None, "", None, None, false).unwrap();
        add_task(&conn, "done", None, "", None, None, false).unwrap();
        mark_done(&conn, "done").unwrap();
        let tasks = list_tasks(&conn, None, true, None).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn list_filter_status() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "open", None, "", None, None, false).unwrap();
        add_task(&conn, "active", None, "", None, None, false).unwrap();
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
        add_task(&conn, "root", None, "", None, None, false).unwrap();
        add_task(&conn, "child", Some("root"), "", None, None, false).unwrap();
        add_task(&conn, "other", None, "", None, None, false).unwrap();
        let tasks = list_tasks(&conn, None, false, Some("root")).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn notes_crud() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
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
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        add_task(&conn, "b", None, "", None, None, false).unwrap();
        add_block(&conn, "a", "b").unwrap();
        assert_eq!(get_blockers(&conn, "b").unwrap(), vec!["a"]);
        assert_eq!(get_dependents(&conn, "a").unwrap(), vec!["b"]);
        remove_block(&conn, "a", "b").unwrap();
        assert!(get_blockers(&conn, "b").unwrap().is_empty());
    }

    #[test]
    fn self_block_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        assert!(add_block(&conn, "a", "a").is_err());
    }

    #[test]
    fn claim_already_claimed_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        claim_task(&conn, "t", "agent-1").unwrap();
        let err = claim_task(&conn, "t", "agent-2").unwrap_err();
        assert!(
            err.to_string().contains("already claimed by 'agent-1'"),
            "unexpected error: {err}"
        );
        // Original assignee unchanged
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.assignee.as_deref(), Some("agent-1"));
    }

    #[test]
    fn claim_idempotent_for_same_assignee() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        claim_task(&conn, "t", "agent-1").unwrap();
        // Re-claiming by same assignee succeeds
        claim_task(&conn, "t", "agent-1").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.assignee.as_deref(), Some("agent-1"));
    }

    #[test]
    fn dep_cycle_detected() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        add_task(&conn, "b", None, "", None, None, false).unwrap();
        add_task(&conn, "c", None, "", None, None, false).unwrap();
        add_block(&conn, "a", "b").unwrap();
        add_block(&conn, "b", "c").unwrap();
        assert!(add_block(&conn, "c", "a").is_err());
    }

    #[test]
    fn parent_cycle_detected() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        add_task(&conn, "b", Some("a"), "", None, None, false).unwrap();
        add_task(&conn, "c", Some("b"), "", None, None, false).unwrap();
        assert!(reparent_task(&conn, "a", Some("c")).is_err());
    }

    #[test]
    fn notes_cascade_on_delete() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
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
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        add_task(&conn, "b", None, "", None, None, false).unwrap();
        add_block(&conn, "a", "b").unwrap();
        remove_task(&conn, "a", false).unwrap();
        assert!(get_blockers(&conn, "b").unwrap().is_empty());
    }

    #[test]
    fn claim_next_no_tasks() {
        let conn = db::open_memory().unwrap();
        assert_eq!(claim_next_task(&conn, "agent", None).unwrap(), None);
    }

    #[test]
    fn claim_next_picks_oldest() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "second", None, "", None, None, false).unwrap();
        add_task(&conn, "third", None, "", None, None, false).unwrap();
        // "second" has lower id, should be picked first
        let picked = claim_next_task(&conn, "agent", None).unwrap();
        assert_eq!(picked.as_deref(), Some("second"));
    }

    #[test]
    fn claim_next_skips_done_and_assigned() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "done-task", None, "", None, None, false).unwrap();
        mark_done(&conn, "done-task").unwrap();
        add_task(&conn, "claimed-task", None, "", None, None, false).unwrap();
        claim_task(&conn, "claimed-task", "other-agent").unwrap();
        add_task(&conn, "available", None, "", None, None, false).unwrap();

        let picked = claim_next_task(&conn, "agent", None).unwrap();
        assert_eq!(picked.as_deref(), Some("available"));
    }

    #[test]
    fn claim_next_skips_blocked_tasks() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "blocker", None, "", None, None, false).unwrap();
        add_task(&conn, "blocked", None, "", None, None, false).unwrap();
        add_block(&conn, "blocker", "blocked").unwrap();

        // "blocked" has undone blocker, so only "blocker" is available
        let picked = claim_next_task(&conn, "agent", None).unwrap();
        assert_eq!(picked.as_deref(), Some("blocker"));
    }

    #[test]
    fn claim_next_prefers_unblockers() {
        let conn = db::open_memory().unwrap();
        // "plain" is older (lower id), but "unblocker" unblocks "downstream"
        add_task(&conn, "plain", None, "", None, None, false).unwrap();
        add_task(&conn, "unblocker", None, "", None, None, false).unwrap();
        add_task(&conn, "downstream", None, "", None, None, false).unwrap();
        add_block(&conn, "unblocker", "downstream").unwrap();

        let picked = claim_next_task(&conn, "agent", None).unwrap();
        assert_eq!(picked.as_deref(), Some("unblocker"));
    }

    #[test]
    fn claim_next_with_preference() {
        let conn = db::open_memory().unwrap();
        // "backend" is older but doesn't match preference
        add_task(
            &conn,
            "backend",
            None,
            "server-side API work",
            None,
            None,
            false,
        )
        .unwrap();
        add_task(
            &conn,
            "frontend",
            None,
            "UI components for dashboard",
            None,
            None,
            false,
        )
        .unwrap();

        let picked = claim_next_task(&conn, "agent", Some("UI components")).unwrap();
        assert_eq!(picked.as_deref(), Some("frontend"));
    }

    #[test]
    fn claim_next_preference_matches_notes() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "task-a", None, "generic task", None, None, false).unwrap();
        add_task(
            &conn,
            "task-b",
            None,
            "another generic task",
            None,
            None,
            false,
        )
        .unwrap();
        add_note(&conn, "task-b", "needs database migration work").unwrap();

        let picked = claim_next_task(&conn, "agent", Some("database migration")).unwrap();
        assert_eq!(picked.as_deref(), Some("task-b"));
    }

    #[test]
    fn claim_next_preference_is_soft() {
        let conn = db::open_memory().unwrap();
        // No task matches the preference, but tasks should still be returned
        add_task(&conn, "only-task", None, "some work", None, None, false).unwrap();

        let picked = claim_next_task(&conn, "agent", Some("nonexistent-xyz")).unwrap();
        assert_eq!(picked.as_deref(), Some("only-task"));
    }

    #[test]
    fn claim_next_sets_assignee() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();

        let picked = claim_next_task(&conn, "my-agent", None).unwrap();
        assert_eq!(picked.as_deref(), Some("t"));

        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.assignee.as_deref(), Some("my-agent"));
        assert!(task.status_changed_at.is_some());
    }

    #[test]
    fn add_with_claim() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "work", None, Some("agent-1"), false).unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.assignee.as_deref(), Some("agent-1"));
        assert!(task.status_changed_at.is_some());
        assert_eq!(task.status, "active");
    }

    #[test]
    fn add_without_claim() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "work", None, None, false).unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert!(task.assignee.is_none());
        assert!(task.status_changed_at.is_none());
    }

    #[test]
    fn pause_open_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        pause_task(&conn, "t").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.status, "paused");
        assert!(task.assignee.is_none());
        assert!(task.status_changed_at.is_some());
    }

    #[test]
    fn pause_active_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        claim_task(&conn, "t", "agent").unwrap();
        pause_task(&conn, "t").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.status, "paused");
        assert!(task.assignee.is_none());
    }

    #[test]
    fn pause_done_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        mark_done(&conn, "t").unwrap();
        let err = pause_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("is done"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn pause_already_paused_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        pause_task(&conn, "t").unwrap();
        let err = pause_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("already paused"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unpause_paused_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        pause_task(&conn, "t").unwrap();
        super::unpause_task(&conn, "t").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.status, "open");
    }

    #[test]
    fn unpause_non_paused_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        let err = super::unpause_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("not paused"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn claim_paused_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        pause_task(&conn, "t").unwrap();
        let err = claim_task(&conn, "t", "agent").unwrap_err();
        assert!(
            err.to_string().contains("paused"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn claim_next_skips_paused() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "paused-task", None, "", None, None, false).unwrap();
        pause_task(&conn, "paused-task").unwrap();
        add_task(&conn, "available", None, "", None, None, false).unwrap();

        let picked = claim_next_task(&conn, "agent", None).unwrap();
        assert_eq!(picked.as_deref(), Some("available"));
    }

    #[test]
    fn list_excludes_paused_by_default() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "open-task", None, "", None, None, false).unwrap();
        add_task(&conn, "paused-task", None, "", None, None, false).unwrap();
        pause_task(&conn, "paused-task").unwrap();
        let tasks = list_tasks(&conn, None, false, None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "open-task");
    }

    #[test]
    fn list_filter_paused() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "open-task", None, "", None, None, false).unwrap();
        add_task(&conn, "paused-task", None, "", None, None, false).unwrap();
        pause_task(&conn, "paused-task").unwrap();

        let paused = list_tasks(&conn, Some(StatusFilter::Paused), false, None).unwrap();
        assert_eq!(paused.len(), 1);
        assert_eq!(paused[0].name, "paused-task");
    }

    #[test]
    fn list_children_returns_direct_children_only() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "root", None, "", None, None, false).unwrap();
        add_task(&conn, "child1", Some("root"), "", None, None, false).unwrap();
        add_task(&conn, "child2", Some("root"), "", None, None, false).unwrap();
        add_task(&conn, "grandchild", Some("child1"), "", None, None, false).unwrap();
        add_task(&conn, "unrelated", None, "", None, None, false).unwrap();

        let children = list_children(&conn, "root", None, false).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, "child1");
        assert_eq!(children[1].name, "child2");
    }

    #[test]
    fn list_children_excludes_done_by_default() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "root", None, "", None, None, false).unwrap();
        add_task(&conn, "child-open", Some("root"), "", None, None, false).unwrap();
        add_task(&conn, "child-done", Some("root"), "", None, None, false).unwrap();
        mark_done(&conn, "child-done").unwrap();

        let children = list_children(&conn, "root", None, false).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "child-open");
    }

    #[test]
    fn list_children_all_includes_done() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "root", None, "", None, None, false).unwrap();
        add_task(&conn, "child-open", Some("root"), "", None, None, false).unwrap();
        add_task(&conn, "child-done", Some("root"), "", None, None, false).unwrap();
        mark_done(&conn, "child-done").unwrap();

        let children = list_children(&conn, "root", None, true).unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn list_children_with_status_filter() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "root", None, "", None, None, false).unwrap();
        add_task(&conn, "child-open", Some("root"), "", None, None, false).unwrap();
        add_task(&conn, "child-active", Some("root"), "", None, None, false).unwrap();
        claim_task(&conn, "child-active", "agent").unwrap();

        let active = list_children(&conn, "root", Some(StatusFilter::Active), false).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "child-active");
    }

    #[test]
    fn list_children_nonexistent_parent_fails() {
        let conn = db::open_memory().unwrap();
        assert!(list_children(&conn, "nonexistent", None, false).is_err());
    }

    #[test]
    fn list_children_no_children_returns_empty() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "leaf", None, "", None, None, false).unwrap();
        let children = list_children(&conn, "leaf", None, false).unwrap();
        assert!(children.is_empty());
    }

    #[test]
    fn steal_active_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        claim_task(&conn, "t", "agent-1").unwrap();
        let prev = steal_task(&conn, "t", "agent-2").unwrap();
        assert_eq!(prev, "agent-1");
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.assignee.as_deref(), Some("agent-2"));
        assert_eq!(task.status, "active");
    }

    #[test]
    fn steal_open_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        let err = steal_task(&conn, "t", "agent-2").unwrap_err();
        assert!(
            err.to_string().contains("not active"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn steal_paused_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        pause_task(&conn, "t").unwrap();
        let err = steal_task(&conn, "t", "agent-2").unwrap_err();
        assert!(
            err.to_string().contains("not active"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn steal_done_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        mark_done(&conn, "t").unwrap();
        let err = steal_task(&conn, "t", "agent-2").unwrap_err();
        assert!(
            err.to_string().contains("not active"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn force_unassign_active_task() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        claim_task(&conn, "t", "agent-1").unwrap();
        force_unassign_task(&conn, "t").unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.status, "open");
        assert!(task.assignee.is_none());
    }

    #[test]
    fn force_unassign_open_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        let err = force_unassign_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("not active"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn force_unassign_done_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        mark_done(&conn, "t").unwrap();
        let err = force_unassign_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("not active"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn force_unassign_paused_task_fails() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "", None, None, false).unwrap();
        pause_task(&conn, "t").unwrap();
        let err = force_unassign_task(&conn, "t").unwrap_err();
        assert!(
            err.to_string().contains("not active"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn force_unassign_nonexistent_task_fails() {
        let conn = db::open_memory().unwrap();
        let err = force_unassign_task(&conn, "nope").unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn add_with_paused() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "t", None, "work", None, None, true).unwrap();
        let task = get_task(&conn, "t").unwrap();
        assert_eq!(task.status, "paused");
        assert!(task.assignee.is_none());
        assert!(task.status_changed_at.is_some());
    }

    #[test]
    fn add_with_paused_excluded_from_claim_next() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "paused-task", None, "", None, None, true).unwrap();
        add_task(&conn, "open-task", None, "", None, None, false).unwrap();
        let picked = claim_next_task(&conn, "agent", None).unwrap();
        assert_eq!(picked.as_deref(), Some("open-task"));
    }

    #[test]
    fn add_with_paused_and_claim_fails() {
        let conn = db::open_memory().unwrap();
        let err = add_task(&conn, "t", None, "", None, Some("agent"), true).unwrap_err();
        assert!(
            err.to_string().contains("mutually exclusive"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn get_all_deps_returns_blockers_and_dependents() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        add_task(&conn, "b", None, "", None, None, false).unwrap();
        add_task(&conn, "c", None, "", None, None, false).unwrap();
        add_block(&conn, "a", "b").unwrap();
        add_block(&conn, "a", "c").unwrap();
        add_block(&conn, "b", "c").unwrap();

        let deps = get_all_deps(&conn).unwrap();

        // a blocks b and c
        let a = deps.get("a").unwrap();
        assert!(a.0.is_empty()); // blocked_by
        assert_eq!(a.1, vec!["b", "c"]); // blocks

        // b is blocked by a, blocks c
        let b = deps.get("b").unwrap();
        assert_eq!(b.0, vec!["a"]); // blocked_by
        assert_eq!(b.1, vec!["c"]); // blocks

        // c is blocked by a and b
        let c = deps.get("c").unwrap();
        assert_eq!(c.0, vec!["a", "b"]); // blocked_by
        assert!(c.1.is_empty()); // blocks
    }

    #[test]
    fn get_all_deps_excludes_done_blockers() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        add_task(&conn, "b", None, "", None, None, false).unwrap();
        add_block(&conn, "a", "b").unwrap();
        mark_done(&conn, "a").unwrap();

        let deps = get_all_deps(&conn).unwrap();

        // b should have no blockers since a is done
        let b_blocked_by = deps
            .get("b")
            .map(|(bb, _)| bb.clone())
            .unwrap_or_default();
        assert!(b_blocked_by.is_empty());

        // a should still list b in blocks
        let a_blocks = deps
            .get("a")
            .map(|(_, bl)| bl.clone())
            .unwrap_or_default();
        assert_eq!(a_blocks, vec!["b"]);
    }

    #[test]
    fn get_all_deps_empty() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "a", None, "", None, None, false).unwrap();
        let deps = get_all_deps(&conn).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn search_matches_task_name() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "auth-login", None, "handles login", None, None, false).unwrap();
        add_task(&conn, "billing", None, "payment processing", None, None, false).unwrap();

        let results = search_tasks(&conn, "auth").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task.name, "auth-login");
        assert!(results[0].matched_in.contains(&"task".to_string()));
    }

    #[test]
    fn search_matches_description() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "task-a", None, "implement authentication", None, None, false).unwrap();
        add_task(&conn, "task-b", None, "fix CSS styling", None, None, false).unwrap();

        let results = search_tasks(&conn, "authentication").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task.name, "task-a");
        assert!(results[0].matched_in.contains(&"task".to_string()));
    }

    #[test]
    fn search_matches_notes() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "task-a", None, "generic task", None, None, false).unwrap();
        add_task(&conn, "task-b", None, "another task", None, None, false).unwrap();
        add_note(&conn, "task-b", "needs database migration").unwrap();

        let results = search_tasks(&conn, "migration").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task.name, "task-b");
        assert!(results[0].matched_in.contains(&"notes".to_string()));
    }

    #[test]
    fn search_includes_done_tasks() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "done-task", None, "completed authentication work", None, None, false)
            .unwrap();
        mark_done(&conn, "done-task").unwrap();

        let results = search_tasks(&conn, "authentication").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task.name, "done-task");
        assert_eq!(results[0].task.status, "done");
    }

    #[test]
    fn search_no_matches_returns_empty() {
        let conn = db::open_memory().unwrap();
        add_task(&conn, "task-a", None, "some work", None, None, false).unwrap();

        let results = search_tasks(&conn, "nonexistent").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_empty_query_fails() {
        let conn = db::open_memory().unwrap();
        assert!(search_tasks(&conn, "").is_err());
        assert!(search_tasks(&conn, "   ").is_err());
    }

    #[test]
    fn search_deduplicates_task_and_note_matches() {
        let conn = db::open_memory().unwrap();
        add_task(
            &conn,
            "auth-task",
            None,
            "authentication system",
            None,
            None,
            false,
        )
        .unwrap();
        add_note(&conn, "auth-task", "authentication details here").unwrap();

        let results = search_tasks(&conn, "authentication").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task.name, "auth-task");
        assert!(results[0].matched_in.contains(&"task".to_string()));
        assert!(results[0].matched_in.contains(&"notes".to_string()));
    }
}
