#!/usr/bin/env bash
# Run the #311 two-principal loop once against the stack `stack.sh up` and
# `stack.sh seed` made: agent.py's one `trawl mcp` process and board.mjs's
# phone session side by side, handing over through files in the run's
# signals directory. Waits for both and exits non-zero when either fails.
# Usage: run.sh <out-dir> <video-dir>
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
E=$(cd "$(dirname "$0")" && pwd)
RUN=${TRAWL_EVIDENCE_DIR:-$WT/target/evidence-311}
TARGET=${CARGO_TARGET_DIR:-$WT/target}
OUT=$1
VIDEO=$2
mkdir -p "$OUT" "$VIDEO"
rm -rf "$RUN/signals" && mkdir -p "$RUN/signals"
cp "$RUN/seed.ndjson" "$RUN/roles.txt" "$RUN/keys.txt" "$OUT/"
python3 -I "$E/agent.py" "$TARGET/debug/trawl" "$RUN/home" "$RUN/signals" "$OUT" > "$OUT/agent.stdout" 2>&1 &
agent=$!
board=0
node "$E/board.mjs" "$RUN" "$OUT" "$VIDEO" > "$OUT/board.stdout" 2>&1 || board=$?
agent_exit=0
if [ "$board" -ne 0 ]; then
  # The agent waits on the board's signals; nothing will come.
  kill "$agent" 2>/dev/null || true
fi
wait "$agent" || agent_exit=$?
echo "agent.py exit $agent_exit, board.mjs exit $board"
[ "$board" -eq 0 ] && [ "$agent_exit" -eq 0 ]
