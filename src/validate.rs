use anyhow::{bail, Result};
use rusqlite::Connection;

/// Validate a task name: must be non-empty and match [a-zA-Z0-9_-]+
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("task name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("task name '{name}' contains invalid characters: only a-z, A-Z, 0-9, _, - allowed");
    }
    Ok(())
}

/// Detect if setting `task_name`'s parent to `new_parent` would create a cycle.
/// A cycle exists if `new_parent` is a descendant of `task_name` (or is `task_name` itself).
pub fn detect_parent_cycle(conn: &Connection, task_name: &str, new_parent: &str) -> Result<bool> {
    if task_name == new_parent {
        return Ok(true);
    }
    // Walk up from new_parent to see if we reach task_name
    let mut current = Some(new_parent.to_string());
    while let Some(ref name) = current {
        let parent: Option<String> = conn.query_row(
            "SELECT parent FROM tasks WHERE name = ?1",
            [name.as_str()],
            |row| row.get(0),
        )?;
        match parent {
            Some(p) if p == task_name => return Ok(true),
            Some(p) => current = Some(p),
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// Detect if adding a blocker->blocked dependency would create a cycle in the dependency graph.
pub fn detect_dep_cycle(conn: &Connection, blocker: &str, blocked: &str) -> Result<bool> {
    if blocker == blocked {
        return Ok(true);
    }
    // BFS from blocker following reverse edges (who blocks the blocker?)
    // If we reach `blocked`, then adding blocker->blocked would create a cycle.
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(blocker.to_string());
    visited.insert(blocker.to_string());

    while let Some(current) = queue.pop_front() {
        let mut stmt = conn.prepare_cached(
            "SELECT blocker FROM task_deps WHERE blocked = ?1",
        )?;
        let blockers = stmt.query_map([&current], |row| row.get::<_, String>(0))?;
        for b in blockers {
            let b = b?;
            if b == blocked {
                return Ok(true);
            }
            if visited.insert(b.clone()) {
                queue.push_back(b);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        assert!(validate_name("foo").is_ok());
        assert!(validate_name("foo-bar").is_ok());
        assert!(validate_name("foo_bar").is_ok());
        assert!(validate_name("FooBar123").is_ok());
    }

    #[test]
    fn invalid_names() {
        assert!(validate_name("").is_err());
        assert!(validate_name("foo bar").is_err());
        assert!(validate_name("foo.bar").is_err());
        assert!(validate_name("foo/bar").is_err());
    }
}
