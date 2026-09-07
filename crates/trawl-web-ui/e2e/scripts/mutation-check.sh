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
#   e2e/scripts/mutation-check.sh                 # run all five
#   e2e/scripts/mutation-check.sh 02-editor-onchange.patch   # just one
#
# Refuses to run against a dirty tree — a patch applied on top of your
# own uncommitted work can't be cleanly reverted, and a failed `git
# apply -R` would otherwise silently leave a mutation live in your working
# tree.
#
# Known residual: a transient failure confined to the target spec (a
# flake that clears before the control runs) still reads as a kill. A
# PASS here is evidence only NEXT TO a green baseline run of the full
# suite on the same commit — which is exactly what CI enforces.

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
  [05-search-url-codec.patch]="search-url.spec.ts"
)

# patch-file -> a CONTROL spec the mutation does NOT touch, which must
# PASS in the same run environment. This is what tells a genuine kill
# from an infra failure: a missing browser or dead stub fails the control
# too, and playwright even records launch failures as executed-and-failed
# tests, so neither exit codes nor the JSON reporter's stats can make
# that call on the target spec alone (probed: an empty
# PLAYWRIGHT_BROWSERS_PATH yields "3 tests failed", not "no tests").
declare -A CONTROL_FOR=(
  [01-route.patch]="api-failure.spec.ts"
  [02-editor-onchange.patch]="routing.spec.ts"
  [03-sse-teardown.patch]="routing.spec.ts"
  [04-error-fallback.patch]="routing.spec.ts"
  [05-search-url-codec.patch]="routing.spec.ts"
)

PATCHES=()
if [[ $# -gt 0 ]]; then
  PATCHES=("$@")
else
  PATCHES=(
    01-route.patch
    02-editor-onchange.patch
    03-sse-teardown.patch
    04-error-fallback.patch
    05-search-url-codec.patch
  )
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
  # EXIT/INT/TERM: the apply→revert window spans a trunk build plus a
  # Playwright run, and neither a fatal error (set -e exits → EXIT trap)
  # nor a Ctrl-C may strand a live mutation. No ERR trap: bash fires ERR
  # even inside set +e blocks, which would revert the patch mid-iteration
  # the moment the target spec fails as expected.
  trap 'cleanup_patch "$patch_path"' EXIT
  trap 'cleanup_patch "$patch_path"; exit 130' INT TERM

  if ! git apply "$patch_path"; then
    echo "mutation-check: failed to apply $name" >&2
    RESULT[$name]="APPLY-FAILED"
    trap - EXIT INT TERM
    continue
  fi

  echo "-- trunk build --"
  (cd "$WEB_UI_DIR" && trunk build) || {
    echo "mutation-check: trunk build failed for $name (unexpected — a Rust-level breakage, not a browser-observable one)" >&2
    cleanup_patch "$patch_path"
    RESULT[$name]="BUILD-FAILED"
    trap - EXIT INT TERM
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
    trap - EXIT INT TERM
    continue
  fi

  set +e
  (cd "$E2E_DIR" && npx playwright test "tests/$spec")
  status=$?
  set -e

  control="${CONTROL_FOR[$name]}"
  echo "-- control: playwright test tests/$control (must pass) --"
  set +e
  (cd "$E2E_DIR" && npx playwright test "tests/$control")
  control_status=$?
  set -e

  if [[ $control_status -ne 0 ]]; then
    RESULT[$name]="INFRA-FAILED (control spec $control failed — the environment, not the mutation, is broken)"
  elif [[ $status -ne 0 ]]; then
    RESULT[$name]="PASS (target spec failed, control passed, exit $status)"
  else
    RESULT[$name]="FAIL (suite passed despite the mutation — mechanism didn't catch it)"
  fi

  cleanup_patch "$patch_path"
  trap - EXIT INT TERM
done

# dist/ is gitignored, so the clean-tree check below can't see it — and it
# still holds the LAST mutant's trunk build. Rebuild from the now-reverted
# sources so a later `npm run test` exercises the real SPA, not a mutant.
echo "-- trunk build (restore pristine dist) --"
(cd "$WEB_UI_DIR" && trunk build) || {
  echo "mutation-check: pristine rebuild failed — dist/ may still hold a mutant build" >&2
  exit 2
}

echo
echo "mutation-check results:"
printf '%-28s %s\n' "patch" "outcome"
overall=0
for name in "${PATCHES[@]}"; do
  outcome="${RESULT[$name]:-not run}"
  printf '%-28s %s\n' "$name" "$outcome"
  [[ $outcome == PASS* ]] || overall=1
done
# A cleanup that could not cleanly reverse its patch (e.g. an affected
# file changed underneath it) is silent inside cleanup_patch — refuse to
# report success over a tree that still carries a mutation.
if [[ -n "$(git status --porcelain)" ]]; then
  echo "mutation-check: working tree is NOT clean after cleanup — a mutation may still be applied:" >&2
  git status --short >&2
  exit 2
fi

# Exit 0 only when every requested mutation was killed by its own spec —
# an APPLY/BUILD/INFRA failure or a surviving mutation must fail the
# command, not just color a table.
exit $overall
