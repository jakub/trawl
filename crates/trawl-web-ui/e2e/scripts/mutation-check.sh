#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling, not CI: for each mutations/*.patch, apply it, rebuild
# the SPA, run the ONE spec tagged as that mutation's subject, expect a
# NONZERO exit (the mutation must break something the suite catches),
# then revert. Prints a PASS/FAIL table at the end.
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

# patch-file -> spec file -> grep pattern (the one test whose subject the
# mutation breaks; keeping this narrow means an unrelated spec failure
# doesn't get miscounted as this mutation's signal).
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
  trap 'cleanup_patch "$patch_path"' ERR

  if ! git apply "$patch_path"; then
    echo "mutation-check: failed to apply $name" >&2
    RESULT[$name]="APPLY-FAILED"
    trap - ERR
    continue
  fi

  echo "-- trunk build --"
  (cd "$WEB_UI_DIR" && trunk build) || {
    echo "mutation-check: trunk build failed for $name (unexpected — a Rust-level breakage, not a browser-observable one)" >&2
    cleanup_patch "$patch_path"
    RESULT[$name]="BUILD-FAILED"
    trap - ERR
    continue
  }

  echo "-- playwright test --grep $spec --"
  set +e
  (cd "$E2E_DIR" && npx playwright test --grep "$spec")
  status=$?
  set -e

  if [[ $status -ne 0 ]]; then
    RESULT[$name]="PASS (suite correctly failed, exit $status)"
  else
    RESULT[$name]="FAIL (suite passed despite the mutation — mechanism didn't catch it)"
  fi

  cleanup_patch "$patch_path"
  trap - ERR
done

echo
echo "mutation-check results:"
printf '%-28s %s\n' "patch" "outcome"
for name in "${PATCHES[@]}"; do
  printf '%-28s %s\n' "$name" "${RESULT[$name]:-not run}"
done
