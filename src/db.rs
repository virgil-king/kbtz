use anyhow::Result;
use rusqlite::Connection;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE CHECK(name GLOB '[a-zA-Z0-9_-]*' AND length(name) > 0),
    parent      TEXT REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE RESTRICT,
    description TEXT NOT NULL DEFAULT '',
    done        INTEGER NOT NULL DEFAULT 0 CHECK(done IN (0, 1)),
    assignee    TEXT,
    assigned_at TEXT,
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

CREATE VIRTUAL TABLE IF NOT EXISTS tasks_fts USING fts5(
    name, description,
    content='tasks', content_rowid='id'
);

CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
    content,
    content='notes', content_rowid='id'
);

-- Keep tasks_fts in sync with tasks
CREATE TRIGGER IF NOT EXISTS tasks_fts_ai AFTER INSERT ON tasks BEGIN
    INSERT INTO tasks_fts(rowid, name, description) VALUES(new.id, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS tasks_fts_ad AFTER DELETE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, name, description)
    VALUES('delete', old.id, old.name, old.description);
END;
CREATE TRIGGER IF NOT EXISTS tasks_fts_au AFTER UPDATE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, name, description)
    VALUES('delete', old.id, old.name, old.description);
    INSERT INTO tasks_fts(rowid, name, description) VALUES(new.id, new.name, new.description);
END;

-- Keep notes_fts in sync with notes
CREATE TRIGGER IF NOT EXISTS notes_fts_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, content) VALUES(new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS notes_fts_ad AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, content) VALUES('delete', old.id, old.content);
END;
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

    // One-time FTS rebuild for databases created before FTS5 tables existed.
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "INSERT INTO tasks_fts(tasks_fts) VALUES('rebuild');
             INSERT INTO notes_fts(notes_fts) VALUES('rebuild');
             PRAGMA user_version = 1;",
        )?;
    }

    Ok(())
}

#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    set_pragmas(&conn)?;
    init(&conn)?;
    Ok(conn)
}
