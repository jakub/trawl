#!/usr/bin/env bash
# Which arguments do the #308, #309 and #310 recording scripts hand `claude`?
#
# Runs each transcripts.sh from this checkout with a stub `claude` first on
# PATH. The stub records its arguments and prints a three-line stream in which
# an investigation is created, so #310's script goes on to its second session.
# No Claude Code session, Trawl stack or network is used.
#
# Stdout holds, per `claude` invocation, the arguments in order as bash quotes
# them (`''` is an empty argument), with the prompt shortened to its byte
# count and the checkout path to `<checkout>`. Exit 0 means the five
# invocations each pass `--setting-sources ''` and keep `--strict-mcp-config`,
# `--mcp-config`, `--allowedTools`, `--no-session-persistence` and
# `--permission-mode dontAsk`.
# Usage: args-capture.sh > args-capture.txt
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
D=$(mktemp -d)
# #308's script leaves its MCP configs beside itself.
trap 'rm -rf "$D"; rm -f "$WT"/docs/evidence/issue-308/mcp-current.json "$WT"/docs/evidence/issue-308/mcp-legacy.json' EXIT
mkdir -p "$D/bin"
cat > "$D/bin/claude" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
{
  echo "invocation${MCP_PROTOCOL_NEGOTIATION:+ (MCP_PROTOCOL_NEGOTIATION=$MCP_PROTOCOL_NEGOTIATION)}"
  prev=
  for arg in "$@"; do
    if [ "$prev" = -p ]; then echo "  <prompt, ${#arg} bytes>"; else echo "  ${arg@Q}"; fi
    prev=$arg
  done
} >> "$ARGS_CAPTURE"
cat <<'STREAM'
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"stub-1","name":"mcp__trawl__investigation","input":{"action":"create"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"stub-1","content":"{\"investigation\":{\"id\":\"inv-stub\"}}"}]}}
{"type":"result","subtype":"success","is_error":false,"num_turns":1,"duration_ms":0}
STREAM
STUB
chmod +x "$D/bin/claude"

fail=0
for n in 308 309 310; do
  capture=$D/args-$n.txt
  : > "$capture"
  mkdir -p "$D/run-$n"
  # #309's prompt reads the seeded event's time from the run directory.
  echo '{"time": "2026-01-01T00:00:00Z"}' > "$D/run-$n/focus.json"
  status=0
  PATH=$D/bin:$PATH LC_ALL=C ARGS_CAPTURE=$capture TRAWL_EVIDENCE_DIR=$D/run-$n \
    "$WT/docs/evidence/issue-$n/transcripts.sh" "$D/out-$n" > "$D/script-$n.log" 2>&1 || status=$?
  echo "docs/evidence/issue-$n/transcripts.sh: exit status $status, $(grep -c '^invocation' "$capture") claude invocation(s)"
  sed "s|$WT|<checkout>|g" "$capture"
  [ "$status" = 0 ] || fail=1
done

cat "$D"/args-*.txt > "$D/all.txt"
count() { grep -c -x -- "$1" "$D/all.txt" || true; }
invocations=$(count 'invocation.*')
empty_sources=$(awk "prev == \"  '--setting-sources'\" && \$0 == \"  ''\" { n++ } { prev = \$0 } END { print n + 0 }" "$D/all.txt")
dont_ask=$(awk "prev == \"  '--permission-mode'\" && \$0 == \"  'dontAsk'\" { n++ } { prev = \$0 } END { print n + 0 }" "$D/all.txt")
echo "summary: $invocations invocations, $empty_sources with --setting-sources '', $dont_ask with --permission-mode dontAsk"
[ "$invocations" = 5 ] && [ "$empty_sources" = 5 ] && [ "$dont_ask" = 5 ] || fail=1
for flag in --strict-mcp-config --mcp-config --allowedTools --no-session-persistence; do
  n=$(count "  '$flag'")
  echo "summary: $n with $flag"
  [ "$n" = 5 ] || fail=1
done
if [ "$fail" = 0 ]; then
  echo "verdict: pass"
else
  echo "verdict: FAIL"
  exit 1
fi
