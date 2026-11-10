#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #410, the Rust acceptance criterion: every
# touched Rust test passes 50 of 50 under 16 CPU spinners and 4
# `dd oflag=dsync` writers, in the default and the --no-default-features
# selections. Runs at the checked-out commit only: the criterion has no
# base side. Prints the transcript to stdout.
#
# What a run proves: at the commit it names, each listed test passed
# exactly 50 times in each selection, with no other outcome and no retry,
# while the load ran from the first test to the last. The list is checked
# against `cargo nextest list` before the load starts, so a filter that
# matches nothing, or a renamed test, cannot pass.
#
# What it trusts: the caller's shell, the Rust toolchain and nextest, and
# the two Postgres clusters it is given. It does not check that the
# clusters are disposable.
#
# Pass rule: each selection's nextest exits 0, and every test the
# selection expects passed exactly 50 times with no other outcome. A
# listed test that does not exist in a selection must be named, with its
# reason, in that selection's ABSENT list below; any other difference
# between the list and what nextest lists stops the run before the load.
#
# The two selections run at once, as in docs/evidence/issue-316/stress.sh:
# the load starts once, after both are built, and is the same fixed load
# for both from their first test to their last. Each runs with
# --test-threads 8, so the run adds its own contention on top of the load.
#
# The load is 16 shell spinners and 4 writers that each loop `dd
# oflag=dsync` over 16 MiB in 64 KiB blocks, into the work directory, on
# the filesystem that holds the tests' tempdirs (TMPDIR points there). The
# writers copy a random file made before the load: zeros would compress to
# almost nothing on a compressing filesystem such as btrfs with
# compress=zstd. The load stops, and its files are removed, on every exit
# path.
#
# Usage:
#   docs/evidence/issue-410/stress.sh > <transcript>
#     Redirect outside the tree (or into an ignored path): the script
#     refuses a dirty tree, and the redirect creates its file first.
#
# Environment:
#   STRESS_DATABASE_URL     Postgres for the default-features selection.
#   STRESS_ND_DATABASE_URL  Postgres for the --no-default-features
#                           selection: a different cluster, since two
#                           concurrent suites on one cluster share its
#                           connection limit. Both roles must be able to
#                           create databases. Name the host localhost, not
#                           a bracketed address.
#   STRESS_WORK_DIR         logs, the tests' TMPDIR and the load's files
#                           (default: <repo>/target/issue-410-stress). Not
#                           tmpfs. Short enough that a Unix socket fits
#                           under it: trial_engine binds
#                           $TMPDIR/.tmpXXXXXX/stub/docker.sock, and a
#                           socket path has at most 107 bytes.
#   CARGO_TARGET_DIR        default: <repo>/target. Not tmpfs.
#
# Exit: 0 when the pass rule holds, 1 when it does not, 2 on a harness
# error (a refusal, a build or list failure, a list mismatch).
#
# Refuses to run as root, because root bypasses the file-permission checks
# that tests and their fixtures make, so a root run could pass where the
# user's would not. Refuses a dirty tree, because the transcript names the
# commit. Takes a lock in the work directory, so two runs cannot overlap
# and double the load.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"

cd "$ROOT_DIR"

WORK_DIR="${STRESS_WORK_DIR:-$ROOT_DIR/target/issue-410-stress}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"

STRESS_COUNT=50
TEST_THREADS=8
SPINNERS=16
WRITERS=4

# The packages that hold a listed test, and the features of each
# selection. The no-default-features flags are the ones
# .github/workflows/pg-soak.yml runs that selection with.
PACKAGES=(-p fleet-auth -p trawl-web -p trawl-server -p trawl-engine -p trawl-client -p trawl-cli)
ND_FEATURES=(--no-default-features --features fleet-auth/keystore,fleet-auth/axum,fleet-auth/fast-hash)

# The reporter settings the outcome count below parses: one status line per
# test per iteration ("PASS [ 1.234s] [3/50] (7/52) <binary> <test>"), no
# colour, no progress bar, and the final summary repeats only failures.
REPORTER=(--color never --show-progress none --status-level pass
  --final-status-level fail --failure-output immediate --success-output never)

if [[ $(id -u) -eq 0 ]]; then
  echo "stress: refusing to run as root; run as a normal user" >&2
  exit 2
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "stress: working tree is dirty; commit first." >&2
  git status --short >&2
  exit 2
fi

if [[ -z "${STRESS_DATABASE_URL:-}" || -z "${STRESS_ND_DATABASE_URL:-}" ]]; then
  echo "stress: set STRESS_DATABASE_URL and STRESS_ND_DATABASE_URL to two disposable clusters" >&2
  exit 2
fi
if [[ "$STRESS_DATABASE_URL" == "$STRESS_ND_DATABASE_URL" ]]; then
  echo "stress: the two selections need separate clusters" >&2
  exit 2
fi

LOAD_DIR="$WORK_DIR/load"
TEST_TMP="$WORK_DIR/tmp"
LOG_DIR="$WORK_DIR/logs"
mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "stress: $dir is on tmpfs; point it at a disk-backed directory" >&2
    exit 2
  fi
done
# The longest socket path a listed test binds under TMPDIR, against the
# 107 bytes sockaddr_un holds.
SOCKET_PROBE="$TEST_TMP/.tmpXXXXXX/stub/docker.sock"
if [[ ${#SOCKET_PROBE} -gt 107 ]]; then
  echo "stress: $TEST_TMP is too long for trial_engine's Unix socket" \
    "(${#SOCKET_PROBE} > 107 bytes); set STRESS_WORK_DIR to a shorter path" >&2
  exit 2
fi
exec 9>"$WORK_DIR/lock"
if ! flock -n 9; then
  echo "stress: another run holds $WORK_DIR/lock; wait for it to finish" >&2
  exit 2
fi
rm -rf "$LOAD_DIR" "$TEST_TMP" "$LOG_DIR"
mkdir -p "$LOAD_DIR" "$TEST_TMP" "$LOG_DIR"

# The touched Rust tests, as "binary-id test-name" pairs the way nextest
# lists them. A test that uses a changed helper (closed_port, free_ports,
# the trial Engine's up() and resumable(), and doctor_web's doctor() and
# Origin::trawl_web fixtures) counts as touched. trawl-web compiles
# trawl-server's test_timing module, so its tests run in both binaries.
TESTS=(
  'fleet-auth::migration_pg cases::cancelled_lock_wait_does_not_recycle_a_locked_connection'
  'trawl-server::schema_pg cases::cancelled_lock_wait_does_not_recycle_a_locked_connection'
  'trawl-client client::tests::refused_connection_is_connect_kind'
  'trawl-engine::query_budgets spill_cap_refuses_a_low_fixed_cap'
  'trawl-engine::query_budgets spill_stays_within_budget_plus_class_slack'
  'trawl-server investigations::step::pg_tests::a_stalled_rollback_does_not_stop_the_writer'
  'trawl-server::investigations_attach a_step_admission_spends_one_deadline_across_statements'
  'trawl-server::investigations_attach a_step_admissions_log_insert_waits_only_what_remains_of_the_capture_deadline'
  'trawl-server::publication_consistency aborting_request_keeps_running_reader_protected'
  'trawl-server::publication_consistency query_timeout_keeps_publication_guard_until_duckdb_task_finishes'
  'trawl-web routes::proxy::tests::proxy_body_budget_bounds_buffered_bodies_at_an_8_mib_limit'
  'trawl-web routes::proxy::tests::proxy_body_budget_charges_the_declared_length_until_the_body_is_sent_and_answered'
  'trawl-web routes::proxy::tests::proxy_body_budget_is_held_while_the_sent_body_is_still_queued'
  'trawl-web routes::proxy::tests::proxy_body_budget_never_refuses_before_the_count_at_the_default_limit'
  'trawl-web routes::proxy::tests::proxy_stalled_logins_release_the_whole_count_at_the_deadline'
)
for binary in trawl-server trawl-web; do
  for name in frozen_keeps_the_paused_clock_across_a_blocking_sleep \
    frozen_keeps_the_paused_clock_across_a_loopback_round_trip \
    frozen_moves_the_clock_only_by_an_explicit_advance \
    until_leaves_the_paused_clock_alone \
    until_polling_reports_which_side_came_first; do
    TESTS+=("$binary test_timing::tests::$name")
  done
done
for name in other_commands_still_read_the_environment doctor_url_key_sources \
  doctor_missing_profile_fails_without_contact; do
  TESTS+=("trawl-cli::doctor $name")
done
for name in doctor_web_origin_accepted doctor_web_origin_rejected \
  doctor_web_foreign_endpoint doctor_web_truncated_body_is_too_large \
  doctor_web_body_cut_short_is_broken doctor_web_health_rate_limited; do
  TESTS+=("trawl-cli::doctor_web $name")
done
for name in ambient_variables_are_refused_by_name flags_that_move_the_target_are_refused \
  the_key_comes_from_exactly_one_source stdout_carries_protocol_messages_only \
  a_key_file_serves_a_profile_without_a_token a_broken_stdout_after_initialize_exits_one \
  a_hostile_request_id_cannot_forge_stderr_lines; do
  TESTS+=("trawl-cli::mcp_stdio $name")
done
for name in up_refuses_port_zero \
  a_denied_create_response_goes_on_when_the_claim_is_ours \
  a_denied_create_response_and_a_failed_inspect_keep_the_state \
  a_foreign_claim_removes_the_new_state \
  a_refused_create_keeps_the_state_and_a_rerun_creates_the_claim \
  a_dropped_create_keeps_the_state down_removes_a_kept_state_that_owns_nothing \
  a_foreign_claim_keeps_files_the_new_state_did_not_write \
  trial_teardown_interruptions trial_resume_storage_matrix \
  up_on_another_engine_names_the_engines_before_the_volumes; do
  TESTS+=("trawl-cli::trial_engine $name")
done

# Listed tests a selection does not compile, each with its reason. Both
# are empty: fleet-auth's migration_pg needs the keystore feature, which
# the no-default-features selection turns back on.
ABSENT_DEFAULT=()
ABSENT_ND=()

SELECTIONS=(default no-default-features)
declare -A EXPECTED_FILE

expected_for() {
  local selection="$1" pair absent=()
  if [[ $selection == default ]]; then
    absent=("${ABSENT_DEFAULT[@]+"${ABSENT_DEFAULT[@]}"}")
  else
    absent=("${ABSENT_ND[@]+"${ABSENT_ND[@]}"}")
  fi
  printf '%s\n' "${TESTS[@]}" \
    | grep -vxF -f <(printf '%s\n' "${absent[@]+"${absent[@]}"}" | grep .) || true
}

filter_for() {
  local pair binary name out=""
  for pair in "${TESTS[@]}"; do
    binary="${pair%% *}"
    name="${pair#* }"
    out+="${out:+ | }(binary_id(=$binary) & test(=$name))"
  done
  printf '%s' "$out"
}
FILTER="$(filter_for)"

LOAD_PIDS=()
RUN_PIDS=()

# Kill a process and every descendant, children first, so a writer's
# running dd or a selection's cargo and test processes do not outlive it.
kill_tree() {
  local child
  for child in $(pgrep -P "$1" 2>/dev/null); do
    kill_tree "$child"
  done
  kill "$1" 2>/dev/null || true
}

stop_load() {
  local pid
  for pid in "${LOAD_PIDS[@]+"${LOAD_PIDS[@]}"}"; do
    kill_tree "$pid"
  done
  for pid in "${LOAD_PIDS[@]+"${LOAD_PIDS[@]}"}"; do
    wait "$pid" 2>/dev/null || true
  done
  LOAD_PIDS=()
  rm -rf "$LOAD_DIR"
}

# An interrupted run also stops both selections, so no test keeps running
# after the script has gone.
stop_run() {
  local pid
  for pid in "${RUN_PIDS[@]+"${RUN_PIDS[@]}"}"; do
    kill_tree "$pid"
  done
  for pid in "${RUN_PIDS[@]+"${RUN_PIDS[@]}"}"; do
    wait "$pid" 2>/dev/null || true
  done
  RUN_PIDS=()
}

trap 'stop_run; stop_load' EXIT
trap 'stop_run; stop_load; exit 130' INT TERM

# nextest for one selection. The rest of the arguments follow the shared
# ones.
nextest() {
  local selection="$1" sub="$2"
  shift 2
  local url="$STRESS_DATABASE_URL" features=()
  if [[ $selection == no-default-features ]]; then
    url="$STRESS_ND_DATABASE_URL"
    features=("${ND_FEATURES[@]}")
  fi
  TMPDIR="$TEST_TMP" DATABASE_URL="$url" \
    cargo nextest "$sub" "${PACKAGES[@]}" "${features[@]+"${features[@]}"}" "$@"
}

say() { printf '%s\n' "$*"; }

say "# issue #410 stress run (the Rust acceptance criterion)"
say "script:        docs/evidence/issue-410/stress.sh"
say "commit:        $(git rev-parse HEAD)"
say "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "uid:           $(id -u)"
say "nextest:       $(cargo nextest --version 2>/dev/null | head -1)"
say "rustc:         $(rustc --version)"
say "cpus:          $(nproc)"
say "tempdir fs:    $(stat -f -c %T "$TEST_TMP") ($TEST_TMP)"
say "load:          $SPINNERS CPU spinners, $WRITERS dd oflag=dsync writers in the tempdir filesystem"
say "selections:    default and no-default-features, at once; no-default flags: ${ND_FEATURES[*]}"
say "per selection: --stress-count $STRESS_COUNT --retries 0 --no-fail-fast --no-tests=fail --test-threads $TEST_THREADS"
say "reporter:      ${REPORTER[*]}"
say "listed tests:  ${#TESTS[@]}"
say "load average before the load: $(cut -d' ' -f1-3 /proc/loadavg)"
say ""

# Build and list both selections before the load starts. Each selection's
# list must equal its expected tests exactly.
for selection in "${SELECTIONS[@]}"; do
  list="$LOG_DIR/$selection.list"
  expected="$LOG_DIR/$selection.expected"
  expected_for "$selection" | sort >"$expected"
  EXPECTED_FILE[$selection]="$expected"
  if ! nextest "$selection" list --message-format oneline -E "$FILTER" \
    >"$list" 2>"$LOG_DIR/$selection.build.log"; then
    say "stress: $selection does not build or list; see $LOG_DIR/$selection.build.log"
    exit 2
  fi
  if ! diff <(sort "$list") "$expected" >"$LOG_DIR/$selection.list.diff"; then
    say "stress: $selection lists a different set than it expects (< listed, > expected):"
    sed 's/^/  /' "$LOG_DIR/$selection.list.diff"
    exit 2
  fi
  say "$selection: builds, and lists exactly its $(grep -c . "$expected") expected tests"
done
say ""

head -c $((16 * 1024 * 1024)) /dev/urandom >"$LOAD_DIR/source"
for _ in $(seq "$SPINNERS"); do
  (while :; do :; done) &
  LOAD_PIDS+=($!)
done
for i in $(seq "$WRITERS"); do
  (while :; do
    dd if="$LOAD_DIR/source" of="$LOAD_DIR/writer-$i" bs=64k count=256 oflag=dsync status=none
  done) &
  LOAD_PIDS+=($!)
done
# Give the load average a minute to show the load before the run.
sleep 60
say "load started:  $(date -u +%Y-%m-%dT%H:%M:%SZ), load average after 60 s: $(cut -d' ' -f1-3 /proc/loadavg)"
say "filter:        $FILTER"
say ""

# Each test's outcomes in one nextest log, as "count<TAB>status<TAB>test".
# Only the lines before the final summary count: it repeats every failure.
count_outcomes() {
  awk '
    /^ *Summary \[/ { exit }
    match($0, /^ *[A-Z][A-Z-]*( [0-9]+)? +\[ *[0-9.]+s\] \[ *[0-9]+\/[0-9]+\] \( *[0-9]+\/[0-9]+\) /) {
      status = $1
      if (status == "SLOW" || status == "START") next
      print status "\t" substr($0, RSTART + RLENGTH)
    }
  ' "$1" | sort | uniq -c | sed -E 's/^ *([0-9]+) /\1\t/'
}

# Each distinct panic in one nextest log: the test, the panic site and the
# first line of its message, before the final summary.
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

ARGS=(run --stress-count "$STRESS_COUNT" --retries 0 --no-fail-fast --no-tests=fail
  --test-threads "$TEST_THREADS" "${REPORTER[@]}" -E "$FILTER")

say "## run"
say "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ), load average $(cut -d' ' -f1-3 /proc/loadavg)"
set +e
nextest default "${ARGS[@]}" >"$LOG_DIR/default.log" 2>&1 &
pid_default=$!
nextest no-default-features "${ARGS[@]}" >"$LOG_DIR/no-default-features.log" 2>&1 &
pid_nd=$!
RUN_PIDS=("$pid_default" "$pid_nd")
wait "$pid_default"
status_default=$?
wait "$pid_nd"
status_nd=$?
RUN_PIDS=()
set -e
say "finished:      $(date -u +%Y-%m-%dT%H:%M:%SZ), load average $(cut -d' ' -f1-3 /proc/loadavg)"
say "exit:          default $status_default, no-default-features $status_nd"
say ""

stop_load

overall=0
for selection in "${SELECTIONS[@]}"; do
  log="$LOG_DIR/$selection.log"
  if [[ $selection == default ]]; then status=$status_default; else status=$status_nd; fi
  say "## $selection"
  grep -E '^ *Summary \[' "$log" | sed 's/^ */nextest: /' || true
  counts="$(count_outcomes "$log")"
  met=1
  [[ $status -eq 0 ]] || met=0
  while IFS= read -r pair; do
    passed="$(awk -F'\t' -v t="$pair" '$2 == "PASS" && $3 == t { print $1 }' <<<"$counts")"
    other="$(awk -F'\t' -v t="$pair" '$2 != "PASS" && $3 == t { printf "%s%s %s", sep, $1, $2; sep = ", " }' <<<"$counts")"
    say "$(printf '%4s passed  %-14s %s' "${passed:-0}" "${other:--}" "$pair")"
    if [[ -n $other || ${passed:-0} -ne $STRESS_COUNT ]]; then met=0; fi
  done <"${EXPECTED_FILE[$selection]}"
  unexpected="$(awk -F'\t' '{ print $3 }' <<<"$counts" | sort -u | grep -vxF -f "${EXPECTED_FILE[$selection]}" || true)"
  if [[ -n $unexpected ]]; then
    met=0
    sed 's/^/unexpected test in the log: /' <<<"$unexpected"
  fi
  if [[ $status -ne 0 ]]; then
    count_panics "$log"
  fi
  if [[ $met -eq 1 ]]; then
    say "$selection: met (exit 0, every expected test passed $STRESS_COUNT of $STRESS_COUNT)"
  else
    say "$selection: NOT met"
    overall=1
  fi
  say ""
done

say "## summary"
if [[ $overall -eq 0 ]]; then
  say "every listed test passed $STRESS_COUNT of $STRESS_COUNT in both selections: met"
else
  say "every listed test passed $STRESS_COUNT of $STRESS_COUNT in both selections: NOT met"
fi
say "finished:      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "load average after the load stopped: $(cut -d' ' -f1-3 /proc/loadavg)"
exit $overall
