#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
set -euo pipefail

# Run only against an agent-owned Postgres database with CREATEDB support.
# The caller supplies DATABASE_URL. Never echo it or enable shell tracing.
: "${DATABASE_URL:?Set DATABASE_URL to an agent-owned Postgres instance}"
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(git -C "$script_dir" rev-parse --show-toplevel)
patch="$script_dir/history-clear-unscoped.patch"
test_name=history_clear_is_scoped_counted_and_preserves_saved
if [[ -n $(git -C "$root" status --porcelain --untracked-files=normal) ]]; then
    echo 'Mutation check requires a clean committed tree.' >&2
    exit 1
fi
git -C "$root" check-ignore -q target/
mkdir -p "$root/target/issue-165"
logs=$(mktemp -d "$root/target/issue-165/mutation.XXXXXX")
mutated=0
restore_source() {
    if (( mutated )); then
        # A signal may arrive after arming but before git applies the patch.
        if git -C "$root" diff --quiet -- crates/trawl-server/src/store/history.rs; then
            mutated=0
            return 0
        fi
        git -C "$root" apply --reverse --check "$patch" || return 1
        git -C "$root" apply --reverse "$patch" || return 1
        mutated=0
    fi
}
cleanup() {
    status=$?
    trap - EXIT INT TERM
    set +e
    restore_source
    restored=$?
    if (( restored != 0 )) || [[ -n $(git -C "$root" status --porcelain --untracked-files=normal) ]]; then
        echo 'FAIL: mutation restoration did not produce a clean tree.' >&2
        status=1
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

run_test() {
    local phase=$1
    # A fixed exact test and no retries make the result count check meaningful.
    (cd "$root" && timeout --signal=TERM --kill-after=30s 900s cargo nextest run -p trawl-server --test store_pg \
        -E "test(=$test_name)" --no-tests fail --retries 0 --color never \
        --status-level all --final-status-level none \
        --failure-output immediate --success-output never) >"$logs/$phase.log" 2>&1
}
check_result() {
    python3 - "$logs/$1.log" "$2" "$test_name" <<'PY'
import pathlib
import re
import sys

log = pathlib.Path(sys.argv[1]).read_text()
expected, name = sys.argv[2:]
records = re.findall(r"^\s*(PASS|FAIL)\s+\[[^\n]+\]\s+(?:\([^\n)]+\)\s+)?trawl-server::store_pg\s+(\S+)\s*$", log, re.M)
if records != [(expected, name)]:
    raise SystemExit("Expected exactly one executed test with the requested result; inspect ignored log")
summary = r"Summary\s+\[[^\n]+\]\s+1 test run: " + ("1 passed" if expected == "PASS" else "0 passed, 1 failed")
if not re.search(summary, log):
    raise SystemExit("Missing exact one-test summary; inspect ignored log")
if expected == "FAIL":
    marker = "assertion `left == right` failed: history_clear preserves other keys"
    if marker not in log or len(re.findall(r"panicked at", log)) != 1:
        raise SystemExit("Mutation did not fail at the other-key preservation assertion")
    if "left: HistoryPage { entries: [], total: 0 }" not in log or 'query: "foreign"' not in log:
        raise SystemExit("Missing evidence that the mutation deleted the foreign row")
print("PASS: " + ("one baseline/control test passed" if expected == "PASS" else "mutant failed at history_clear preserves other keys; foreign row was deleted"))
PY
}

echo "Source commit: $(git -C "$root" rev-parse HEAD)"
echo "Store blob: $(git -C "$root" rev-parse HEAD:crates/trawl-server/src/store/history.rs)"
echo "Test: $test_name"
echo 'Command: cargo nextest run -p trawl-server --test store_pg -E test(=history_clear_is_scoped_counted_and_preserves_saved) --no-tests fail --retries 0 --color never --status-level all --final-status-level none --failure-output immediate --success-output never'
echo 'Database: caller-supplied agent-owned Postgres; connection details omitted'
echo 'Each test phase has a 900-second deadline and a 30-second termination grace period'
run_test baseline
check_result baseline PASS
git -C "$root" apply --check "$patch"
# Arm restoration before applying so a signal cannot leave an applied patch
# without an active restoration obligation.
mutated=1
git -C "$root" apply "$patch"
mutant_status=0
run_test mutant || mutant_status=$?
[[ $mutant_status == 100 ]] || { echo "FAIL: expected nextest test-failure exit 100, got $mutant_status" >&2; exit 1; }
check_result mutant FAIL
restore_source
run_test control
check_result control PASS
[[ -z $(git -C "$root" status --porcelain --untracked-files=normal) ]]
echo 'PASS: reverse patch restored source; control passed; worktree clean'
echo "Detailed logs: target/issue-165/$(basename "$logs")"
