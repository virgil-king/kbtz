#!/usr/bin/env bash
# Write session environment variables to CLAUDE_ENV_FILE.
# Extracts session_id from hook JSON stdin and exports it as KBTZ_SESSION_ID.

set -euo pipefail

jq -r '"export KBTZ_SESSION_ID=\(.session_id)"' >> "$CLAUDE_ENV_FILE"
