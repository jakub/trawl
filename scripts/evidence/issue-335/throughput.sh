#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #335, acceptance criterion "a committed
# transcript records live-tail matching throughput over that corpus at the
# chosen capacity, against today's default" (ADR-0063). The probe in
# live-probe/ times the live lane's search-stage match and its regex
# extract through trawl-core's public stream API, so it builds no regex of
# its own: built against the pinned SHA, it times this branch's owner (the
# meta engine at each pattern's own lazy-DFA capacity, one cache a
# pattern); built against BASE, main before this branch, it times the
# regex crate at its 2 MiB default capacity with its pool of caches. The
# probe's module documentation describes each cell.
#
# Method (as scripts/evidence/issue-335/re2.sh):
#   1. Export the pinned SHA's tree and BASE's tree with `git archive`,
#      and copy the pinned SHA's probe into BASE's tree at the same path.
#      Each build reads its own tree's Cargo.lock, so each runs the
#      versions its workspace pins.
#   2. Build the probe in release against each tree, outside the measured
#      run, then wait for the build's load to decay (THROUGHPUT_QUIET_LOAD).
#   3. Print the corpus with the pinned build, the one tree that defines it
#      (trawl-core pattern/charge.rs, LOG_REGEXES and UNICODE_REGEXES).
#   4. Run ROUNDS rounds, each one pinned-build measure and then one BASE
#      measure, RUNS timed passes a cell each, so the two backings
#      alternate over the run; the report takes each cell's median over
#      every pass and checks both backings matched the same lines.
#   5. Write the transcript: a header naming both SHAs, the host, the
#      versions and the load average, the report, then the end time and
#      the load average. Check it with docs/evidence/issue-310/sanitize.py
#      --verify.
#
# Usage:
#   scripts/evidence/issue-335/throughput.sh [SHA [BASE]]
#     SHA defaults to HEAD, BASE to e4f2552c (main before this branch).
#     The run takes about two minutes after the builds.
#
# Environment:
#   THROUGHPUT_WORK_DIR    the exported trees and the measurements
#                          (default: <repo>/.flow-scratch/issue-335-throughput).
#                          Must not be on tmpfs.
#   CARGO_TARGET_DIR       the two release builds go under it, one a tree
#                          (default: <THROUGHPUT_WORK_DIR>/target).
#   THROUGHPUT_TRANSCRIPT  the transcript path
#                          (default: docs/evidence/issue-335/throughput.txt).
#   THROUGHPUT_ROUNDS      rounds (default: 5).
#   THROUGHPUT_RUNS        timed passes a cell a round (default: 3).
#   THROUGHPUT_QUIET_LOAD  the 1-minute load average the run waits for
#                          after the builds, at most ten minutes (default: 3).
#   CURL_CA_BUNDLE         passed through to the DuckDB runtime download
#                          (default: /etc/ssl/certs/ca-certificates.crt when
#                          it exists).
#
# Exit status: 1 when a build fails or the transcript fails verification;
# 3 when the two backings matched different lines; otherwise 0.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
PROBE=scripts/evidence/issue-335/live-probe

SHA="$(git -C "$ROOT_DIR" rev-parse --verify "${1:-HEAD}^{commit}")"
BASE="$(git -C "$ROOT_DIR" rev-parse --verify "${2:-e4f2552c}^{commit}")"
WORK_DIR="${THROUGHPUT_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-335-throughput}"
TARGET_ROOT="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
TRANSCRIPT="${THROUGHPUT_TRANSCRIPT:-$ROOT_DIR/docs/evidence/issue-335/throughput.txt}"
ROUNDS="${THROUGHPUT_ROUNDS:-5}"
RUNS="${THROUGHPUT_RUNS:-3}"
QUIET="${THROUGHPUT_QUIET_LOAD:-3}"
if [[ -z "${CURL_CA_BUNDLE:-}" && -f /etc/ssl/certs/ca-certificates.crt ]]; then
  export CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
fi

mkdir -p "$WORK_DIR" "$TARGET_ROOT" "$(dirname "$TRANSCRIPT")"
for dir in "$WORK_DIR" "$TARGET_ROOT"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "throughput: $dir is on tmpfs; release builds do not belong there" >&2
    exit 1
  fi
done

# Fixed checkout paths keep each target dir's fingerprints valid from one
# SHA to the next.
PINNED="$WORK_DIR/pinned"
BEFORE="$WORK_DIR/base"
RESULTS="$WORK_DIR/results-${SHA:0:12}"
rm -rf "$PINNED" "$BEFORE" "$RESULTS"
mkdir -p "$PINNED" "$BEFORE" "$RESULTS"
trap 'rm -rf "$PINNED" "$BEFORE" "$RESULTS"' EXIT
git -C "$ROOT_DIR" archive --format=tar "$SHA" | tar -x -C "$PINNED"
git -C "$ROOT_DIR" archive --format=tar "$BASE" | tar -x -C "$BEFORE"
if [[ ! -f "$PINNED/$PROBE/src/main.rs" ]]; then
  echo "throughput: $SHA has no $PROBE" >&2
  exit 2
fi
if [[ -e "$BEFORE/$PROBE" ]]; then
  echo "throughput: $BASE already holds $PROBE; BASE must be a tree before the probe" >&2
  exit 2
fi
mkdir -p "$BEFORE/$PROBE"
cp -r "$PINNED/$PROBE/Cargo.toml" "$PINNED/$PROBE/src" "$BEFORE/$PROBE/"
cp "$PINNED/Cargo.lock" "$PINNED/$PROBE/Cargo.lock"
cp "$BEFORE/Cargo.lock" "$BEFORE/$PROBE/Cargo.lock"

# Build the probe against one tree: TREE, TARGET, cargo's extra arguments.
build() {
  local tree="$1" target="$2"
  shift 2
  mkdir -p "$target"
  # The DuckDB runtime trawl-core's build script prepares is
  # checksum-verified and cached per target dir; seed it from the caller's.
  if [[ ! -d "$target/duckdb-runtime-cache" && -d "$ROOT_DIR/target/duckdb-runtime-cache" ]]; then
    cp -r "$ROOT_DIR/target/duckdb-runtime-cache" "$target/"
  fi
  (cd "$tree/$PROBE" && CARGO_TARGET_DIR="$target" cargo build --release "$@" \
    --message-format=json-render-diagnostics) |
    jq -r 'select(.target.name == "issue-335-live-probe") | .executable | select(. != null)' |
    tail -1
}
echo "throughput: building the probe at ${SHA:0:12} and at ${BASE:0:12} (outside the measured run)…" >&2
NEW_BIN="$(build "$PINNED" "$TARGET_ROOT/pinned" --features corpus)"
OLD_BIN="$(build "$BEFORE" "$TARGET_ROOT/base")"
for binary in "$NEW_BIN" "$OLD_BIN"; do
  if [[ -z "$binary" || ! -x "$binary" ]]; then
    echo "throughput: a probe build left no binary" >&2
    exit 1
  fi
done

# The builds' own load must decay before the measured run starts: wait
# while the 1-minute load average is over QUIET, a minute at a time and at
# most ten times, then run and record what it was.
waited=0
while ((waited < 600)) && awk -v q="$QUIET" '{ exit !($1 > q) }' /proc/loadavg; do
  echo "throughput: the 1-minute load average is $(cut -d' ' -f1 /proc/loadavg), over $QUIET; waiting 60 s" >&2
  sleep 60
  waited=$((waited + 60))
done
if awk -v q="$QUIET" '{ exit !($1 > q) }' /proc/loadavg; then
  quiet_note="still over $QUIET after waiting ${waited} s; ran anyway"
else
  quiet_note="at or under $QUIET after waiting ${waited} s for the builds' load to decay"
fi

# A package's version from a tree's Cargo.lock.
locked() {
  awk -v name="$2" '$1 == "name" && $3 == "\"" name "\"" { getline; gsub(/"/, "", $3); print $3; exit }' \
    "$1/Cargo.lock"
}
# The regex crates a tree's trawl-core depends on, as its manifest names them.
regex_deps() {
  grep -oE '^regex(-automata|-syntax)? ' "$1/crates/trawl-core/Cargo.toml" | tr -d ' ' | paste -sd' '
}

started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
load_start="$(cut -d' ' -f1-3 /proc/loadavg)"
"$NEW_BIN" corpus >"$RESULTS/corpus.jsonl"
for round in $(seq 1 "$ROUNDS"); do
  echo "throughput: round $round of $ROUNDS" >&2
  "$NEW_BIN" measure shipped "$RESULTS/corpus.jsonl" "$RUNS" >"$RESULTS/shipped-$round.tsv"
  "$OLD_BIN" measure main "$RESULTS/corpus.jsonl" "$RUNS" >"$RESULTS/main-$round.tsv"
done
set +e
"$NEW_BIN" report "$RESULTS"/shipped-*.tsv "$RESULTS"/main-*.tsv >"$RESULTS/report.txt"
status=$?
set -e
finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
load_end="$(cut -d' ' -f1-3 /proc/loadavg)"

{
  echo "# issue #335: live-tail matching throughput, this branch's owner against main's regex crate (ADR-0063)"
  echo "script:        scripts/evidence/issue-335/throughput.sh"
  echo "script sha256: $(sha256sum "$PINNED/scripts/evidence/issue-335/throughput.sh" | cut -d' ' -f1)"
  echo "probe:         $PROBE, at the pinned SHA, built against both trees"
  echo "probe sha256:  $(sha256sum "$PINNED/$PROBE/src/main.rs" | cut -d' ' -f1)"
  echo "pinned SHA:    $SHA (shipped)"
  echo "base SHA:      $BASE (main)"
  echo "started:       $started"
  echo "rustc:         $(cd "$PINNED" && rustc --version)"
  echo "shipped:       trawl-core on $(regex_deps "$PINNED"): regex-automata $(locked "$PINNED" regex-automata), regex-syntax $(locked "$PINNED" regex-syntax)"
  echo "main:          trawl-core on $(regex_deps "$BEFORE"): regex $(locked "$BEFORE" regex), regex-automata $(locked "$BEFORE" regex-automata), regex-syntax $(locked "$BEFORE" regex-syntax)"
  echo "build:         cargo build --release (thin LTO), against each tree's Cargo.lock, before the run"
  echo "cpu:           $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//'), $(nproc) threads"
  echo "kernel:        $(uname -sr)"
  echo "load average:  $load_start at the start (1, 5 and 15 minutes), $quiet_note"
  echo "passes:        $ROUNDS rounds of $RUNS timed passes a cell a backing, the backings alternating by round, one warm-up pass first"
  echo
  echo "What it measures: one live stream's per-line regex work, through the calls"
  echo "trawld's stream setup and its per-event door make (trawl-core's parse,"
  echo "CompiledFilter::compile, compile_stream_plan, matches_at and accept_event),"
  echo "on one thread. is_match is the search stage message=/P/; captures is"
  echo "* | extract \"(?P<m>P)\", which also converts each event to a row, as the"
  echo "control row (* alone) shows. Each line set is 50000 log lines in five shapes"
  echo "(syslog, an access log, logfmt, JSON and prose), over ASCII words or over"
  echo "words in a dozen scripts. The patterns are trawl-core's LOG_REGEXES (log)"
  echo "and UNICODE_REGEXES (unicode), as the pinned tree defines them."
  echo
  echo "shipped is the pinned tree: Pattern on regex-automata's meta engine, each"
  echo "pattern's lazy-DFA capacity set from its own minimum, one cache a pattern"
  echo "behind a lock, the bounded backtracker off. main is the base tree: Pattern"
  echo "on the regex crate, the lazy DFA at the crate's 2 MiB default capacity, its"
  echo "pool of caches, the bounded backtracker on."
  echo
  echo "Not measured here: the meta engine at its default capacity on this branch,"
  echo "and how often the lazy DFA clears its cache. Both need a regex built"
  echo "outside the owner, which the construction inventory"
  echo "(trawl-core pattern::tests::construction_inventory) lists by file, and"
  echo "this probe builds none."
  echo
  cat "$RESULTS/report.txt"
  echo
  echo "finished:      $finished"
  echo "load average:  $load_end at the end (1, 5 and 15 minutes)"
  echo "report exit:   $status"
} >"$TRANSCRIPT"

if ! python3 -I "$ROOT_DIR/docs/evidence/issue-310/sanitize.py" --verify "$TRANSCRIPT"; then
  echo "throughput: the transcript failed verification" >&2
  exit 1
fi
echo "throughput: wrote ${TRANSCRIPT#"$ROOT_DIR"/}" >&2
exit "$status"
