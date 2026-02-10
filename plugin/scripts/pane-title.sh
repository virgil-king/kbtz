#!/usr/bin/env bash
# Set tmux pane title to show Claude Code state and current kbtz task.
# Usage: bash pane-title.sh <idle|active|blocked>

set -euo pipefail

[ -n "${TMUX_PANE:-}" ] || exit 0

sid=$(jq -r '.session_id // empty' 2>/dev/null) || true
sid="${sid:-${CLAUDE_CODE_SESSION_ID:-}}"

state="${1:?Usage: pane-title.sh <idle|active|blocked>}"

case "$state" in
  idle)    emoji="🟡" ;;
  active)  emoji="🟢" ;;
  blocked) emoji="🔴" ;;
  *)       echo "Unknown state: $state" >&2; exit 1 ;;
esac

task=$(
  kbtz list --status active --json 2>/dev/null \
    | jq -r --arg sid "$sid" \
        'map(select(.assignee == $sid)) | first // empty | .name' 2>/dev/null
) || true

title="$emoji ${task:-(no task)}"

tmux set-option -t "$TMUX_PANE" automatic-rename off 2>/dev/null || true
tmux rename-window -t "$TMUX_PANE" "$title" 2>/dev/null || true
