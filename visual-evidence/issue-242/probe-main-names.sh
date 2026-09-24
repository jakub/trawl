#!/usr/bin/env bash
# Print the dialog names a build of main gives at EVERY step of
# drawer-names.spec.ts, including the steps after a failed assertion that
# the negative-leg transcript never reaches (the rename and deleted-net
# legs fail at their opening assertion on main).
#
# It derives a throwaway copy of the spec in which each named-dialog
# assertion logs the page's dialog names from its ARIA snapshot instead
# of asserting, runs that copy once, and deletes it.
#
# Usage, from the repository root: visual-evidence/issue-242/probe-main-names.sh DIST
#   DIST: a trunk build of the revision under test (the negative leg used
#   a release build of 656e7622).
set -euo pipefail
dist=$(realpath "$1")
e2e=crates/trawl-web-ui/e2e
probe=$e2e/tests/zz-probe-242.spec.ts
trap 'rm -f "$probe"' EXIT
python3 - "$e2e/tests/drawer-names.spec.ts" "$probe" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
src = src.replace(
    "function dialogNamed(page: Page, name: string) {\n  return page.getByRole('dialog', { name, exact: true });\n}",
    "async function names(page: Page, expected: string) {\n"
    "  // The assertion this replaces waited for its dialog; wait for one.\n"
    "  await page.getByRole('dialog').first().waitFor();\n"
    "  const snap = await page.locator('body').ariaSnapshot();\n"
    "  const seen = snap.match(/dialog( \"[^\"]*\")?:?$/gm) ?? [];\n"
    "  console.log(`PROBE expected ${JSON.stringify(expected)} -> ${seen.join(' | ')}`);\n"
    "}",
)
src, n = re.subn(r"await expect\(dialogNamed\(page, ([^)]*)\)\)\.toBeVisible\(\);",
                 r"await names(page, String(\1));", src)
assert n == 11, n
open(sys.argv[2], "w").write(src)
PY
echo "# probe of $(git rev-parse HEAD):$e2e/tests/drawer-names.spec.ts against $1"
(cd "$e2e" && TRAWL_E2E_DIST=$dist E2E_PORT=${E2E_PORT:-8172} \
  npx playwright test tests/zz-probe-242.spec.ts --workers=1 --reporter=list 2>&1) \
  | grep -E '^PROBE|passed|failed'
