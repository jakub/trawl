#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# The mutation check of #410's step admission budget test. It proves that
# a_step_admission_spends_one_deadline_across_statements, in
# crates/trawl-server/tests/investigations_attach.rs, fails when a step
# admission's log insert gets a fresh five-second budget, and passes when
# the insert gets what remains of the capture's deadline.
#
# mutation.patch, beside this script, makes insert_entry in
# crates/trawl-server/src/store/investigations.rs replace its bounded
# write transaction's deadline with Deadline::after(5 s) just before the
# insert. WriteTx::run turns that deadline into the insert's
# statement_timeout, so the insert is sent with a fresh 5000 ms instead of
# the 2000 ms left after the test's three-second advance.
#
# The script applies the patch, runs the test once with --retries 0,
# reverts the patch, and runs the test once more on the unpatched tree.
# A kill is nextest exit 100 with the test failed on its log-insert
# assertion, whose message names the 2 s that remained: exit 100 for any
# other failure is not a kill. The script exits 0 only when the patched
# run is a kill and the unpatched run passes. nextest's reporter flags are
# pinned so the check reads the same output whatever NEXTEST_* settings
# the caller has.
#
# It trusts the caller's toolchain, cargo-nextest and Postgres:
# DATABASE_URL must name a role that can create databases, as for the
# test suite. It guards against a misread run, not against a caller who
# wants it to lie. It refuses to run when a tracked file differs from
# HEAD, so the commit it prints is the code it ran.
#
# Usage, from any directory of a clean checkout. Write the transcript
# outside the tracked files first: a redirect into mutation.txt would
# change that file before the script checks the tree.
#   DATABASE_URL=postgres://... docs/evidence/issue-410/mutation.sh > /path/outside/the/checkout/mutation.txt
#   cp /path/outside/the/checkout/mutation.txt docs/evidence/issue-410/mutation.txt

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
PATCH="$SCRIPT_DIR/mutation.patch"
TEST=a_step_admission_spends_one_deadline_across_statements
KILL="the log insert, 3 s into the 5 s deadline, was not bounded by the 2 s that remained"
cd "$ROOT_DIR"

if [[ -z "${DATABASE_URL:-}" ]]; then
  echo "mutation.sh: DATABASE_URL must name the test Postgres." >&2
  exit 1
fi
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "mutation.sh: tracked files differ from HEAD; commit first." >&2
  git status --short --untracked-files=no >&2
  exit 1
fi

run_test() {
  cargo nextest run -p trawl-server --test investigations_attach --cargo-quiet \
    --no-fail-fast --no-tests=fail --retries 0 \
    --color never --show-progress none --status-level pass --final-status-level fail \
    --failure-output immediate --success-output never --message-format human \
    -E "test(=$TEST)" 2>&1
}

revert() {
  if git -C "$ROOT_DIR" apply --check -R "$PATCH" 2>/dev/null; then
    git -C "$ROOT_DIR" apply -R "$PATCH"
  fi
}

echo "#410 mutation check: $TEST"
echo "commit:  $(git rev-parse HEAD)"
echo "patch:   $(sha256sum "$PATCH" | cut -d' ' -f1)  docs/evidence/issue-410/mutation.patch"
echo "rustc:   $(rustc -V)"
echo "nextest: $(cargo nextest --version 2>/dev/null | head -n 1)"
echo
echo "=== mutation.patch ==="
cat "$PATCH"

echo
echo "=== patched: the log insert is sent with a fresh 5 s budget ==="
git -C "$ROOT_DIR" apply --check "$PATCH"
trap revert EXIT
trap 'revert; exit 130' INT TERM
git -C "$ROOT_DIR" apply "$PATCH"
patched_status=0
patched="$(run_test)" || patched_status=$?
revert
trap - EXIT INT TERM
echo "$patched"
echo "nextest exit: $patched_status"

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "mutation.sh: tracked files differ from HEAD after the revert:" >&2
  git status --short --untracked-files=no >&2
  exit 2
fi

echo
echo "=== unpatched: the log insert is sent with what remains ==="
clean_status=0
clean="$(run_test)" || clean_status=$?
echo "$clean"
echo "nextest exit: $clean_status"

killed=no
if [[ $patched_status -eq 100 ]] \
  && grep -qE "^ +FAIL .* $TEST\$" <<<"$patched" \
  && grep -qF "$KILL" <<<"$patched"; then
  killed=yes
fi

echo
echo "=== result ==="
echo "patched run:   nextest exit $patched_status, failed on the log-insert assertion: $killed"
echo "unpatched run: nextest exit $clean_status"
if [[ $killed == yes && $clean_status -eq 0 ]]; then
  echo "PASS: the mutation is killed, and the unpatched test passes"
  exit 0
fi
echo "FAIL: the mutation survived, or the unpatched test did not pass"
exit 1
