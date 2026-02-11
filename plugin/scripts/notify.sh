#!/usr/bin/env bash
# Notify the user when Claude Code needs attention.
# - Rings the tmux bell so the window gets flagged
# - Sends a desktop notification that switches to the tmux window on click

if [ -n "$TMUX_PANE" ]; then
  window_id=$(tmux display-message -t "$TMUX_PANE" -p '#{session_name}:#{window_index}')
  printf '\a' # BEL — tmux flags the window
  notify-send --action="switch=Switch to window" "Claude Code" "Needs your attention" \
    | while read -r action; do
        if [ "$action" = "switch" ]; then
          tmux select-window -t "$window_id"
        fi
      done &
else
  notify-send "Claude Code" "Needs your attention"
fi
