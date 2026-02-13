use anyhow::Result;
use rusqlite::Connection;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id                 INTEGER PRIMARY KEY,
    name               TEXT NOT NULL UNIQUE CHECK(name GLOB '[a-zA-Z0-9_-]*' AND length(name) > 0),
    parent             TEXT REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE RESTRICT,
    description        TEXT NOT NULL DEFAULT '',
    status             TEXT NOT NULL DEFAULT 'open' CHECK(status IN ('open', 'paused', 'active', 'done')),
    assignee           TEXT,
    status_changed_at  TEXT,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    CHECK((status = 'active') = (assignee IS NOT NULL))
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
";

const TRIGGERS: &str = "
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
CREATE TRIGGER IF NOT EXISTS notes_fts_au AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, content) VALUES('delete', old.id, old.content);
    INSERT INTO notes_fts(rowid, content) VALUES(new.id, new.content);
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
    conn.execute_batch(TRIGGERS)?;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "INSERT INTO tasks_fts(tasks_fts) VALUES('rebuild');
             INSERT INTO notes_fts(notes_fts) VALUES('rebuild');
             PRAGMA user_version = 2;",
        )?;
    } else if version < 2 {
        migrate_v1_to_v2(conn)?;
    }

    Ok(())
}

fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    // SQLite requires foreign_keys OFF for table-rebuild migrations.
    // The pragma cannot be changed inside a transaction.
    // See: https://www.sqlite.org/lang_altertable.html#otheralter
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;

    let result = (|| -> Result<()> {
        conn.execute_batch(
            "BEGIN;

            CREATE TABLE tasks_new (
                id                 INTEGER PRIMARY KEY,
                name               TEXT NOT NULL UNIQUE CHECK(name GLOB '[a-zA-Z0-9_-]*' AND length(name) > 0),
                parent             TEXT REFERENCES tasks_new(name) ON UPDATE CASCADE ON DELETE RESTRICT,
                description        TEXT NOT NULL DEFAULT '',
                status             TEXT NOT NULL DEFAULT 'open' CHECK(status IN ('open', 'paused', 'active', 'done')),
                assignee           TEXT,
                status_changed_at  TEXT,
                created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                updated_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                CHECK((status = 'active') = (assignee IS NOT NULL))
            );

            INSERT INTO tasks_new (id, name, parent, description, status, assignee, status_changed_at, created_at, updated_at)
                SELECT id, name, parent, description,
                       CASE WHEN done = 1 THEN 'done'
                            WHEN assignee IS NOT NULL THEN 'active'
                            ELSE 'open'
                       END,
                       CASE WHEN done = 1 THEN NULL ELSE assignee END,
                       assigned_at,
                       created_at,
                       updated_at
                FROM tasks;

            DROP TABLE tasks;
            ALTER TABLE tasks_new RENAME TO tasks;

            PRAGMA user_version = 2;",
        )?;

        // Verify FK integrity before committing
        let fk_violations: Vec<String> = conn
            .prepare("PRAGMA foreign_key_check")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        if !fk_violations.is_empty() {
            anyhow::bail!(
                "foreign key violations after migration: {}",
                fk_violations.join(", ")
            );
        }

        conn.execute_batch("COMMIT;")?;
        Ok(())
    })();

    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
    }

    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    result?;

    // Recreate triggers (DROP TABLE killed them)
    conn.execute_batch(TRIGGERS)?;

    // Rebuild FTS since the content table was recreated
    conn.execute_batch("INSERT INTO tasks_fts(tasks_fts) VALUES('rebuild');")?;

    Ok(())
}

#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    set_pragmas(&conn)?;
    init(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_SCHEMA: &str = "
    CREATE TABLE tasks (
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

    CREATE TABLE notes (
        id         INTEGER PRIMARY KEY,
        task       TEXT NOT NULL REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE CASCADE,
        content    TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
    );

    CREATE TABLE task_deps (
        blocker TEXT NOT NULL REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE CASCADE,
        blocked TEXT NOT NULL REFERENCES tasks(name) ON UPDATE CASCADE ON DELETE CASCADE,
        PRIMARY KEY (blocker, blocked),
        CHECK (blocker != blocked)
    );

    CREATE VIRTUAL TABLE tasks_fts USING fts5(
        name, description,
        content='tasks', content_rowid='id'
    );

    CREATE VIRTUAL TABLE notes_fts USING fts5(
        content,
        content='notes', content_rowid='id'
    );

    PRAGMA user_version = 1;
    ";

    /// Create an in-memory v1 database with test data.
    fn open_v1_memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        set_pragmas(&conn).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn
    }

    #[test]
    fn migrate_v1_empty() {
        let conn = open_v1_memory();
        init(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn migrate_v1_with_parent_tasks() {
        let conn = open_v1_memory();
        conn.execute_batch(
            "INSERT INTO tasks (name, description) VALUES ('parent', 'parent task');
             INSERT INTO tasks (name, parent, description) VALUES ('child', 'parent', 'child task');",
        )
        .unwrap();

        init(&conn).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM tasks WHERE name = 'child'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "open");

        let parent: Option<String> = conn
            .query_row(
                "SELECT parent FROM tasks WHERE name = 'child'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parent.as_deref(), Some("parent"));
    }

    #[test]
    fn migrate_v1_preserves_notes_and_deps() {
        let conn = open_v1_memory();
        conn.execute_batch(
            "INSERT INTO tasks (name, description) VALUES ('a', 'task a');
             INSERT INTO tasks (name, description) VALUES ('b', 'task b');
             INSERT INTO notes (task, content) VALUES ('a', 'a note on a');
             INSERT INTO task_deps (blocker, blocked) VALUES ('a', 'b');",
        )
        .unwrap();

        init(&conn).unwrap();

        let note_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE task = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(note_count, 1);

        let dep_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_deps WHERE blocker = 'a' AND blocked = 'b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dep_count, 1);
    }

    #[test]
    fn migrate_v1_status_conversion() {
        let conn = open_v1_memory();
        conn.execute_batch(
            "INSERT INTO tasks (name, done) VALUES ('open-task', 0);
             INSERT INTO tasks (name, done, assignee, assigned_at) VALUES ('active-task', 0, 'agent', '2025-01-01T00:00:00Z');
             INSERT INTO tasks (name, done) VALUES ('done-task', 1);",
        )
        .unwrap();

        init(&conn).unwrap();

        let open_status: String = conn
            .query_row(
                "SELECT status FROM tasks WHERE name = 'open-task'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open_status, "open");

        let (active_status, active_assignee): (String, Option<String>) = conn
            .query_row(
                "SELECT status, assignee FROM tasks WHERE name = 'active-task'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(active_status, "active");
        assert_eq!(active_assignee.as_deref(), Some("agent"));

        let (done_status, done_assignee): (String, Option<String>) = conn
            .query_row(
                "SELECT status, assignee FROM tasks WHERE name = 'done-task'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(done_status, "done");
        assert!(done_assignee.is_none());
    }

    #[test]
    fn migrate_v1_fts_works_after() {
        let conn = open_v1_memory();
        conn.execute_batch(
            "INSERT INTO tasks (name, description) VALUES ('searchable', 'uniquekeywordxyz');",
        )
        .unwrap();

        init(&conn).unwrap();

        let found: String = conn
            .query_row(
                "SELECT name FROM tasks_fts WHERE tasks_fts MATCH 'uniquekeywordxyz'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, "searchable");
    }

    #[test]
    fn fresh_db_gets_version_2() {
        let conn = Connection::open_in_memory().unwrap();
        set_pragmas(&conn).unwrap();
        init(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn idempotent_init_on_v2() {
        let conn = Connection::open_in_memory().unwrap();
        set_pragmas(&conn).unwrap();
        init(&conn).unwrap();
        // Second init should be a no-op
        init(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }
}
