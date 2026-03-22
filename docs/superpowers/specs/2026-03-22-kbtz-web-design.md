# kbtz-web: Web Server for kbtz-workspace

## Overview

kbtz-web is a new binary in the kbtz Cargo workspace that provides browser-based access to the kbtz workspace. It replaces the terminal TUI with a web UI while reusing the same task database, lifecycle logic, and agent orchestration patterns from kbtz-workspace.

**Goals:**
- Remote access from devices without a terminal (phone, tablet)
- Simultaneous multi-device access with independent viewports
- API-first architecture (JSON over WebSocket + REST)

**Non-goals:**
- Replacing kbtz-workspace — the two are mutually exclusive (only one runs at a time)
- Full terminal emulation in the browser
- Multi-user/multi-tenant access

## Architecture

Single async process (axum + tokio) with detached JSON shepherd processes for session persistence.

```
Browser(s)
   │ WebSocket + REST (via reverse proxy with TLS)
   ▼
┌────────────────────────────────┐
│         kbtz-web               │
│                                │
│  axum server                   │
│    ├─ /api/*  (task CRUD)      │
│    ├─ /ws     (WebSocket)      │
│    └─ /*      (static SPA)     │
│                                │
│  lifecycle tick() ◄── inotify  │
│       │                        │
│  session manager               │
│       │ Unix sockets           │
└───────┼────────────────────────┘
        │
   ┌────┴────┐
   │ shep/1  │  detached JSON shepherds
   │ shep/2  │  (claude --output-format stream-json)
   └─────────┘
        │
   SQLite DB (~/.kbtz/kbtz.db)
```

**Crate dependencies:**
- `kbtz` — DB ops, config, model, paths
- `kbtz-workspace-core` (new crate) — `lifecycle::tick()`, `WorldSnapshot`, `SessionPhase`, `Action`, prompt assembly
- Does NOT depend on PTY/VTE/terminal code

**New shared crate:** The `lifecycle.rs` and `prompt.rs` modules must be extracted from `kbtz-workspace` into a new `kbtz-workspace-core` crate. The current `kbtz-workspace` crate unconditionally pulls in `portable-pty`, `vt100`, `ratatui`, and `crossterm` — depending on it would defeat the goal of avoiding PTY/terminal dependencies. Both `kbtz-workspace` and `kbtz-web` will depend on `kbtz-workspace-core` for the shared lifecycle logic.

**Exclusivity:** kbtz-web and kbtz-workspace both claim tasks and spawn agents against the same database. Only one may run at a time. No shared orchestrator is planned for v1.

## JSON Shepherd

Each agent session runs as a detached shepherd process that communicates with the server over a Unix socket using a JSON-native protocol. Shepherds survive server restarts.

### Shepherd Responsibilities

1. Spawn the agent CLI (e.g. `claude --output-format stream-json`)
2. Read JSON events from the agent's stdout, buffer in a ring buffer
3. Listen on a Unix socket for the server to connect
4. On connect: send buffered event history, then stream new events live
5. Accept user input messages from the server, forward to agent stdin
6. Write an atomic state file on start, remove on exit

### Wire Protocol

Framed messages: 4-byte big-endian length + 1-byte type + payload.

| Type | Byte | Direction | Payload |
|------|------|-----------|---------|
| `EventBatch` | `0x01` | shepherd → server | JSON array of buffered agent events (sent on connect) |
| `Event` | `0x02` | shepherd → server | Single JSON agent event (streamed live) |
| `Input` | `0x03` | server → shepherd | JSON string — user message to forward to agent |
| `Shutdown` | `0x04` | server → shepherd | Empty — request graceful exit |

### Event History

The shepherd maintains a ring buffer of agent events with a configurable **event count cap** (default: 10,000 events, configurable via `[web] event_history_limit` in `workspace.toml`). The server passes this value to the shepherd as a command-line argument at spawn time. Oldest events are dropped when the buffer is full. On reconnect, the server receives the full buffer as an `EventBatch`. The batch includes metadata indicating whether history was truncated, so the UI can display a "history truncated" indicator.

### Spawn Readiness (from shepherd-redesign)

The server creates a pipe before spawning the shepherd, passes the write-end fd. The shepherd writes its socket path to the pipe when ready, then closes it. The server's read() blocks until the shepherd is ready. If the shepherd crashes before writing, read() returns EOF — no filesystem polling, no stale file races.

### Atomic State File (design from shepherd-redesign, new implementation)

Replaces the current per-session `.pid`, `.child-pid`, and `.sock` files with a single JSON file per session, written atomically (write temp + rename):

```json
{
  "session_id": "ws/3",
  "shepherd_pid": 12345,
  "child_pid": 12346,
  "socket_path": "/home/user/.kbtz/workspace/ws-3.sock",
  "task": "auth-api",
  "agent_type": "claude"
}
```

The shepherd creates the file on start and removes it on exit. The server reads state files only for reconnection and only deletes them when the shepherd is confirmed dead.

### Monotonic Session IDs (design from shepherd-redesign, new implementation)

Session IDs are never decremented on spawn failure. Failed spawns consume their ID, preventing ID reuse and stale file collisions. Note: the current kbtz-workspace decrements the counter on spawn failure — this is a new behavior that must be implemented in kbtz-web (and eventually backported to kbtz-workspace via the shepherd-redesign task).

### Reconnection

On server startup, scan state files in the workspace directory. For each live shepherd (verify pid is alive), connect to its Unix socket and receive an `EventBatch` with buffered history. Dead shepherds get their state files cleaned up.

## Session Manager

The session manager runs inside the main tokio runtime and bridges lifecycle decisions to shepherd processes.

### Lifecycle Loop

1. Filesystem watcher fires (DB changed) via the `notify` crate (cross-platform: inotify on Linux, kqueue on macOS) or fallback timer ticks (~60s)
2. Build `WorldSnapshot` from DB state + tracked sessions
3. Call `lifecycle::tick()` (reused from kbtz-workspace) → `Vec<Action>`
4. Apply actions:
   - `SpawnUpTo(n)` — claim tasks via DB, spawn JSON shepherds, connect via pipe readiness
   - `RequestExit(session_id)` — send `Shutdown` to shepherd via socket
   - `ForceKill(session_id)` — SIGKILL the shepherd pid
   - `Remove(session_id)` — clean up tracking, push updated task tree to browsers

### Session Tracking

In-memory state per active session:

```rust
struct TrackedSession {
    session_id: String,
    task_name: String,
    agent_type: String,
    phase: SessionPhase,  // Running, Stopping, Exited
    socket: JsonShepherdConnection,
    event_buffer: Vec<AgentEvent>,  // mirror of shepherd's ring buffer
}
```

When a shepherd streams an event, the session manager appends it to the local buffer and fans out to all WebSocket clients subscribed to that session.

### Claude Code Integration

Shepherds spawn Claude Code with `--output-format stream-json --session-id <id> --resume` and inject the task prompt via `--append-system-prompt`. Claude Code's built-in session resumption means agents can pick up where they left off even if the shepherd process was restarted.

## WebSocket Protocol (Server ↔ Browser)

All browser communication uses JSON messages over a single WebSocket connection per client. Each client maintains its own subscriptions and task filter.

### Server → Browser

| Message | Payload | Trigger |
|---------|---------|---------|
| `task_tree` | Filtered task tree snapshot | On connect, on DB change (per-client filter) |
| `session_event` | `{session_id, event}` | Streamed from shepherd, only for subscribed sessions |
| `session_history` | `{session_id, events[], truncated}` | When client subscribes to a session |

### Browser → Server

| Message | Payload | Effect |
|---------|---------|--------|
| `subscribe` | `{session_id}` | Start receiving events for this session; server sends `session_history` |
| `unsubscribe` | `{session_id}` | Stop receiving events for this session |
| `send_input` | `{session_id, text}` | Forward user message to agent via shepherd. Silently dropped if session is not in `Running` phase. |
| `task_mutation` | `{action, ...}` | Create task, add note, mark done, etc. |
| `set_task_filter` | `{include_done, include_paused, root?, search?}` | Update this client's filter; triggers immediate `task_tree` push |

### Task Tree with Session Info

Session state is bundled into the task tree rather than sent as a separate message. Each task node includes an optional `session` field:

```json
{
  "name": "auth-api",
  "status": "active",
  "assignee": "ws/3",
  "description": "Implement auth API endpoints.",
  "session": {
    "session_id": "ws/3",
    "agent_type": "claude",
    "phase": "running"
  },
  "children": []
}
```

Tasks without an active session have `session: null`. Spawn/reap events trigger a `task_tree` push like any other DB change.

### Per-Client Filtering

Each WebSocket connection stores its own task filter on the server side. When inotify fires, the server re-queries and pushes filtered trees per-client (only if the result changed). Default filter: exclude done, exclude paused.

## Web UI

### Technology

Solid (SolidJS) frontend, built as static assets served by axum. Fine-grained reactivity maps well to WebSocket-driven state updates.

### Layout

**Desktop:** Two-panel layout.
- **Left panel:** Task tree with collapsible hierarchy, status icons, session indicators (pulsing dot, agent type badge). Filter controls at top (toggle done/paused, search). Click a task to see details and notes.
- **Right panel:** Chat view for the selected session. Agent events rendered as a message list. Text input at bottom for sending messages. Tabs or session switcher for watching multiple sessions.

**Mobile:** Panels stack vertically. Task tree collapses to a drawer/overlay. Chat view takes full screen.

### Modular Renderer

Each agent event type maps to a renderer function. v1 ships with text-only renderers; richer renderers (diff views, syntax highlighting) can be added per content type without changing the data flow.

```typescript
const renderers: Record<string, (event: AgentEvent) => JSX.Element> = {
  "assistant": renderAssistantText,
  "tool_use": renderToolUseText,
  "tool_result": renderToolResultText,
  "system": renderSystemText,
}
```

Fallback: any unmapped event type renders as a monospace text block with the raw JSON.

### Task Mutations

The tree UI supports basic task operations directly: create task (name + description), add note, pause, reopen. These send `task_mutation` messages over WebSocket. Agents also mutate tasks via the `kbtz` CLI from within their sessions — both paths write to the same SQLite database and the UI stays in sync via inotify.

## Authentication

### Token-Based Auth with Cookie Session

Single-user system with token authentication:

1. Server generates a random token on first start, stores it in `~/.kbtz/web-token` (file permissions `0600`)
2. Token persists across restarts; only regenerated explicitly
3. User navigates to `https://host/` — server sees no session cookie, serves a login page with a "Paste access token" input
4. User pastes token, form POSTs to `/auth`
5. Server validates the token, sets an `HttpOnly` / `SameSite=Strict` / `Secure` cookie, redirects to `/`
6. All subsequent requests (REST and WebSocket upgrade) authenticate via the cookie

The token never appears in a URL — no leakage via browser history, reverse proxy logs, or referer headers.

**CLI command:** `kbtz-web token` prints the current token and access URL. Run once on the server per new device.

### Auth Trait

The auth layer is abstracted behind a trait for future extensibility (e.g. OAuth):

```rust
trait Authenticator: Send + Sync {
    fn authenticate(&self, request: &Request) -> Result<(), AuthError>;
}
```

Note: synchronous trait method — token validation is a simple string comparison with no async I/O. If a future OAuth implementation needs async, this can be converted using `async-trait` or Rust edition 2024 `async fn` in traits at that time.

## TLS and Network

The server binds to `127.0.0.1` by default and expects a reverse proxy (e.g. Caddy, nginx) to handle TLS termination:

```
Browser → Reverse proxy (HTTPS, :443) → kbtz-web (HTTP, 127.0.0.1:8080)
```

Binding to a non-loopback address without the `--allow-insecure` flag causes the server to refuse to start with an error message explaining the risk. This prevents accidentally exposing unencrypted cookies on the network.

## Configuration

Extends the existing `~/.kbtz/workspace.toml`:

```toml
[web]
bind = "127.0.0.1:8080"

[workspace]
concurrency = 4
backend = "claude"

[agent.claude]
command = "claude"
args = ["--output-format", "stream-json"]
```

The `kbtz-web` binary reads the same config file as `kbtz-workspace`. Agent config, concurrency, and workspace directory settings are shared. Adding the `[web]` section requires adding a `web` field to the `Config` struct in the `kbtz` crate's `config.rs`. The existing `Config` struct does not use `deny_unknown_fields`, so this is a backwards-compatible addition — `kbtz-workspace` will simply ignore the `[web]` section.

**CLI:**
- `kbtz-web` — start the server
- `kbtz-web token` — print the current access token and URL (subcommand of kbtz-web, not kbtz, to avoid coupling the core CLI to the web feature)

## Deployment

Container image as a distribution package, not an isolation boundary:

- Bundles the `kbtz-web` binary and pre-built Solid frontend assets
- Claude Code CLI must be available inside the container
- Host volume mount for `~/.kbtz/` (database, token, state files, config)
- Host volume mount for project directory (agent file access)
- Host network (`--network=host`) or explicit port mapping
- Dev tools, git, SSH keys available in the container (agents need them)

Alternative: install `kbtz-web` as a native binary directly on the host.

## SQLite in Async Context

The existing codebase uses synchronous `rusqlite` with `PRAGMA busy_timeout`. In the async tokio runtime, blocking on SQLite calls would stall the executor. All DB access in kbtz-web must use `tokio::task::spawn_blocking` or a dedicated DB thread to avoid blocking the async runtime.

## Code Reuse from kbtz-workspace

| Component | Reuse | Notes |
|-----------|-------|-------|
| `lifecycle::tick()` | Direct — via new `kbtz-workspace-core` crate | Pure state machine, no I/O coupling |
| `lifecycle` types | Direct — `WorldSnapshot`, `SessionPhase`, `Action` | Via `kbtz-workspace-core` |
| Shepherd concept | Adapted — new JSON-native protocol | Same pattern: detached process, Unix socket, ring buffer. New implementation, not code reuse. |
| Shepherd robustness | New implementation based on shepherd-redesign designs | Monotonic IDs, pipe readiness, atomic state files. These designs exist as task specs but are not yet implemented in the current codebase. |
| `kbtz` DB ops | Direct — same crate dependency | Task CRUD, claiming, blocking, notes |
| `kbtz` config | Extended — add `[web]` section and `WebConfig` to `Config` struct | Same TOML file, shared agent config. Requires modifying `kbtz` crate. |
| Backend trait | Not reused — kbtz-web builds agent commands directly | Current trait is PTY-oriented |
| Session/VTE/PTY | Not reused | Replaced by JSON shepherd |
| Tree UI (ratatui) | Not reused | Replaced by Solid frontend |
| Prompt assembly | Reused via `kbtz-workspace-core`, with minor adaptation | Current `AGENT_PROMPT` references "the TUI" — needs web-neutral language when used by kbtz-web |
