#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #241: prove signed-out-probes.spec.ts depends
# on the status bar's session gate in src/pages/layout.rs. Removes ONLY that
# gate (the connected-label change stays), rebuilds the SPA, and requires
# the spec to fail; then restores the file, rebuilds, and requires it to
# pass. Prints both outcomes and exits 0 only if both legs behave.
#
# Usage: e2e/scripts/issue241-negative-leg.sh   (E2E_PORT defaults to 8123)
#
# Refuses to run against a dirty tree: the restore is `git checkout` of
# layout.rs, which would discard uncommitted work in that file.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WEB_UI_DIR="$(cd "$E2E_DIR/.." && pwd)"
LAYOUT="$WEB_UI_DIR/src/pages/layout.rs"
SPEC="tests/signed-out-probes.spec.ts"

if ! git -C "$WEB_UI_DIR" diff --quiet HEAD --; then
  echo "refusing to run: working tree has uncommitted changes" >&2
  exit 2
fi

restore() { git -C "$WEB_UI_DIR" checkout -- "$LAYOUT"; }
trap restore EXIT

build() {
  (cd "$WEB_UI_DIR" && env -u NO_COLOR trunk build >/dev/null 2>&1) || { echo "trunk build failed" >&2; exit 1; }
}

# Run the spec; print the verdict and failing assertion lines; return its exit code.
run_spec() {
  local out status=0
  out="$(cd "$E2E_DIR" && npx playwright test "$SPEC" --workers=1 --reporter=line 2>&1)" || status=$?
  printf '%s\n' "$out" | grep -E '^\s+[0-9]+ (passed|failed)|Error:|Expected:|Received:' || true
  echo "playwright exit: $status"
  return "$status"
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
build
if run_spec; then gated_out=pass; else gated_out=fail; fi
echo "leg 1 outcome: $gated_out (expected fail)"
echo
echo "== leg 2: footer session gate restored"
restore
git -C "$WEB_UI_DIR" diff --quiet HEAD -- "$LAYOUT" && echo "layout.rs identical to HEAD"
build
if run_spec; then restored_out=pass; else restored_out=fail; fi
echo "leg 2 outcome: $restored_out (expected pass)"
echo
if [ "$gated_out" = fail ] && [ "$restored_out" = pass ]; then
  echo "RESULT: PASS (the spec fails without the gate and passes with it)"
else
  echo "RESULT: FAIL"
  exit 1
fi
