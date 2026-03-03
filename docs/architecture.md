# kbtz-tmux Architecture

System diagram showing all components, data stores, and data flows.

## Component Overview

```
+------------------------------------------------------------------+
|                         tmux session                              |
|                                                                   |
|  +-------------+  +-------------+  +--------+  +--------+        |
|  | Window 0    |  | Window 1    |  | Win 2  |  | Win 3  |  ...   |
|  | kbtz watch  |  | orchestrator|  | agent  |  | agent  |        |
|  | (TUI)       |  | (kbtz-tmux  |  | (cc)   |  | (cc)   |        |
|  |             |  |  --no-attach)|  |        |  |        |        |
|  +------+------+  +------+------+  +---+----+  +---+----+        |
|         |                |             |            |             |
+------------------------------------------------------------------+
          |                |             |            |
          |reads           |spawns/kills |writes      |writes
          v                v             v            v
  +-------+--------+  +---+----+  +-----+------------+-----+
  | SQLite DB       |  | tmux   |  | Workspace status dir   |
  | (kbtz.db)       |  | API    |  | ($KBTZ_WORKSPACE_DIR)  |
  +-----------------+  +--------+  +------------------------+
```

## Full Data Flow

```
                              USER
                               |
          +--------------------+---------------------+
          | keyboard           | keyboard             | tmux prefix keys
          v                    v                      v
  +-------+-------+   +-------+-------+     +--------+--------+
  | kbtz watch    |   | manager       |     | tmux keybinds   |
  | (Window 0)    |   | (Window N)    |     | prefix-t: tasks |
  | TUI tree view |   | Claude Code   |     | prefix-g: mgr   |
  |               |   | toplevel      |     | prefix-Tab: jump |
  +---+-----------+   +-------+-------+     +-----------------+
      |                       |
      | reads                 | runs kbtz commands
      |                       v
      |               +-------+--------+
      |               | kbtz CLI       |
      |               | add, note,     |
      |               | done, block... |
      |               +-------+--------+
      |                       |
      | inotify               | writes
      v                       v
  +---+-----------------------+---+
  |         SQLite DB             |    <--- single source of truth
  |         (kbtz.db)             |         for task state
  |                               |
  |  tasks: name, status,        |
  |         assignee, parent,     |
  |         description           |
  |  notes: task_name, content    |
  |  task_deps: blocker, blocked  |
  +---+---------------------------+
      ^                       ^
      | reads                 | claims tasks,
      | inotify               | releases on exit
      |                       |
  +---+-----------+   +-------+--------+
  | kbtz watch    |   | Orchestrator   |
  | (cont.)       |   | (kbtz-tmux     |
  |               |   |  --no-attach)  |
  +---+-----------+   +-------+--------+
      ^                       |
      | inotify               | spawns Claude Code
      |                       | in tmux windows
  +---+-----------------------+---+
  |    Workspace status dir       |
  |    ($KBTZ_WORKSPACE_DIR)      |
  |                               |
  |  ws-0: "active"              |
  |  ws-1: "needs_input"         |
  |  ws-2: "idle"                |
  |  orchestrator.lock            |
  |  orchestrator.log             |
  +---+---------------------------+
      ^
      | writes
      |
  +---+-----------------------+
  | Plugin hooks              |
  | (workspace-status.sh)     |
  | runs inside each agent    |
  +---------------------------+
```

## Orchestrator Spawn Sequence

```
Orchestrator                    tmux                    SQLite DB
    |                            |                          |
    |  claim_next_task(sid)      |                          |
    |----------------------------------------------------->|
    |                            |         UPDATE tasks     |
    |                            |         SET status=active|
    |                            |             assignee=sid |
    |<-----------------------------------------------------|
    |  task_name                 |                          |
    |                            |                          |
    |  spawn_window(             |                          |
    |    session, title,         |                          |
    |    env{KBTZ_DB,            |                          |
    |        KBTZ_TASK,          |                          |
    |        KBTZ_SESSION_ID,    |                          |
    |        KBTZ_WORKSPACE_DIR},|                          |
    |    "claude", args)         |                          |
    |--------------------------->|                          |
    |  window_id                 |                          |
    |<---------------------------|                          |
    |                            |                          |
    |  set @kbtz_task=task_name  |                          |
    |--------------------------->|                          |
    |  set @kbtz_sid=session_id  |                          |
    |--------------------------->|                          |
    |                            |                          |
    |  track(session_id,         |                          |
    |        window_id,          |                          |
    |        task_name)          |                          |
    |                            |                          |
```

## Agent Session Status Flow

```
Claude Code             Plugin Hooks              Status File          kbtz watch
    |                       |                         |                    |
    | SessionStart          |                         |                    |
    |---------------------->|                         |                    |
    |                       | write "active"          |                    |
    |                       |------------------------>|                    |
    |                       |                         |-- inotify -------->|
    |                       |                         |                    | show 🤖🟢
    | (working...)          |                         |                    |
    | PreToolUse            |                         |                    |
    |---------------------->|                         |                    |
    |                       | write "active"          |                    |
    |                       |------------------------>|                    |
    |                       |                         |                    |
    | Notification          |                         |                    |
    | (needs permission)    |                         |                    |
    |---------------------->|                         |                    |
    |                       | write "needs_input"     |                    |
    |                       |------------------------>|                    |
    |                       |                         |-- inotify -------->|
    |                       |                         |                    | show 🤖🔔
    | Stop                  |                         |                    |
    |---------------------->|                         |                    |
    |                       | (skipped: prev was      |                    |
    |                       |  needs_input, event     |                    |
    |                       |  is Stop not SessionEnd)|                    |
    |                       |                         |                    |
    | UserPromptSubmit      |                         |                    |
    |---------------------->|                         |                    |
    |                       | write "active"          |                    |
    |                       |------------------------>|                    |
    |                       |                         |-- inotify -------->|
    |                       |                         |                    | show 🤖🟢
    | Stop (done)           |                         |                    |
    |---------------------->|                         |                    |
    |                       | write "idle"            |                    |
    |                       |------------------------>|                    |
    |                       |                         |-- inotify -------->|
    |                       |                         |                    | show 🤖🟡
    | SessionEnd            |                         |                    |
    |---------------------->|                         |                    |
    |                       | write "idle"            |                    |
    |                       | (always, even if        |                    |
    |                       |  prev=needs_input)      |                    |
    |                       |------------------------>|                    |
```

## Session Indicator Resolution

```
kbtz watch receives a TreeRow from the database:

  TreeRow { name: "my-task", status: "active", assignee: Some("ws/3"), ... }
                                                        |
                                                        v
                                        paths::session_id_to_filename("ws/3")
                                                        |
                                                        v
                                                      "ws-3"
                                                        |
                                                        v
                                  read($KBTZ_WORKSPACE_DIR / "ws-3")
                                                        |
                              +-------------------------+-------------------------+
                              |                         |                         |
                          file exists               file exists               file missing
                          "active"                  "needs_input"             (no status)
                              |                         |                         |
                              v                         v                         v
                         🤖🟢 my-task            🤖🔔 my-task            👽⭕  my-task
                         (robot+green)           (robot+bell)            (alien+open)
```

## Tmux Session Layout

```
+--tmux session "workspace"-----------------------------------------------+
|                                                                         |
|  Window 0: "📋 tasks"                                                   |
|  +-----------------------------------------------------------------+   |
|  | kbtz watch --workspace-dir $DIR --action "..."                  |   |
|  |                                                                 |   |
|  |  v 🤖🟢 auth-backend       Implement auth backend              |   |
|  |      🤖🔔 auth-backend-db  Set up database schema              |   |
|  |      ⭕   auth-backend-api  Implement API endpoints             |   |
|  |  > ⭕   frontend            Build frontend                     |   |
|  |    ✅   docs                Write documentation                 |   |
|  +-----------------------------------------------------------------+   |
|                                                                         |
|  Window 1: "🔧 orchestrator"                                            |
|  +-----------------------------------------------------------------+   |
|  | kbtz-tmux --no-attach --session workspace --max 4               |   |
|  | [INFO] Starting (max=4, poll=60s, session=workspace)            |   |
|  | [INFO] Spawning auth-backend-db (slot 0)                        |   |
|  +-----------------------------------------------------------------+   |
|                                                                         |
|  Window 2: "💬 manager"        (@kbtz_toplevel=true)                    |
|  +-----------------------------------------------------------------+   |
|  | claude --append-system-prompt "..." "You are the task manager"  |   |
|  | > Help me plan the auth feature                                 |   |
|  +-----------------------------------------------------------------+   |
|                                                                         |
|  Window 3: "🤖🔔 auth-backend-db"  (@kbtz_task=auth-backend-db,        |
|  +-----------------------------------------------------------------+  @kbtz_sid=ws/0)
|  | claude --append-system-prompt "..." "Work on task 'auth-...'"   |   |
|  | ? Allow Read tool on schema.sql? [y/n]                          |   |
|  +-----------------------------------------------------------------+   |
|                                                                         |
+-------------------------------------------------------------------------+

Keybindings:
  prefix-t     → select Window 0 (task tree)
  prefix-g     → find window with @kbtz_toplevel=true (manager)
  prefix-Tab   → cycle through windows with needs_input status
  Enter on task → jump to that task's agent window (via --action)
```

## Crate Dependencies

```
+------------------+
|   kbtz (lib)     |  Shared library: DB ops, CLI, TUI, tree rendering,
|                  |  TreeDecorator trait, session_indicator(), paths
+--------+---------+
         |
    +----+----+------------------+
    |         |                  |
    v         v                  v
+---+----+ +--+-------------+ +-+---------------+
| kbtz   | | kbtz-workspace | | kbtz-tmux       |
| (bin)  | | (bin)          | | (bin)           |
|        | |                | |                 |
| watch  | | In-process     | | tmux-based      |
| add    | | orchestrator   | | orchestrator    |
| done   | | PTY sessions   | | tmux windows    |
| ...    | +-------+--------+ +--------+--------+
+-+------+         |                   |
  |                |                   |
  |         +------+------+    +-------+-------+
  |         | kbtz-       |    | kbtz-tmux     |
  |         | workspace   |    | (lib)         |
  |         | sessions    |    | tmux API,     |
  |         | (in-memory) |    | lifecycle     |
  |         +-------------+    +---------------+
  |
  +--- plugin/
       hooks/hooks.json          Hook definitions
       scripts/
         workspace-status.sh     Write status files
         pane-title.sh           Update tmux pane title
         session-env.sh          Set up session environment
         notify.sh               Desktop notification on needs_input
```

## Filesystem Layout

```
~/.kbtz/
  kbtz.db                      Task database (SQLite)
  workspace/                   Session status directory
    orchestrator.lock           Orchestrator instance lock
    orchestrator.log            Orchestrator log file
    ws-0                        Status file: "active" | "idle" | "needs_input"
    ws-1                        Status file for session ws/1
    ...

~/.claude/
  plugins/
    kbtz/                       Installed plugin
      plugin.json               Plugin manifest
      hooks/hooks.json          Hook definitions
      scripts/                  Hook scripts
```

## Two Orchestrators, One Protocol

kbtz-workspace and kbtz-tmux are alternative orchestrators sharing the same
protocol. They both use the same database, status files, and plugin hooks.
The only difference is the session backend:

```
                    Shared                          Backend-specific
              +------------------+          +---------------------------+
              | SQLite DB        |          | kbtz-workspace            |
              | Status files     |    vs    |   PTY sessions (in-proc)  |
              | Plugin hooks     |          |   Embedded terminal       |
              | kbtz CLI         |          |   Single TUI process      |
              | TreeDecorator    |          +---------------------------+
              +------------------+          | kbtz-tmux                 |
                                            |   tmux windows            |
                                            |   Separate processes      |
                                            |   External terminal       |
                                            +---------------------------+
```
