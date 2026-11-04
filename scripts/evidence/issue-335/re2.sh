#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #335, acceptance criterion "DuckDB RE2 memory
# is measured" (ADR-0063). It measures what the bundled DuckDB's RE2 holds
# in memory for 256 of the worst patterns admission allows in one query,
# per pattern, per thread and at max_concurrent_queries, and whether the
# query database's memory_limit counts that memory. The harness that
# measures is crates/trawl-engine/tests/re2_memory.rs; its module
# documentation explains the method.
#
# Method (as scripts/evidence/issue-334/cost.sh):
#   1. Export the pinned SHA's tree with `git archive` into a fresh
#      directory under WORK_DIR, so the build sees exactly what the commit
#      holds and nothing from the caller's working tree.
#   2. Build the harness once in release, outside the measured run.
#   3. Run each part, one ignored test of the harness each, and write its
#      stdout to the transcript under a banner that names the part. The
#      probe part starts the transcript with a header naming the SHA, the
#      host, the toolchain, the DuckDB and regex-automata versions and the
#      load average.
#   4. Check the transcript with docs/evidence/issue-310/sanitize.py
#      --verify, which fails on recording-machine paths and credentials.
#
# Usage:
#   scripts/evidence/issue-335/re2.sh [SHA [PART ...]]
#     SHA defaults to HEAD. PART is probe, patterns, threads, queries,
#     limit or summary; with none, all six run, in that order, in about 25
#     minutes. A PART list that holds probe starts a new transcript. Any
#     other list appends to the transcript of the same SHA, so each part
#     can run as a command of its own where a command has a time limit.
#     The parts share their corpus, the probe's choice and their results
#     through the run directory, so run them in order.
#
# Environment:
#   RE2_WORK_DIR      where the exported tree and the run directory go
#                     (default: <repo>/.flow-scratch/issue-335-re2). Must
#                     not be on tmpfs.
#   CARGO_TARGET_DIR  the release build (default: <RE2_WORK_DIR>/target),
#                     kept between runs. Must not be on tmpfs.
#   RE2_TRANSCRIPT    the transcript path
#                     (default: docs/evidence/issue-335/re2-memory.txt).
#   CURL_CA_BUNDLE    passed through to the DuckDB runtime download
#                     (default: /etc/ssl/certs/ca-certificates.crt when it
#                     exists).
#
# Exit status: 1 when the transcript fails verification; otherwise the
# first non-zero exit of a part; otherwise 0.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
HARNESS=crates/trawl-engine/tests/re2_memory.rs
ALL_PARTS=(probe patterns threads queries limit summary)

REV="${1:-HEAD}"
SHA="$(git -C "$ROOT_DIR" rev-parse --verify "$REV^{commit}")"
shift $(($# > 0 ? 1 : 0))
WORK_DIR="${RE2_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-335-re2}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
TRANSCRIPT="${RE2_TRANSCRIPT:-$ROOT_DIR/docs/evidence/issue-335/re2-memory.txt}"
if [[ -z "${CURL_CA_BUNDLE:-}" && -f /etc/ssl/certs/ca-certificates.crt ]]; then
  export CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
fi

declare -A asked=()
for part in "$@"; do
  case "$part" in
    probe | patterns | threads | queries | limit | summary) asked[$part]=1 ;;
    *)
      echo "re2: unknown part $part (want ${ALL_PARTS[*]})" >&2
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
if [[ "${parts[0]}" == probe ]]; then
  starts_transcript=true
elif ! grep -qx "pinned SHA:    $SHA" "$TRANSCRIPT" 2>/dev/null; then
  echo "re2: $TRANSCRIPT is not a transcript of $SHA; run probe first" >&2
  exit 2
fi

mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR" "$(dirname "$TRANSCRIPT")"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "re2: $dir is on tmpfs; a release build and the corpus do not belong there" >&2
    exit 1
  fi
done

# The run directory holds the corpus, the probe's choice and each part's
# results, for the parts after it. The probe starts it afresh.
RUN_DIR="$WORK_DIR/run-${SHA:0:12}"
if $starts_transcript; then
  rm -rf "$RUN_DIR"
fi
mkdir -p "$RUN_DIR"
export RE2_WORK_DIR="$RUN_DIR"

CHECKOUT="$WORK_DIR/checkout-${SHA:0:12}"
rm -rf "$CHECKOUT"
mkdir -p "$CHECKOUT"
trap 'rm -rf "$CHECKOUT"' EXIT
git -C "$ROOT_DIR" archive --format=tar "$SHA" | tar -x -C "$CHECKOUT"
if [[ ! -f "$CHECKOUT/$HARNESS" ]]; then
  echo "re2: $SHA has no $HARNESS" >&2
  exit 2
fi

# The DuckDB runtime is checksum-verified and cached per target dir. Seed
# a new target dir from the caller's, so a first run need not download it.
if [[ ! -d "$CARGO_TARGET_DIR/duckdb-runtime-cache" && -d "$ROOT_DIR/target/duckdb-runtime-cache" ]]; then
  cp -r "$ROOT_DIR/target/duckdb-runtime-cache" "$CARGO_TARGET_DIR/"
fi

echo "re2: building the harness at ${SHA:0:12} (outside the measured run)…" >&2
binary="$(cd "$CHECKOUT" && cargo test -p trawl-engine --test re2_memory --release --no-run \
  --message-format=json-render-diagnostics |
  jq -r 'select(.target.name == "re2_memory") | .executable | select(. != null)' |
  tail -1)"
if [[ -z "$binary" || ! -x "$binary" ]]; then
  echo "re2: could not locate the re2_memory test binary" >&2
  exit 1
fi

# A package's version from the pinned tree's Cargo.lock.
locked() {
  awk -v name="$1" '$1 == "name" && $3 == "\"" name "\"" { getline; gsub(/"/, "", $3); print $3; exit }' \
    "$CHECKOUT/Cargo.lock"
}

if $starts_transcript; then
  {
    echo "# issue #335: what DuckDB's RE2 holds in memory (ADR-0063)"
    echo "script:        scripts/evidence/issue-335/re2.sh"
    echo "script sha256: $(sha256sum "$SCRIPT_DIR/re2.sh" | cut -d' ' -f1)"
    echo "pinned SHA:    $SHA"
    echo "harness:       $HARNESS, at the pinned SHA"
    echo "harness sha256: $(sha256sum "$CHECKOUT/$HARNESS" | cut -d' ' -f1)"
    echo "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "rustc:         $(cd "$CHECKOUT" && rustc --version)"
    echo "crates:        duckdb $(locked duckdb), libduckdb-sys $(locked libduckdb-sys), regex-automata $(locked regex-automata)"
    echo "cpu:           $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//'), $(nproc) threads"
    echo "memory:        $(awk '/^MemTotal:/ { printf "%.1f GiB", $2 / 1048576 }' /proc/meminfo)"
    echo "kernel:        $(uname -sr)"
    echo "load average:  $(cut -d' ' -f1-3 /proc/loadavg) (1, 5 and 15 minutes)"
    echo
    echo "What it measures: the resident memory (VmRSS) and private dirty"
    echo "pages a trawld-shaped query database grows by while batch queries"
    echo "of 256 regex patterns run in DuckDB's RE2, over a baseline taken"
    echo "after a warm-up query, and what is still resident after the"
    echo "statements drop. Every point runs in a fresh process. It enforces"
    echo "nothing: ADR-0063 ships RE2's memory as a measured envelope."
  } >"$TRANSCRIPT"
fi

banner() {
  local rule="======================================================================"
  echo
  echo "$rule"
  case "$1" in
    probe) echo "PROBE: which admitted pattern family RE2 holds the most for" ;;
    patterns) echo "PATTERNS: 1, 64 and 256 patterns in one query" ;;
    threads) echo "THREADS: 256 patterns on 1, 2 and 4 database threads" ;;
    queries) echo "QUERIES: 256 patterns at max_concurrent_queries" ;;
    limit) echo "LIMIT: does memory_limit see RE2's memory" ;;
    summary) echo "SUMMARY" ;;
  esac
  echo "$rule"
}

status=0
for part in "${parts[@]}"; do
  echo "re2: running $part…" >&2
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

if [[ "${parts[-1]}" == summary ]]; then
  {
    echo "finished:      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "load average:  $(cut -d' ' -f1-3 /proc/loadavg) (1, 5 and 15 minutes)"
  } >>"$TRANSCRIPT"
fi

if ! python3 -I "$ROOT_DIR/docs/evidence/issue-310/sanitize.py" --verify "$TRANSCRIPT"; then
  echo "re2: the transcript failed verification" >&2
  exit 1
fi
echo "re2: wrote ${TRANSCRIPT#"$ROOT_DIR"/}" >&2
exit "$status"
