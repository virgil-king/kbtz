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

No changes to `claim-next` -- agent routing happens in the workspace, not in
the claiming logic.

## kbtz-workspace: routing after claim

The workspace claims the best available task (no agent filtering), then
routes to the right backend based on the task's `agent` field.

### App struct changes

`backend: Box<dyn Backend>` becomes `backends: HashMap<String, Box<dyn Backend>>`
plus `default_backend: String`.

At startup, build the backends map from `[agent.*]` config sections. The
default comes from `workspace.backend` (falling back to "claude" if unset).

### Spawn logic

After `claim_next_task()` returns a task:

1. Read `task.agent` (or use `default_backend` if `NULL`).
2. Look up the backend in `backends`.
3. If found, spawn the session with that backend.
4. If not found, release the task and log a warning.

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

Use `--agent <type>` when creating a task that requires a specific backend:

    kbtz add gemini-review "Review the design doc." --agent gemini

Only use `--agent` when a task specifically needs a non-default backend.
Omitting it means the workspace default is used, which is correct for
most tasks. Available agent types depend on the workspace configuration.
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

- `kbtz add --agent nonexistent`: Succeeds. kbtz is config-agnostic; the task
  stays open until a workspace with that agent type runs.
- Workspace claims a task with unconfigured agent type: Releases the task and
  logs a warning.

## Not included (YAGNI)

- No inheritance from parent tasks.
- No per-agent-type concurrency limits (global concurrency shared).
- No runtime agent-type validation in kbtz against workspace config.
- No `--agent` flag on `claim-next`.
