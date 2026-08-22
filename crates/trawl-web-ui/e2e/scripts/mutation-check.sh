#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling, not CI: for each mutations/*.patch, apply it, rebuild
# the SPA, run the spec file that is that mutation's subject, and require
# at least one test to EXECUTE AND FAIL (exit code alone can't tell a
# kill from a missing browser), then revert. Prints a PASS/FAIL table and
# exits 0 only if every requested mutation was killed.
#
# Usage:
#   e2e/scripts/mutation-check.sh                 # run all four
#   e2e/scripts/mutation-check.sh 02-editor-onchange.patch   # just one
#
# Refuses to run against a dirty tree — a patch applied on top of your
# own uncommitted work can't be cleanly reverted, and a failed `git
# apply -R` would otherwise silently leave a mutation live in your working
# tree.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WEB_UI_DIR="$(cd "$E2E_DIR/.." && pwd)"
ROOT_DIR="$(cd "$WEB_UI_DIR/../.." && pwd)"
MUTATIONS_DIR="$E2E_DIR/mutations"

cd "$ROOT_DIR"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "mutation-check: working tree is dirty — commit or stash first." >&2
  git status --short >&2
  exit 1
fi

# patch-file -> the spec FILE whose subject the mutation breaks (keeping
# this narrow means an unrelated spec failure doesn't get miscounted as
# this mutation's signal).
declare -A SPEC_FOR=(
  [01-route.patch]="routing.spec.ts"
  [02-editor-onchange.patch]="editor-input.spec.ts"
  [03-sse-teardown.patch]="teardown-sse.spec.ts"
  [04-error-fallback.patch]="api-failure.spec.ts"
)

PATCHES=()
if [[ $# -gt 0 ]]; then
  PATCHES=("$@")
else
  PATCHES=(01-route.patch 02-editor-onchange.patch 03-sse-teardown.patch 04-error-fallback.patch)
fi

declare -A RESULT

cleanup_patch() {
  local patch_path="$1"
  if git apply --check -R "$patch_path" 2>/dev/null; then
    git apply -R "$patch_path"
  fi
}

for name in "${PATCHES[@]}"; do
  patch_path="$MUTATIONS_DIR/$name"
  spec="${SPEC_FOR[$name]:-}"
  if [[ -z "$spec" ]]; then
    echo "mutation-check: no spec mapping for $name, skipping" >&2
    continue
  fi

  echo "=== $name -> $spec ==="
  # INT/TERM/EXIT too: the apply→revert window spans a trunk build plus a
  # Playwright run, and a Ctrl-C in it must not strand a live mutation.
  # Signals get their own handler because a bare-cleanup trap would let
  # the loop CONTINUE past the interrupt (and count the 130 as a kill).
  trap 'cleanup_patch "$patch_path"' ERR EXIT
  trap 'cleanup_patch "$patch_path"; exit 130' INT TERM

  if ! git apply "$patch_path"; then
    echo "mutation-check: failed to apply $name" >&2
    RESULT[$name]="APPLY-FAILED"
    trap - ERR EXIT INT TERM
    continue
  fi

  echo "-- trunk build --"
  (cd "$WEB_UI_DIR" && trunk build) || {
    echo "mutation-check: trunk build failed for $name (unexpected — a Rust-level breakage, not a browser-observable one)" >&2
    cleanup_patch "$patch_path"
    RESULT[$name]="BUILD-FAILED"
    trap - ERR EXIT INT TERM
    continue
  }

  # Select the spec by FILE, not --grep: a renamed spec would make a grep
  # match nothing, and playwright's "no tests found" nonzero exit would
  # read as a successful kill. Prove the file resolves to at least one
  # test first (a spec file may legitimately hold several — the mutation
  # kills if any of them fails), so an infra failure (missing browser,
  # port collision, bad path) can't masquerade as one either.
  echo "-- playwright test tests/$spec --"
  set +e
  listed=$(cd "$E2E_DIR" && npx playwright test "tests/$spec" --list 2>&1)
  list_status=$?
  set -e
  if [[ $list_status -ne 0 ]] || ! grep -Eq 'Total: [1-9][0-9]* tests?' <<<"$listed"; then
    echo "mutation-check: tests/$spec did not resolve to any tests — infra/mapping failure, not a kill" >&2
    echo "$listed" >&2
    cleanup_patch "$patch_path"
    RESULT[$name]="INFRA-FAILED (spec resolved to no tests)"
    trap - ERR EXIT INT TERM
    continue
  fi

  # A kill is proven by an EXECUTED test that FAILED, not by exit code
  # alone — a missing browser or port collision also exits nonzero. The
  # JSON reporter's stats distinguish them: `unexpected` counts tests
  # that ran and failed.
  report="$E2E_DIR/test-results/mutation-report.json"
  rm -f "$report"
  set +e
  (cd "$E2E_DIR" && PLAYWRIGHT_JSON_OUTPUT_NAME="$report" \
    npx playwright test "tests/$spec" --reporter=json > /dev/null)
  status=$?
  set -e

  unexpected=$(node -e "
    const r = require(process.argv[1]);
    console.log(r.stats ? r.stats.unexpected : 'no-stats');
  " "$report" 2>/dev/null || echo "no-report")

  if [[ $status -eq 0 ]]; then
    RESULT[$name]="FAIL (suite passed despite the mutation — mechanism didn't catch it)"
  elif [[ $unexpected =~ ^[1-9][0-9]*$ ]]; then
    RESULT[$name]="PASS (suite correctly failed: $unexpected test(s) executed and failed)"
  else
    RESULT[$name]="INFRA-FAILED (nonzero exit but no executed-and-failed test — unexpected=$unexpected)"
  fi

  cleanup_patch "$patch_path"
  trap - ERR EXIT INT TERM
done

echo
echo "mutation-check results:"
printf '%-28s %s\n' "patch" "outcome"
overall=0
for name in "${PATCHES[@]}"; do
  outcome="${RESULT[$name]:-not run}"
  printf '%-28s %s\n' "$name" "$outcome"
  [[ $outcome == PASS* ]] || overall=1
done
# Exit 0 only when every requested mutation was killed by its own spec —
# an APPLY/BUILD/INFRA failure or a surviving mutation must fail the
# command, not just color a table.
exit $overall
