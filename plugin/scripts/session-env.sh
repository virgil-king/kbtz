#!/usr/bin/env bash
# Write session environment variables to CLAUDE_ENV_FILE.
# Extracts session_id from hook JSON stdin and exports it as KBTZ_SESSION_ID.
# Guards against overwriting on resume — if already set, keep the original.

set -euo pipefail

if [ -z "${KBTZ_SESSION_ID:-}" ]; then
  jq -r '"export KBTZ_SESSION_ID=\(.session_id)"' >> "$CLAUDE_ENV_FILE"
fi
