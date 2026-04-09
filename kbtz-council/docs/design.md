# AI Agent Orchestrator Design

A standalone tool for leader-driven AI agent orchestration with structured
feedback loops. The orchestrator manages a project lifecycle where a leader
agent autonomously decomposes a goal into implementation jobs, dispatches
them to implementation agents, collects structured feedback from stakeholder
agents, and merges results.

## System Overview

Four components:

**Orchestrator** -- a persistent Rust binary. Owns all deterministic
lifecycle operations: clone pool management, session spawning/reaping,
commit extraction, cleanup, state management. Exposes an MCP server
(HTTP, in-process) for the leader. Provides a TUI dashboard for
observability and a chat-like input for interacting with sessions.

**Leader** -- always a headless Claude Code session (`claude -p --resume`).
Every invocation is `claude -p --resume <uuid>`, guaranteed to terminate.
The orchestrator maintains a FIFO queue of events for the leader: user
messages (typed in the TUI), feedback-ready notifications, and other
orchestrator events. When the leader finishes one invocation, the
orchestrator pops the next item and invokes again. The TUI displays the
leader's formatted stream-json output and provides a text input field for
user messages, creating a chat-like experience without PTY embedding.
The leader uses MCP tools provided by the orchestrator's HTTP server.

**Stakeholders** -- headless Claude Code sessions (`claude -p`). Each has a
persona (security reviewer, docs reviewer, etc.) defined during project
setup. Invoked in parallel when a job completes. Stakeholder sessions are
scoped per job -- each (job, stakeholder) pair is an independent session.
This allows parallel review of multiple jobs without serialization or
wrong attribution. Produce structured feedback and exit.

**Implementation agents** -- headless Claude Code sessions (`claude -p`).
Each gets a session directory with clones from the pool. Do the work,
commit, produce a summary, exit. Can be resumed with `claude -p --resume`
for rework iterations.

## Principles

- The orchestrator is the event loop. It decides when to start sessions,
  what to pass them, and what to do with results.
- All sessions use `claude -p`, guaranteed to produce output and terminate.
  No agent can stop and wait for input it won't receive.
- The leader is fully autonomous. It decomposes goals, dispatches jobs,
  incorporates feedback, merges results, and dispatches follow-up jobs
  without waiting for human input. The user can send messages via the TUI
  to provide guidance.
- Important lifecycle events (clone setup, session spawning/reaping, commit
  extraction, cleanup) are handled deterministically by the orchestrator,
  not by agents.
- Full system state is provided to the leader at every invocation to prevent
  context rot.
- The user can kill and re-prompt any session via the TUI.
- Backend abstraction: default to Claude Code, don't preclude Agent SDK.

## Lifecycle Architecture

The orchestrator loop has three phases per tick:

1. **poll_sessions** -- I/O only. Drains stream-json events from active
   sessions. Detects process exits (marks sessions as exited but does NOT
   reap them). Extracts results (summaries, feedback, commits) from newly
   exited sessions. Does NOT transition job phases.

2. **process_tick** -- Pure decisions. Builds a WorldSnapshot including
   exited-but-not-reaped sessions. Calls `lifecycle::tick()` which returns
   actions: SpawnImplementation, SpawnStakeholders, InvokeLeader,
   TransitionJob. The orchestrator executes these actions.

3. **reap_and_dispatch** -- Cleanup. Removes exited sessions from the map.
   Dispatches queued items for idle sessions.

This separation ensures the lifecycle state machine sees all session exits
before they are cleaned up, preventing the bug where reaped sessions become
invisible to the decision logic.

## Project Definition

No upfront configuration file. The user starts the orchestrator, which
shows a TUI with the leader session selected. The user types a message
describing the project. The leader calls `define_project` via MCP to
register repos and stakeholders, then dispatches jobs.

The leader persists the project definition to `project.md` in the project
directory.

## Data Model

All state lives in a project directory:

```
project/
  project.md              # Project definition (leader-authored)
  state.json              # Orchestrator state (orchestrator-owned)
  .mcp.json               # MCP config pointing at orchestrator HTTP server
  mcp-port                # Port number for the MCP server
  pool/                   # Clone pool (one shallow clone per repo)
    kbtz/                 # Shallow clone, branches fetched on demand
  repos/                  # Leader's copies (cloned from pool on first merge)
    kbtz/
  steps/                  # Per-job metadata
    job-001/
      dispatch.json
      feedback/
  sessions/               # Implementation session working directories
    job-001-impl/
      kbtz/               # Clone from pool for this job
      files/
  traces/                 # Stream-json logs per session
    leader.jsonl
    job-001-impl.jsonl
    job-001-security.jsonl
```

### Job Phases

1. **dispatched** -- leader produced dispatch, orchestrator setting up.
2. **running** -- implementation session active.
3. **completed** -- implementation session exited. Summary extracted.
4. **reviewing** -- stakeholder sessions running in parallel.
5. **reviewed** -- all stakeholder feedback collected. Leader invoked.
6. **merged** -- leader merged and called close_job.
7. **rework** -- leader called rework_job. New implementation session
   spawns with rework feedback as the prompt.

## Communication Protocol

### Leader -> Orchestrator (MCP tools via HTTP)

The orchestrator runs an in-process HTTP MCP server (Streamable HTTP
protocol via tiny_http). Four tools:

**`define_project(repos, stakeholders, goal_summary)`**
Registers repos and stakeholder personas. Does not clone repos (cloning
is deferred to job dispatch via the pool).

**`dispatch_job(prompt, repos, files)`**
Leader describes a job. repos is an array of `{name, branch}` objects.
Orchestrator assigns a job ID, ensures pool clones have the needed
branches, creates session directory with clones from pool, launches
implementation session.

**`rework_job(job_id, feedback)`**
Leader rejects the implementation. Orchestrator spawns a new
implementation session with the rework feedback as the prompt (combined
with the original task description).

**`close_job(job_id)`**
Leader is done with this job. Orchestrator cleans up session directory.

### Orchestrator -> Leader

When all stakeholder feedback for a job is collected, the orchestrator
enqueues a leader invocation with: full state snapshot (including
project.md), all job statuses, and stakeholder feedback.

### Orchestrator -> Implementation Agents

`claude -p` with implementation prompt, working directory set to the
session directory containing repo clones.

### Orchestrator -> Stakeholders

`claude -p` per stakeholder, scoped per job. Each stakeholder session is
independent -- parallel review of multiple jobs works correctly.

## Session Queues

Every session has a FIFO queue of invocations. When a session finishes
one invocation, the orchestrator pops the next item and invokes
`claude -p --resume` again. A session is either running (processing an
item) or idle (queue empty).

Leader queue items: user messages, feedback-ready notifications.
Stakeholder queue items: job reviews.
Implementation queue items: initial dispatch, rework feedback.

## Clone Pool

One shallow clone per repo in `pool/<repo-name>/`. Branches are fetched
on demand with `git fetch --depth 1 origin <branch>:<branch>`.

Session directories clone from the pool (local, fast). Multiple jobs
can use the same repo concurrently -- each gets its own session dir
clone from the shared pool.

Leader's repos in `repos/` are initialized from the pool on first merge.

## Git Workflow

When an implementation job completes, the orchestrator:
1. Fetches commits from the session clone into the leader's repo as a
   named branch (e.g. `job-001` or `job-001/repo-name` for multi-repo).
2. The leader merges or cherry-picks when invoked with feedback.

## TUI

The orchestrator provides a monitoring TUI with:

- **Dashboard panel** (left): job list with phases, session list with
  status indicators (running/queued/idle).
- **Session panel** (right): formatted stream-json output of the selected
  session. Shows thinking, tool calls, text, and results.
- **Input field** (bottom): multi-line text input for sending messages to
  the selected session. Press Enter to type, Ctrl+S to send, Esc to cancel.
- **Navigation**: Up/Down arrows or Tab to switch sessions.

User messages appear in blue in the stream view. A running session shows
a spinner emoji in the title.

## Session Execution

All sessions use `claude -p`:
- `--output-format stream-json --verbose` for observability.
- `--session-id <uuid>` on first invocation, `--resume <uuid>` thereafter.
- `--permission-mode bypassPermissions` for headless tool access.
- `--mcp-config <path>` for the leader (to access council tools).
- `--append-system-prompt` for role-specific instructions.

## Prompts

Prompts live in `prompts/*.md` files, compiled in via `include_str!`.
Overridable by placing files in the project's `prompts/` directory.

- `leader-system.md` -- leader's system prompt (MCP tool docs, workflow)
- `implementation.md` -- implementation agent template
- `stakeholder.md` -- stakeholder review template
