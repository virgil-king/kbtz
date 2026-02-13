# kbtz

A task tracker for AI agents. Backed by SQLite, designed for concurrent multi-agent workflows.

The name comes from "kibitz" -- to watch and offer commentary.

## Install

```bash
cargo install --path .
```

## Quick start

```bash
# Create tasks
kbtz add setup-db "Design and create the database schema"
kbtz add build-api "Implement REST API endpoints" -p setup-db
kbtz block setup-db build-api

# Claim and work
kbtz claim setup-db agent-1
kbtz note setup-db "Created migrations for users and sessions tables"
kbtz done setup-db

# Check status
kbtz list --tree
kbtz show build-api --json
```

## Database

Default location: `~/.kbtz/kbtz.db`

Override with `--db <path>` or the `KBTZ_DB` environment variable. The database is created automatically on first use.

Uses WAL mode and `busy_timeout = 5000ms` for safe concurrent access from multiple agents.

## Commands

### Task lifecycle

| Command | Description |
|---------|-------------|
| `kbtz add <name> <desc> [-p parent] [-n note] [-c assignee]` | Create a task |
| `kbtz done <name>` | Mark complete |
| `kbtz reopen <name>` | Reopen a completed task |
| `kbtz rm <name> [--recursive]` | Remove a task |
| `kbtz describe <name> <desc>` | Update description |
| `kbtz reparent <name> [-p parent]` | Move under a different parent |

Task names must match `[a-zA-Z0-9_-]+`.

### Claiming

| Command | Description |
|---------|-------------|
| `kbtz claim <name> <assignee>` | Claim a task |
| `kbtz claim-next <assignee> [--prefer text]` | Atomically claim the best available task |
| `kbtz release <name> <assignee>` | Release a claimed task |

`claim-next` picks the best unclaimed, unblocked, undone task in a single atomic transaction. It ranks by:

1. FTS5 relevance against `--prefer` text (matched on name, description, and notes)
2. Number of other tasks this would unblock
3. Age (oldest first)

Prints the claimed task details to stdout (same format as `kbtz show`) on success, exits with code 1 if nothing is available.

### Dependencies

| Command | Description |
|---------|-------------|
| `kbtz block <blocker> <blocked>` | Mark a task as blocking another |
| `kbtz unblock <blocker> <blocked>` | Remove a blocking relationship |

Cycle detection prevents circular dependencies.

### Notes

| Command | Description |
|---------|-------------|
| `kbtz note <name> <content>` | Add a note (reads from stdin if content omitted) |
| `kbtz notes <name> [--json]` | List notes for a task |

### Viewing

| Command | Description |
|---------|-------------|
| `kbtz show <name> [--json]` | Show task details, notes, and dependencies |
| `kbtz list [--tree] [--status S] [--all] [--root name] [--json]` | List tasks |
| `kbtz watch [--root name] [--poll-interval ms]` | Interactive TUI with live updates |

`list` hides completed tasks by default. Use `--all` to include them, or `--status open|active|done` to filter.

### Coordination

| Command | Description |
|---------|-------------|
| `kbtz wait` | Block until the database changes (uses inotify) |

## Multi-agent usage

Multiple agents can safely share a single kbtz database. Claims use compare-and-swap guards so only one agent can claim a given task. A typical agent loop:

```bash
while true; do
    kbtz wait
    TASK=$(kbtz claim-next "$SESSION_ID" --prefer "$PREFER" 2>/dev/null | awk '/^Name:/{print $2}') || continue
    # ... work on $TASK ...
    kbtz done "$TASK"
    PREFER="$TASK"
done
```

## Claude Code plugin

The `plugin/` directory contains a Claude Code plugin that teaches Claude how to operate as a kbtz worker agent. See `plugin/skills/worker/SKILL.md` for the full protocol.

Install from the marketplace:

```
/plugin marketplace add https://github.com/virgil-king/kbtz.git
/plugin install kbtz@kbtz
```

## Architecture

```
src/
  cli.rs       Clap command definitions
  db.rs        SQLite schema, FTS5 tables, WAL pragmas
  model.rs     Task and Note data types
  ops.rs       All database operations (40 tests)
  output.rs    Text, tree, and JSON formatters
  validate.rs  Name validation, cycle detection
  main.rs      CLI dispatch
  tui/         Ratatui TUI with tree view and notes panel
  watch.rs     inotify-based file change watcher
```
