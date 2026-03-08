# Per-Task Agent Types

Route tasks to different agent backends (e.g., Claude vs Gemini) by storing
an optional agent type on each task.

## Schema

Add a nullable `agent TEXT` column to the tasks table. `NULL` means "use
workspace default."

```sql
ALTER TABLE tasks ADD COLUMN agent TEXT;
```

Migration v2 -> v3 in `db.rs`. No data backfill needed -- all existing tasks
get `NULL`.

## Task model

Add `pub agent: Option<String>` to the `Task` struct. Update
`read_task_row`, `TASK_COLUMNS`, `INSERT_TASK`, and JSON serialization.

## CLI changes

- `kbtz add`: Add `--agent <name>` flag (optional).
- `kbtz show`: Display agent type when non-null. Include in `--json` output.
- `kbtz list --json`: Include agent field.
- `kbtz exec`: Support `--agent` in `add` commands within exec blocks.

`claim_next_task` accepts an optional `agent_types: Option<&[&str]>` filter.
When provided, only tasks with `agent IS NULL` or `agent IN (...)` are
eligible. This prevents claim-release spin loops on tasks with unconfigured
agent types. The kbtz CLI and kbtz-tmux pass `None` (no filtering).

## Discoverability: kbtz agents command

Move config types from `kbtz-workspace/src/config.rs` to `kbtz/src/config.rs`
so the kbtz CLI can read `~/.kbtz/workspace.toml`. Both kbtz and kbtz-workspace
import from `kbtz::config`.

New command:

```
$ kbtz agents
claude
gemini
```

Lists the keys from `[agent.*]` sections in the config file. If no config
file exists or no agents are configured, prints nothing (exit 0).

### No validation on add

Any agent name is accepted by `kbtz add --agent <type>`. Agent types that
have a named Rust backend (e.g., "claude") get type-specific behavior
(session resume, custom arg injection). All other types use a generic
backend that passes prompts as positional args and exits via SIGTERM.

### Backend field

`AgentConfig` has an optional `backend` field that selects which Rust
backend implementation to use. This allows multiple agent types to share
the same backend with different command/args:

```toml
[agent.claude]
command = "claude"
args = ["--verbose"]

[agent.claude-yolo]
backend = "claude"
command = "claude"
args = ["--dangerously-skip-permissions"]
```

If `backend` is not set, the agent name is used as the backend name.

## kbtz-workspace: routing with generic fallback

The workspace claims any task regardless of its agent type. Backends are
built at startup from `[agent.*]` config sections. If a task has an agent
type that doesn't match any configured backend, a generic backend is
created on-the-fly using the type name as the command.

### App struct changes

`backend: Box<dyn Backend>` becomes `backends: HashMap<String, Box<dyn Backend>>`
plus `default_backend: String`. A `session_backends: HashMap<String, String>`
tracks which backend each session was spawned with (for `request_exit` routing).

At startup, build the backends map from `[agent.*]` config sections. The
default comes from `workspace.backend` (falling back to "claude" if unset).

### Spawn logic

1. `claim_next_task` claims any eligible task (no agent type filter).
2. Read `task.agent` (or use `default_backend` if `NULL`).
3. Call `ensure_backend` to create a generic backend if none exists.
4. Spawn the session with that backend.

### Config

No config format changes needed. The existing format already supports this:

```toml
[workspace]
backend = "claude"

[agent.claude]
command = "claude"
args = ["--verbose"]

[agent.gemini]
command = "gemini-cli"
args = ["--model", "gemini-2.5-pro"]
```

## Environment

Pass `KBTZ_AGENT_TYPE` to worker sessions so agents know what backend type
they are running as:

```rust
env_vars.push(("KBTZ_AGENT_TYPE", &agent_type));
```

## Prompt changes

### TOPLEVEL_PROMPT

Add `--agent` to the `kbtz add` command reference:

```
- `kbtz add <name> "<description>" [-p parent] [-n note] [--agent type]` -- create a task
```

Add an "Agent types" section:

```markdown
## Agent types

The workspace supports multiple agent backends (e.g., claude, gemini).
Tasks default to the workspace's default backend unless overridden.

Run `kbtz agents` to see available agent types. Use `--agent <type>` when
creating a task that requires a specific backend:

    kbtz add gemini-review "Review the design doc." --agent gemini

Only use `--agent` when a task specifically needs a non-default backend.
Omitting it means the workspace default is used, which is correct for
most tasks.
```

### AGENT_PROMPT

Add `$KBTZ_AGENT_TYPE` to the Environment section:

```
- $KBTZ_AGENT_TYPE -- the agent backend type for this session (e.g. "claude")
```

Add `--agent` to the `kbtz add` examples in the "How to decompose" section.

### kbtz-basics skill

Update the command table to include `--agent`:

```
| `kbtz add <name> <desc> [-p parent] [-c assignee] [-n note] [--agent type] [--paused]` | Create a task |
```

Add a short "Agent types" subsection with example usage.

### worker skill (kbtz-mux)

Update delegation examples to show `--agent` flag when creating subtasks
for different backends.

## kbtz show display

When a task has a non-null agent, display it:

```
Name:        my-task
Status:      open
Agent:       gemini
Description: Review the design doc.
```

## Error handling

- `kbtz add --agent <name>`: Always succeeds (any agent name is valid).
- Workspace `spawn_up_to`: Claims any task. If the agent type has no
  configured backend, a generic backend is created on-the-fly.
- Workspace `spawn_for_task`: Same generic fallback behavior.

## Not included (YAGNI)

- No inheritance from parent tasks.
- No per-agent-type concurrency limits (global concurrency shared).
- No `--agent` flag on `claim-next`.
