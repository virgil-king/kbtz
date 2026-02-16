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

state="${1:?Usage: workspace-status.sh <idle|active|needs_input>}"

# Sanitize session ID for filename (ws/3 -> ws-3)
filename="${KBTZ_SESSION_ID//\//-}"

printf '%s' "$state" > "${KBTZ_WORKSPACE_DIR}/${filename}"
