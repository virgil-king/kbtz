#!/usr/bin/env bash
# PostToolUse hook: remind agents not to work on tasks they create.
#
# Fires after Bash tool calls. Checks if the command was `kbtz add` or
# `kbtz exec` and injects a reminder that the workspace spawns dedicated
# agents for new tasks.

set -euo pipefail

# Only relevant inside a workspace session with an assigned task.
[ -n "${KBTZ_TASK:-}" ] || exit 0

input=$(cat)
command=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')

# Match `kbtz add` or `kbtz exec` anywhere in the command.
if [[ "$command" =~ kbtz[[:space:]]+(add|exec)([[:space:]]|$) ]]; then
  cat <<'MSG'
{"systemMessage": "Reminder: the workspace automatically assigns agents to new tasks. Do not work on tasks you create — a dedicated agent session will be spawned for each one. Focus on your assigned task ($KBTZ_TASK)."}
MSG
fi
