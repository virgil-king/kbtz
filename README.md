# kbtz

A task tracker and workspace for coding agents. Backed by SQLite, designed for concurrent multi-agent workflows.

The name comes from "kibitz" -- to watch and offer commentary.

- **See the status of all agents and tasks in one place** — a terminal workspace shows a live task tree with status indicators for every running agent
- **Press Tab to chat with the next agent that needs input** — jump straight to the agent waiting for your attention, review its work, and move on
- **Tasks and notes are under your control** — work is tracked in a SQLite database you own, not hidden inside agent context windows
- **Structure work with dependencies** — parent/child and blocking relationships between tasks so agents work in the right order
- **Unblocked tasks immediately get their own agent** — when a task's dependencies are satisfied, the workspace claims it and spawns a new session automatically

kbtz has two components:

- **`kbtz-workspace`** — The terminal workspace you interact with. Manages concurrent agent sessions against a shared task database with a tmux-like interface for monitoring and interacting with them.
- **`kbtz`** — The underlying CLI that agents use to interact with the task database: creating tasks, setting dependencies, claiming work, and adding notes.

## Install

```bash
cargo install --path kbtz             # task tracker CLI
cargo install --path kbtz-workspace   # workspace manager
```

## kbtz-workspace

`kbtz-workspace` is a terminal workspace manager that orchestrates multiple AI agent sessions against a shared kbtz task database. It automatically claims tasks, spawns agent sessions in PTYs, monitors their lifecycle, and reaps them when tasks complete — giving you a tmux-like interface over a fleet of concurrent agents.

### Usage

```bash
kbtz-workspace [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--db <path>` | `$KBTZ_DB` or `~/.kbtz/kbtz.db` | Path to kbtz database |
| `-j, --concurrency <N>` | `4` | Max concurrent agent sessions |
| `--prefer <text>` | | FTS preference hint for task selection |
| `--command <cmd>` | `claude` | Command to run per session |
| `--manual` | | Disable auto-spawning; use `s` to spawn manually |

### Screens

The workspace has three screens:

**Task tree** — the default view. Shows all non-done tasks in a tree with session status indicators. Navigate tasks, zoom into sessions, and manage task state (pause, done, force-unassign).

**Task sessions** — full-screen view of a single agent's PTY. The agent's terminal output fills the screen with a status bar on the last line. You interact directly with the agent (e.g. Claude Code) as if it were a normal terminal session.

**Manager session** — a dedicated session (not tied to any task) with an interactive agent for manipulating the task list: creating tasks, reparenting, blocking/unblocking, etc. Press `c` from the task tree to open.

### Task tree keybindings

| Key | Action |
|-----|--------|
| `j` / `k`, Up / Down | Navigate tasks |
| `Enter` | Zoom into session |
| `s` | Spawn session for selected task |
| `r` | Restart (kill and respawn) session |
| `c` | Switch to manager session |
| `Space` | Collapse/expand subtree |
| `p` | Pause/unpause task |
| `d` | Mark task done |
| `U` | Force-unassign task |
| `Tab` | Jump to next session needing input |
| `?` | Help |
| `q` / `Esc` | Quit (releases all sessions) |

### Task session keybindings

All commands use a `Ctrl-B` prefix (like tmux):

| Key | Action |
|-----|--------|
| `^B t` | Return to task tree |
| `^B c` | Switch to manager session |
| `^B n` | Next session |
| `^B p` | Previous session |
| `^B Tab` | Jump to next session needing input |
| `^B [` | Enter scroll mode |
| `^B ^B` | Send literal Ctrl-B to agent |
| `^B ?` | Show help |
| `^B q` | Quit |

Page Up and left-click also enter scroll mode.

### Scroll mode

Scroll mode freezes the session output and renders the frozen viewport directly over the current screen with mouse tracking disabled. This enables:

- **Scrolling** via keyboard (`j`/`k`, arrows, PgUp/PgDn, `g`/`G`)
- **Native text selection** via click-drag, with copy using your terminal's native shortcut (Ctrl+Shift+C on Linux, Cmd+C on macOS)

| Key | Action |
|-----|--------|
| `q` / `Esc` | Exit scroll mode |
| `k` / Up | Scroll up 1 line |
| `j` / Down | Scroll down 1 line |
| PgUp / PgDn | Scroll by page |
| `g` | Jump to top of scrollback |
| `G` | Jump to bottom and exit scroll mode |

### Session lifecycle

1. **Claim** — When there is an available task and available session capacity, the workspace generates a new session ID, atomically claims the best available task for that ID, and spawns a new session. Tasks are ranked by FTS relevance (if `--prefer` is set), number of tasks they would unblock, and age.

2. **Spawn** — A PTY is allocated and the configured command (default: `claude`) is launched with the agent protocol injected via `--append-system-prompt`. Each session gets environment variables: `KBTZ_DB`, `KBTZ_TASK`, `KBTZ_SESSION_ID`, and `KBTZ_WORKSPACE_DIR`.

3. **Monitor** — A lifecycle tick runs every 100ms. It checks each session's process liveness and its task's database state. Sessions are reaped when:
   - The task is marked done, paused, or deleted
   - The task is released (e.g. agent decomposed into subtasks)
   - The task is reassigned to a different session
   - The agent process exits

4. **Reap** — The workspace sends SIGTERM and waits up to 5 seconds for graceful exit, then SIGKILL. The task claim is released so it can be picked up again. The concurrency slot is freed and a new task is claimed.

5. **Shutdown** — On quit (`q` or Ctrl-C), all sessions receive SIGTERM in parallel. After a 5-second grace period, any remaining sessions are force-killed and all task claims are released.

### Agent protocol

Each spawned agent receives a system prompt that teaches it the workspace contract:

- **Environment variables**: `$KBTZ_DB` (database path), `$KBTZ_TASK` (assigned task name), `$KBTZ_SESSION_ID` (e.g. `ws/3`), `$KBTZ_WORKSPACE_DIR` (status directory)
- **Completion**: Agents create PRs, wait for CI to pass, display the diff, and wait for user review; the user requests changes or asks the agent to merge
- **Decomposition**: Agents can split work into subtasks using `kbtz exec` for atomic creation of subtasks with blocking relationships
- **Notes**: Agents document decisions and progress with `kbtz note` for cross-session continuity
- **Branch/PR tracking**: Agents note branch names and PR URLs on their tasks

### Status reporting

Agents report their status by writing to files in the workspace status directory (`$KBTZ_WORKSPACE_DIR`, default `~/.kbtz/workspace/`). Each session gets a file named after its session ID (with `/` replaced by `-`). The workspace watches this directory and updates the task tree with status indicators:

| Status      | Indicator | Meaning                   |
|-------------|-----------|---------------------------|
| Starting    | ⏳        | Session just spawned      |
| Active      | 🟢        | Agent is working          |
| Idle        | 🟡        | Agent is waiting          |
| Needs input | 🔔        | Agent needs user attention |

## kbtz CLI

The `kbtz` CLI is the interface agents use to interact with the task database. You can also use it directly for scripting and manual task management.

### Quick start

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

### Database

Default location: `~/.kbtz/kbtz.db`

Override with `--db <path>` or the `KBTZ_DB` environment variable. The database is created automatically on first use.

Uses WAL mode and `busy_timeout = 5000ms` for safe concurrent access from multiple agents.

### Commands

#### Task lifecycle

| Command | Description |
|---------|-------------|
| `kbtz add <name> <desc> [-p parent] [-n note] [-c assignee]` | Create a task |
| `kbtz done <name>` | Mark complete |
| `kbtz reopen <name>` | Reopen a completed task |
| `kbtz pause <name>` | Pause a task (remove from active work and default listing) |
| `kbtz unpause <name>` | Unpause a paused task (return to open) |
| `kbtz rm <name> [--recursive]` | Remove a task |
| `kbtz describe <name> <desc>` | Update description |
| `kbtz reparent <name> [-p parent]` | Move under a different parent |

Task names must match `[a-zA-Z0-9_-]+`. Names are immutable — they cannot be changed after creation.

#### Claiming

| Command | Description |
|---------|-------------|
| `kbtz claim <name> <assignee>` | Claim a task |
| `kbtz claim-next <assignee> [--prefer text]` | Atomically claim the best available task |
| `kbtz steal <name> <assignee>` | Atomically transfer task ownership to a new assignee |
| `kbtz release <name> <assignee>` | Release a claimed task |
| `kbtz force-unassign <name>` | Forcibly clear a task's assignee (regardless of who holds it) |

`claim-next` picks the best unclaimed, unblocked, undone task in a single atomic transaction. It ranks by:

1. FTS5 relevance against `--prefer` text (matched on name, description, and notes)
2. Number of other tasks this would unblock
3. Age (oldest first)

Prints the claimed task details to stdout (same format as `kbtz show`) on success, exits with code 1 if nothing is available.

#### Dependencies

| Command | Description |
|---------|-------------|
| `kbtz block <blocker> <blocked>` | Mark a task as blocking another |
| `kbtz unblock <blocker> <blocked>` | Remove a blocking relationship |

Cycle detection prevents circular dependencies.

#### Notes

| Command | Description |
|---------|-------------|
| `kbtz note <name> <content>` | Add a note (reads from stdin if content omitted) |
| `kbtz notes <name> [--json]` | List notes for a task |

#### Viewing

| Command | Description |
|---------|-------------|
| `kbtz show <name> [--json]` | Show task details, notes, and dependencies |
| `kbtz list [--tree] [--status S] [--all] [--root name] [--json]` | List tasks |
| `kbtz watch [--root name] [--poll-interval ms]` | Interactive TUI with live updates |

`list` hides completed tasks by default. Use `--all` to include them, or `--status open|active|paused|done` to filter.

#### Coordination

| Command | Description |
|---------|-------------|
| `kbtz wait` | Block until the database changes (uses inotify) |
| `kbtz exec` | Execute commands from stdin atomically in a single transaction |

### Multi-agent usage

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

### Claude Code plugin

The `plugin/` directory contains a Claude Code plugin with a kbtz command reference and hooks.

Install from the marketplace:

```
/plugin marketplace add https://github.com/virgil-king/kbtz.git
/plugin install kbtz@kbtz
```

## Architecture

```
kbtz/src/
  cli.rs       Clap command definitions
  db.rs        SQLite schema, FTS5 tables, WAL pragmas
  model.rs     Task and Note data types
  ops.rs       All database operations (40+ tests)
  output.rs    Text, tree, and JSON formatters
  validate.rs  Name validation, cycle detection
  main.rs      CLI dispatch
  tui/         Ratatui TUI with tree view and notes panel
  watch.rs     inotify-based file change watcher

kbtz-workspace/src/
  main.rs      Entry point, tree/zoomed/toplevel mode loops
  app.rs       Application state, session management, lifecycle execution
  session.rs   PTY session spawning, passthrough I/O, VTE buffering
  lifecycle.rs Pure state machine for session reaping and spawning
  tree.rs      Ratatui tree view rendering
  prompt.rs    Agent and task manager system prompts
```
