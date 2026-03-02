# Data Model

Technical reference for the state management mechanisms in the kbtz system.

## Task database

SQLite with WAL mode and `busy_timeout = 5000ms` for concurrent access.

### Tables

| Table | Purpose |
|-------|---------|
| `tasks` | Core task state: name, parent, description, status, assignee, timestamps |
| `notes` | Append-only audit trail per task |
| `task_deps` | Blocking relationships (blocker, blocked) |
| `tasks_fts` / `notes_fts` | FTS5 virtual tables for full-text search |

### Task statuses

| Status | Meaning |
|--------|---------|
| `open` | Unclaimed, available for agents |
| `active` | Claimed by an agent (assignee set) |
| `paused` | Excluded from claiming |
| `done` | Completed |

**Invariant:** `(status = 'active') = (assignee IS NOT NULL)` — enforced at the database level.

### Task fields

| Column | Type | Description |
|--------|------|-------------|
| `name` | TEXT UNIQUE | Immutable identifier, `[a-zA-Z0-9_-]+` |
| `parent` | TEXT | FK to tasks(name), CASCADE delete |
| `description` | TEXT | One-line summary |
| `status` | TEXT | One of: open, active, paused, done |
| `assignee` | TEXT | Session ID that holds the claim (e.g. `ws/3`) |
| `status_changed_at` | TEXT | ISO 8601 timestamp of last status change |
| `created_at` | TEXT | ISO 8601 creation timestamp |
| `updated_at` | TEXT | ISO 8601 last-modified timestamp |

## Session status files

Agents report their runtime status by writing to files in `$KBTZ_WORKSPACE_DIR` (default `~/.kbtz/workspace/`).

### Filename encoding

Session IDs contain `/` (e.g. `ws/3`), which can't appear in filenames. The encoding replaces `/` with `-`: `ws/3` → `ws-3`.

The canonical implementation is `kbtz::paths::session_id_to_filename()` / `filename_to_session_id()`. All consumers must use these functions rather than implementing their own encoding.

### File content

One of `active`, `idle`, or `needs_input` (plain text, no newline).

### Writers

Claude Code plugin hooks (`plugin/hooks/hooks.json`) fire `workspace-status.sh` on lifecycle events:

| Hook | Status written |
|------|---------------|
| SessionStart | `active` |
| UserPromptSubmit | `active` |
| PreToolUse | `active` |
| Notification | `needs_input` |
| Stop | `idle` |

### Readers

- `kbtz-workspace` reads status files to update task tree indicators
- `kbtz-tmux jump-needs-input` reads them to find sessions needing attention

### Cleanup

The orchestrator deletes orphaned status files during reconciliation on startup. Files that don't correspond to any live window are removed.

## Tmux window options (kbtz-tmux only)

The orchestrator tags each spawned window with tmux options for crash recovery:

| Option | Value | Purpose |
|--------|-------|---------|
| `@kbtz_task` | Task name | Identifies which task a window is working on |
| `@kbtz_sid` | Session ID (e.g. `ws/3`) | Identifies the session for status file correlation |
| `@kbtz_toplevel` | `true` | Marks the manager window (for `^B c` keybinding) |
| `@kbtz_workspace_dir` | Path | Session-level option for keybinding scripts |

On startup, `reconcile()` scans all windows for these options and re-adopts orphaned windows whose tasks are still active and claimed.

## Environment variables

| Variable | Set by | Purpose |
|----------|--------|---------|
| `KBTZ_DB` | Orchestrator/workspace | Database path for agents |
| `KBTZ_TASK` | Orchestrator/workspace | Assigned task name |
| `KBTZ_SESSION_ID` | Orchestrator/workspace | Session ID (e.g. `ws/3`) |
| `KBTZ_WORKSPACE_DIR` | Orchestrator/workspace | Status file directory |
| `KBTZ_TMUX_SESSION` | User | Override tmux session name |
| `KBTZ_DEBUG` | User | Enable debug logging |

## Lock files

| File | Purpose |
|------|---------|
| `orchestrator.lock` | Prevents concurrent kbtz-tmux orchestrator instances |
| `workspace.lock` | Prevents concurrent kbtz-workspace instances |

Locks use `flock(LOCK_EX | LOCK_NB)` and are held for the lifetime of the process. Released automatically when the process exits (even on crash).
