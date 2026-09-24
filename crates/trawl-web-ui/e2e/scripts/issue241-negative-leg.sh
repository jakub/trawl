#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #241: prove signed-out-probes.spec.ts depends
# on the status bar's session gate in src/pages/layout.rs. Removes ONLY that
# gate (the connected-label change stays), builds the SPA, and requires the
# spec to fail on its health-request assertion; then restores the file,
# builds again, and requires the spec to pass. Prints both outcomes and
# exits 0 only if both legs behave.
#
# Each leg's outcome comes from the JSON report
# (issue241-negative-leg.verdict.mjs): pass, fail, or INCONCLUSIVE. A failure
# counts only if its error is located at the spec's health-count assertion
# and its message header is that assertion's failure, so a port collision,
# launch failure, timeout, crash, unreadable report, or an error on a
# neighbouring line is INCONCLUSIVE (exit 3), never a kill. A surviving
# mutant, or a gated build that fails, is RESULT: FAIL (exit 1).
#
# Both builds go to script-owned dist directories under target/, served
# through TRAWL_E2E_DIST, so crates/trawl-web-ui/dist is never written and no
# exit path can leave the ungated app there. The exit and signal traps
# restore layout.rs and remove those directories.
#
# Usage: e2e/scripts/issue241-negative-leg.sh   (E2E_PORT defaults to 8123)
#        e2e/scripts/issue241-negative-leg.sh --self-test   (verdict parser only)
#
# Refuses to run against a dirty tree: the restore is `git checkout` of
# layout.rs, which would discard uncommitted work in that file.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WEB_UI_DIR="$(cd "$E2E_DIR/.." && pwd)"
REPO_DIR="$(git -C "$WEB_UI_DIR" rev-parse --show-toplevel)"
LAYOUT="$WEB_UI_DIR/src/pages/layout.rs"
SPEC="tests/signed-out-probes.spec.ts"

if [ "${1:-}" = --self-test ]; then
  exec node "$SCRIPT_DIR/issue241-negative-leg.verdict.mjs" --self-test
fi

if ! git -C "$WEB_UI_DIR" diff --quiet HEAD --; then
  echo "refusing to run: working tree has uncommitted changes" >&2
  exit 2
fi

mkdir -p "$REPO_DIR/target"
WORK="$(mktemp -d "$REPO_DIR/target/issue241-negative-leg.XXXXXX")"
cleanup() {
  git -C "$WEB_UI_DIR" checkout -- "$LAYOUT"
  rm -rf "$WORK"
}
trap cleanup EXIT
# Turn INT and TERM into an exit, so the EXIT trap restores layout.rs even
# when the run is killed mid-build.
trap 'exit 130' INT
trap 'exit 143' TERM

# build DIST: trunk build into a script-owned directory.
build() {
  (cd "$WEB_UI_DIR" && env -u NO_COLOR trunk build --dist "$1" >/dev/null 2>&1) \
    || { echo "trunk build failed" >&2; exit 1; }
}

# run_spec DIST EXPECT (fail|pass): run the spec against DIST, print the
# reporter summary, and write the verdict (fail, pass or inconclusive) to
# $WORK/verdict-EXPECT.
run_spec() {
  local dist="$1" expect="$2" report="$WORK/report-$2.json" out status=0
  out="$(cd "$E2E_DIR" && TRAWL_E2E_DIST="$dist" PLAYWRIGHT_JSON_OUTPUT_NAME="$report" \
    npx playwright test "$SPEC" --workers=1 --reporter=line,json 2>&1)" || status=$?
  printf '%s\n' "$out" | grep -E '^\s+[0-9]+ (passed|failed)|Error:|Expected:|Received:' || true
  echo "playwright exit: $status"
  node "$SCRIPT_DIR/issue241-negative-leg.verdict.mjs" "$report" "$status" "$WORK/verdict-$expect"
}

echo "== issue #241 negative leg at $(git -C "$WEB_UI_DIR" rev-parse HEAD)"
echo
echo "== leg 1: footer session gate removed"
python3 - "$LAYOUT" <<'PY'
import re, sys
path = sys.argv[1]
src = open(path).read()
gate = re.compile(
    r"[ \t]*<Show when=move \|\| me\.get\(\)\.is_some\(\)>\n"
    r"((?:[ \t]*<StatusBar\n)(?:.*\n)*?[ \t]*/>\n)"
    r"[ \t]*</Show>\n"
)
out, n = gate.subn(lambda m: m.group(1), src)
if n != 1:
    sys.exit(f"expected exactly one gated StatusBar in {path}, found {n}")
open(path, "w").write(out)
PY
git -C "$WEB_UI_DIR" diff -- "$LAYOUT"
build "$WORK/dist-ungated"
run_spec "$WORK/dist-ungated" fail
leg1="$(cat "$WORK/verdict-fail")"
echo "leg 1 outcome: $leg1 (expected fail)"
echo
echo "== leg 2: footer session gate restored"
git -C "$WEB_UI_DIR" checkout -- "$LAYOUT"
git -C "$WEB_UI_DIR" diff --quiet HEAD -- "$LAYOUT" && echo "layout.rs identical to HEAD"
build "$WORK/dist-gated"
run_spec "$WORK/dist-gated" pass
leg2="$(cat "$WORK/verdict-pass")"
echo "leg 2 outcome: $leg2 (expected pass)"
echo
node "$SCRIPT_DIR/issue241-negative-leg.verdict.mjs" --result "$leg1" "$leg2"
