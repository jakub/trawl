#!/usr/bin/env bash
# Launch wrapper for the evidence transcripts: copies every client->server
# frame to a log so the session's tool calls are on record, then runs the
# real `trawl mcp` on the profile the session names. stdout is the server's
# alone. Copied from docs/evidence/issue-308/mcp-tee.sh with the profile
# taken from TRAWL_EVIDENCE_PROFILE, since #310 records two keys.
set -euo pipefail
LOG=${TRAWL_EVIDENCE_FRAMES:?}
TRAWL=${TRAWL_EVIDENCE_BIN:?}
PROFILE=${TRAWL_EVIDENCE_PROFILE:?}
exec tee -a "$LOG" | exec "$TRAWL" mcp --profile "$PROFILE"
