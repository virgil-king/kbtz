---
name: worker
description: This skill should be used when the user asks to "start working", "become a worker", "work on tasks", "run as a worker agent", "claim tasks from kbtz", or wants to operate as an autonomous task worker that processes tasks from the kbtz task tracker.
---

# Kbtz Worker Agent

This skill transforms Claude into an autonomous worker agent that processes tasks from the kbtz task tracker database.

## Overview

A worker agent operates in a continuous loop:
1. Wait for tasks to appear using `kbtz wait`
2. Claim the best available task using `kbtz claim-next`
3. Work on the task until completion or blocked
4. For large tasks, create subtasks for other workers to handle in parallel
5. Use `kbtz wait` to wait for other workers to complete delegated subtasks
6. After closing a task, wait for new tasks and repeat

## Session Identity

Use the `$CLAUDE_CODE_SESSION_ID` environment variable as your session ID. This is automatically set by Claude Code and remains consistent throughout the session.

Use `$CLAUDE_CODE_SESSION_ID` directly in all kbtz commands.

## Main Work Loop

### Step 1: Wait for Tasks

Block until the database changes:

```bash
kbtz wait
```

This uses inotify/FSEvents to efficiently wait for any database modification.

### Step 2: Claim a Task

Use `claim-next` to atomically select and claim the best available task:

```bash
TASK=$(kbtz claim-next $CLAUDE_CODE_SESSION_ID --prefer "keywords from recent work")
```

`claim-next` automatically:
- Skips done, claimed, and blocked tasks
- Prefers tasks that unblock other tasks
- Prefers older tasks (FIFO ordering)
- With `--prefer`, soft-ranks tasks by FTS5 relevance against task names, descriptions, and notes

The `--prefer` flag accepts free-form text. Use it to express affinity for related work:

```bash
# Prefer tasks related to your recent work area
kbtz claim-next $CLAUDE_CODE_SESSION_ID --prefer "frontend UI components"

# Prefer tasks that mention your session ID (e.g., handoff notes)
kbtz claim-next $CLAUDE_CODE_SESSION_ID --prefer "$CLAUDE_CODE_SESSION_ID"
```

`claim-next` prints the task name to stdout on success, or exits with code 1 if no tasks are available. If no tasks are available, return to step 1.

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

When finished:

```bash
kbtz done <task-name>
```

### Step 6: Wait for User Review

After completing a task, stop and wait for the user to review your work before looking for the next task. Do not proceed to claim new tasks until the user confirms.

### Step 7: Take a Break

Take a moment to relax or do something fun if you'd like before looking for your next task.

### Step 8: Wait and Continue

Once the user approves, wait for new tasks:

```bash
kbtz wait
```

When new tasks appear, use `--prefer` to express affinity for related work. If switching to an unrelated domain, request context compaction (`/compact`) to free the context window.

Return to Step 2.

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
kbtz claim backend-api $CLAUDE_CODE_SESSION_ID
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

## Creating New Tasks

When a task needs decomposition but will be done sequentially by you:

```bash
# Create subtasks under current task, already claimed by you
kbtz add subtask-1 "First part" -p <parent-task> -c $CLAUDE_CODE_SESSION_ID
kbtz add subtask-2 "Second part" -p <parent-task> -c $CLAUDE_CODE_SESSION_ID

# Set up dependencies if needed
kbtz block subtask-1 subtask-2  # subtask-1 blocks subtask-2
```

## Coordinating with Other Workers

Multiple workers can operate on the same database:

- Each worker has a unique session ID
- `kbtz claim` and `kbtz claim-next` are atomic - only one worker can claim a task
- Use notes to communicate with other workers
- Check task ownership before modifying: `kbtz show <task>`

To release a task (e.g., when blocked and moving to another):

```bash
kbtz release <task-name> $CLAUDE_CODE_SESSION_ID
```

## Error Handling

If a task cannot be completed:

1. Add a note explaining the blocker
2. Create a new task describing what is needed
3. Set up the blocking relationship
4. Release the current task or leave it claimed while working on the blocker

```bash
kbtz note my-task "Blocked: need API credentials"
kbtz add get-api-creds "Obtain API credentials for service X"
kbtz block get-api-creds my-task
```

## Example Worker Session

```bash
PREFER=""

while true; do
    kbtz wait

    TASK=$(kbtz claim-next "$CLAUDE_CODE_SESSION_ID" --prefer "$PREFER" 2>/dev/null) || continue

    echo "Claimed: $TASK"

    # Work on task...
    # (actual work happens here)

    kbtz done "$TASK"
    echo "Completed: $TASK"

    # Use completed task as preference hint for next iteration
    PREFER="$TASK"
done
```

## Command Reference

| Command | Description |
|---------|-------------|
| `kbtz wait` | Block until database changes |
| `kbtz list [--status S] [--json]` | List tasks (open/active/done) |
| `kbtz show <name> [--json]` | Show task details and blockers |
| `kbtz claim <name> <assignee>` | Claim a specific task |
| `kbtz claim-next <assignee> [--prefer text]` | Atomically claim the best available task |
| `kbtz release <name> <assignee>` | Release a claimed task |
| `kbtz done <name>` | Mark task complete |
| `kbtz add <name> <description> [-p parent] [-c assignee]` | Create a task |
| `kbtz note <name> <content>` | Add a note |
| `kbtz notes <name>` | List notes |
| `kbtz block <blocker> <blocked>` | Set dependency |

## Starting the Worker

To begin operating as a worker agent, use `$CLAUDE_CODE_SESSION_ID` as your session ID and enter the work loop.
