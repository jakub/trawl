#!/usr/bin/env bash
# Run the two Claude Code transcripts for AC3 against the evidence stack
# (stack.sh up && stack.sh seed first). Usage: transcripts.sh <out-dir>
# Each session's raw stream stays in <out-dir> as <name>.raw.jsonl and its
# client frames as <name>.frames.raw.jsonl; issue-310's sanitize.py and
# frames.py write <name>.jsonl and <name>.frames.jsonl, the files that may be
# committed. The repository's .gitignore refuses `*.raw.jsonl`.
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
E=$(cd "$(dirname "$0")" && pwd)
RUN=${TRAWL_EVIDENCE_DIR:-$WT/target/evidence-308}
OUT=${1:?out dir}
mkdir -p "$OUT" "$RUN/cwd"
TRAWL=${CARGO_TARGET_DIR:-$WT/target}/debug/trawl
PROMPT='You are connected to a Trawl log server through the "trawl" MCP server. Work only with its tools, in this order, one call at a time, and do not skip a step even if an earlier answer seems sufficient:
1. Call orient.
2. Call fields with source "service" for one service orient lists.
3. Call search with mode "validate" for a query over that service of your choice (use the DSL essentials orient gave you).
4. Call search with mode "execute" for the same query.
5. Call read with action "page" for the result id step 4 returned, offset 0, rows 20.
6. Call read with action "cell" for one row and one column of that result.
7. Call fields with source "result" for that result id.
Then answer in at most eight short lines: what the logs show, which host or service stands out, any hint the tools raised, and the search URL verbatim if one was given.'
run_one() {
  local name=$1; shift
  local raw_frames=$OUT/$name.frames.raw.jsonl frames=$OUT/$name.frames.jsonl
  : > "$raw_frames"
  local cfg=$E/mcp-$name.json
  python3 - "$cfg" "$E/mcp-tee.sh" "$raw_frames" "$TRAWL" "$RUN/home" <<'PY'
import json, sys
cfg, wrapper, frames, trawl, home = sys.argv[1:]
json.dump({"mcpServers": {"trawl": {"command": wrapper, "args": [],
  "env": {"TRAWL_EVIDENCE_FRAMES": frames, "TRAWL_EVIDENCE_BIN": trawl, "HOME": home, "PATH": "/usr/bin:/bin"}}}},
  open(cfg, "w"))
PY
  (cd $RUN/cwd && env "$@" claude -p "$PROMPT" --setting-sources user --strict-mcp-config --mcp-config "$cfg" \
      --output-format stream-json --verbose --no-session-persistence --permission-mode dontAsk \
      --allowedTools 'mcp__trawl__orient,mcp__trawl__fields,mcp__trawl__search,mcp__trawl__read' \
      --model opus > "$OUT/$name.raw.jsonl" 2> "$OUT/$name.stderr.log") || echo "claude exit $? for $name"
  python3 -I "$E/../issue-310/sanitize.py" "$OUT/$name.raw.jsonl" "$OUT/$name.jsonl"
  python3 -I "$E/../issue-310/frames.py" "$raw_frames" "$frames"
  echo "$name: $(wc -l < "$OUT/$name.jsonl") sanitized events, $(wc -l < "$frames") client frames"
  (grep -o '"method":"[a-z/_]*"' "$frames" | sort | uniq -c | sort -rn | head -12) || true
  (grep -o '"protocolVersion":"[0-9-]*"' "$frames" | sort | uniq -c) || true
}
run_one current
run_one legacy MCP_PROTOCOL_NEGOTIATION=legacy
