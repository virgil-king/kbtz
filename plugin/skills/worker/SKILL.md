---
name: worker
description: This skill should be used when the user asks to "start working", "become a worker", "work on tasks", "run as a worker agent", "claim tasks from kbtz", or wants to operate as an autonomous task worker that processes tasks from the kbtz task tracker.
---

# Kbtz Worker Agent

This skill transforms Claude into an autonomous worker agent that processes tasks from the kbtz task tracker database.

## Overview

A worker agent operates in a continuous loop:
1. Claim the best available task using `kbtz claim-next`
2. If no tasks are available, wait using `kbtz wait`, then retry
3. Work on the task until completion or blocked
4. For large tasks, create subtasks for other workers to handle in parallel
5. Use `kbtz wait` to wait for other workers to complete delegated subtasks
6. After closing a task, claim the next task and repeat

## Task Ownership

Tasks are exclusive locks. Claiming a task grants you sole ownership of the task and its associated resources (worktrees, branches, etc.). Resource lifetimes are strictly nested:

```
Session
  └── Task ownership (claim → done/release)
        └── Resources (worktrees, branches, etc.)
```

Rules:
1. **Claim the task before acquiring resources.** Do not create worktrees or branches for a task you haven't claimed -- another worker may claim it and you'll have conflicting work.
2. **Release resources before closing the task.** Clean up worktrees, merge or push branches, and remove temporary files before running `kbtz done` or `kbtz release`.
3. **Close or release all tasks before ending the session.** Every task you own must be resolved via `kbtz done` or `kbtz release` before your session ends. Failing to do so leaves tasks permanently locked, blocking other workers and requiring manual cleanup.

## Main Work Loop

### Step 1: Claim a Task

Use `claim-next` to atomically select and claim the best available task:

```bash
TASK=$(kbtz claim-next $KBTZ_SESSION_ID --prefer "keywords from recent work")
```

`claim-next` automatically:
- Skips done, claimed, and blocked tasks
- Prefers tasks that unblock other tasks
- Prefers older tasks (FIFO ordering)
- With `--prefer`, soft-ranks tasks by FTS5 relevance against task names, descriptions, and notes

The `--prefer` flag accepts free-form text. Use it to express affinity for related work:

```bash
# Prefer tasks related to your recent work area
kbtz claim-next $KBTZ_SESSION_ID --prefer "frontend UI components"

# Prefer tasks that mention your session ID (e.g., handoff notes)
kbtz claim-next $KBTZ_SESSION_ID --prefer "$KBTZ_SESSION_ID"
```

`claim-next` prints the task name to stdout on success, or exits with code 1 if no tasks are available.

### Step 2: Wait for Tasks

If no tasks are available, block until the database changes:

```bash
kbtz wait
```

This uses inotify/FSEvents to efficiently wait for any database modification. When it returns, go back to Step 1.

### Step 3: Work on the Task

Never work on a task you have not claimed, since another worker may claim and begin work on it, resulting in redundant effort.

Read the task details:

```bash
kbtz show <task-name>
kbtz notes <task-name>
```

Execute the work described in the task. Add notes to document progress:

```bash
kbtz note <task-name> "Started investigating the issue"
kbtz note <task-name> "Found root cause: ..."
```

### Step 4: Handle Blockers

If blocked by another task owned by a different session, create subtasks or wait:

```bash
# Create a subtask under the blocking task
kbtz add <subtask-name> "Description" -p <blocker-task>

# Or add a note to the blocking task requesting action
kbtz note <blocker-task> "Blocked on this - need X resolved"
```

If blocked by an unclaimed task, claim and complete it first, or create subtasks to break it down.

### Step 5: Complete the Task

Before closing a task, release all resources acquired under it: merge or push branches, remove worktrees, and clean up temporary files. Then mark the task done:

```bash
kbtz done <task-name>
```

If your session is ending before the task is finished, clean up resources and release the task so another worker can pick it up:

```bash
kbtz release <task-name> $KBTZ_SESSION_ID
```

### Step 6: Wait for User Review

After completing a task, stop and wait for the user to review your work before looking for the next task. Do not proceed to claim new tasks until the user confirms.

### Step 7: Take a Break

Take a moment to relax or do something fun if you'd like before looking for your next task.

### Step 8: Continue

Once the user approves, return to Step 1 to claim the next task. Use `--prefer` to express affinity for related work. If switching to an unrelated domain, request context compaction (`/compact`) to free the context window.

## Delegating to Other Workers

When a task is large enough to benefit from parallel work, or involves different areas of specialization (e.g., frontend + backend, or multiple independent components), delegate subtasks to other workers:

### When to Delegate

- Task has multiple independent components that can be worked in parallel
- Task requires different specializations (e.g., database schema + API + UI)
- Task is too large for a single context window
- Faster completion is possible with parallel execution

### Delegation Pattern

1. Break the task into subtasks under the current task
2. Set up any dependencies between subtasks
3. Complete your own portion of the work
4. Use `kbtz wait` to block until other workers finish their subtasks
5. Once all subtasks are done, mark the parent task complete

```bash
# Break task into parallel subtasks
kbtz add backend-api "Implement REST API endpoints" -p my-task
kbtz add frontend-ui "Build React components for dashboard" -p my-task
kbtz add db-schema "Design and migrate database schema" -p my-task

# Set up dependencies (API needs schema first)
kbtz block db-schema backend-api

# Claim and do your part (e.g., the backend-api)
kbtz claim backend-api $KBTZ_SESSION_ID
# ... work on it ...
kbtz done backend-api

# Wait for other workers to complete remaining subtasks
kbtz wait

# Check if all subtasks are done
kbtz show my-task
# If all children complete, finish the parent
kbtz done my-task
```

### Writing Good Subtask Descriptions

Subtasks will be picked up by other worker agents. Write descriptions that are self-contained:

- Include enough context for another agent to understand the goal
- Specify acceptance criteria when possible
- Reference relevant files or documentation
- Note any constraints or requirements

```bash
kbtz add auth-middleware "Add JWT auth middleware to Express app. Validate tokens from /auth/token endpoint. Protect all /api/* routes. See src/server.ts for existing middleware pattern." -p api-task
```

## Example Worker Session

```bash
PREFER=""

while true; do
    TASK=$(kbtz claim-next "$KBTZ_SESSION_ID" --prefer "$PREFER" 2>/dev/null)
    if [ -z "$TASK" ]; then
        kbtz wait
        continue
    fi

    echo "Claimed: $TASK"

    # Work on task...
    # (actual work happens here)

    kbtz done "$TASK"
    echo "Completed: $TASK"

    # Use completed task as preference hint for next iteration
    PREFER="$TASK"
done
```

## Starting the Worker

To begin operating as a worker agent, use `$KBTZ_SESSION_ID` as your session ID. Try to claim a task immediately — only enter the wait loop if nothing is available.
