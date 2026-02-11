---
name: kbtz-basics
description: This skill should be used when the user asks about "kbtz commands", "task tracking", "how to use kbtz", "create a task", "add a note", "list tasks", "task dependencies", or needs a reference for kbtz CLI usage outside of worker mode.
---

# kbtz Task Tracker Reference

## Commands

| Command | Description |
|---------|-------------|
| `kbtz add <name> <description> [-p parent] [-c assignee]` | Create a task |
| `kbtz claim <name> <assignee>` | Claim a task |
| `kbtz claim-next <assignee> [--prefer text]` | Atomically claim the best available task |
| `kbtz release <name> <assignee>` | Release a claimed task |
| `kbtz done <name>` | Mark task complete |
| `kbtz list [--status S] [--json] [--tree] [--all]` | List tasks |
| `kbtz show <name> [--json]` | Show task details and blockers |
| `kbtz note <name> <content>` | Add a note to a task |
| `kbtz notes <name>` | List notes for a task |
| `kbtz block <blocker> <blocked>` | Set dependency (blocker must finish before blocked can start) |
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
