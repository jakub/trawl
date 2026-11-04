#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #335 (ADR-0063, plan ruling R12): part 1's
# Rust cost gate, rerun at this branch's backing. Part 2 moved Pattern from
# the regex crate to regex-automata's meta engine with one cache a
# pattern, set each pattern's lazy-DFA capacity from its own minimum, and
# turned the bounded backtracker off. A smaller cache gives the lazy DFA
# up sooner, so the 0.5 s ceiling on one Rust call over one stored value
# is measured again, and so is the time to admit the costliest queries,
# whose every build now also compiles the NFA it reads the charge from.
#
# Method:
#   1. Run scripts/evidence/issue-334/cost.sh, parts rust_gate and
#      admission, at the pinned SHA; the checkout's copy of cost.sh must
#      be the pinned SHA's. That script exports the SHA's tree
#      with `git archive`, builds crates/trawl-engine/tests/regex_cost.rs
#      in release outside the timed run, and writes the harness's stdout
#      under its own header. Its RE2 parts are left out: the batch lane's
#      RE2 is DuckDB's and part 2 did not change it.
#   2. Write this transcript: a header naming the SHA, the host, the load
#      average and the backing's constants as the pinned tree holds them,
#      then cost.sh's transcript whole, then a verdict section that
#      restates the gate's result and the admission times.
#   3. Check the transcript with docs/evidence/issue-310/sanitize.py
#      --verify.
#
# Usage:
#   scripts/evidence/issue-335/cost-recheck.sh [SHA]
#     SHA defaults to HEAD. The run takes about 6 minutes after the build.
#
# Environment:
#   RECHECK_WORK_DIR   where cost.sh exports the tree and keeps its own
#                      transcript (default: <repo>/.flow-scratch/issue-335-cost).
#                      Must not be on tmpfs.
#   CARGO_TARGET_DIR   the release build (default: <RECHECK_WORK_DIR>/target).
#   RECHECK_TRANSCRIPT the transcript path
#                      (default: docs/evidence/issue-335/cost-recheck.txt).
#
# Exit status: 1 when the transcript fails verification; otherwise
# cost.sh's, which is non-zero when an admitted shape's Rust call reaches
# the ceiling; otherwise 0.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"

REV="${1:-HEAD}"
SHA="$(git -C "$ROOT_DIR" rev-parse --verify "$REV^{commit}")"
WORK_DIR="${RECHECK_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-335-cost}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
TRANSCRIPT="${RECHECK_TRANSCRIPT:-$ROOT_DIR/docs/evidence/issue-335/cost-recheck.txt}"

mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR" "$(dirname "$TRANSCRIPT")"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "cost-recheck: $dir is on tmpfs; a release build does not fit there" >&2
    exit 1
  fi
done

for path in scripts/evidence/issue-334/cost.sh crates/trawl-engine/tests/regex_cost.rs \
  crates/trawl-core/src/pattern/charge.rs crates/trawl-core/src/pattern/memory.rs; do
  if ! git -C "$ROOT_DIR" cat-file -e "$SHA:$path" 2>/dev/null; then
    echo "cost-recheck: $SHA has no $path" >&2
    exit 2
  fi
done

# A file of the pinned tree, and one constant's definition in it.
pinned() {
  git -C "$ROOT_DIR" show "$SHA:$1"
}
constant() {
  pinned "$1" | grep -m1 -E "^pub const $2: " | sed -E 's/^pub const //; s/;$//'
}
locked() {
  pinned Cargo.lock |
    awk -v name="$1" '$1 == "name" && $3 == "\"" name "\"" { getline; gsub(/"/, "", $3); print $3; exit }'
}

# cost.sh runs from this checkout, so it must be the pinned SHA's copy:
# the method is the one the SHA holds.
COST_SH="$ROOT_DIR/scripts/evidence/issue-334/cost.sh"
if ! cmp -s "$COST_SH" <(pinned scripts/evidence/issue-334/cost.sh); then
  echo "cost-recheck: scripts/evidence/issue-334/cost.sh differs from $SHA's copy" >&2
  exit 2
fi
INNER="$WORK_DIR/cost-${SHA:0:12}.txt"

started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
load_start="$(cut -d' ' -f1-3 /proc/loadavg)"
set +e
COST_WORK_DIR="$WORK_DIR" COST_TRANSCRIPT="$INNER" "$COST_SH" "$SHA" rust_gate admission
status=$?
set -e
finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
load_end="$(cut -d' ' -f1-3 /proc/loadavg)"

if [[ ! -s "$INNER" ]]; then
  echo "cost-recheck: cost.sh wrote no transcript" >&2
  exit 1
fi

gate="$(grep -E '^RUST GATE (PASS|FAIL)' "$INNER" | tail -1 || true)"
worst="$(grep -E '^worst Rust shape: ' "$INNER" | tail -1 || true)"
worst_body="$(grep -A1 -E '^worst Rust shape: ' "$INNER" | tail -1 || true)"
units="$(grep -E '^most translation units' "$INNER" | tail -1 || true)"
compile="$(grep -E '^most compile work' "$INNER" | tail -1 || true)"
per_build="$(sed -nE 's/.*256 distinct .* median ([0-9.]+) ms, max ([0-9.]+) ms.*/\1 \2/p' <<<"$compile")"

{
  echo "# issue #335: part 1's Rust cost gate at part 2's backing (ADR-0063, R12)"
  echo "script:        scripts/evidence/issue-335/cost-recheck.sh"
  echo "script sha256: $(sha256sum "$SCRIPT_DIR/cost-recheck.sh" | cut -d' ' -f1)"
  echo "pinned SHA:    $SHA"
  echo "method:        scripts/evidence/issue-334/cost.sh at the pinned SHA, parts rust_gate and admission"
  echo "started:       $started"
  echo "finished:      $finished"
  echo "cpu:           $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//'), $(nproc) threads"
  echo "load average:  $load_start at the start, $load_end at the end (1, 5 and 15 minutes)"
  echo "crates:        regex-automata $(locked regex-automata), regex-syntax $(locked regex-syntax)"
  echo
  echo "The backing at the pinned SHA (crates/trawl-core/src/pattern):"
  echo "  charge.rs: $(constant crates/trawl-core/src/pattern/charge.rs REGEX_QUERY_BUDGET)"
  echo "  charge.rs: $(constant crates/trawl-core/src/pattern/charge.rs K_R)"
  echo "  charge.rs: $(constant crates/trawl-core/src/pattern/charge.rs CACHE_FLOOR)"
  echo "  charge.rs: $(constant crates/trawl-core/src/pattern/charge.rs CACHE_MAX)"
  echo "  charge.rs: $(constant crates/trawl-core/src/pattern/charge.rs CACHE_SLACK)"
  echo "  charge.rs: $(constant crates/trawl-core/src/pattern/charge.rs BUILD_TRANSIENT)"
  echo "  memory.rs: $(constant crates/trawl-core/src/pattern/memory.rs REGEX_RESERVATION_BYTES)"
  echo "  mod.rs:    meta_config sets $(pinned crates/trawl-core/src/pattern/mod.rs | grep -oE '\.(dfa|backtrack)\(false\)' | sort -u | paste -sd' '): the full DFA and the bounded backtracker stay off"
  echo "  capacity:  each pattern's lazy DFA builds at K_R times its forward minimum, within CACHE_FLOOR and CACHE_MAX"
  echo
  echo "Every Rust call the harness times is the owner's: a Pattern built by a"
  echo "metered Preparer, searched through Pattern::is_match and"
  echo "Pattern::captures with the pattern's one cache. The admission part times"
  echo "trawl_core::parser::parse, whose check ledger builds each pattern"
  echo "through the same build as a metered ledger: the profile NFA, then the"
  echo "meta program at the capacity the profile sets, then the batch view."
  echo
  echo "======================================================================"
  echo "COST.SH TRANSCRIPT (scripts/evidence/issue-334/cost.sh at the pinned SHA)"
  echo "======================================================================"
  cat "$INNER"
  echo
  echo "======================================================================"
  echo "VERDICT"
  echo "======================================================================"
  if [[ -n "$gate" ]]; then
    echo "$gate"
  else
    echo "RUST GATE: no result; cost.sh exited $status before the gate printed"
  fi
  if [[ -n "$worst" ]]; then
    echo "$worst"
    echo "$worst_body"
  fi
  echo "R12: the 0.5 s ceiling $(case "$gate" in "RUST GATE PASS"*) echo "holds" ;; "RUST GATE FAIL"*) echo "FAILS" ;; *) echo "was not measured" ;; esac) at part 2's backing"
  if [[ -n "$units" ]]; then
    echo "admission, $units"
  fi
  if [[ -n "$compile" ]]; then
    echo "admission, $compile"
  fi
  if [[ -n "$per_build" ]]; then
    read -r median max <<<"$per_build"
    awk -v m="$median" -v x="$max" 'BEGIN { printf "admission per build of the costliest body: median %.2f ms, max %.2f ms (the query time over its 256 builds, parse and price included)\n", m / 256, x / 256 }'
  fi
  echo "cost.sh exit: $status"
} >"$TRANSCRIPT"

if ! python3 -I "$ROOT_DIR/docs/evidence/issue-310/sanitize.py" --verify "$TRANSCRIPT"; then
  echo "cost-recheck: the transcript failed verification" >&2
  exit 1
fi
echo "cost-recheck: wrote ${TRANSCRIPT#"$ROOT_DIR"/}" >&2
exit "$status"
