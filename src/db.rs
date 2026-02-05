use anyhow::Result;
use rusqlite::Connection;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE CHECK(name GLOB '[a-zA-Z0-9_-]*' AND length(name) > 0),
    parent      TEXT REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE RESTRICT,
    description TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'idle', 'done')),
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS notes (
    id         INTEGER PRIMARY KEY,
    task       TEXT NOT NULL REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE CASCADE,
    content    TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS task_deps (
    blocker TEXT NOT NULL REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE CASCADE,
    blocked TEXT NOT NULL REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE CASCADE,
    PRIMARY KEY (blocker, blocked),
    CHECK (blocker != blocked)
);
";

fn set_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    set_pragmas(&conn)?;
    Ok(conn)
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    set_pragmas(&conn)?;
    init(&conn)?;
    Ok(conn)
}
