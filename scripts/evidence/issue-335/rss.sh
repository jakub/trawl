#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #335, the supplemental RSS run (ADR-0063, as
# amended 2026-11-04): a release trawld with 32 live streams open and
# max_concurrent_queries batch queries sent at the worst admitted RE2
# shape, so some are refused with regex_reservation_exhausted. Peak RSS
# growth over the idle baseline must stay within the regex reservation
# plus the allocator retention that docs/evidence/issue-335/re2-memory.txt
# records. The driver, rss.py beside this script, documents each step.
#
# Method (as scripts/evidence/issue-335/re2.sh):
#   1. Export the pinned SHA's tree with `git archive`, so the build and
#      the driver are exactly what the commit holds.
#   2. Build trawld and fleet-admin in release, outside the measured run,
#      then wait for the build's load to decay (RSS_QUIET_LOAD).
#   3. Read the bounds from the pinned tree: the reservation, the per-query
#      cap and the matches site's allowance from trawl-core's
#      pattern/memory.rs, and the allocator retention from the pinned
#      re2-memory.txt.
#   4. Run the pinned rss.py. It starts trawld against two fresh databases
#      on the private Postgres, measures, and removes everything it made.
#   5. Write the transcript: a header naming the SHA, the host, the
#      versions and the load average, then the driver's stdout, then the
#      end time, the load average and the driver's exit status. Check it
#      with docs/evidence/issue-310/sanitize.py --verify.
#
# Usage:
#   RSS_DATABASE_URL=postgres://USER:PASSWORD@127.0.0.1:PORT/DB \
#     scripts/evidence/issue-335/rss.sh [SHA]
#   SHA defaults to HEAD. The database is the private test cluster's
#   administrative database; the run creates and drops its own two. Ports
#   5433 and 5434 are refused. The run takes about three minutes after the
#   build. Run it on a quiet machine: after the build it waits up to ten
#   minutes for the load the build left to decay, and records the load
#   average.
#
# Environment:
#   RSS_DATABASE_URL   the private Postgres (required).
#   RSS_WORK_DIR       where the exported tree and the run directory go
#                      (default: <repo>/.flow-scratch/issue-335-rss). Must
#                      not be on tmpfs.
#   CARGO_TARGET_DIR   the release build (default: <RSS_WORK_DIR>/target),
#                      kept between runs. Must not be on tmpfs.
#   RSS_TRANSCRIPT     the transcript path
#                      (default: docs/evidence/issue-335/rss.txt).
#   RSS_QUIET_LOAD     the 1-minute load average the run waits for after the
#                      build, at most ten minutes (default: 3).
#   CURL_CA_BUNDLE     passed through to the DuckDB runtime download
#                      (default: /etc/ssl/certs/ca-certificates.crt when it
#                      exists).
#
# Exit status: 1 when the run stopped on an error or the transcript fails
# verification; 3 when a check of the run fails; otherwise 0.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"

REV="${1:-HEAD}"
SHA="$(git -C "$ROOT_DIR" rev-parse --verify "$REV^{commit}")"
WORK_DIR="${RSS_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-335-rss}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
TRANSCRIPT="${RSS_TRANSCRIPT:-$ROOT_DIR/docs/evidence/issue-335/rss.txt}"
if [[ -z "${CURL_CA_BUNDLE:-}" && -f /etc/ssl/certs/ca-certificates.crt ]]; then
  export CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
fi
if [[ -z "${RSS_DATABASE_URL:-}" ]]; then
  echo "rss: set RSS_DATABASE_URL to the private test Postgres" >&2
  exit 1
fi
export RSS_DATABASE_URL

mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR" "$(dirname "$TRANSCRIPT")"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "rss: $dir is on tmpfs; a release build and the run's data do not belong there" >&2
    exit 1
  fi
done

# A fixed checkout path keeps the target dir's fingerprints valid from one
# SHA to the next, so a rerun rebuilds only what changed.
CHECKOUT="$WORK_DIR/checkout"
RUN_DIR="$WORK_DIR/run-${SHA:0:12}"
BODY="$WORK_DIR/body-${SHA:0:12}.txt"
rm -rf "$CHECKOUT" "$RUN_DIR"
mkdir -p "$CHECKOUT"
trap 'rm -rf "$CHECKOUT" "$RUN_DIR" "$BODY"' EXIT
git -C "$ROOT_DIR" archive --format=tar "$SHA" | tar -x -C "$CHECKOUT"
for path in scripts/evidence/issue-335/rss.py crates/trawl-core/src/pattern/memory.rs \
  docs/evidence/issue-335/re2-memory.txt; do
  if [[ ! -f "$CHECKOUT/$path" ]]; then
    echo "rss: $SHA has no $path" >&2
    exit 2
  fi
done

# The DuckDB runtime is checksum-verified and cached per target dir. Seed
# a new target dir from the caller's, so a first run need not download it.
if [[ ! -d "$CARGO_TARGET_DIR/duckdb-runtime-cache" && -d "$ROOT_DIR/target/duckdb-runtime-cache" ]]; then
  cp -r "$ROOT_DIR/target/duckdb-runtime-cache" "$CARGO_TARGET_DIR/"
fi

echo "rss: building trawld and fleet-admin at ${SHA:0:12} in release (outside the measured run)…" >&2
(cd "$CHECKOUT" && cargo build --release --locked -p trawl-server -p fleet-admin)
BIN_DIR="$CARGO_TARGET_DIR/release"
for binary in trawld fleet-admin deps/libduckdb.so; do
  if [[ ! -f "$BIN_DIR/$binary" ]]; then
    echo "rss: the build left no $binary" >&2
    exit 1
  fi
done

# One `pub const NAME: u64 = N * KIB|MIB;` of memory.rs, in bytes.
bytes_of() {
  local line
  line="$(grep -m1 -E "^pub const $1: u64 = [0-9]+ \* (KIB|MIB);" "$CHECKOUT/crates/trawl-core/src/pattern/memory.rs")"
  if [[ -z "$line" ]]; then
    echo "rss: $1 is not N * KIB or N * MIB in memory.rs" >&2
    exit 2
  fi
  sed -E 's/.* = ([0-9]+) \* KIB;/\1 1024/; s/.* = ([0-9]+) \* MIB;/\1 1048576/' <<<"$line" | awk '{ print $1 * $2 }'
}
RESERVATION="$(bytes_of REGEX_RESERVATION_BYTES)"
QUERY_CAP="$(bytes_of QUERY_RESERVATION_CAP)"
MATCH_SITE="$(bytes_of MATCH_SITE_RE2_BYTES)"
RETENTION="$(sed -nE 's/^Retained: .*, ([0-9.]+) MiB of resident memory stayed over the baseline.*/\1/p' \
  "$CHECKOUT/docs/evidence/issue-335/re2-memory.txt" | head -1)"
if [[ -z "$RETENTION" ]]; then
  echo "rss: the pinned re2-memory.txt has no Retained line" >&2
  exit 2
fi

# The build's own load must decay before the measured run starts: wait
# while the 1-minute load average is over RSS_QUIET_LOAD, a minute at a
# time and at most ten times, then run and record what it was.
QUIET="${RSS_QUIET_LOAD:-3}"
waited=0
while ((waited < 600)) && awk -v q="$QUIET" '{ exit !($1 > q) }' /proc/loadavg; do
  echo "rss: the 1-minute load average is $(cut -d' ' -f1 /proc/loadavg), over $QUIET; waiting 60 s" >&2
  sleep 60
  waited=$((waited + 60))
done
if awk -v q="$QUIET" '{ exit !($1 > q) }' /proc/loadavg; then
  quiet_note="still over $QUIET after waiting ${waited} s; ran anyway"
else
  quiet_note="at or under $QUIET after waiting ${waited} s for the build's load to decay"
fi

# A package's version from the pinned tree's Cargo.lock.
locked() {
  awk -v name="$1" '$1 == "name" && $3 == "\"" name "\"" { getline; gsub(/"/, "", $3); print $3; exit }' \
    "$CHECKOUT/Cargo.lock"
}

started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
load_start="$(cut -d' ' -f1-3 /proc/loadavg)"
set +e
python3 -I "$CHECKOUT/scripts/evidence/issue-335/rss.py" --bin-dir "$BIN_DIR" --run-dir "$RUN_DIR" \
  --reservation-bytes "$RESERVATION" --query-cap-bytes "$QUERY_CAP" --match-site-bytes "$MATCH_SITE" \
  --retention-mib "$RETENTION" >"$BODY"
status=$?
set -e
finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
load_end="$(cut -d' ' -f1-3 /proc/loadavg)"

{
  echo "# issue #335: trawld's resident memory under the regex reservation (ADR-0063)"
  echo "script:        scripts/evidence/issue-335/rss.sh"
  echo "script sha256: $(sha256sum "$CHECKOUT/scripts/evidence/issue-335/rss.sh" | cut -d' ' -f1)"
  echo "driver:        scripts/evidence/issue-335/rss.py, at the pinned SHA"
  echo "driver sha256: $(sha256sum "$CHECKOUT/scripts/evidence/issue-335/rss.py" | cut -d' ' -f1)"
  echo "pinned SHA:    $SHA"
  echo "started:       $started"
  echo "rustc:         $(cd "$CHECKOUT" && rustc --version)"
  echo "python:        $(python3 -I -c 'import platform; print(platform.python_version())')"
  echo "crates:        duckdb $(locked duckdb), libduckdb-sys $(locked libduckdb-sys), regex-automata $(locked regex-automata)"
  echo "build:         cargo build --release --locked -p trawl-server -p fleet-admin, before the run"
  echo "cpu:           $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//'), $(nproc) threads"
  echo "memory:        $(awk '/^MemTotal:/ { printf "%.1f GiB", $2 / 1048576 }' /proc/meminfo)"
  echo "kernel:        $(uname -sr)"
  echo "load average:  $load_start at the start (1, 5 and 15 minutes), $quiet_note"
  echo
  echo "What it measures: trawld's resident memory (VmRSS, /proc/<pid>/status) and"
  echo "private dirty memory (Private_Dirty, /proc/<pid>/smaps_rollup), sampled every"
  echo "quarter second, while 32 live streams match a varied Unicode feed and a burst"
  echo "of batch queries at the worst admitted RE2 shape meets the regex reservation."
  echo "A 503's cause is not on the wire; each is read from the http_failure event"
  echo "trawld logs for the response's request id. trawld listens on 127.0.0.1 on a"
  echo "port the kernel picks, with keys made for this run and two fresh databases,"
  echo "all removed at the end."
  echo
  cat "$BODY"
  echo
  echo "finished:      $finished"
  echo "load average:  $load_end at the end (1, 5 and 15 minutes)"
  echo "driver exit:   $status"
} >"$TRANSCRIPT"

if ! python3 -I "$ROOT_DIR/docs/evidence/issue-310/sanitize.py" --verify "$TRANSCRIPT"; then
  echo "rss: the transcript failed verification" >&2
  exit 1
fi
echo "rss: wrote ${TRANSCRIPT#"$ROOT_DIR"/}" >&2
exit "$status"
