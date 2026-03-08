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

### Validation on add

`kbtz add --agent <type>` validates the type against the config:

- If the type is not in `config.agent` keys, error with a message listing
  available types.
- If no config file exists, skip validation (standalone usage).

This catches typos immediately rather than leaving tasks silently unclaimed.

## kbtz-workspace: routing with SQL-level filtering

The workspace passes its configured backend names to `claim_next_task`, which
filters at the SQL level. Tasks with unconfigured agent types are never
claimed -- they stay open until a workspace with the right backend picks them
up.

### App struct changes

`backend: Box<dyn Backend>` becomes `backends: HashMap<String, Box<dyn Backend>>`
plus `default_backend: String`. A `session_backends: HashMap<String, String>`
tracks which backend each session was spawned with (for `request_exit` routing).

At startup, build the backends map from `[agent.*]` config sections. The
default comes from `workspace.backend` (falling back to "claude" if unset).

### Spawn logic

1. `claim_next_task` filters by configured agent types at the SQL level.
2. Read `task.agent` (or use `default_backend` if `NULL`).
3. Look up the backend in `backends` (guaranteed to exist by SQL filter).
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

- `kbtz add --agent nonexistent`: Fails with error listing available types
  (if config exists). Succeeds with no validation if no config file exists.
- Workspace `spawn_up_to`: SQL filter prevents claiming tasks with unconfigured
  agent types. They stay open for other workspaces.
- Workspace `spawn_for_task`: Checks backend exists before claiming. Returns
  error if unconfigured.

## Not included (YAGNI)

- No inheritance from parent tasks.
- No per-agent-type concurrency limits (global concurrency shared).
- No `--agent` flag on `claim-next`.
