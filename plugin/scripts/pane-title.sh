#!/usr/bin/env bash
# Set tmux pane title to show Claude Code state and current kbtz task.
# Usage: bash pane-title.sh <idle|active|needs_input> [hook_event]

set -euo pipefail

[ -n "${TMUX_PANE:-}" ] || exit 0

# Only set titles for task agent windows (those with KBTZ_TASK set).
[ -n "${KBTZ_TASK:-}" ] || exit 0

state="${1:?Usage: pane-title.sh <idle|active|needs_input> [hook_event]}"
event="${2:-unknown}"

# Diagnostic logging (enabled by KBTZ_DEBUG=<path>)
_hook_log() {
  [ -n "${KBTZ_DEBUG:-}" ] || return 0
  printf '[%s] %s\n' "$(date -Iseconds)" "$*" >> "$KBTZ_DEBUG" 2>/dev/null || true
}

# Don't let Stop (idle) overwrite needs_input — see workspace-status.sh for
# the full explanation of the AskUserQuestion event ordering.
if [ "$state" = "idle" ] && [ -n "${KBTZ_WORKSPACE_DIR:-}" ] && [ -n "${KBTZ_SESSION_ID:-}" ]; then
  filename="${KBTZ_SESSION_ID//\//-}"
  current=$(cat "${KBTZ_WORKSPACE_DIR}/${filename}" 2>/dev/null) || true
  [ "$current" = "needs_input" ] && exit 0
fi

case "$state" in
  idle)        emoji="🟡" ;;
  active)      emoji="🟢" ;;
  needs_input) emoji="🔔" ;;
  *)           echo "Unknown state: $state" >&2; exit 1 ;;
esac

task="${KBTZ_TASK:-}"

title="$emoji ${task:-(no task)}"

_hook_log "pane-title: event=$event sid=${KBTZ_SESSION_ID:-?} state=$state task=${task:-(none)} title=$title"

tmux set-option -t "$TMUX_PANE" automatic-rename off 2>/dev/null || true
tmux rename-window -t "$TMUX_PANE" "$title" 2>/dev/null || true
