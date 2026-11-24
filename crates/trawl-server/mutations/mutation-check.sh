#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# A kill is nextest exit 100 (tests ran and at least one failed). Any
# other non-zero exit is a job failure, not a kill: 101 is a build
# failure and 4 is a filter that matched no test. Exit 0 means the
# mutation survived.
#
# Usage:
#   crates/trawl-server/mutations/mutation-check.sh               # every patch
#   crates/trawl-server/mutations/mutation-check.sh 02-nets-no-preclaim.patch
#
# The nets filter boots real servers, so DATABASE_URL must name a
# Postgres role that can create databases, as for the test suite.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
MUTATIONS_DIR="$SCRIPT_DIR"

cd "$ROOT_DIR"

# CI needs CARGO_INCREMENTAL set explicitly, because rust-cache exports
# CARGO_INCREMENTAL=0 for the whole job.
export CARGO_INCREMENTAL=1

if [[ -n "$(git status --porcelain)" ]]; then
  echo "mutation-check: working tree is dirty; commit or stash first." >&2
  git status --short >&2
  exit 1
fi

# patch -> the nextest arguments that select the tests its guard exists
# for. Narrow on purpose: an unrelated failure must not count as a kill.
# Every test these names match today is in the binary named here, and
# naming the binary keeps each mutant from rebuilding the whole package.
FILTER=()
filter_for() {
  case "$1" in
    01-coverage-proof-no-recheck.patch) FILTER=(--lib coverage_proof) ;;
    02-nets-no-preclaim.patch) FILTER=(--test restart_overhang nets_) ;;
    03-snapshot-none-fallback.patch) FILTER=(--lib snapshot_failure) ;;
    # Each filter names one test, and the test fails on its own assertion,
    # never on the 120 s hang guard.
    04-cadence-ignores-persistent-failures.patch)
      FILTER=(--lib a_failing_pass_that_ends_open_still_cools_down)
      ;;
    05-pressure-pass-plans-rollup.patch)
      FILTER=(--lib a_pressure_pass_skips_the_daily_rollup)
      ;;
    06-pressure-moves-normal-deadline.patch)
      FILTER=(--lib the_normal_deadline_rolls_up_under_held_pressure)
      ;;
    07-tcp-send-inside-idle-timeout.patch)
      FILTER=(--lib tcp_stalls_without_disconnect_and_ingests_each_frame_once_after_drain)
      ;;
    09-sweep-skips-unlink.patch)
      FILTER=(--test results_sweep the_scheduler_s_retention_tick_sweeps_captured_results
        the_standalone_sweep_tick_expires_captures_without_the_scheduler)
      ;;
    10-repin-gauge-never-resets.patch)
      FILTER=(--test repin ingest_queries_and_a_second_repin_ride_through_a_slow_rewrite)
      ;;
    11-repin-post-swap-sweep-skipped.patch)
      FILTER=(--test repin repin_is_invisible_to_queries_and_resurrects_shelved_values)
      ;;
    12-query-lane-bypasses-publication-read.patch)
      FILTER=(--test publication_consistency queries_and_exports_wait_for_publish_and_drain)
      ;;
    13-refusal-does-not-wake-the-loop.patch)
      FILTER=(--lib refusals_against_in_flight_charge_wait_for_an_insert)
      ;;
    14-tcp-accept-error-ends-listener.patch)
      FILTER=(--lib tcp_accept_recovers)
      ;;
    15-udp-recv-error-ends-listener.patch)
      FILTER=(--lib udp_receive_recovers)
      ;;
    16-audit-exclusive-key-arms.patch)
      FILTER=(--lib first_seen_inactive_key_reports_created_then_revoked
        roles_change_and_revoke_in_one_poll_report_both)
      ;;
    17-rollup-tmp-sync-skipped.patch | 18-rollup-ancestor-chain-sync-skipped.patch | \
      19-rollup-marker-best-effort.patch | 20-rollup-rename-day-sync-skipped.patch | \
      21-rollup-hour-sync-skipped.patch | 27-rollup-footer-check-skipped.patch)
      FILTER=(--lib rollup_publish_order rollup_sync_failure_matrix)
      ;;
    22-rollup-unlink-sync-error-ignored.patch)
      FILTER=(--lib rollup_sync_failure_matrix)
      ;;
    23-removal-obligation-ends-on-absence.patch)
      FILTER=(--lib rollup_unlink_sync_retry)
      ;;
    24-recovery-observes-tmp-first.patch | 25-recovery-aborts-without-input-proof.patch | \
      28-recovery-promote-chain-sync-skipped.patch | 29-recovery-promote-day-sync-skipped.patch | \
      30-recovery-published-day-sync-skipped.patch)
      FILTER=(--lib rollup_recovery_matrix)
      ;;
    26-repin-admission-ignores-removal-obligation.patch)
      FILTER=(--lib rollup_unlink_sync_retry_refuses_repin_admission)
      ;;
    31-rollup-ignores-removal-obligation.patch)
      FILTER=(--lib ingest::compaction::tests::rollup_unlink_sync_retry)
      ;;
    32-conformance-ignores-removal-obligation.patch)
      FILTER=(--lib rollup_unlink_sync_retry_defers_conformance_of_an_owed_day)
      ;;
    33-removal-retry-ignores-deleted-day.patch)
      FILTER=(--lib rollup_unlink_sync_retry_ends_when_retention_deletes_the_day)
      ;;
    34-manual-lease-after-claim.patch)
      FILTER=(--test scheduler_windows manual_run_refusal_is_preclaim)
      ;;
    35-manual-lease-dropped-at-timeout.patch)
      FILTER=(--test scheduler_windows manual_run_lease_outlives_its_request)
      ;;
    *) return 1 ;;
  esac
}

run_filter() {
  cargo nextest run -p trawl-server --no-fail-fast --no-tests=fail "${FILTER[@]}"
}

PATCHES=()
if [[ $# -gt 0 ]]; then
  PATCHES=("$@")
else
  for path in "$MUTATIONS_DIR"/*.patch; do
    PATCHES+=("$(basename "$path")")
  done
fi

for name in "${PATCHES[@]}"; do
  if [[ ! -f "$MUTATIONS_DIR/$name" ]]; then
    echo "mutation-check: no such patch: $name" >&2
    exit 1
  fi
  if ! filter_for "$name"; then
    echo "mutation-check: no filter mapped for $name; add it to filter_for" >&2
    exit 1
  fi
done

echo "=== control: every selected filter on the pristine tree ==="
for name in "${PATCHES[@]}"; do
  filter_for "$name"
  echo "-- control for $name: ${FILTER[*]} --"
  set +e
  run_filter
  status=$?
  set -e
  if [[ $status -ne 0 ]]; then
    echo "mutation-check: control run for $name exited $status on the pristine tree; no kill can count" >&2
    exit 1
  fi
done

declare -A RESULT

cleanup_patch() {
  local patch_path="$1"
  if git apply --check -R "$patch_path" 2>/dev/null; then
    git apply -R "$patch_path"
  fi
}

for name in "${PATCHES[@]}"; do
  patch_path="$MUTATIONS_DIR/$name"
  filter_for "$name"
  echo "=== $name -> ${FILTER[*]} ==="

  if ! git apply --check "$patch_path"; then
    echo "mutation-check: $name does not apply to this tree" >&2
    RESULT[$name]="APPLY-FAILED"
    continue
  fi

  # EXIT/INT/TERM: neither a fatal error nor a Ctrl-C may strand a live
  # mutation. No ERR trap: it would fire on the failing run a kill needs.
  trap 'cleanup_patch "$patch_path"' EXIT
  trap 'cleanup_patch "$patch_path"; exit 130' INT TERM
  git apply "$patch_path"

  set +e
  run_filter
  status=$?
  set -e

  cleanup_patch "$patch_path"
  trap - EXIT INT TERM

  case $status in
    100) RESULT[$name]="PASS (killed: nextest exit 100)" ;;
    0) RESULT[$name]="FAIL (survived: every selected test passed)" ;;
    *) RESULT[$name]="ERROR (nextest exit $status: not a kill)" ;;
  esac
done

echo
echo "mutation-check results:"
printf '%-36s %s\n' "patch" "outcome"
overall=0
for name in "${PATCHES[@]}"; do
  outcome="${RESULT[$name]:-not run}"
  printf '%-36s %s\n' "$name" "$outcome"
  [[ $outcome == PASS* ]] || overall=1
done

# A revert that did not apply cleanly is silent inside cleanup_patch, so
# refuse to report anything over a tree that still differs from HEAD.
if [[ -n "$(git status --porcelain)" ]]; then
  echo "mutation-check: working tree is NOT clean after cleanup; a mutation may still be applied:" >&2
  git status --short >&2
  exit 2
fi

exit $overall
