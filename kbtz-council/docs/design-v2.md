# kbtz-council v2: Multi-Project Workspace

Replaces both kbtz-council v1 (single project) and kbtz-workspace (flat
task pool) with a unified multi-project orchestrator.

## Overview

A single persistent process manages multiple concurrent projects. Each
project has a leader agent that either delegates work to implementation
agents or does it directly. Stakeholder agents review all changes
regardless of who made them. A concierge session helps create and manage
projects.

## Components

**Orchestrator** — persistent Rust binary. Manages all projects, sessions,
clone pools, MCP servers, and the TUI. One process, one event loop.

**Concierge** — a persistent headless session (claude -p --resume) at the
global level. Helps the user create projects, browse history, and manage
active work. Has MCP tools for project management.

**Leader** (per project) — a persistent headless session. In full mode,
decomposes goals into jobs and delegates. In quick mode, does the work
directly. Calls `dispatch_job` or `request_review` via MCP.

**Implementation agents** (per job) — headless sessions that execute
delegated work in sandboxed clones. One per job.

**Stakeholders** (per job) — headless sessions that review changes. Scoped
per (job, stakeholder name). Run in parallel.

## Directory Layout

```
~/.kbtz-council/
  config.toml                  # Global config
  index.json                   # Project registry
  pool/                        # Global shallow clone pool (shared across projects)
    <repo-name>/
  concierge/                   # Concierge session state
    session_id.json
  projects/
    <project-name>/            # Active project
      project.md               # Leader-authored project definition
      state.json               # Current state (jobs, artifacts, session IDs)
      notes.md                 # User notes (append-only, editable)
      .mcp.json                # MCP config for this project's leader
      repos/                   # Leader's repo copies (cloned from pool on first merge)
        <repo-name>/
      sessions/                # Implementation session working directories
        <job-id>-impl/
      traces/                  # Stream-json logs per session
        leader.jsonl
        <job-id>-impl.jsonl
        <art-id>-<stakeholder>.jsonl
      hooks/
        start.sh               # Runs on project create/resume
        stop.sh                # Runs on project archive/pause
  archive/                     # Completed/abandoned projects (same format)
    <project-name>/
```

## Project Lifecycle

1. User talks to concierge: "I need to fix the auth bug in kbtz"
2. Concierge calls `create_project(name, goal)` → project directory created
3. Start hooks run (clone repos, install deps, etc.)
4. Leader session starts — user chats to refine goal, configure stakeholders
5. Leader works (quick) or dispatches jobs (full)
6. Stakeholders review changes
7. Leader incorporates feedback, iterates
8. User or leader archives the project
9. Stop hooks run

## Project Hooks

Scripts in `hooks/` run at lifecycle boundaries:

- `start.sh` — after project creation or on resume. Sets up repos, branches,
  dependencies. Replaces the `define_project` cloning logic.
- `stop.sh` — before archiving. Push branches, clean up, generate reports.

The orchestrator runs these deterministically, not agents.

## Project Status

Three states: **active**, **paused**, **archived**.

- **active** — sessions run, lifecycle ticks, user can interact.
- **paused** — no sessions run, state preserved. Resume runs start hooks.
- **archived** — moved to `archive/`, read-only. Can be resumed (moves back
  to `projects/`, runs start hooks).

The concierge or the project leader can trigger transitions via MCP tools.
The user can also pause/archive from the TUI.

## Jobs and Artifacts

A **job** is a durable container for a unit of work that goes through
the review pipeline. Jobs persist across rework cycles. Each cycle
produces an **artifact** — a snapshot of changes submitted for review.

### Two ways to create jobs

**`dispatch_job(prompt, repos)`** — leader delegates to an implementation
agent. Creates a job with `implementor: "agent"`. The agent runs, and
on completion the orchestrator creates an artifact automatically.

**`create_artifact(description)`** — leader did the work directly.
Creates a job with `implementor: "leader"` and an artifact immediately.
Skips the implementation phase.

### Lifecycle

```
dispatch_job:     Job created → Dispatched → Running → Artifact → Reviewing → Reviewed
create_artifact:  Job created → Artifact → Reviewing → Reviewed
                                                                ↘ Rework (new artifact) → ...
```

On rework, the job stays the same. A new artifact is created for the
new attempt. The agent (or leader) is resumed with rework feedback.

### Data Model

**Job** — durable identity across revisions. References its artifacts
in order.

```json
{
  "id": "job-001",
  "dispatch": { "prompt": "...", "repos": [...] },
  "implementor": "agent",
  "agent_id": "uuid-1",
  "phase": "merged",
  "artifacts": ["art-001", "art-002"]
}
```

**Artifact** — immutable snapshot of one revision. Has the implementation
summary, commits, stakeholder feedback, and the leader's decision. One
artifact per review round. Rework creates a new artifact on the same job.

```json
{
  "id": "art-001",
  "job_id": "job-001",
  "ts": "2026-04-09T01:30:00Z",
  "summary": "Created README with architecture section...",
  "commits": ["81b28b2", "ca06ce2"],
  "feedback": [
    {
      "stakeholder": "security",
      "agent_id": "uuid-2",
      "content": "No credentials found. Two accuracy issues..."
    }
  ],
  "decision": { "rework": { "feedback": "Fix the lifecycle table..." } }
}

{
  "id": "art-002",
  "job_id": "job-001",
  "ts": "2026-04-09T01:45:00Z",
  "summary": "Fixed lifecycle table, added rework row...",
  "commits": ["ca06ce2"],
  "feedback": [
    {
      "stakeholder": "security",
      "agent_id": "uuid-3",
      "content": "Looks clean, no issues."
    }
  ],
  "decision": "merge"
}
```

For leader-created jobs, `implementor` is `"leader"` and `agent_id` is
the leader's session UUID.

Every agent involved has its UUID, so its full trace can be found in
`traces/`. The revision history is the ordered artifact list on the job.
Each artifact has feedback entries with stakeholder agent UUIDs linking
to trace files.

### Future: feedback discussion

Currently one-shot: stakeholders produce feedback, leader decides. A
future enhancement could allow the leader to reply to stakeholder
feedback for clarification before deciding, turning feedback into a
thread. Not in v2 scope.

### Leader MCP Tools (updated)

- `define_project(repos, stakeholders, goal_summary)` — register repos
  and stakeholder personas.
- `dispatch_job(prompt, repos)` — delegate work to an implementation
  agent. Creates a job and starts the implementation phase.
- `create_artifact(description, job_id?)` — leader did the work, submit
  for review. If `job_id` is provided, this is a revision of that job.
  If null, a new job is created implicitly.
- `rework_job(job_id, feedback)` — send the latest artifact back for
  changes. A new artifact will be created on the next completion.
- `close_job(job_id)` — mark job as merged, clean up.

## Concierge MCP Tools

The concierge has global project management tools:

- `create_project(name, goal)` — creates project directory, runs start
  hooks, returns project path.
- `list_projects(status?)` — returns active/archived projects with
  summaries.
- `archive_project(name)` — runs stop hooks, moves to archive.
- `resume_project(name)` — moves from archive to active, runs start hooks.

The concierge decides project scope based on the user's request. Simple
requests get quick projects (leader works directly). Complex requests get
full projects (leader delegates).

## Session Queues

Every session (concierge, leaders, implementation agents, stakeholders)
has a FIFO queue. When one invocation finishes, the next is dispatched
with `claude -p --resume`. User messages from the TUI go into the queue.

## Clone Pool

Global pool at `~/.kbtz-council/pool/<repo>/`. Shallow clones with
branches fetched on demand. Shared across all projects — no duplication.
Session directories clone from the pool (local, fast).

Multiple jobs across multiple projects can use the same repo concurrently
— each gets its own session dir clone from the shared pool.

## Multi-Project Orchestration

```rust
struct Orchestrator {
    projects: HashMap<String, ProjectState>,
    concierge: ManagedSession,
    config: GlobalConfig,
    focused_project: Option<String>,
}

struct ProjectState {
    project_dir: Arc<Mutex<ProjectDir>>,
    sessions: HashMap<SessionKey, ManagedSession>,
    mcp_port: u16,
}
```

The event loop polls all projects each tick:
1. poll_sessions for all projects
2. process_tick for all projects
3. reap_and_dispatch for all projects

Global concurrency limit gates how many sessions run across all projects.

## TUI

Three navigation levels:

### Home View

Project list + concierge chat. What you see on startup.

```
┌ Projects ────────────────┐┌ Concierge ─────────────────────┐
│>> readme-feature  [2 jobs]││▶ fix the auth bug              │
│   auth-overhaul   [5 jobs]││Creating a quick project for    │
│   ─── archive ───         ││the auth fix...                 │
│   old-refactor   [merged] ││                                │
└──────────────────────────┘├─────────────────────────────────┤
                            │ Enter | ↑↓ | q quit             │
                            └─────────────────────────────────┘
```

### Project View

Sessions + stream viewer + input. Enter a project from home.

```
┌ Sessions ────────────────┐┌ job-001-impl ⏳ ───────────────┐
│>> job-001-impl [RUNNING]  ││[thinking] Let me read...       │
│   leader                  ││...                             │
│   job-001-security        ││                                │
└──────────────────────────┘├─────────────────────────────────┤
                            │ Enter | ↑↓ | Esc back           │
                            └─────────────────────────────────┘
```

### History View

Browse archived projects. Select a project to see its jobs and cycles.
Select a session to read its trace. Read-only.

```
┌ old-refactor ────────────┐┌ job-001 [MERGED] ──────────────┐
│  job-001 [MERGED]  2 cyc  ││ Cycle 1: Created auth module   │
│  job-002 [MERGED]  1 cyc  ││   security: flagged hardcoded  │
│  job-003 [ABANDONED]      ││   → rework                     │
│                            ││ Cycle 2: Removed hardcoded key │
└──────────────────────────┘│   security: clean               │
                            │   → merge                       │
                            └─────────────────────────────────┘
```

## Session Execution

All sessions use `claude -p`:
- `--output-format stream-json --verbose`
- `--session-id <uuid>` first time, `--resume <uuid>` thereafter
- `--permission-mode bypassPermissions`
- `--strict-mcp-config` for non-leader/concierge sessions
- `--append-system-prompt` for role-specific instructions
- `KBTZ_COUNCIL=1` env var (allows CLAUDE.md to skip irrelevant instructions)

## Session Recovery

On restart, `recover_from_state()` per project:
- `Running` → `Dispatched` (tick re-spawns with --resume)
- `Reviewing` → `Completed` (tick re-spawns stakeholders)
- Other phases handled normally by tick
- ManagedSession entries recreated with persisted UUIDs

## Lifecycle Architecture (per project)

Three-phase tick, unchanged from v1:

1. **poll_sessions** — I/O only. Drain events, detect exits, extract
   results. Does NOT reap or transition phases.
2. **process_tick** — Pure decisions. Builds WorldSnapshot including
   exited-but-not-reaped sessions. tick() returns actions.
3. **reap_and_dispatch** — Cleanup. Remove exited sessions, dispatch
   queued items.

## Notes

`notes.md` in the project directory — user-editable, append-only. The
leader reads it at every invocation (like project.md). Users can add
notes via the TUI or by editing the file directly.

## Configuration

```toml
# ~/.kbtz-council/config.toml
[global]
concurrency = 4          # max concurrent agent sessions
default_stakeholders = [
  { name = "security", persona = "Review for leaked credentials..." }
]

[hooks]
# Global hooks that run for every project
start = ["echo 'project started'"]
stop = ["echo 'project stopped'"]
```

Project-level config overrides global. Project hooks run after global hooks.
