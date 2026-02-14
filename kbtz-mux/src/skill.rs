/// Protocol instructions given to every agent spawned by kbtz-mux.
///
/// These instructions are prepended to the agent's prompt so it knows
/// how to interact with the kbtz task database and follow the mux
/// lifecycle contract.
pub const AGENT_SKILL: &str = r#"
# kbtz task protocol

You are working inside kbtz-mux, a task multiplexer. You have been assigned
a specific task. Follow these rules exactly.

## Environment

- $KBTZ_DB — path to the SQLite task database
- $KBTZ_TASK — name of your assigned task
- $KBTZ_SESSION_ID — your session ID (e.g. "mux/3")

## Completing your task

When you finish the work, mark the task done:

```
kbtz done $KBTZ_TASK
```

Then exit. The mux will detect the exit and clean up.

## Decomposing into subtasks

If a task is too large to complete in one session, break it into subtasks.
Use `kbtz exec` to create all subtasks, blocking relationships, and release
your task atomically. This prevents the mux from seeing a partially-created
decomposition.

Pipe all the commands into `kbtz exec` via a heredoc:

```
kbtz exec <<'EOF'
add <subtask-1> "<description>" -p $KBTZ_TASK
add <subtask-2> "<description>" -p $KBTZ_TASK
block <subtask-1> $KBTZ_TASK
block <subtask-2> $KBTZ_TASK
release $KBTZ_TASK $KBTZ_SESSION_ID
EOF
```

All commands run in a single transaction — if any command fails, none take
effect. The release MUST be last. The mux will then kill your session,
claim the subtasks, and spawn new agents for them. When all subtasks are
done, your parent task becomes unblocked and the mux will respawn an agent
for it. Use `kbtz note` to leave context so the new agent can pick up
where you left off.

## Adding notes

Document important decisions, progress, or context for future agents:

```
kbtz note $KBTZ_TASK "Chose X approach because Y"
```

Notes persist across session restarts and are visible to any agent that
later works on this task.

## Checking task state

- Show your task details: `kbtz show $KBTZ_TASK`
- List subtasks: `kbtz list --root $KBTZ_TASK --tree`
- Read notes: `kbtz notes $KBTZ_TASK`
- Show what blocks a task: `kbtz show <name>` (shows blocked_by field)

## Rules

1. Only work on your assigned task ($KBTZ_TASK). Do not claim or modify
   other tasks.
2. Always create blocking relationships BEFORE releasing your task.
3. Never call `kbtz release` unless you are decomposing into subtasks.
   If you are done, use `kbtz done` instead.
4. Use `kbtz note` to leave context for future agents working on this
   task or its parent.
5. Subtask names should be scoped under the parent using "-" as separator:
   e.g. if your task is "auth", name subtasks "auth-db", "auth-api".
   Only a-z, A-Z, 0-9, _, - are allowed in task names.
6. If you resume a previously-started task, check notes and subtask
   status first with `kbtz show` and `kbtz notes` before starting work.
"#;
