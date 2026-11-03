#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #334, acceptance criterion "The cost of one
# call is measured" (ADR-0063). It times one regex call, in Rust and in
# DuckDB RE2, for the worst pattern both budgets admit, over a 65 536-
# character adversarial subject, against a 0.25 s ceiling, and the
# admission time of the worst admitted queries. The harness that measures
# is crates/trawl-engine/tests/regex_cost.rs; its module documentation
# explains how the worst pattern is searched for and what the ceiling
# covers.
#
# Method:
#   1. Export the pinned SHA's tree with `git archive` into a fresh
#      directory under WORK_DIR, so the build sees exactly what the commit
#      holds and nothing from the caller's working tree.
#   2. If the pinned tree has no harness (a SHA from before the harness
#      landed), copy in the caller's harness. The transcript says which,
#      with the harness's sha256 either way.
#   3. Build the harness once in release, outside the timed run.
#   4. Run it and write its stdout, under a header naming the SHA, the
#      host and the toolchain, to the transcript.
#   5. Check the transcript with docs/evidence/issue-310/sanitize.py
#      --verify, which fails on recording-machine paths and credentials.
#
# Usage:
#   scripts/evidence/issue-334/cost.sh [SHA]
#     SHA defaults to HEAD.
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
# Exit status: the harness's own, which is non-zero when any admitted case
# exceeds the ceiling, or 1 when the transcript fails verification. The
# transcript is written either way.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
HARNESS=crates/trawl-engine/tests/regex_cost.rs

REV="${1:-HEAD}"
SHA="$(git -C "$ROOT_DIR" rev-parse --verify "$REV^{commit}")"
WORK_DIR="${COST_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-334-cost}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
TRANSCRIPT="${COST_TRANSCRIPT:-$ROOT_DIR/docs/evidence/issue-334/transcript.txt}"

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
  echo "load average:  $(cut -d' ' -f1-3 /proc/loadavg)"
  echo
  echo "Scope: the ceiling is per call over one stored value of at most"
  echo "MAX_RAW_CHARS (65 536) characters. Values stored before the ingest"
  echo "cap keep their length, and a computed subject such as"
  echo "concat(message, message) can be longer than one stored value; both"
  echo "fall outside the ceiling and cost in proportion to their length."
  echo
} >"$TRANSCRIPT"

echo "cost: running the harness…" >&2
set +e
"$binary" --exact regex_cost_transcript --ignored --nocapture --test-threads=1 >>"$TRANSCRIPT"
status=$?
set -e
{
  echo
  echo "harness exit: $status"
} >>"$TRANSCRIPT"

if ! python3 -I "$ROOT_DIR/docs/evidence/issue-310/sanitize.py" --verify "$TRANSCRIPT"; then
  echo "cost: the transcript failed verification" >&2
  exit 1
fi
echo "cost: wrote ${TRANSCRIPT#"$ROOT_DIR"/}" >&2
exit "$status"
