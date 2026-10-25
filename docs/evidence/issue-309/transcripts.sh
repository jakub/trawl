#!/usr/bin/env bash
# Run the Claude Code transcript for #309 AC10 against the evidence stack
# (stack.sh up && stack.sh seed first, then this within 15 minutes, so the
# compare windows hold the seeded hours). Usage: transcripts.sh <out-dir>
# Copied from docs/evidence/issue-308/transcripts.sh: one session with the
# default negotiation and a prompt that asks the three correlation questions.
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
E=$(cd "$(dirname "$0")" && pwd)
RUN=${TRAWL_EVIDENCE_DIR:-$WT/target/evidence-309}
OUT=${1:?out dir}
mkdir -p "$OUT" "$RUN/cwd"
TRAWL=${CARGO_TARGET_DIR:-$WT/target}/debug/trawl
FOCUS_TIME=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["time"])' "$RUN/focus.json")
PROMPT="You are connected to a Trawl log server through the \"trawl\" MCP server. Work only with its tools, in this order, one call at a time, and do not skip a step even if an earlier answer seems sufficient:
1. Call orient.
2. Where else does the IP 203.0.113.77 appear in the last 24 hours? Call sightings with literal \"203.0.113.77\" and no fields.
3. Call sightings again with the same literal and fields [\"client_ip\"].
4. What happened around the nginx event on host web-02 at _time ${FOCUS_TIME} whose message contains order=7731? Find that event with search mode \"execute\" (scope it by service, host and the marker, with time bounds that hold that _time), then call context on its result id and row.
5. Did error-level events rise against the previous hour? Call compare with filter \"level=error\", the foreground window the hour that ends at the time now orient reported, no baseline bounds (the default is the hour before), and group_by [\"service\"].
6. Call read with action \"page\" for the result id step 2 returned, offset 0, rows 20.
Then answer in at most eight short lines: the sightings counts by service and by field, the _raw-only count and both denominators; the context's earlier, same-_time and later row counts and what it says about equal timestamps; the compare counts per service with which rose, which stayed flat and which vanished, in rank order; and every result id the tools returned, verbatim."
cfg=$E/mcp-current.json
frames=$OUT/current.frames.jsonl
: > "$frames"
python3 - "$cfg" "$E/mcp-tee.sh" "$frames" "$TRAWL" "$RUN/home" <<'PY'
import json, sys
cfg, wrapper, frames, trawl, home = sys.argv[1:]
json.dump({"mcpServers": {"trawl": {"command": wrapper, "args": [],
  "env": {"TRAWL_EVIDENCE_FRAMES": frames, "TRAWL_EVIDENCE_BIN": trawl, "HOME": home, "PATH": "/usr/bin:/bin"}}}},
  open(cfg, "w"))
PY
(cd "$RUN/cwd" && claude -p "$PROMPT" --setting-sources user --strict-mcp-config --mcp-config "$cfg" \
    --output-format stream-json --verbose --no-session-persistence --permission-mode dontAsk \
    --allowedTools 'mcp__trawl__orient,mcp__trawl__sightings,mcp__trawl__context,mcp__trawl__compare,mcp__trawl__read,mcp__trawl__search,mcp__trawl__fields' \
    --model opus > "$OUT/current.stream.jsonl" 2> "$OUT/current.stderr.log") || echo "claude exit $?"
rm -f "$cfg"
echo "current: $(wc -l < "$OUT/current.stream.jsonl") stream events, $(wc -l < "$frames") client frames"
(grep -o '"name":"[a-z]*","arguments"' "$frames" | sort | uniq -c) || true
(grep -o '"protocolVersion":"[0-9-]*"' "$frames" | sort | uniq -c) || true
