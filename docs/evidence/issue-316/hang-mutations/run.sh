#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Runs the hang-guard-only mutations of #316 once each. These patches
# remove a guard whose tests can only fail by the 120 s hang guard in
# test_timing, because cadence time is frozen and nothing else can end the
# wait. They stay out of crates/trawl-server/mutations/ so the CI
# `server-mutations` job never pays two minutes per patch.
#
# For each patch the script runs its tests on the pristine tree (the
# control, which must pass), applies the patch, runs them again with
# `--retries 0`, and reverts the patch. Expected result: nextest exit 100
# and `timed out after 120s waiting for: ...` from every listed test.
#
# Usage:
#   docs/evidence/issue-316/hang-mutations/run.sh              # both patches
#   docs/evidence/issue-316/hang-mutations/run.sh m7-insert-does-not-wake-the-loop.patch
#
# DATABASE_URL is not needed: these tests start no Postgres.
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

FILTER=()
filter_for() {
  case "$1" in
    m2-open-plan-swallows-pressure-generation.patch)
      FILTER=(--lib a_normal_pass_held_across_a_burst_keeps_its_pressure_wake)
      ;;
    m7-insert-does-not-wake-the-loop.patch)
      FILTER=(--lib a_refusal_before_the_wal_exists_does_not_strand_the_burst
        a_clean_rerun_that_races_in_flight_charge_drains_it_after)
      ;;
    *) return 1 ;;
  esac
}

run_filter() {
  cargo nextest run -p trawl-server --no-fail-fast --no-tests=fail --retries 0 "${FILTER[@]}"
}

PATCHES=("$@")
if [[ ${#PATCHES[@]} -eq 0 ]]; then
  PATCHES=(m2-open-plan-swallows-pressure-generation.patch m7-insert-does-not-wake-the-loop.patch)
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
  set +e
  run_filter
  status=$?
  set -e
  cleanup_patch "$patch_path"
  trap - EXIT INT TERM
  RESULT[$name]="nextest exit $status"
done

echo
echo "hang-mutations results (100 = killed):"
for name in "${PATCHES[@]}"; do
  printf '%-48s %s\n' "$name" "${RESULT[$name]}"
done

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "hang-mutations: tracked files differ from HEAD after cleanup; a mutation may still be applied:" >&2
  git status --short --untracked-files=no >&2
  exit 2
fi
