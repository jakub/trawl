#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #316, acceptance criterion 2. Each failure in
# the issue's Context table is reproduced by one delay patch in repro/,
# which puts the starvation that made the test fail at the point where the
# scheduler could deschedule the code. This script applies each patch to
# the base SHA (main before the fix) and to the head SHA (the fix), runs
# the test it reproduces, and classifies each run:
#
#   passed                   nextest exit 0
#   expected named failure   nextest exit 100 (tests ran, one failed) AND
#                            the output carries every needle of the row,
#                            the assertion text the issue names
#   unknown                  anything else: a build failure (101), no test
#                            matched (4), another assertion, a patch that
#                            does not apply
#
# Pass rule: every base run is `expected named failure` and every head run
# is `passed`.
#
# A control run of each row's filter on the unpatched tree comes first, on
# both sides. It is not part of the pass rule, but a base control that
# fails means that row's patched failure proves nothing about the patch.
#
# Usage:
#   docs/evidence/issue-316/repro.sh [BASE_SHA [HEAD_SHA]]
#     BASE_SHA defaults to 505e2e14, HEAD_SHA to HEAD.
#
# Environment:
#   REPRO_WORK_DIR    where the detached worktrees and run logs go
#                     (default: <repo>/target/issue-316-repro). Must not be
#                     on tmpfs: two trawl-server test builds do not fit.
#   CARGO_TARGET_DIR  shared by both worktrees (default: <repo>/target), so
#                     dependencies build once. Must not be on tmpfs either.
#   REPRO_TRANSCRIPT  the transcript path
#                     (default: docs/evidence/issue-316/reproductions.txt).
#   REPRO_ROWS        space-separated row ids to run instead of every row,
#                     for debugging one patch. The transcript says so, and
#                     only a run of every row is the evidence.
#   DATABASE_URL      a Postgres role that can create databases, as for the
#                     test suite: the repin, results_sweep and
#                     query_resources tests boot real servers.
#
# Refuses to run against a dirty tree: the transcript names patch digests
# and SHAs, and an uncommitted patch or script would make them describe
# something no commit holds. The worktrees are removed on exit, so no
# patch outlives the run, and each patch is reverted and the worktree
# checked clean before the next one is applied.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
PATCH_DIR="$SCRIPT_DIR/repro"

cd "$ROOT_DIR"

BASE_REV="${1:-505e2e14}"
HEAD_REV="${2:-HEAD}"
WORK_DIR="${REPRO_WORK_DIR:-$ROOT_DIR/target/issue-316-repro}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
TRANSCRIPT="${REPRO_TRANSCRIPT:-$SCRIPT_DIR/reproductions.txt}"

# Each patch and each revert rebuilds trawl-server and one test binary, so
# incremental compilation pays for itself, as in mutation-check.sh.
export CARGO_INCREMENTAL=1

if [[ -n "$(git status --porcelain)" ]]; then
  echo "repro: working tree is dirty; commit or stash first." >&2
  git status --short >&2
  exit 1
fi

if [[ -z "${DATABASE_URL:-}" ]]; then
  echo "repro: DATABASE_URL is not set; the repin, results_sweep and query_resources rows need it" >&2
  exit 1
fi

BASE_SHA="$(git rev-parse --verify "$BASE_REV^{commit}")"
HEAD_SHA="$(git rev-parse --verify "$HEAD_REV^{commit}")"

mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "repro: $dir is on tmpfs; point it at a disk-backed directory" >&2
    exit 1
  fi
done

# row id -> patch for each side, the test binary, the nextest filterset for
# each side, and the needles a base failure must carry. A row whose test
# the fix split names both successors in its head filter.
ROWS=(
  1-tcp-stall
  2-repin-gauge
  3a-failing-pass-cools-down
  3b-refusal-before-wal
  3c-clean-rerun-in-flight
  3d-normal-pass-held-burst
  3e-undeletable-wal
  3f-pressure-pass-rollup
  3g-refusals-wait-for-insert
  4-timeout-verdict
  5-publication-wait
  6-repin-aside-root
  7-results-sweep-unlink
)

if [[ -n "${REPRO_ROWS:-}" ]]; then
  read -r -a ROWS <<<"$REPRO_ROWS"
fi

BASE_PATCH="" HEAD_PATCH="" BINARY=() BASE_FILTER="" HEAD_FILTER="" NEEDLES=()
row_spec() {
  local compaction='ingest::compaction::tests::'
  case "$1" in
    1-tcp-stall)
      BASE_PATCH=tcp-test-deschedule.base.patch
      HEAD_PATCH=tcp-test-deschedule.head.patch
      BINARY=(--lib)
      BASE_FILTER='test(=syslog::tcp::tests::tcp_stalls_without_disconnect_and_ingests_each_frame_once_after_drain)'
      NEEDLES=('still open after the drain')
      ;;
    2-repin-gauge)
      BASE_PATCH=repin-gauge-reset-delay.patch
      BINARY=(--test repin)
      BASE_FILTER='test(=ingest_queries_and_a_second_repin_ride_through_a_slow_rewrite)'
      NEEDLES=('the running gauge must fall back to 0 at the terminal state' 'left: Some(1.0)' 'right: Some(0.0)')
      ;;
    3a-failing-pass-cools-down)
      BASE_PATCH=pipeline-slow-wal-write.patch
      BINARY=(--lib)
      BASE_FILTER="test(=${compaction}a_failing_pass_that_ends_open_still_cools_down)"
      NEEDLES=('the large request kept retrying')
      ;;
    3b-refusal-before-wal)
      BASE_PATCH=compaction-slow-conversion.patch
      BINARY=(--lib)
      BASE_FILTER="test(=${compaction}a_refusal_before_the_wal_exists_does_not_strand_the_burst)"
      NEEDLES=('the burst stayed resident for 2s')
      ;;
    3c-clean-rerun-in-flight)
      BASE_PATCH=compaction-slow-conversion.patch
      BINARY=(--lib)
      BASE_FILTER="test(=${compaction}a_clean_rerun_that_races_in_flight_charge_drains_it_after)"
      NEEDLES=('the in-flight group stayed resident for 2s')
      ;;
    3d-normal-pass-held-burst)
      BASE_PATCH=compaction-slow-conversion.patch
      BINARY=(--lib)
      BASE_FILTER="test(=${compaction}a_normal_pass_held_across_a_burst_keeps_its_pressure_wake)"
      NEEDLES=('the burst stayed resident' 'after the cutover ended')
      ;;
    3e-undeletable-wal)
      BASE_PATCH=publication-arrival-delay.patch
      BINARY=(--lib)
      BASE_FILTER="test(=${compaction}compaction_undeletable_wal_never_duplicates)"
      NEEDLES=('called `Result::unwrap()` on an `Err` value: Timeout')
      ;;
    3f-pressure-pass-rollup)
      BASE_PATCH=compaction-slow-conversion.patch
      BINARY=(--lib)
      BASE_FILTER="test(=${compaction}pressure_pass_skips_rollup_and_the_normal_deadline_still_rolls_up)"
      HEAD_FILTER="test(=${compaction}a_pressure_pass_skips_the_daily_rollup) | test(=${compaction}the_normal_deadline_rolls_up_under_held_pressure)"
      NEEDLES=('the insert under pressure woke a pass')
      ;;
    3g-refusals-wait-for-insert)
      BASE_PATCH=pipeline-refusal-deschedule.patch
      BINARY=(--lib)
      BASE_FILTER="test(=${compaction}refusals_against_in_flight_charge_wait_for_an_insert)"
      NEEDLES=('refusals kept coming')
      ;;
    4-timeout-verdict)
      BASE_PATCH=pool-finished-arrival-delay.patch
      BINARY=(--test query_resources)
      BASE_FILTER='test(=timeout_verdict_survives_resource_failure)'
      NEEDLES=('the work failed before the deadline' 'left: 0')
      ;;
    5-publication-wait)
      BASE_PATCH=publication-arrival-delay.patch
      BINARY=(--test publication_consistency)
      BASE_FILTER='test(=queries_and_exports_wait_for_publish_and_drain)'
      NEEDLES=('called `Result::unwrap()` on an `Err` value: Timeout')
      ;;
    6-repin-aside-root)
      BASE_PATCH=repin-post-swap-sweep-delay.patch
      BINARY=(--test repin)
      BASE_FILTER='test(=repin_is_invisible_to_queries_and_resurrects_shelved_values)'
      NEEDLES=('assertion failed: !trawl_server::repin::aside_root(&h.data_dir).exists()')
      ;;
    7-results-sweep-unlink)
      BASE_PATCH=results-sweep-unlink-delay.patch
      BINARY=(--test results_sweep)
      BASE_FILTER='test(=the_scheduler_s_retention_tick_sweeps_captured_results)'
      NEEDLES=('assertion failed: !exists(&results.root, &id(90))')
      ;;
    *) return 1 ;;
  esac
}

# Fill in the per-side defaults: a row names a head patch or filter only
# where it differs from the base one.
row_for() {
  BASE_PATCH="" HEAD_PATCH="" BINARY=() BASE_FILTER="" HEAD_FILTER="" NEEDLES=()
  row_spec "$1" || return 1
  HEAD_PATCH="${HEAD_PATCH:-$BASE_PATCH}"
  HEAD_FILTER="${HEAD_FILTER:-$BASE_FILTER}"
}

# Every row needs a spec and its patches before anything runs.
for row in "${ROWS[@]}"; do
  if ! row_for "$row"; then
    echo "repro: no such row: $row" >&2
    exit 1
  fi
  for name in "$BASE_PATCH" "$HEAD_PATCH"; do
    if [[ ! -f "$PATCH_DIR/$name" ]]; then
      echo "repro: row $row names a missing patch: $name" >&2
      exit 1
    fi
  done
done

WORKTREES=()
APPLIED_WT="" APPLIED_PATCH=""

revert_applied() {
  if [[ -n "$APPLIED_PATCH" ]]; then
    git -C "$APPLIED_WT" apply -R "$APPLIED_PATCH" 2>/dev/null || true
    APPLIED_WT="" APPLIED_PATCH=""
  fi
}

cleanup() {
  revert_applied
  local wt
  for wt in "${WORKTREES[@]}"; do
    git -C "$ROOT_DIR" worktree remove --force "$wt" 2>/dev/null || true
  done
  git -C "$ROOT_DIR" worktree prune
}

# EXIT/INT/TERM: neither a fatal error nor a Ctrl-C may strand a patch or
# a worktree. No ERR trap: it would fire on the failing run a base row
# needs.
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

# Sets WT. Not a command substitution: WORKTREES must reach the trap.
WT=""
make_worktree() {
  local side="$1" sha="$2"
  WT="$WORK_DIR/$side"
  if [[ -e "$WT" ]]; then
    git -C "$ROOT_DIR" worktree remove --force "$WT" 2>/dev/null || rm -rf "$WT"
    git -C "$ROOT_DIR" worktree prune
  fi
  WORKTREES+=("$WT")
  git -C "$ROOT_DIR" worktree add --detach "$WT" "$sha" >/dev/null
}

require_clean() {
  local wt="$1"
  if [[ -n "$(git -C "$wt" status --porcelain)" ]]; then
    echo "repro: worktree $wt is not clean; a patch may still be applied:" >&2
    git -C "$wt" status --short >&2
    exit 2
  fi
}

LOG_DIR="$WORK_DIR/logs"
rm -rf "$LOG_DIR"
mkdir -p "$LOG_DIR"

# Run one row's filter in a worktree; prints the nextest exit status.
run_filter() {
  local wt="$1" filter="$2" log="$3"
  local status
  set +e
  (cd "$wt" && cargo nextest run -p trawl-server --retries 0 --no-fail-fast \
    --no-tests=fail "${BINARY[@]}" -E "$filter") >"$log" 2>&1
  status=$?
  set -e
  echo "$status"
}

# Classify a run from its exit status and log.
classify() {
  local status="$1" log="$2" needle
  if [[ $status -eq 0 ]]; then
    echo "passed"
    return
  fi
  if [[ $status -eq 100 ]]; then
    for needle in "${NEEDLES[@]}"; do
      if ! grep -qF -- "$needle" "$log"; then
        echo "unknown (exit 100 without: $needle)"
        return
      fi
    done
    echo "expected named failure"
    return
  fi
  echo "unknown (nextest exit $status)"
}

sha_of() { sha256sum "$1" | cut -d' ' -f1; }

OUT="$(mktemp "$WORK_DIR/transcript.XXXXXX")"
say() { printf '%s\n' "$*" | tee -a "$OUT"; }

say "# issue #316 reproductions (acceptance criterion 2)"
say "script:        docs/evidence/issue-316/repro.sh"
say "script commit: $(git rev-parse HEAD)"
say "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "base SHA:      $BASE_SHA"
say "head SHA:      $HEAD_SHA"
say "uid:           $(id -u)"
say "nextest:       $(cargo nextest --version | head -1)"
say "rustc:         $(rustc --version)"
say "load average:  $(cut -d' ' -f1-3 /proc/loadavg)"
say "rows:          ${REPRO_ROWS:-all}"
say ""
say "## patches (sha256)"
for path in "$PATCH_DIR"/*.patch; do
  say "$(sha_of "$path")  repro/$(basename "$path")"
done
say ""

declare -A OUTCOME CONTROL

# One side at a time, one patch at a time: rows sharing a patch run under
# one application of it, so each patch costs one rebuild per side.
run_side() {
  local side="$1" sha="$2" wt patch row filter patch_name log status outcome
  make_worktree "$side" "$sha"
  wt="$WT"
  require_clean "$wt"
  say "## $side $sha"
  say ""

  say "### control: every row's filter on the unpatched tree"
  for row in "${ROWS[@]}"; do
    row_for "$row"
    if [[ $side == base ]]; then filter="$BASE_FILTER"; else filter="$HEAD_FILTER"; fi
    log="$LOG_DIR/$side-control-$row.log"
    status="$(run_filter "$wt" "$filter" "$log")"
    outcome="$(classify "$status" "$log")"
    CONTROL[$side/$row]="$outcome"
    say "$row: $outcome (exit $status)"
  done
  say ""

  local -A seen=()
  local patches=()
  for row in "${ROWS[@]}"; do
    row_for "$row"
    if [[ $side == base ]]; then patch_name="$BASE_PATCH"; else patch_name="$HEAD_PATCH"; fi
    if [[ -z "${seen[$patch_name]:-}" ]]; then
      seen[$patch_name]=1
      patches+=("$patch_name")
    fi
  done

  for patch_name in "${patches[@]}"; do
    patch="$PATCH_DIR/$patch_name"
    say "### $patch_name (sha256 $(sha_of "$patch"))"
    require_clean "$wt"
    if ! git -C "$wt" apply --check "$patch" 2>>"$OUT"; then
      for row in "${ROWS[@]}"; do
        row_for "$row"
        if [[ $side == base && $BASE_PATCH == "$patch_name" ]] \
          || [[ $side == head && $HEAD_PATCH == "$patch_name" ]]; then
          OUTCOME[$side/$row]="unknown (patch does not apply)"
          say "$row: unknown (patch does not apply)"
        fi
      done
      continue
    fi
    git -C "$wt" apply "$patch"
    APPLIED_WT="$wt" APPLIED_PATCH="$patch"

    for row in "${ROWS[@]}"; do
      row_for "$row"
      if [[ $side == base ]]; then
        [[ $BASE_PATCH == "$patch_name" ]] || continue
        filter="$BASE_FILTER"
      else
        [[ $HEAD_PATCH == "$patch_name" ]] || continue
        filter="$HEAD_FILTER"
      fi
      log="$LOG_DIR/$side-$row.log"
      local started ended
      started="$(date +%s)"
      status="$(run_filter "$wt" "$filter" "$log")"
      ended="$(date +%s)"
      outcome="$(classify "$status" "$log")"
      OUTCOME[$side/$row]="$outcome"
      say "-- $row"
      say "   filter:  ${BINARY[*]} -E '$filter'"
      say "   exit:    $status after $((ended - started))s"
      say "   outcome: $outcome"
      # The per-test lines nextest printed, and for a failure the lines
      # carrying the named assertion, so the transcript shows which test
      # ran and what it said.
      grep -E '^\s+(PASS|FAIL|SIGABRT|SIGSEGV|TIMEOUT|LEAK)' "$log" | sed 's/^ */   nextest: /' | tee -a "$OUT" || true
      if [[ $status -ne 0 ]]; then
        local needle
        for needle in "${NEEDLES[@]}"; do
          grep -F -- "$needle" "$log" | head -2 | sed 's/^/   saw:     /' | tee -a "$OUT" || true
        done
        grep -E "panicked at" "$log" | head -3 | sed 's/^/   panic:   /' | tee -a "$OUT" || true
      fi
    done

    git -C "$wt" apply -R "$patch"
    APPLIED_WT="" APPLIED_PATCH=""
    require_clean "$wt"
    say ""
  done
}

run_side base "$BASE_SHA"
run_side head "$HEAD_SHA"

say "## summary"
printf '%-30s %-26s %-26s %s\n' row base head verdict | tee -a "$OUT"
overall=0
for row in "${ROWS[@]}"; do
  base="${OUTCOME[base/$row]:-not run}"
  head="${OUTCOME[head/$row]:-not run}"
  verdict=PASS
  if [[ $base != "expected named failure" || $head != "passed" ]]; then
    verdict=FAIL
    overall=1
  fi
  printf '%-30s %-26s %-26s %s\n' "$row" "$base" "$head" "$verdict" | tee -a "$OUT"
done
for row in "${ROWS[@]}"; do
  for side in base head; do
    if [[ ${CONTROL[$side/$row]:-not run} != passed ]]; then
      say "note: the $side control for $row was not a pass: ${CONTROL[$side/$row]:-not run}"
    fi
  done
done
say "finished:      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "load average:  $(cut -d' ' -f1-3 /proc/loadavg)"
say "pass rule (every base run expected named failure, every head run passed): $([[ $overall -eq 0 ]] && echo met || echo NOT met)"

mv "$OUT" "$TRANSCRIPT"
echo "repro: transcript written to $TRANSCRIPT"
exit $overall
