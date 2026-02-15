---
name: kbtz-basics
description: This skill should be used when the user asks about "kbtz commands", "task tracking", "how to use kbtz", "create a task", "add a note", "list tasks", "task dependencies", or needs a reference for kbtz CLI usage outside of worker mode.
---

# kbtz Task Tracker Reference

## Commands

| Command | Description |
|---------|-------------|
| `kbtz add <name> <desc> [-p parent] [-c assignee] [-n note] [--paused]` | Create a task |
| `kbtz claim <name> <assignee>` | Claim a task |
| `kbtz claim-next <assignee> [--prefer text]` | Atomically claim the best available task |
| `kbtz steal <name> <assignee>` | Atomically transfer task ownership (requires user approval) |
| `kbtz release <name> <assignee>` | Release a claimed task |
| `kbtz done <name>` | Mark task complete |
| `kbtz reopen <name>` | Reopen a completed task |
| `kbtz pause <name>` | Pause a task (remove from active work and default listing) |
| `kbtz unpause <name>` | Unpause a paused task (return to open) |
| `kbtz describe <name> <desc>` | Update a task's description |
| `kbtz reparent <name> [-p parent]` | Change a task's parent (omit -p to make root-level) |
| `kbtz rm <name> [--recursive]` | Remove a task (--recursive to remove children) |
| `kbtz list [--status S] [--json] [--tree] [--all] [--root name]` | List tasks |
| `kbtz show <name> [--json]` | Show task details and blockers |
| `kbtz note <name> [content]` | Add a note to a task (reads stdin if content omitted) |
| `kbtz notes <name> [--json]` | List notes for a task |
| `kbtz block <blocker> <blocked>` | Set dependency (blocker must finish before blocked can start) |
| `kbtz unblock <blocker> <blocked>` | Remove a blocking relationship |
| `kbtz watch [--root name]` | Launch interactive TUI |
| `kbtz wait` | Block until database changes |

## Task Naming

Task names must be **kebab-case**: lowercase letters, numbers, and hyphens only.

## Session ID

Use `$KBTZ_SESSION_ID` as your assignee in all kbtz commands. This environment variable is set automatically by Claude Code.

```bash
kbtz claim my-task $KBTZ_SESSION_ID
kbtz release my-task $KBTZ_SESSION_ID
```

## Common Patterns

### Creating tasks with subtasks

```bash
kbtz add parent-task "Top-level description"
kbtz add child-one "First subtask" -p parent-task
kbtz add child-two "Second subtask" -p parent-task
```

Use `-c $KBTZ_SESSION_ID` to create and claim in one step:

```bash
kbtz add my-subtask "Description" -p parent -c $KBTZ_SESSION_ID
```

Use `--paused` to create a task that shouldn't be worked on yet:

```bash
kbtz add deferred-task "Not ready yet" --paused
```

### Specifying closure conditions

When creating a task, include a **closure condition** in the description or an initial note so the worker knows what "done" means. Without a closure condition, the worker will simply mark the task done after committing changes.

Examples:

```bash
kbtz add fix-auth "Fix the auth token refresh bug" -n "Closure: create a PR and close when merged"

kbtz add add-tests "Add integration tests for payment flow" -n "Closure: close when tests pass on the feature branch"

kbtz add update-deps "Update outdated dependencies" -n "Closure: close when changes are committed to branch update-deps"
```

### Adding progress notes

```bash
kbtz note my-task "Investigated root cause, found X"
kbtz note my-task "Fix applied, running tests"
```

### Managing dependencies

```bash
# child-two cannot start until child-one finishes
kbtz block child-one child-two
```

### Viewing task tree

```bash
kbtz list --tree          # open tasks in tree form
kbtz list --tree --all    # include completed tasks
```

### Transferring task ownership

`steal` requires user approval before use. It atomically transfers an active task to a new assignee:

```bash
kbtz steal my-task $KBTZ_SESSION_ID
```
