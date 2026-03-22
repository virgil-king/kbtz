#!/usr/bin/env bash
# Report session state to kbtz-workspace via status file.
# Usage: bash workspace-status.sh <idle|active|needs_input|error> [hook_event]
#
# When running under kbtz-workspace, KBTZ_WORKSPACE_DIR and KBTZ_SESSION_ID
# are set. The workspace watches KBTZ_WORKSPACE_DIR with inotify and reads
# status files to update session indicators in the tree view.

set -euo pipefail

[ -n "${KBTZ_WORKSPACE_DIR:-}" ] || exit 0
[ -n "${KBTZ_SESSION_ID:-}" ] || exit 0

state="${1:?Usage: workspace-status.sh <idle|active|needs_input|error> [hook_event]}"
event="${2:-unknown}"

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

_hook_log "workspace-status: event=$event sid=$KBTZ_SESSION_ID state=$state prev=${prev:-<none>} task=${KBTZ_TASK:-?}"

# Don't let Stop (idle) overwrite needs_input or error — this guard
# exists for AskUserQuestion, where the sequence is:
#   PreToolUse → active, Notification → needs_input, Stop → idle
# Without it the session would show idle when it's actually waiting.
#
# For permission_prompt the sequence is:
#   PreToolUse → active, Notification → needs_input, (user approves),
#   PostToolUse → active, ..., Stop → idle
# PostToolUse clears needs_input after approval, so Stop sees prev=active
# and correctly sets idle.
#
# Error state is sticky — once set by StopFailure, only active/SessionEnd
# should clear it. The session will resume on the next user prompt.
#
# SessionEnd bypasses this (a dead session can't need input or be errored).
if [ "$state" = "idle" ] && [ "$event" != "SessionEnd" ]; then
  [ "$prev" = "needs_input" ] && exit 0
  [ "$prev" = "error" ] && exit 0
fi

printf '%s' "$state" > "$status_file"
