#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #334 (ADR-0063): how long a batch regex query
# that has timed out keeps its executor permit. DuckDB checks for an
# interrupt between the 2048-row vectors it filters, never inside the
# regex calls over one vector, so the permit is held until each DuckDB
# thread finishes its vector. The probe in cancel-probe/ measures that
# through trawl-server's own executor pool; its module documentation
# describes the corpus, the bodies and the three instants it records.
#
# Two cases, both of which finish within minutes:
#   plain  the 64-position body 漢[漢字文]{62}[^漢字文] over 65 536-character
#          values: 4 files of 2048 rows, one vector a DuckDB thread.
#   fan    漢F{48}[^漢字文], F the fan class, over 2048-character values:
#          8 files of 4096 rows, two vectors a file.
# The fan body over 65 536-character values holds its permit for about 40
# minutes and is left out; ADR-0063 records it.
#
# Method:
#   1. Export the pinned SHA's tree with `git archive` into a fresh
#      directory under WORK_DIR. If the tree has no probe (a SHA from
#      before the probe landed), copy in the caller's probe. The transcript
#      says which, with the sha256 of each probe file either way.
#   2. Build the probe there with its own Cargo.lock (--locked), against
#      the exported trawl-server and trawl-core.
#   3. Run each case, generating its corpus under WORK_DIR unless a corpus
#      of the same shape is there, and write the probe's stdout to the
#      transcript under a banner. A run that starts the transcript writes
#      a header first.
#   4. Check the transcript with docs/evidence/issue-310/sanitize.py
#      --verify.
#
# Usage:
#   scripts/evidence/issue-334/cancel.sh [SHA [CASE ...]]
#     SHA defaults to HEAD. CASE is plain or fan; with none, both run,
#     plain first, in about 6 minutes after the build. A CASE list that
#     holds plain starts a new transcript; fan alone appends to the
#     transcript of the same SHA.
#
# Environment:
#   CANCEL_WORK_DIR    the exported tree and the corpora
#                      (default: <repo>/.flow-scratch/issue-334-cancel).
#                      Must not be on tmpfs: the plain corpus is about
#                      320 MB.
#   CARGO_TARGET_DIR   the probe's build (default: <CANCEL_WORK_DIR>/target).
#   CANCEL_TRANSCRIPT  the transcript path
#                      (default: docs/evidence/issue-334/cancel-overhang.txt).
#
# Exit status: 1 when the transcript fails verification; otherwise the
# first non-zero exit of a case; otherwise 0.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
PROBE=scripts/evidence/issue-334/cancel-probe
ALL_CASES=(plain fan)

REV="${1:-HEAD}"
SHA="$(git -C "$ROOT_DIR" rev-parse --verify "$REV^{commit}")"
shift $(($# > 0 ? 1 : 0))
WORK_DIR="${CANCEL_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-334-cancel}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
TRANSCRIPT="${CANCEL_TRANSCRIPT:-$ROOT_DIR/docs/evidence/issue-334/cancel-overhang.txt}"

declare -A asked=()
for case_name in "$@"; do
  case "$case_name" in
    plain | fan) asked[$case_name]=1 ;;
    *)
      echo "cancel: unknown case $case_name (want ${ALL_CASES[*]})" >&2
      exit 2
      ;;
  esac
done
cases=()
for case_name in "${ALL_CASES[@]}"; do
  if [[ $# -eq 0 || -n "${asked[$case_name]:-}" ]]; then
    cases+=("$case_name")
  fi
done

starts_transcript=false
if [[ "${cases[0]}" == plain ]]; then
  starts_transcript=true
elif ! grep -qx "pinned SHA:    $SHA" "$TRANSCRIPT" 2>/dev/null; then
  echo "cancel: $TRANSCRIPT is not a transcript of $SHA; run plain first" >&2
  exit 2
fi

mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR" "$(dirname "$TRANSCRIPT")"
if [[ "$(stat -f -c %T "$WORK_DIR")" == tmpfs ]]; then
  echo "cancel: $WORK_DIR is on tmpfs; the corpora do not fit there" >&2
  exit 1
fi

CHECKOUT="$WORK_DIR/checkout-${SHA:0:12}"
rm -rf "$CHECKOUT"
mkdir -p "$CHECKOUT"
trap 'rm -rf "$CHECKOUT"' EXIT
git -C "$ROOT_DIR" archive --format=tar "$SHA" | tar -x -C "$CHECKOUT"

if git -C "$ROOT_DIR" cat-file -e "$SHA:$PROBE/src/main.rs" 2>/dev/null; then
  probe_source="at the pinned SHA"
else
  rm -rf "${CHECKOUT:?}/$PROBE"
  mkdir -p "$CHECKOUT/$PROBE"
  cp -r "$ROOT_DIR/$PROBE/Cargo.toml" "$ROOT_DIR/$PROBE/Cargo.lock" "$ROOT_DIR/$PROBE/src" "$CHECKOUT/$PROBE/"
  probe_source="copied from the script's checkout; the pinned SHA has none"
fi

# The DuckDB runtime is checksum-verified and cached per target dir. Seed
# a new target dir from the caller's, so a first run need not download it.
if [[ ! -d "$CARGO_TARGET_DIR/duckdb-runtime-cache" && -d "$ROOT_DIR/target/duckdb-runtime-cache" ]]; then
  cp -r "$ROOT_DIR/target/duckdb-runtime-cache" "$CARGO_TARGET_DIR/"
fi

# Build from inside the tree, so the repository's .cargo/config.toml
# applies as it does to the workspace.
echo "cancel: building the probe against ${SHA:0:12}…" >&2
binary="$(cd "$CHECKOUT/$PROBE" && cargo build --locked \
  --message-format=json-render-diagnostics |
  jq -r 'select(.target.name == "issue-334-cancel-probe") | .executable | select(. != null)' |
  tail -1)"
if [[ -z "$binary" || ! -x "$binary" ]]; then
  echo "cancel: could not locate the probe binary" >&2
  exit 1
fi

if $starts_transcript; then
  {
    echo "# issue #334: how long a timed-out batch regex query holds its permit (ADR-0063)"
    echo "script:        scripts/evidence/issue-334/cancel.sh"
    echo "script sha256: $(sha256sum "$SCRIPT_DIR/cancel.sh" | cut -d' ' -f1)"
    echo "pinned SHA:    $SHA"
    echo "probe:         $PROBE, $probe_source"
    for file in Cargo.toml Cargo.lock src/main.rs; do
      printf 'probe sha256:  %s  %s\n' "$(sha256sum "$CHECKOUT/$PROBE/$file" | cut -d' ' -f1)" "$file"
    done
    echo "build:         debug; the regex work runs in the prebuilt libduckdb"
    echo "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "rustc:         $(cd "$CHECKOUT" && rustc --version)"
    echo "cpu:           $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//'), $(nproc) threads"
    echo "load average:  $(cut -d' ' -f1-3 /proc/loadavg) (1, 5 and 15 minutes)"
    echo
    echo "Each case runs one /query through ExecutorPool::execute with one"
    echo "permit, the server's default 4 DuckDB threads and a 5 s deadline."
    echo "The overhang is the time from the deadline to the permit being"
    echo "free again."
  } >"$TRANSCRIPT"
fi

status=0
for case_name in "${cases[@]}"; do
  case "$case_name" in
    plain) shape=(65536 2048 4 5 600) ;;
    fan) shape=(2048 4096 8 5 300) ;;
  esac
  {
    echo
    echo "======================================================================"
    echo "CASE $case_name"
    echo "======================================================================"
  } >>"$TRANSCRIPT"
  echo "cancel: running $case_name…" >&2
  set +e
  "$binary" "$WORK_DIR/corpus-$case_name" "$case_name" "${shape[@]}" >>"$TRANSCRIPT"
  case_status=$?
  set -e
  echo "case $case_name exit: $case_status" >>"$TRANSCRIPT"
  if [[ $status -eq 0 ]]; then
    status=$case_status
  fi
done

if ! python3 -I "$ROOT_DIR/docs/evidence/issue-310/sanitize.py" --verify "$TRANSCRIPT"; then
  echo "cancel: the transcript failed verification" >&2
  exit 1
fi
echo "cancel: wrote ${TRANSCRIPT#"$ROOT_DIR"/}" >&2
exit "$status"
