#!/usr/bin/env bash
# Launch wrapper for the evidence transcripts: copies every client->server
# frame to a log so the negotiated protocol era is on record, then runs the
# real `trawl mcp`. stdout is the server's alone.
set -euo pipefail
LOG=${TRAWL_EVIDENCE_FRAMES:?}
TRAWL=${TRAWL_EVIDENCE_BIN:?}
exec tee -a "$LOG" | exec "$TRAWL" mcp --profile agent
