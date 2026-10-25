#!/usr/bin/env bash
# Run the two Claude Code transcripts for #310 against the evidence stack
# (stack.sh up && stack.sh seed first, then this within 30 minutes, so
# `last=1h` still holds the incident). Usage: transcripts.sh <out-dir>
# Copied from docs/evidence/issue-309/transcripts.sh: the agent session on the
# `agent` profile opens and works an investigation; the person's session on the
# read-only `human` profile then finds it through orient and reads the board.
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
E=$(cd "$(dirname "$0")" && pwd)
RUN=${TRAWL_EVIDENCE_DIR:-$WT/target/evidence-310}
OUT=${1:?out dir}
mkdir -p "$OUT" "$RUN/cwd"
TRAWL=${CARGO_TARGET_DIR:-$WT/target}/debug/trawl

# One session: write its MCP config, run `claude -p` through mcp-tee.sh on the
# named profile, and count the tool calls the frames log holds.
session() {
  local name=$1 profile=$2 tools=$3 prompt=$4
  local cfg=$E/mcp-$name.json frames=$OUT/$name.frames.jsonl
  : > "$frames"
  python3 - "$cfg" "$E/mcp-tee.sh" "$frames" "$TRAWL" "$RUN/home" "$profile" <<'PY'
import json, sys
cfg, wrapper, frames, trawl, home, profile = sys.argv[1:]
json.dump({"mcpServers": {"trawl": {"command": wrapper, "args": [],
  "env": {"TRAWL_EVIDENCE_FRAMES": frames, "TRAWL_EVIDENCE_BIN": trawl, "TRAWL_EVIDENCE_PROFILE": profile,
          "HOME": home, "PATH": "/usr/bin:/bin"}}}},
  open(cfg, "w"))
PY
  (cd "$RUN/cwd" && claude -p "$prompt" --setting-sources user --strict-mcp-config --mcp-config "$cfg" \
      --output-format stream-json --verbose --no-session-persistence --permission-mode dontAsk \
      --allowedTools "$tools" \
      --model opus > "$OUT/$name.stream.jsonl" 2> "$OUT/$name.stderr.log") || echo "claude exit $?"
  rm -f "$cfg"
  echo "$name: $(wc -l < "$OUT/$name.stream.jsonl") stream events, $(wc -l < "$frames") client frames"
  (grep -o '"name":"[a-z]*","arguments"' "$frames" | sort | uniq -c) || true
}

AGENT_PROMPT='You are connected to a Trawl log server through the "trawl" MCP server. Work only with its tools, in this order, one call at a time, and do not skip a step even if an earlier answer seems sufficient:
1. Call orient.
2. Open a case file: call investigation with action "create", title "502s on checkout", question "Why does checkout return 502 since the last deploy?" and scope "env=prod: nginx, shop-api, postgres".
3. Find when checkout started answering 502: call search with mode "execute", query "service=nginx status=502 last=1h", and the investigation id step 2 returned as investigation, with a purpose that says why you ran it.
4. Keep the first and the last 502 of that result: call investigation with action "keep", the result id step 3 returned and the two row numbers, with a reason.
5. See what shop-api logged around the first 502: call search with mode "execute", query "service=shop-api last=1h", attached to the same investigation with a purpose.
6. Record what you found: call investigation with action "finding", status "supported" or "hypothesis" as the rows warrant, a one-sentence statement, references to the rows of both results that support it, and the question that remains.
7. Call investigation with action "read" on the investigation.
Then answer in at most six short lines: the investigation id, each result id the tools returned, verbatim, the finding you wrote and the rows it cites, and the sequence the read reported.'

session agent agent 'mcp__trawl__orient,mcp__trawl__search,mcp__trawl__read,mcp__trawl__fields,mcp__trawl__investigation' "$AGENT_PROMPT"

INV=$(python3 - "$OUT/agent.stream.jsonl" <<'PY'
import re, sys
m = re.search(r'Created investigation \\?"([0-9a-f]{32})', open(sys.argv[1]).read())
print(m.group(1) if m else "")
PY
)
[ -n "$INV" ] || { echo "the agent session created no investigation; the human session has nothing to read"; exit 1; }

HUMAN_PROMPT="You are connected to a Trawl log server through the \"trawl\" MCP server, with a key that may read investigations but not write them. Work only with its tools, in this order, one call at a time:
1. Call orient, and find the open investigation it lists.
2. Call investigation with action \"read\" on the id orient listed.
Then answer in at most six short lines: the investigation's id, title and status as orient listed them, who opened it, its latest finding, and the result ids and rows that finding cites. The id the agent session created was ${INV}; say whether orient listed the same one."

session human human 'mcp__trawl__orient,mcp__trawl__investigation,mcp__trawl__read' "$HUMAN_PROMPT"
