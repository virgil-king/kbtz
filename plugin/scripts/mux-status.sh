#!/usr/bin/env bash
# Report session state to kbtz-mux via status file.
# Usage: bash mux-status.sh <idle|active|blocked>
#
# When running under kbtz-mux, KBTZ_MUX_DIR and KBTZ_SESSION_ID are set.
# The mux watches KBTZ_MUX_DIR with inotify and reads status files to
# update session indicators in the tree view.

set -euo pipefail

[ -n "${KBTZ_MUX_DIR:-}" ] || exit 0
[ -n "${KBTZ_SESSION_ID:-}" ] || exit 0

state="${1:?Usage: mux-status.sh <idle|active|blocked>}"

# Sanitize session ID for filename (mux/3 -> mux-3)
filename="${KBTZ_SESSION_ID//\//-}"

printf '%s' "$state" > "${KBTZ_MUX_DIR}/${filename}"
