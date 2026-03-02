#!/usr/bin/env bash
# Report session state to kbtz-workspace via status file.
# Usage: bash workspace-status.sh <idle|active|needs_input>
#
# When running under kbtz-workspace, KBTZ_WORKSPACE_DIR and KBTZ_SESSION_ID
# are set. The workspace watches KBTZ_WORKSPACE_DIR with inotify and reads
# status files to update session indicators in the tree view.

set -euo pipefail

[ -n "${KBTZ_WORKSPACE_DIR:-}" ] || exit 0
[ -n "${KBTZ_SESSION_ID:-}" ] || exit 0

state="${1:?Usage: workspace-status.sh <idle|active|needs_input> [--force]}"
force="${2:-}"

# Diagnostic logging (enabled by KBTZ_DEBUG=<path>)
_hook_log() {
  [ -n "${KBTZ_DEBUG:-}" ] || return 0
  printf '[%s] %s\n' "$(date -Iseconds)" "$*" >> "$KBTZ_DEBUG" 2>/dev/null || true
}

# Sanitize session ID for filename (ws/3 -> ws-3)
filename="${KBTZ_SESSION_ID//\//-}"
status_file="${KBTZ_WORKSPACE_DIR}/${filename}"

# Read previous state for change detection
prev=""
[ -f "$status_file" ] && prev=$(cat "$status_file" 2>/dev/null) || true

_hook_log "workspace-status: sid=$KBTZ_SESSION_ID state=$state prev=${prev:-<none>} task=${KBTZ_TASK:-?}"

# Don't let Stop (idle) overwrite needs_input — the Notification hook fires
# before Stop when the agent calls AskUserQuestion, so the sequence is:
#   PreToolUse → active, Notification → needs_input, Stop → idle
# Without this guard the session would show idle when it's actually waiting.
# SessionEnd passes --force to bypass this (a dead session can't need input).
if [ "$state" = "idle" ] && [ "$force" != "--force" ]; then
  [ "$prev" = "needs_input" ] && exit 0
fi

printf '%s' "$state" > "$status_file"
