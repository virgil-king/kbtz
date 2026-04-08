# AI Agent Orchestrator Design

A standalone tool for leader-driven AI agent orchestration with structured
feedback loops. The orchestrator manages a project lifecycle where a leader
agent autonomously decomposes a goal into implementation steps, dispatches
them to implementation agents, collects structured feedback from stakeholder
agents, and merges results.

## System Overview

Four components:

**Orchestrator** -- a persistent Rust binary. Owns all deterministic
lifecycle operations: clone management, session spawning/reaping, commit
extraction, cleanup, state management. Exposes an MCP server for the leader.
Provides a TUI dashboard for observability and interactive leader access.

**Leader** -- a Claude Code session with two modes:
- Interactive TUI: user chats with the leader to define the project, provide
  guidance, or review state. Embedded in the orchestrator's TUI via PTY
  forwarding (reusing kbtz-workspace library code).
- Headless decision mode: orchestrator invokes with `claude -p --resume`
  when feedback is ready. Leader reviews state and feedback, produces
  decisions (dispatch/merge/rework), exits. Guaranteed to terminate.

Both modes share conversation history via `--resume`. The leader uses MCP
tools provided by the orchestrator.

**Stakeholders** -- headless Claude Code sessions (`claude -p`). Each has a
persona (security reviewer, API design reviewer, etc.) defined during project
setup. Invoked in parallel when a step completes. Receive read-only access to
the implementation session directory (including clone commit history) and the
leader's repos for broader codebase context. Produce structured feedback and
exit.

**Implementation agents** -- headless Claude Code sessions (`claude -p`).
Each gets a private sandboxed directory containing shallow clones of relevant
repos plus any leader-provided files. Do the work, commit, produce a summary,
exit. Can be resumed with `claude -p --resume` for rework iterations.

## Principles

- The orchestrator is the event loop. It decides when to start sessions, what
  to pass them, and what to do with results. Agents are functions: prompt in,
  commits + text out.
- All sessions use `claude -p` (except interactive leader). This guarantees
  sessions produce output and terminate. No agent can stop and wait for input
  it won't receive.
- The leader is fully autonomous. It decomposes goals, dispatches steps,
  incorporates feedback, merges results, and dispatches follow-up steps
  without waiting for human input. The user can drop into the interactive TUI
  at any time to provide guidance, but the leader does not block on it.
- Important lifecycle events (clone setup, session spawning/reaping, commit
  extraction, cleanup) are handled deterministically by the orchestrator, not
  by agents.
- Full system state is provided to the leader at every invocation to prevent
  context rot. The leader never has to guess or remember what's going on.
- The user can kill and re-prompt any session via the TUI. The orchestrator
  does not auto-intervene.
- Backend abstraction: default to Claude Code, don't preclude Agent SDK.

## Project Definition

No upfront configuration file. The user starts the orchestrator, which
launches an interactive leader session. The user chats with the leader to
define:
- Project goal
- Repos involved (multiple repos supported)
- Stakeholder personas and their concerns

The leader persists the project definition to `project.md` in the project
directory. This file is the durable source of truth -- it survives context
compression and session restarts. The leader reads it at every invocation and
updates it as the project evolves. The orchestrator never modifies this file.

## Data Model

All state lives in a project directory:

```
project/
  project.md              # Project definition (leader-authored)
  state.json              # Orchestrator state (orchestrator-owned)
  repos/                  # Leader's copies of declared repos
    repo-a/
    repo-b/
  steps/
    step-001/
      dispatch.json       # Step spec from leader
      summary.md          # Implementation agent's completion summary
      feedback/
        security.json     # One file per stakeholder persona
        api-design.json
      decision.json       # Leader's merge/rework/close decision
    step-002/
      ...
  sessions/
    step-001-impl/        # Implementation session private dir (sandbox)
      repo-a/             # Shallow clone
      repo-b/             # Shallow clone
      files/              # Leader-provided files
    step-001-security/    # Stakeholder working dir (if needed)
    ...
  claude-sessions/        # Claude Code session IDs for --resume
    leader.json
    step-001-impl.json
    ...
```

### Step States

1. **dispatched** -- leader produced dispatch.json, orchestrator is setting
   up clones and launching the implementation session.
2. **running** -- implementation session active.
3. **completed** -- implementation session exited. Commits and summary
   extracted.
4. **reviewing** -- stakeholder sessions running in parallel.
5. **reviewed** -- all stakeholder feedback collected. Orchestrator has
   fetched commits into leader's repos as a branch. Leader invoked.
6. **merged** -- leader merged the branch and called `close_step`.
   Orchestrator cleans up the session directory.
7. **rework** -- leader called `rework_step`. Orchestrator resumes the
   implementation session with feedback.

Unacted-on steps (leader never closes or reworks) surface as "pending leader
action" in future state snapshots.

## Communication Protocol

### Leader -> Orchestrator (MCP tools)

The orchestrator runs an MCP server that the leader's Claude Code session
connects to. Four tools:

**`define_project(repos, stakeholders, goal_summary)`**
Registers repos and stakeholder personas. Orchestrator clones repos into
`project/repos/`.

**`dispatch_step(prompt, repos, files)`**
Leader describes a step. Orchestrator assigns a step ID, writes
`dispatch.json`, creates a session directory with shallow clones of the
specified repos, copies provided files, and launches an implementation
session.

The dispatch is structured:
- Step ID (assigned by orchestrator, used for tracking and resumption)
- Session prompt (what the implementation agent should do)
- Repos to clone (subset of project repos relevant to this step)
- Files to provide (specs, context docs, design notes)

**`rework_step(step_id, feedback)`**
Leader rejects the implementation. Orchestrator resumes the implementation
session with `claude -p --resume`, passing the feedback as the new prompt.

**`close_step(step_id)`**
Leader is done with this step (merged or abandoned). Orchestrator deletes the
session directory and marks the step as finished.

### Orchestrator -> Leader

When all stakeholder feedback for a step is collected:

1. Fetch commits from implementation clone(s) into the corresponding leader
   repos as a branch (e.g., `step-001`).
2. Assemble full state snapshot: `project.md` contents, all step statuses,
   pending feedback, any stale/stuck sessions.
3. Invoke leader with `claude -p --resume <leader-session-id>`, passing the
   state snapshot + collected feedback as the prompt.
4. Stream JSON output for observability.
5. Parse the leader's MCP tool calls as decisions.

The leader then:
- Reviews feedback and forms its own judgment.
- Merges the branch in its repos (git merge/cherry-pick, resolves conflicts).
- Calls `close_step` for finished steps.
- Calls `rework_step` for steps that need changes.
- Calls `dispatch_step` for new follow-up steps.

### Orchestrator -> Implementation Agents

`claude -p` with:
- Working directory set to the session directory.
- Step prompt as the message.
- `--output-format stream-json` for observability.

The agent sees its session directory as its entire world. Clones are in the
directory (or subdirectories for multi-repo steps). Leader-provided files are
in `files/`. No paths communicated in the prompt -- the agent just explores
its working directory.

### Orchestrator -> Stakeholders

`claude -p` with:
- Persona prompt (role, concerns, review criteria).
- Read-only access to the implementation session directory (clone with commit
  history) and the leader's repos (broader codebase context).
- Step context (dispatch prompt, implementation summary).
- `--output-format stream-json` for observability.

All stakeholders for a step run in parallel. Each produces structured
feedback and exits.

## Review Flow

When an implementation session completes:

1. Orchestrator extracts commits and summary from the clone(s).
2. Orchestrator fetches commits into the leader's repos as a named branch.
3. Orchestrator launches all stakeholder sessions in parallel. Each gets
   read-only access to the implementation session directory and the leader's
   repos.
4. Orchestrator waits for all stakeholders to exit and collects feedback.
5. Orchestrator invokes the leader (headless) with: full state snapshot,
   implementation summary, all stakeholder feedback, and the branch name(s)
   to review.
6. Leader reviews everything, merges/cherry-picks the branch, and calls
   `close_step`, `rework_step`, or `dispatch_step` as appropriate.

## Git Model

### Leader's Repos

The leader has full copies of all declared repos in `project/repos/`. These
are the source of truth for the project. The leader merges implementation
results into these repos directly. The leader can push to remotes when
appropriate (e.g., at project milestones).

### Implementation Clones

Each implementation session gets fresh shallow clones in its private
directory. Clones are created at dispatch time by cloning from the leader's
repos (ensuring the implementation agent starts from the latest merged state).

For multi-repo steps, the session directory contains one subdirectory per
repo.

After the implementation session exits, the orchestrator fetches commits from
the clone(s) into the corresponding leader repos as named branches
(e.g., `step-001` or `step-001/repo-a`). The leader merges these branches.

After `close_step`, the orchestrator deletes the entire session directory.

### Isolation

Implementation sessions are fully sandboxed to their session directory. They
cannot read or write outside it. This prevents implementation agents from
modifying the leader's repos, other sessions' work, or project state.

Stakeholder sessions have read-only access to the implementation session
directory and the leader's repos. They produce feedback but don't modify
anything.

## TUI

The orchestrator provides a terminal UI with two main areas: a dashboard
panel and a session panel.

### Dashboard Panel

- Project state: goal, repos, stakeholder personas.
- Step list with current phase (dispatched/running/completed/reviewing/
  reviewed/merged/rework).
- Active session list with status.
- Controls: select session to watch, kill + re-prompt a session, attach to
  leader interactively.

### Session Panel

The session panel shows one of three things depending on state:

**Stream-json view (default):** Read-only rendering of the selected
session's stream-json output -- thinking, tool calls, and results. This
applies to any headless session: implementation agents, stakeholders, or
the leader when running in headless decision mode. The user can switch
between active sessions to observe different agents.

**Interactive leader view:** When the user attaches to the leader, the
session panel becomes an interactive PTY session embedded via
kbtz-workspace's raw byte forwarding library. The user chats with the
leader directly.

**Idle:** When no session is selected or running.

### Leader Panel States

The leader is special -- its session panel cycles through states:

1. **Idle** -- leader is not running. User can launch it interactively.
2. **Headless** -- orchestrator invoked the leader for a decision. Panel
   shows stream-json output (read-only). User cannot attach interactively
   until this completes (or is killed).
3. **Interactive** -- user attached to the leader. Panel shows the PTY.
   User can only launch interactive mode when the leader is idle (not
   currently in a headless invocation).

## Session Execution

All sessions except the interactive leader use `claude -p`:
- Guaranteed to produce output and terminate.
- `--output-format stream-json` for real-time observability.
- `--resume <session-id>` for rework iterations and leader continuity.

The orchestrator:
- Spawns sessions as child processes.
- Consumes the stream-json output for logging and TUI display.
- Extracts structured results (summary text, tool calls) from the output.
- Detects session exit and transitions step state.
- Stores Claude Code session IDs for resumption.

## Backend Abstraction

The orchestrator abstracts over agent backends. Claude Code is the default
(cheapest via subscription). The interface is:

- Start a headless session: command + args + prompt + working directory ->
  stream-json output + exit code.
- Resume a session: session ID + new prompt -> stream-json output + exit
  code.
- Start an interactive session: PTY-based, for the leader.

Agent SDK or other backends can implement this interface. Per-session backend
override is possible (e.g., use a different model for stakeholder reviews).

## kbtz-workspace Library Reuse

The orchestrator reuses kbtz-workspace code for one purpose: embedding the
interactive leader session in the orchestrator's TUI. Specifically:

- PTY spawning and management.
- Raw byte forwarding between the PTY and the terminal.
- VTE state tracking for view switching (dashboard <-> leader).

Everything else (dashboard rendering, stream-json parsing, clone management,
MCP server, state machine) is new code in the orchestrator.

## Open Questions

- Exact Claude Code CLI flags for per-session read/write sandboxing (needed
  for stakeholder read-only access). Investigate during implementation.
- Clone cache strategy: creating shallow clones at dispatch time may be slow
  for large repos. A warm cache of pre-cloned repos that get `git fetch` +
  reset before assignment could help.
- Concurrency limits: how many implementation sessions can run in parallel.
  Configurable, default TBD.
- Project persistence across orchestrator restarts: the project directory on
  disk plus Claude Code session IDs should be sufficient, but needs
  verification.
- MCP server lifecycle: does it run as a subprocess or embedded in the
  orchestrator process? Embedded is simpler.
