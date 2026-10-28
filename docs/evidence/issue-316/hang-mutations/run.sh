#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Runs the hang-guard-only mutations of #316 once each. The tests each
# patch selects can only fail by the 120 s hang guard in test_timing,
# because nothing else can end the wait they are stuck in. They stay out
# of crates/trawl-server/mutations/ so the CI `server-mutations` job never
# pays two minutes per patch.
#
# Three patches lose one wake of the compaction loop, where cadence time
# is frozen. Together they kill every test converted to controlled cadence
# time that waits for a wake. The m6 patch makes the timeout verdict wait
# for a worker parked at a seam that only the test's drop releases, so its
# request never answers.
#
# For each patch the script runs its tests on the pristine tree (the
# control, which must pass), applies the patch, runs them again with
# `--retries 0`, and reverts the patch. Expected result: nextest exit 100
# and `timed out after 120s waiting for: ...` from every listed test.
#
# A kill is nextest exit 100 with every selected test failed, each on the
# hang guard at the wait the patch starves: its panic names the test's
# thread and `timed out after 120s waiting for: <label>`, and EXPECT lists
# those test and label pairs. Exit 100 alone only says one or more tests
# failed, so a run where one selected test fails on another assertion, or
# hangs at a different wait, is not a kill. Exit 0 means the mutation
# survived, and any other status (101 for a build failure, 4 for a filter
# that matched no test) is an error. The script exits 0 only when every
# patch was killed.
#
# The check proves that each selected test died at its expected wait. It
# parses nextest's human output under the reporter flags run_filter pins,
# for cargo-nextest 0.9.149. It trusts the caller's shell, toolchain and
# patches: it guards against a misread run, not against a caller who
# wants it to lie.
#
# Usage:
#   docs/evidence/issue-316/hang-mutations/run.sh              # every patch
#   docs/evidence/issue-316/hang-mutations/run.sh m7-insert-does-not-wake-the-loop.patch
#
# The m6 test boots a real server, so DATABASE_URL must name a Postgres
# role that can create databases, as for the test suite. The compaction
# tests start no Postgres.
#
# Refuses to run when a tracked file differs from HEAD: a patch applied
# over uncommitted work may not revert cleanly. Untracked files are
# ignored, since `git apply` never touches them.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
cd "$ROOT_DIR"

export CARGO_INCREMENTAL=1

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "hang-mutations: tracked files differ from HEAD; commit or stash first." >&2
  git status --short --untracked-files=no >&2
  exit 1
fi

# FILTER selects the tests. EXPECT holds one `<thread>|<label>` pair per
# selected test: the panicking thread's name, which is the test's path in
# its binary, and the label of the wait the patch starves.
FILTER=()
EXPECT=()
filter_for() {
  local lib=ingest::compaction::tests
  case "$1" in
    m2-open-plan-swallows-pressure-generation.patch)
      FILTER=(--lib a_normal_pass_held_across_a_burst_keeps_its_pressure_wake)
      EXPECT=("$lib::a_normal_pass_held_across_a_burst_keeps_its_pressure_wake|the burst's pressure pass")
      ;;
    m7-insert-does-not-wake-the-loop.patch)
      FILTER=(--lib a_refusal_before_the_wal_exists_does_not_strand_the_burst
        a_clean_rerun_that_races_in_flight_charge_drains_it_after
        a_normal_pass_held_across_a_burst_keeps_its_pressure_wake
        a_pressure_pass_skips_the_daily_rollup
        the_normal_deadline_rolls_up_under_held_pressure)
      EXPECT=("$lib::a_refusal_before_the_wal_exists_does_not_strand_the_burst|the burst drains"
        "$lib::a_clean_rerun_that_races_in_flight_charge_drains_it_after|the in-flight group drains"
        "$lib::a_normal_pass_held_across_a_burst_keeps_its_pressure_wake|the burst's pressure pass"
        "$lib::a_pressure_pass_skips_the_daily_rollup|the insert's pressure pass"
        "$lib::the_normal_deadline_rolls_up_under_held_pressure|the insert's pressure pass")
      ;;
    # The refused request is this test's only wake. Its prompt kill, in
    # refusals_against_in_flight_charge_wait_for_an_insert, is
    # crates/trawl-server/mutations/13-refusal-does-not-wake-the-loop.patch.
    refusal-does-not-wake-the-loop.patch)
      FILTER=(--lib a_failing_pass_that_ends_open_still_cools_down)
      EXPECT=("$lib::a_failing_pass_that_ends_open_still_cools_down|the refusal's pass")
      ;;
    m6-late-failure-replaces-timeout.patch)
      FILTER=(--test query_resources timeout_verdict_survives_resource_failure)
      EXPECT=("timeout_verdict_survives_resource_failure|the request answers")
      ;;
    *) return 1 ;;
  esac
}

# The reporter flags fix the output all_died_by_the_guard parses, so a
# NEXTEST_* or CARGO_TERM_COLOR setting in the caller's environment cannot
# move a failure's output past the summary, colour its status lines, or add
# a JSON copy of each failure's message.
run_filter() {
  cargo nextest run -p trawl-server --no-fail-fast --no-tests=fail --retries 0 \
    --color never --show-progress none --status-level pass --final-status-level fail \
    --failure-output immediate --success-output never --message-format human "${FILTER[@]}"
}

# Is the patched run in log a kill? Every selected test ran and failed,
# and the guard panics, paired with the thread that raised each, are
# exactly EXPECT. Only the lines before the final summary count: nextest
# repeats every failure there.
all_died_by_the_guard() {
  local log="$1" expected=${#EXPECT[@]} ran failed observed want
  ran="$(grep -cE '^ +(PASS|FAIL|SIG[A-Z]+|TIMEOUT|ABORT) +\[' <(sed '/^ *Summary \[/q' "$log"))" || true
  failed="$(grep -cE '^ +(FAIL|SIG[A-Z]+|TIMEOUT|ABORT) +\[' <(sed '/^ *Summary \[/q' "$log"))" || true
  observed="$(sed '/^ *Summary \[/q' "$log" | awk -v q="'" '
    BEGIN { head = "^ *thread " q "[^" q "]+" q }
    match($0, head) {
      thread = substr($0, RSTART, RLENGTH); sub("^ *thread " q, "", thread); sub(q "$", "", thread)
      next
    }
    thread != "" && sub(/^ *timed out after 120s waiting for: /, "") { print thread "|" $0 }
    { thread = "" }' | LC_ALL=C sort)"
  want="$(printf '%s\n' "${EXPECT[@]}" | LC_ALL=C sort)"
  echo "  ran $ran, failed $failed, guard panics $(grep -c . <<<"$observed" || true), selected $expected"
  [[ $ran -eq $expected && $failed -eq $expected && $observed == "$want" ]]
}

PATCHES=("$@")
if [[ ${#PATCHES[@]} -eq 0 ]]; then
  PATCHES=(m2-open-plan-swallows-pressure-generation.patch m7-insert-does-not-wake-the-loop.patch
    refusal-does-not-wake-the-loop.patch m6-late-failure-replaces-timeout.patch)
fi

for name in "${PATCHES[@]}"; do
  if [[ ! -f "$SCRIPT_DIR/$name" ]] || ! filter_for "$name"; then
    echo "hang-mutations: no such patch or no filter: $name" >&2
    exit 1
  fi
done

cleanup_patch() {
  local patch_path="$1"
  if git apply --check -R "$patch_path" 2>/dev/null; then
    git apply -R "$patch_path"
  fi
}

declare -A RESULT
for name in "${PATCHES[@]}"; do
  patch_path="$SCRIPT_DIR/$name"
  filter_for "$name"
  echo "=== control for $name: ${FILTER[*]} ==="
  run_filter

  echo "=== $name ==="
  git apply --check "$patch_path"
  trap 'cleanup_patch "$patch_path"' EXIT
  trap 'cleanup_patch "$patch_path"; exit 130' INT TERM
  git apply "$patch_path"
  log="$(mktemp)"
  set +e
  run_filter 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  set -e
  cleanup_patch "$patch_path"
  trap - EXIT INT TERM
  case $status in
    100)
      if all_died_by_the_guard "$log"; then
        RESULT[$name]="PASS (killed: every selected test died by the hang guard at its expected wait)"
      else
        RESULT[$name]="FAIL (nextest exit 100, but not every selected test died by the hang guard at its expected wait)"
      fi
      ;;
    0) RESULT[$name]="FAIL (survived: every selected test passed)" ;;
    *) RESULT[$name]="ERROR (nextest exit $status: not a kill)" ;;
  esac
  rm -f "$log"
done

echo
echo "hang-mutations results:"
overall=0
for name in "${PATCHES[@]}"; do
  printf '%-48s %s\n' "$name" "${RESULT[$name]}"
  [[ ${RESULT[$name]} == PASS* ]] || overall=1
done

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "hang-mutations: tracked files differ from HEAD after cleanup; a mutation may still be applied:" >&2
  git status --short --untracked-files=no >&2
  exit 2
fi

exit $overall
