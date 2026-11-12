#!/usr/bin/env bash
# Release-binary evidence for #447, the tree depth limit.
#
# Builds `trawl` in release from a clean detached checkout of SHA (default:
# HEAD of the repo this script lives in), then runs `trawl query --data` on
# a one-row parquet under `ulimit -s 2048` for:
#   - every shape the first premise checkpoint names, at depth 64 (its one
#     row, with the value the shape computes) and at depth 65 (the depth
#     refusal);
#   - the longest texts MAX_QUERY_LEN admits, which end in the depth
#     refusal or a grammar error, and never in a signal;
#   - an alias chain DuckDB may refuse or run, which only must not end in
#     a signal.
# One line per case. The script exits non-zero if any case dies of a signal
# or misses its expected outcome. arm64 is not measured.
#
# Needs: git, cargo, and the duckdb CLI (it writes the one-row parquet).
# CARGO_TARGET_DIR overrides the build directory, which defaults to
# <repo>/target/evidence-447 because /tmp is a RAM tmpfs. The checkout
# lives in EVIDENCE_TMPDIR (default <repo>/target) for the same reason.
#
# Usage: docs/evidence/issue-447/release-stack.sh [SHA] > release-stack.txt
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")" && git rev-parse --show-toplevel)
SHA=$(git -C "$REPO" rev-parse --verify "${1:-HEAD}^{commit}")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$REPO/target/evidence-447}
WORK_ROOT=${EVIDENCE_TMPDIR:-$REPO/target}
STACK_KB=2048
mkdir -p "$WORK_ROOT"
WORK=$(mktemp -d "$WORK_ROOT/evidence-447.XXXXXX")
CHECKOUT=$WORK/checkout

cleanup() {
  git -C "$REPO" worktree remove --force "$CHECKOUT" >/dev/null 2>&1 || true
  git -C "$REPO" worktree prune >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

command -v duckdb >/dev/null || {
  echo "release-stack.sh: the duckdb CLI is needed to write the one-row parquet" >&2
  exit 2
}

# A fresh target dir has no DuckDB runtime archive, and the build downloads
# one. Reuse the archive a normal build left in <repo>/target when there is
# one; the build's own checksum check still verifies it.
if [[ -d $REPO/target/duckdb-runtime-cache && $CARGO_TARGET_DIR != "$REPO/target" ]]; then
  mkdir -p "$CARGO_TARGET_DIR/duckdb-runtime-cache"
  cp -rn "$REPO/target/duckdb-runtime-cache/." "$CARGO_TARGET_DIR/duckdb-runtime-cache/"
fi

git -C "$REPO" worktree add --detach "$CHECKOUT" "$SHA" >&2
# The build is the slow step and its output is noise in the capture.
(cd "$CHECKOUT" && cargo build --release -p trawl-cli >&2)
TRAWL=$CARGO_TARGET_DIR/release/trawl

# The limit, read from the source under test rather than restated here.
MAX_QUERY_LEN=$(sed -n 's/^pub const MAX_QUERY_LEN: usize = \([0-9_]*\);.*/\1/p' \
  "$CHECKOUT/crates/trawl-core/src/parser/mod.rs" | tr -d _)
MAX_TREE_DEPTH=$(sed -n 's/^pub const MAX_TREE_DEPTH: usize = \([0-9_]*\);.*/\1/p' \
  "$CHECKOUT/crates/trawl-core/src/parser/mod.rs" | tr -d _)
MAX_SEARCH_PRESCAN=$(sed -n 's/^pub const MAX_SEARCH_PRESCAN: usize = \([0-9_]*\);.*/\1/p' \
  "$CHECKOUT/crates/trawl-core/src/parser/mod.rs" | tr -d _)
: "${MAX_QUERY_LEN:?MAX_QUERY_LEN not found}" "${MAX_TREE_DEPTH:?MAX_TREE_DEPTH not found}"
: "${MAX_SEARCH_PRESCAN:?MAX_SEARCH_PRESCAN not found}"

# One row: the base columns, a message `extract kv` splits into k=1, and a
# BIGINT column `a` of 1. `k` is in no column, so only the kv tail reads it.
# Every runnable case below is built to return the row.
PARQUET=$WORK/one.parquet
duckdb :memory: "COPY (SELECT TIMESTAMP '2026-03-14 03:00:00' AS _time, \
  TIMESTAMP '2026-03-14 03:00:00' AS _ingested, \
  'web-1' AS host, 'nginx' AS service, 'k=1' AS message, 'k=1' AS _raw, \
  CAST(1 AS BIGINT) AS a) TO '$PARQUET' (FORMAT PARQUET)" >&2

# ---- query text builders ---------------------------------------------------

# rep STR N: STR, N times.
rep() {
  local n=$2
  ((n > 0)) || return 0
  yes -- "$1" | head -n "$n" | tr -d '\n'
}

# chain N: N ones joined by " + ", a chain N-1 levels deep.
chain() { printf '1'; rep ' + 1' $(($1 - 1)); }
# compact N: the same joined by "+" with no spaces.
compact() { printf '1'; rep '+1' $(($1 - 1)); }
# ors N / ands N: N comparisons, N levels deep. Only a == 1 holds, so the
# or chain holds for the row, and the and chain is all a == 1.
ors() {
  local i
  for ((i = 0; i < $1; i++)); do
    ((i == 0)) || printf ' or '
    printf 'a == %d' "$i"
  done
}
ands() { printf 'a == 1'; rep ' and a == 1' $(($1 - 1)); }
# kv_ors N: the or chain over the kv field k.
kv_ors() {
  local i
  for ((i = 0; i < $1; i++)); do
    ((i == 0)) || printf ' or '
    printf 'k == %d' "$i"
  done
}
# calls FN N BODY: BODY inside N nested calls to FN.
calls() { rep "$1(" "$2"; printf '%s' "$3"; rep ')' "$2"; }
# parity-correct operand of a not run of N: true after N nots.
not_operand() { if (($1 % 2 == 1)); then printf 'a == 2'; else printf 'a == 1'; fi; }

# shape_text NAME D: the query of one premise-checkpoint shape D levels deep.
SHAPES=(
  "plus chain" "or chain" "and chain" "minus run" "not run"
  "chain as in subject" "chain as in element" "chain as call argument"
  "chain under pass-through calls" "pass-through calls in where"
  "let" "stats" "timechart" "eventstats" "pivot"
  "where after extract kv" "search NOT run" "search brackets"
  "search NOT and brackets"
)
shape_text() {
  local d=$2
  case $1 in
  "plus chain") printf '* | where %s > 0' "$(chain "$d")" ;;
  "or chain") printf '* | where %s' "$(ors "$d")" ;;
  "and chain") printf '* | where %s' "$(ands "$d")" ;;
  "minus run") printf '* | where %sa < 2' "$(rep - $((d - 1)))" ;;
  "not run") printf '* | where %s%s' "$(rep 'not ' $((d - 1)))" "$(not_operand $((d - 1)))" ;;
  "chain as in subject") printf '* | where %s in (%d)' "$(chain "$d")" "$d" ;;
  "chain as in element") printf '* | where a in (1, %s)' "$(chain "$d")" ;;
  "chain as call argument") printf '* | let x = %s' "$(calls abs 16 "$(chain $((d - 15)))")" ;;
  "chain under pass-through calls") printf '* | let x = %s' "$(calls coalesce 16 "$(chain $((d - 15)))")" ;;
  "pass-through calls in where") printf '* | where %s > 0' "$(calls coalesce 16 "$(chain $((d - 16)))")" ;;
  "let") printf '* | let x = %s' "$(chain $((d + 1)))" ;;
  "stats") printf '* | stats sum(%s)' "$(chain $((d + 1)))" ;;
  "timechart") printf '* | timechart span=1m sum(%s)' "$(chain $((d + 1)))" ;;
  "eventstats") printf '* | eventstats sum(%s) as x' "$(chain $((d + 1)))" ;;
  "pivot") printf '* | pivot sum(%s) on a' "$(chain $((d + 1)))" ;;
  "where after extract kv") printf '* | extract kv | where %s' "$(kv_ors "$d")" ;;
  "search NOT run") printf '%sa=1' "$(rep 'NOT ' "$d")" ;;
  "search brackets") printf '%sa=1%s' "$(rep '(' "$d")" "$(rep ' )' "$d")" ;;
  "search NOT and brackets")
    printf '%s%sa=1%s' "$( ((d % 2 == 1)) && printf 'NOT ')" "$(rep 'NOT (' $((d / 2)))" "$(rep ' )' $((d / 2)))"
    ;;
  esac
}

# shape_row NAME D: the key and value the one row of shape NAME at depth D
# must hold, as the CLI's compact JSON spells them. run_case requires a `,`
# or `}` right after it, so "a":1 does not match "a":10.
shape_row() {
  local d=$2
  case $1 in
  "chain as call argument" | "chain under pass-through calls") printf '"x":%d' $((d - 15)) ;;
  "let" | "eventstats") printf '"x":%d' $((d + 1)) ;;
  "stats" | "timechart") printf '"sum":%d' $((d + 1)) ;;
  "pivot") printf '"1":%d' $((d + 1)) ;;
  "where after extract kv") printf '"k":1' ;;
  *) printf '"a":1' ;;
  esac
}

# longest PREFIX UNIT CORE SUFFIX: PREFIX, as many UNITs as fit
# MAX_QUERY_LEN with CORE and SUFFIX, CORE, SUFFIX.
longest() {
  local prefix=$1 unit=$2 core=$3 suffix=$4 n
  n=$(((MAX_QUERY_LEN - ${#prefix} - ${#core} - ${#suffix}) / ${#unit}))
  printf '%s' "$prefix"
  rep "$unit" "$n"
  printf '%s%s' "$core" "$suffix"
}
# longest_brackets CORE: "(" x n, CORE, ")" x n, as long as MAX_QUERY_LEN admits.
longest_brackets() {
  local n=$(((MAX_QUERY_LEN - ${#1}) / 2))
  rep '(' "$n"
  printf '%s' "$1"
  rep ')' "$n"
}

# ---- running a case --------------------------------------------------------

ABORTS=0
UNEXPECTED=0
OUT=$WORK/out
ERR=$WORK/err

# error_line FILE: what the CLI refused with, in one line cut to 140 bytes.
# A parse error prints "trawl: parse error", the query with a ~~~ marker
# under the span, then the message; the line is "<first line>: <message>".
# Any other error is its first line. The query echo is noise at 64 KiB.
error_line() {
  local line
  line=$(awk '
    NF && !first { first = $0; next }
    first && marked && NF { sub(/^ +/, ""); msg = $0; exit }
    first && /^ *[~^]+ *$/ { marked = 1 }
    END { if (msg != "") print first ": " msg; else print first }
  ' "$1")
  if ((${#line} > 140)); then line="${line:0:140}…"; fi
  printf '%s' "$line"
}

# run_case NAME DEPTH EXPECT TEXT [ROW]
# EXPECT is "row" (exactly one row, holding ROW), "depth" (the depth
# refusal), "grammar" (a parse error other than the depth refusal) or "any"
# (any outcome but a signal).
run_case() {
  local name=$1 depth=$2 expect=$3 text=$4 want=${5:-} status=0 outcome mark= rows line
  # the limit applies to the trawl process only, not to the builder above
  (ulimit -s "$STACK_KB" && "$TRAWL" query --data "$PARQUET" --format json -- "$text") \
    >"$OUT" 2>"$ERR" || status=$?
  if ((status > 128)); then
    outcome="ABORT signal $((status - 128))"
    mark=" [ABORT]"
    ABORTS=$((ABORTS + 1))
  elif ((status == 0)); then
    rows=$(grep -c . "$OUT" || true)
    line=$(head -n 1 "$OUT")
    if ((rows == 1)) && [[ -n $want && ($line == *"$want,"* || $line == *"$want}"*) ]]; then
      outcome=row
    else
      outcome="$rows row(s): ${line:0:100}"
    fi
  else
    outcome="refused: $(error_line "$ERR")"
  fi
  case ${mark:+marked}$expect in
  row)
    if [[ $outcome != "row" ]]; then
      mark=" [UNEXPECTED: not the one row holding $want]"
      UNEXPECTED=$((UNEXPECTED + 1))
    fi ;;
  depth)
    if [[ $outcome != *"deeper than $MAX_TREE_DEPTH levels"* ]]; then
      mark=" [UNEXPECTED: not the depth refusal]"
      UNEXPECTED=$((UNEXPECTED + 1))
    fi ;;
  grammar)
    if [[ $outcome != "refused: trawl: parse error: "* || $outcome == *"deeper than"* ]]; then
      mark=" [UNEXPECTED: not a grammar error]"
      UNEXPECTED=$((UNEXPECTED + 1))
    fi ;;
  esac
  printf '%-32s depth=%-6s bytes=%-6d exit=%-3d %s%s\n' \
    "$name" "$depth" "${#text}" "$status" "$outcome" "$mark"
}

# ---- header ----------------------------------------------------------------

echo "release-stack.sh for #447, tree depth limit"
echo "sha:      $SHA"
echo "rustc:    $(rustc -V)"
echo "arch:     $(uname -m)"
echo "ulimit -s: $( (ulimit -s "$STACK_KB" && ulimit -s)) KiB"
echo "profile:  release ($(basename "$CARGO_TARGET_DIR")/release/trawl)"
echo "limits:   MAX_TREE_DEPTH=$MAX_TREE_DEPTH MAX_QUERY_LEN=$MAX_QUERY_LEN"
echo "date:     $(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ---- cases -----------------------------------------------------------------

echo
echo "== every shape at depth $MAX_TREE_DEPTH: its one row =="
for shape in "${SHAPES[@]}"; do
  run_case "$shape" "$MAX_TREE_DEPTH" row "$(shape_text "$shape" "$MAX_TREE_DEPTH")" \
    "$(shape_row "$shape" "$MAX_TREE_DEPTH")"
done

echo
echo "== every shape at depth $((MAX_TREE_DEPTH + 1)): the depth refusal =="
for shape in "${SHAPES[@]}"; do
  run_case "$shape" "$((MAX_TREE_DEPTH + 1))" depth "$(shape_text "$shape" "$((MAX_TREE_DEPTH + 1))")"
done

echo
echo "== the longest texts (MAX_QUERY_LEN $MAX_QUERY_LEN bytes) =="
# `1+` is one term and one level of a chain
run_case "where chain, 32000 terms" 32000 depth "* | where $(compact 32000) > 0"
run_case "let chain, 32000 terms" 32000 depth "* | let x = $(compact 32000)"
run_case "stats chain, 32000 terms" 32000 depth "* | stats sum($(compact 32000))"
run_case "minus run, longest" max depth "$(longest '* | where ' '-' 1 '')"
run_case "not run, longest" max depth "$(longest '* | where ' 'not ' a '')"
run_case "NOT run, longest" max depth "$(longest '' 'NOT ' a '')"
run_case "brackets, longest" max depth "$(longest_brackets a)"
run_case "chain as in subject, longest" max depth "$(longest '* | where ' '1+' 1 ' in (1)')"
run_case "chain as in element, longest" max depth "$(longest '* | where a in (1, ' '1+' 1 ')')"
run_case "chain as call argument, longest" max depth "$(longest '* | let x = abs(' '1+' 1 ')')"
run_case "deep expression, grammar error" 32000 grammar "* | where $(compact 32000) > 0 | where"
# the deepest search the pre-scan lets reach the grammar
run_case "deep search, grammar error" "$MAX_SEARCH_PRESCAN" grammar \
  "$(rep '(' "$MAX_SEARCH_PRESCAN")a=1$(rep ')' "$MAX_SEARCH_PRESCAN") | where"
# a0 = 1, a1 = a0, ...: each value one name, so no level deep. DuckDB's own
# binder limit (1000) may answer, or a later DuckDB may run it. Either is
# the outcome; only a signal fails.
aliases=$(
  printf '* | let a0 = 1'
  for ((i = 1; i < 4000; i++)); do printf ', a%d = a%d' "$i" "$((i - 1))"; done
)
run_case "let alias chain, 4000 links" 4000 any "$aliases"

echo
echo "summary: aborts=$ABORTS unexpected=$UNEXPECTED"
echo "arm64 was not measured; this capture is $(uname -m) only."

if ((ABORTS > 0 || UNEXPECTED > 0)); then
  echo "FAIL: $ABORTS case(s) ended in a signal, $UNEXPECTED missed their outcome" >&2
  exit 1
fi
