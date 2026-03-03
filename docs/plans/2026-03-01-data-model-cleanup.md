# Data Model Cleanup

## Problem

Session state is communicated through filesystem files with fragile conventions:
- Filename encoding (`ws/3` → `ws-3`) is duplicated and the reverse mapping
  (`replace('-', '/')`) is lossy in the general case.
- `workspace_dir` and `db_path` resolution is duplicated in 3+ places.
- Stale status files accumulate when sessions crash without cleanup.

## Changes

### 1. `kbtz::paths` module

Centralize path resolution:
- `db_path() -> String` — checks `KBTZ_DB`, falls back to `$HOME/.kbtz/kbtz.db`
- `workspace_dir() -> String` — checks `KBTZ_WORKSPACE_DIR`, falls back to `$HOME/.kbtz/workspace`
- `session_id_to_filename(id: &str) -> String` — replaces `/` with `-`
- `filename_to_session_id(name: &str) -> String` — replaces first `-` with `/`

Session IDs are system-generated (`ws/{N}`), so the `-` encoding is safe
with a first-occurrence-only reverse mapping.

### 2. Update consumers

Replace inline path resolution in:
- `kbtz-tmux/src/main.rs` (3 places)
- `kbtz-tmux/src/orchestrator.rs` (2 places)
- `kbtz-workspace/src/app.rs` (`session_id_to_filename`)

### 3. Stale status file cleanup

In `Orchestrator::reconcile()`, after adopting/killing windows:
1. List files in workspace dir
2. Skip non-status files (lock files, sentinel, sockets, pid files)
3. For each status file, check if any tracked window has that session ID
4. Delete orphaned status files
