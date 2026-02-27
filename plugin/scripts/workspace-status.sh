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

# Sanitize session ID for filename (ws/3 -> ws-3)
filename="${KBTZ_SESSION_ID//\//-}"
status_file="${KBTZ_WORKSPACE_DIR}/${filename}"

# Don't let Stop (idle) overwrite needs_input — the Notification hook fires
# before Stop when the agent calls AskUserQuestion, so the sequence is:
#   PreToolUse → active, Notification → needs_input, Stop → idle
# Without this guard the session would show idle when it's actually waiting.
# SessionEnd passes --force to bypass this (a dead session can't need input).
if [ "$state" = "idle" ] && [ "$force" != "--force" ]; then
  current=$(cat "$status_file" 2>/dev/null) || true
  [ "$current" = "needs_input" ] && exit 0
fi

printf '%s' "$state" > "$status_file"
