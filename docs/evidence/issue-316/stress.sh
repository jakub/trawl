#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #316, acceptance criteria 1 and 4. Runs every
# listed test `--stress-count 50 --retries 0` under a fixed synthetic load,
# with the default and the `--no-default-features` selections running at
# once, first on the base SHA (main before the fix) and then on the head
# SHA (the fix).
#
# The load is fixed here, before the first run, and is the same for both
# sides: 16 CPU spinners and 4 `dd oflag=dsync` writers on the filesystem
# that holds the tests' tempdir. It starts after every test binary is
# built, so both sides run under it from their first test to their last.
#
# Pass rule:
#   head  both selections exit 0, and every listed test passed exactly 50
#         times in each selection, with no other outcome.
#   base  at least one listed test fails with a failure the issue's
#         Context table names for it, in at least one selection, which
#         shows the harness reproduces the bug. A failure the table does not
#         name, such as a database error, is reported and does not count.
#
# Two runs on one checkout would share the work directory and double the
# load, so a second run refuses to start while the first holds its lock.
#
# The listed tests are the issue's Scope list. The head list replaces the
# one test the fix split with its two successors, and adds the timeout
# verdict's control test, which the base does not have.
#
# Usage:
#   docs/evidence/issue-316/stress.sh [BASE_SHA [HEAD_SHA]]
#     BASE_SHA defaults to 505e2e14, HEAD_SHA to HEAD.
#
# Environment:
#   STRESS_DATABASE_URL     Postgres for the default-features selection.
#   STRESS_ND_DATABASE_URL  Postgres for the --no-default-features
#                           selection. A different cluster from the first:
#                           two concurrent suites on one cluster share its
#                           connection limit and its shared memory.
#                           Both roles must be able to create databases. A
#                           disposable cluster on fractal, for example:
#                             docker run -d --rm --name stress-pg-a \
#                               --shm-size=1g -p '[::1]:5451:5432' \
#                               -e POSTGRES_USER=fleet \
#                               -e POSTGRES_PASSWORD=fleet \
#                               -e POSTGRES_DB=fleet_test \
#                               postgres:18 -c max_connections=300
#                           then postgres://fleet:fleet@localhost:5451/fleet_test.
#                           Name the host localhost, not [::1]: the
#                           fixtures' sqlx connect fails to look up a
#                           bracketed address.
#   STRESS_SIDES            "base head" (the default), or one of them, so a
#                           caller whose commands are time-capped can run
#                           the sides apart.
#   STRESS_APPEND           set to 1 to append to the transcript instead of
#                           replacing it, so sides run apart still land in
#                           one file.
#   STRESS_WORK_DIR         detached worktrees, logs and the load's files
#                           (default: <repo>/target/issue-316-stress). Not
#                           tmpfs: the dd writers must hit the disk the
#                           tests' tempdirs live on, and TMPDIR points here.
#   CARGO_TARGET_DIR        shared by both worktrees (default:
#                           <repo>/target). Not tmpfs either.
#   STRESS_TRANSCRIPT       the transcript path
#                           (default: docs/evidence/issue-316/stress.txt).
#
# Refuses to run as root: the publication-pause tests return early under
# root, which would make their passes vacuous. Refuses a dirty tree, because
# the transcript names the script's commit.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"

cd "$ROOT_DIR"

BASE_REV="${1:-505e2e14}"
HEAD_REV="${2:-HEAD}"
SIDES="${STRESS_SIDES:-base head}"
WORK_DIR="${STRESS_WORK_DIR:-$ROOT_DIR/target/issue-316-stress}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
TRANSCRIPT="${STRESS_TRANSCRIPT:-$SCRIPT_DIR/stress.txt}"

STRESS_COUNT=50
TEST_THREADS=8
SPINNERS=16
WRITERS=4

if [[ $(id -u) -eq 0 ]]; then
  echo "stress: running as root makes the publication-pause tests vacuous; run as a normal user" >&2
  exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "stress: working tree is dirty; commit or stash first." >&2
  git status --short >&2
  exit 1
fi

if [[ -z "${STRESS_DATABASE_URL:-}" || -z "${STRESS_ND_DATABASE_URL:-}" ]]; then
  echo "stress: set STRESS_DATABASE_URL and STRESS_ND_DATABASE_URL to two disposable clusters" >&2
  exit 1
fi
if [[ "$STRESS_DATABASE_URL" == "$STRESS_ND_DATABASE_URL" ]]; then
  echo "stress: the two selections need separate clusters" >&2
  exit 1
fi

for side in $SIDES; do
  if [[ $side != base && $side != head ]]; then
    echo "stress: no such side: $side" >&2
    exit 1
  fi
done

BASE_SHA="$(git rev-parse --verify "$BASE_REV^{commit}")"
HEAD_SHA="$(git rev-parse --verify "$HEAD_REV^{commit}")"

LOAD_DIR="$WORK_DIR/load"
TEST_TMP="$WORK_DIR/tmp"
LOG_DIR="$WORK_DIR/logs"
mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "stress: $dir is on tmpfs; point it at a disk-backed directory" >&2
    exit 1
  fi
done
exec 9>"$WORK_DIR/lock"
if ! flock -n 9; then
  echo "stress: another run holds $WORK_DIR/lock; wait for it to finish" >&2
  exit 1
fi
rm -rf "$LOAD_DIR" "$TEST_TMP" "$LOG_DIR"
mkdir -p "$LOAD_DIR" "$TEST_TMP" "$LOG_DIR"

# "binary-id test-name" pairs, as nextest prints them.
COMPACTION='trawl-server ingest::compaction::tests::'
COMMON_TESTS=(
  'trawl-server syslog::tcp::tests::tcp_stalls_without_disconnect_and_ingests_each_frame_once_after_drain'
  "${COMPACTION}a_failing_pass_that_ends_open_still_cools_down"
  "${COMPACTION}a_refusal_before_the_wal_exists_does_not_strand_the_burst"
  "${COMPACTION}a_clean_rerun_that_races_in_flight_charge_drains_it_after"
  "${COMPACTION}a_normal_pass_held_across_a_burst_keeps_its_pressure_wake"
  "${COMPACTION}compaction_undeletable_wal_never_duplicates"
  "${COMPACTION}operational_consumed_wal_removal_failure_preserves_published_output"
  "${COMPACTION}recovery_keeps_runtime_responsive_and_guards_after_cancellation"
  "${COMPACTION}publication_blocks_reader_until_hot_batch_is_drained"
  "${COMPACTION}rollup_publication_blocks_readers_until_hourly_retirement"
  "${COMPACTION}refusals_against_in_flight_charge_wait_for_an_insert"
  'trawl-server::query_resources timeout_verdict_survives_resource_failure'
  'trawl-server::repin ingest_queries_and_a_second_repin_ride_through_a_slow_rewrite'
  'trawl-server::repin repin_is_invisible_to_queries_and_resurrects_shelved_values'
  'trawl-server::results_sweep the_scheduler_s_retention_tick_sweeps_captured_results'
  'trawl-server::results_sweep the_standalone_sweep_tick_expires_captures_without_the_scheduler'
  'trawl-server::publication_consistency queries_and_exports_wait_for_publish_and_drain'
)
BASE_ONLY=(
  "${COMPACTION}pressure_pass_skips_rollup_and_the_normal_deadline_still_rolls_up"
)
HEAD_ONLY=(
  "${COMPACTION}a_pressure_pass_skips_the_daily_rollup"
  "${COMPACTION}the_normal_deadline_rolls_up_under_held_pressure"
  'trawl-server::query_resources resource_failure_released_before_the_deadline_is_the_answer'
)

EXPECTED=()
expected_for() {
  if [[ $1 == base ]]; then
    EXPECTED=("${COMMON_TESTS[@]}" "${BASE_ONLY[@]}")
  else
    EXPECTED=("${COMMON_TESTS[@]}" "${HEAD_ONLY[@]}")
  fi
}

filter_for() {
  local pair binary name out=""
  for pair in "$@"; do
    binary="${pair%% *}"
    name="${pair#* }"
    out+="${out:+ | }(binary_id(=$binary) & test(=$name))"
  done
  printf '%s' "$out"
}

WORKTREES=()
LOAD_PIDS=()

stop_load() {
  local pid
  for pid in "${LOAD_PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in "${LOAD_PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  LOAD_PIDS=()
  rm -rf "$LOAD_DIR"
}

cleanup() {
  stop_load
  local wt
  for wt in "${WORKTREES[@]}"; do
    git -C "$ROOT_DIR" worktree remove --force "$wt" 2>/dev/null || true
  done
  git -C "$ROOT_DIR" worktree prune
}

trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

make_worktree() {
  local side="$1" sha="$2" wt="$WORK_DIR/$1"
  if [[ -e "$wt" ]]; then
    git -C "$ROOT_DIR" worktree remove --force "$wt" 2>/dev/null || rm -rf "$wt"
    git -C "$ROOT_DIR" worktree prune
  fi
  WORKTREES+=("$wt")
  git -C "$ROOT_DIR" worktree add --detach "$wt" "$sha" >/dev/null
}

# nextest for one selection of one side. The rest of the arguments go to
# nextest after the shared ones.
nextest() {
  local side="$1" selection="$2" sub="$3"
  shift 3
  local url="$STRESS_DATABASE_URL" features=()
  if [[ $selection == no-default-features ]]; then
    url="$STRESS_ND_DATABASE_URL"
    features=(--no-default-features)
  fi
  (cd "$WORK_DIR/$side" && TMPDIR="$TEST_TMP" DATABASE_URL="$url" \
    cargo nextest "$sub" -p trawl-server "${features[@]}" "$@")
}

OUT="$(mktemp "$WORK_DIR/transcript.XXXXXX")"
say() { printf '%s\n' "$*" | tee -a "$OUT"; }

say "# issue #316 stress runs (acceptance criteria 1 and 4)"
say "script:        docs/evidence/issue-316/stress.sh"
say "script commit: $(git rev-parse HEAD)"
say "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "base SHA:      $BASE_SHA"
say "head SHA:      $HEAD_SHA"
say "sides:         $SIDES"
say "uid:           $(id -u)"
say "nextest:       $(cargo nextest --version | head -1)"
say "rustc:         $(rustc --version)"
say "cpus:          $(nproc)"
say "tempdir fs:    $(stat -f -c %T "$TEST_TMP") ($TEST_TMP)"
say "load:          $SPINNERS CPU spinners, $WRITERS dd oflag=dsync writers in the tempdir filesystem"
say "per selection: --stress-count $STRESS_COUNT --retries 0 --no-fail-fast --no-tests=fail --test-threads $TEST_THREADS"
say "load average before the load: $(cut -d' ' -f1-3 /proc/loadavg)"
say ""

# Build and list every side and selection before the load starts. The list
# proves each listed test exists under its exact name, so a filter that
# silently matches nothing cannot pass for a test.
for side in $SIDES; do
  if [[ $side == base ]]; then sha="$BASE_SHA"; else sha="$HEAD_SHA"; fi
  make_worktree "$side" "$sha"
  expected_for "$side"
  filter="$(filter_for "${EXPECTED[@]}")"
  for selection in default no-default-features; do
    list="$LOG_DIR/$side-$selection.list"
    if ! nextest "$side" "$selection" list --message-format oneline -E "$filter" >"$list" 2>"$LOG_DIR/$side-$selection.build.log"; then
      say "stress: $side $selection does not build or list; see $LOG_DIR/$side-$selection.build.log"
      exit 2
    fi
    for pair in "${EXPECTED[@]}"; do
      if ! grep -qxF -- "$pair" "$list"; then
        say "stress: $side $selection has no test: $pair"
        exit 2
      fi
    done
    if [[ $(grep -c . "$list") -ne ${#EXPECTED[@]} ]]; then
      say "stress: $side $selection lists tests the filter should not select:"
      grep -vxF -f <(printf '%s\n' "${EXPECTED[@]}") "$list" | sed 's/^/  /' | tee -a "$OUT"
      exit 2
    fi
  done
done

for _ in $(seq "$SPINNERS"); do
  (while :; do :; done) &
  LOAD_PIDS+=($!)
done
for i in $(seq "$WRITERS"); do
  (while :; do
    dd if=/dev/zero of="$LOAD_DIR/writer-$i" bs=64k count=256 oflag=dsync status=none
  done) &
  LOAD_PIDS+=($!)
done
# Give the load average a minute to show the load before the first run.
sleep 60
say "load started:  $(date -u +%Y-%m-%dT%H:%M:%SZ), load average after 60 s: $(cut -d' ' -f1-3 /proc/loadavg)"
say ""

# Count each test's outcomes in one nextest log. Only the lines before the
# final summary count: nextest repeats every failure there.
count_outcomes() {
  awk '
    /^ *Summary \[/ { exit }
    match($0, /^ *[A-Z][A-Z-]*( [0-9]+)? +\[ *[0-9.]+s\] \[ *[0-9]+\/[0-9]+\] \( *[0-9]+\/[0-9]+\) /) {
      status = $1
      if (status == "SLOW" || status == "START") next
      rest = substr($0, RSTART + RLENGTH)
      print status "\t" rest
    }
  ' "$1" | sort | uniq -c | sed -E 's/^ *([0-9]+) /\1\t/'
}

# Count each distinct panic in one nextest log: the test, the panic site
# and the first line of its message. Only the lines before the final
# summary count, as in count_outcomes.
count_panics() {
  awk '
    /^ *Summary \[/ { exit }
    match($0, /thread \x27[^\x27]+\x27 \([0-9]+\) panicked at [^ ]+:$/) {
      line = $0
      sub(/^ *thread \x27/, "", line)
      test = line; sub(/\x27.*/, "", test)
      site = line; sub(/.* panicked at /, "", site); sub(/:$/, "", site)
      if ((getline message) > 0) { sub(/^ */, "", message) } else { message = "" }
      print test " at " site ": " message
    }
  ' "$1" | sort | uniq -c | sed -E 's/^ *([0-9]+) /panic: \1x /'
}

# The failures the issue's Context table names, as "key|message
# fragment". The fragments are repro.sh's needles for the same rows. A key
# is either `test=` and the test as its panic names it, or `site=` and the
# panic's file and line at the base SHA. Two rows need a site: their
# timeout panics on a helper thread, and the same message from an earlier
# step of the same test is not the named failure.
NAMED_FAILURES=(
  'test=syslog::tcp::tests::tcp_stalls_without_disconnect_and_ingests_each_frame_once_after_drain|still open after the drain'
  'test=syslog::tcp::tests::tcp_stalls_without_disconnect_and_ingests_each_frame_once_after_drain|timed out: every frame lands'
  'test=ingest_queries_and_a_second_repin_ride_through_a_slow_rewrite|the running gauge must fall back to 0'
  'test=ingest::compaction::tests::a_failing_pass_that_ends_open_still_cools_down|the large request kept retrying'
  'test=ingest::compaction::tests::a_refusal_before_the_wal_exists_does_not_strand_the_burst|the burst stayed resident'
  'test=ingest::compaction::tests::a_clean_rerun_that_races_in_flight_charge_drains_it_after|the in-flight group stayed resident'
  'test=ingest::compaction::tests::a_normal_pass_held_across_a_burst_keeps_its_pressure_wake|the burst stayed resident'
  'site=crates/trawl-server/src/ingest/compaction.rs:6291:|Timeout'
  'test=ingest::compaction::tests::pressure_pass_skips_rollup_and_the_normal_deadline_still_rolls_up|the insert under pressure woke a pass'
  'test=ingest::compaction::tests::refusals_against_in_flight_charge_wait_for_an_insert|refusals kept coming'
  'test=timeout_verdict_survives_resource_failure|the work failed before the deadline'
  'site=crates/trawl-server/tests/publication_consistency.rs:81:|Timeout'
  'test=repin_is_invisible_to_queries_and_resurrects_shelved_values|aside_root'
  'test=the_scheduler_s_retention_tick_sweeps_captured_results|exists(&results.root'
)

# Print the count_panics lines that are named failures, marked "named:".
# A count_panics line reads "panic: <n>x <thread> at <site>: <message>".
named_failures() {
  local line entry key fragment thread rest site message
  while IFS= read -r line; do
    rest="${line#panic: * }"
    thread="${rest%% at *}"
    rest="${rest#* at }"
    site="${rest%%: *}"
    message="${rest#*: }"
    for entry in "${NAMED_FAILURES[@]}"; do
      key="${entry%%|*}"
      fragment="${entry#*|}"
      if [[ $message != *"$fragment"* ]]; then
        continue
      fi
      if [[ $key == "test=$thread" || ( $key == site=* && $site: == "${key#site=}"* ) ]]; then
        printf 'named: %s\n' "${line#panic: }"
        break
      fi
    done
  done
}

declare -A VERDICT

run_side() {
  local side="$1" sha log_default log_nd pid_default pid_nd status_default status_nd filter
  if [[ $side == base ]]; then sha="$BASE_SHA"; else sha="$HEAD_SHA"; fi
  expected_for "$side"
  filter="$(filter_for "${EXPECTED[@]}")"
  log_default="$LOG_DIR/$side-default.log"
  log_nd="$LOG_DIR/$side-no-default-features.log"

  say "## $side $sha"
  say "filter: $filter"
  say "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ), load average $(cut -d' ' -f1-3 /proc/loadavg)"

  local args=(run --stress-count "$STRESS_COUNT" --retries 0 --no-fail-fast --no-tests=fail
    --test-threads "$TEST_THREADS" -E "$filter")
  set +e
  nextest "$side" default "${args[@]}" >"$log_default" 2>&1 &
  pid_default=$!
  nextest "$side" no-default-features "${args[@]}" >"$log_nd" 2>&1 &
  pid_nd=$!
  wait "$pid_default"
  status_default=$?
  wait "$pid_nd"
  status_nd=$?
  set -e

  say "finished:      $(date -u +%Y-%m-%dT%H:%M:%SZ), load average $(cut -d' ' -f1-3 /proc/loadavg)"
  say "exit:          default $status_default, no-default-features $status_nd"

  local selection log status pair binary name passed other nonpass=0 short=0 named_seen=0
  for selection in default no-default-features; do
    if [[ $selection == default ]]; then log="$log_default"; status=$status_default
    else log="$log_nd"; status=$status_nd; fi
    say "### $selection"
    grep -E '^ *Summary \[' "$log" | sed 's/^ */nextest: /' | tee -a "$OUT" || true
    local counts
    counts="$(count_outcomes "$log")"
    for pair in "${EXPECTED[@]}"; do
      binary="${pair%% *}"
      name="${pair#* }"
      passed="$(awk -F'\t' -v t="$binary $name" '$2 == "PASS" && $3 == t { print $1 }' <<<"$counts")"
      other="$(awk -F'\t' -v t="$binary $name" '$2 != "PASS" && $3 == t { printf "%s%s %s", sep, $1, $2; sep = ", " }' <<<"$counts")"
      say "$(printf '%4s passed  %-14s %s' "${passed:-0}" "${other:--}" "$pair")"
      if [[ -n $other ]]; then nonpass=1; fi
      if [[ ${passed:-0} -ne $STRESS_COUNT ]]; then short=1; fi
    done
    # Lines that name an outcome for a test outside the list would mean the
    # filter drifted; the list check above should make this empty.
    awk -F'\t' '{ print $3 }' <<<"$counts" | sort -u | grep -vxF -f <(printf '%s\n' "${EXPECTED[@]}") \
      | sed 's/^/unexpected test in the log: /' | tee -a "$OUT" || true
    if [[ $status -ne 0 ]]; then
      local panics named
      panics="$(count_panics "$log")"
      printf '%s\n' "$panics" | tee -a "$OUT"
      named="$(named_failures <<<"$panics")"
      if [[ -n $named ]]; then
        printf '%s\n' "$named" | tee -a "$OUT"
        named_seen=1
      fi
    fi
    if [[ $side == head && $status -ne 0 ]]; then nonpass=1; fi
  done

  if [[ $side == head ]]; then
    if [[ $nonpass -eq 0 && $short -eq 0 ]]; then VERDICT[head]=met; else VERDICT[head]="NOT met"; fi
  else
    if [[ $named_seen -eq 1 ]]; then VERDICT[base]=met; else VERDICT[base]="NOT met"; fi
  fi
  say ""
}

for side in $SIDES; do
  run_side "$side"
done

stop_load

say "## summary"
overall=0
for side in $SIDES; do
  if [[ $side == base ]]; then
    say "base (at least one listed test fails with a named failure under the load): ${VERDICT[base]}"
  else
    say "head (every listed test passes $STRESS_COUNT of $STRESS_COUNT in both selections): ${VERDICT[head]}"
  fi
  [[ ${VERDICT[$side]} == met ]] || overall=1
done
say "finished:      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "load average after the load stopped: $(cut -d' ' -f1-3 /proc/loadavg)"

if [[ ${STRESS_APPEND:-0} == 1 && -f "$TRANSCRIPT" ]]; then
  { echo; cat "$OUT"; } >>"$TRANSCRIPT"
  rm -f "$OUT"
  echo "stress: transcript appended to $TRANSCRIPT"
else
  mv "$OUT" "$TRANSCRIPT"
  echo "stress: transcript written to $TRANSCRIPT"
fi
exit $overall
