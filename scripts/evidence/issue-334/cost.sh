#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #334, acceptance criterion "The cost of one
# call is measured" (ADR-0063). It times one regex call of every shape a
# search composes at the edge of ADR-0063's limits, over a 65 536-character
# adversarial subject, and prints two results in sections of their own:
#
#   - the Rust gate: every Rust call (the live lane) stays under the hard
#     0.5 s ceiling. The harness fails when one does not.
#   - the RE2 envelope: the slowest call in DuckDB's RE2 (the batch lane),
#     reported with no ceiling.
#
# Two more parts follow: RE2's step against the batch-engine view, and the
# admission time of the costliest queries. The harness that measures is
# crates/trawl-engine/tests/regex_cost.rs; its module documentation
# explains the search and what the ceiling covers.
#
# Method:
#   1. Export the pinned SHA's tree with `git archive` into a fresh
#      directory under WORK_DIR, so the build sees exactly what the commit
#      holds and nothing from the caller's working tree.
#   2. If the pinned tree has no harness (a SHA from before the harness
#      landed), copy in the caller's harness. The transcript says which,
#      with the harness's sha256 either way.
#   3. Build the harness once in release, outside the timed run.
#   4. Run each part, one test of the harness each, and write its stdout
#      to the transcript under a banner that names the part. A run that
#      starts the transcript writes a header first, naming the SHA, the
#      host, the toolchain and the load average.
#   5. Check the transcript with docs/evidence/issue-310/sanitize.py
#      --verify, which fails on recording-machine paths and credentials.
#
# Usage:
#   scripts/evidence/issue-334/cost.sh [SHA [PART ...]]
#     SHA defaults to HEAD. PART is rust_gate, re2_envelope, re2_step or
#     admission; with none, all four run, in that order, in about 15
#     minutes. A PART list that holds rust_gate starts a new transcript.
#     Any other list appends to the transcript of the same SHA, so each
#     part can run as a command of its own where a command has a time
#     limit: run rust_gate first, then the others in order.
#
# Environment:
#   COST_WORK_DIR     where the exported tree goes
#                     (default: <repo>/.flow-scratch/issue-334-cost). Must
#                     not be on tmpfs.
#   CARGO_TARGET_DIR  the release build (default: <COST_WORK_DIR>/target),
#                     kept between runs so a rerun builds only what
#                     changed. Must not be on tmpfs.
#   COST_TRANSCRIPT   the transcript path
#                     (default: docs/evidence/issue-334/transcript.txt).
#
# Exit status: 1 when the transcript fails verification; otherwise the
# first non-zero exit of a part, which for rust_gate means an admitted
# shape reached the ceiling; otherwise 0. The transcript is written either
# way.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
HARNESS=crates/trawl-engine/tests/regex_cost.rs
ALL_PARTS=(rust_gate re2_envelope re2_step admission)

REV="${1:-HEAD}"
SHA="$(git -C "$ROOT_DIR" rev-parse --verify "$REV^{commit}")"
shift $(($# > 0 ? 1 : 0))
WORK_DIR="${COST_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-334-cost}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
TRANSCRIPT="${COST_TRANSCRIPT:-$ROOT_DIR/docs/evidence/issue-334/transcript.txt}"

# The parts to run, in the harness's order whatever order they were given.
declare -A asked=()
for part in "$@"; do
  case "$part" in
    rust_gate | re2_envelope | re2_step | admission) asked[$part]=1 ;;
    *)
      echo "cost: unknown part $part (want ${ALL_PARTS[*]})" >&2
      exit 2
      ;;
  esac
done
parts=()
for part in "${ALL_PARTS[@]}"; do
  if [[ $# -eq 0 || -n "${asked[$part]:-}" ]]; then
    parts+=("$part")
  fi
done

starts_transcript=false
if [[ "${parts[0]}" == rust_gate ]]; then
  starts_transcript=true
elif ! grep -qx "pinned SHA:    $SHA" "$TRANSCRIPT" 2>/dev/null; then
  echo "cost: $TRANSCRIPT is not a transcript of $SHA; run rust_gate first" >&2
  exit 2
fi

mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR" "$(dirname "$TRANSCRIPT")"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "cost: $dir is on tmpfs; a release build does not fit there" >&2
    exit 1
  fi
done

CHECKOUT="$WORK_DIR/checkout-${SHA:0:12}"
rm -rf "$CHECKOUT"
mkdir -p "$CHECKOUT"
trap 'rm -rf "$CHECKOUT"' EXIT
git -C "$ROOT_DIR" archive --format=tar "$SHA" | tar -x -C "$CHECKOUT"

if git -C "$ROOT_DIR" cat-file -e "$SHA:$HARNESS" 2>/dev/null; then
  harness_source="at the pinned SHA"
else
  cp "$ROOT_DIR/$HARNESS" "$CHECKOUT/$HARNESS"
  harness_source="copied from the script's checkout; the pinned SHA has none"
fi

# The DuckDB runtime is checksum-verified and cached per target dir. Seed
# a new target dir from the caller's, so a first run need not download it.
if [[ ! -d "$CARGO_TARGET_DIR/duckdb-runtime-cache" && -d "$ROOT_DIR/target/duckdb-runtime-cache" ]]; then
  cp -r "$ROOT_DIR/target/duckdb-runtime-cache" "$CARGO_TARGET_DIR/"
fi

echo "cost: building the harness at ${SHA:0:12} (outside the timed run)…" >&2
binary="$(cd "$CHECKOUT" && cargo test -p trawl-engine --test regex_cost --release --no-run \
  --message-format=json-render-diagnostics |
  jq -r 'select(.target.name == "regex_cost") | .executable | select(. != null)' |
  tail -1)"
if [[ -z "$binary" || ! -x "$binary" ]]; then
  echo "cost: could not locate the regex_cost test binary" >&2
  exit 1
fi

if $starts_transcript; then
  {
    echo "# issue #334: the cost of one regex call (ADR-0063)"
    echo "script:        scripts/evidence/issue-334/cost.sh"
    echo "script sha256: $(sha256sum "$SCRIPT_DIR/cost.sh" | cut -d' ' -f1)"
    echo "pinned SHA:    $SHA"
    echo "harness:       $HARNESS, $harness_source"
    echo "harness sha256: $(sha256sum "$CHECKOUT/$HARNESS" | cut -d' ' -f1)"
    echo "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "rustc:         $(cd "$CHECKOUT" && rustc --version)"
    echo "cpu:           $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//'), $(nproc) threads"
    echo "load average:  $(cut -d' ' -f1-3 /proc/loadavg) (1, 5 and 15 minutes)"
    echo
    echo "Two results, in sections of their own:"
    echo "  RUST GATE     the live lane's Rust engine. Every call of every"
    echo "                admitted shape stays under a hard 0.5 s ceiling;"
    echo "                the harness fails when one does not."
    echo "  RE2 ENVELOPE  the batch lane's DuckDB RE2. The slowest call of"
    echo "                any admitted shape is reported, with no ceiling."
    echo
    echo "Scope: both are per call over one stored value of at most"
    echo "MAX_RAW_CHARS (65 536) characters. Values stored before the ingest"
    echo "cap keep their length, and a computed subject such as"
    echo "concat(message, message) can be longer than one stored value; both"
    echo "fall outside the ceiling and cost in proportion to their length."
  } >"$TRANSCRIPT"
fi

banner() {
  local rule="======================================================================"
  echo
  echo "$rule"
  case "$1" in
    rust_gate)
      echo "RUST GATE: the live lane's Rust engine against a hard 0.5 s ceiling"
      echo "The harness fails this part when any admitted shape's call takes"
      echo "0.5 s or more."
      ;;
    re2_envelope)
      echo "RE2 ENVELOPE: the batch lane's DuckDB RE2, measured, no ceiling"
      echo "ADR-0063 records the slowest call as an envelope; nothing here is"
      echo "asserted about time."
      ;;
    re2_step)
      echo "RE2 STEP: RE2's DFA init budget against the batch-engine view"
      ;;
    admission)
      echo "ADMISSION: parse, price and build of the costliest queries"
      ;;
  esac
  echo "$rule"
}

status=0
for part in "${parts[@]}"; do
  echo "cost: running $part…" >&2
  banner "$part" >>"$TRANSCRIPT"
  set +e
  "$binary" --exact "$part" --ignored --nocapture --test-threads=1 >>"$TRANSCRIPT"
  part_status=$?
  set -e
  echo "part $part exit: $part_status" >>"$TRANSCRIPT"
  if [[ $status -eq 0 ]]; then
    status=$part_status
  fi
done

# The run that finishes the transcript closes it with both results, read
# before the summary is appended to the same file.
if [[ "${parts[-1]}" == admission ]]; then
  gate="$(grep -E '^RUST GATE (PASS|FAIL)' "$TRANSCRIPT" || echo "RUST GATE: no result in this transcript")"
  envelope="$(grep -E '^RE2 ENVELOPE ' "$TRANSCRIPT" || echo "RE2 ENVELOPE: no result in this transcript")"
  {
    echo
    echo "======================================================================"
    echo "SUMMARY"
    echo "======================================================================"
    echo "$gate"
    echo "$envelope"
    echo "finished:      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >>"$TRANSCRIPT"
fi

if ! python3 -I "$ROOT_DIR/docs/evidence/issue-310/sanitize.py" --verify "$TRANSCRIPT"; then
  echo "cost: the transcript failed verification" >&2
  exit 1
fi
echo "cost: wrote ${TRANSCRIPT#"$ROOT_DIR"/}" >&2
exit "$status"
