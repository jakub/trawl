#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Mutation 21 removes only the terminal latch. Speed zero and hiding survive.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
cd_path="$root/crates/trawl-web-ui/e2e"
source_path="$root/crates/fleet-ui/vendor/src/paper-shaders.ts"
bundle="$root/crates/fleet-ui/vendor/paper-shaders.js"
dist="${TRAWL_E2E_DIST:-$root/crates/trawl-web-ui/dist}"
if [[ -n $(git -C "$root" status --porcelain --untracked-files=normal) ]]; then
  echo 'Mutation check requires a clean committed baseline.' >&2
  exit 1
fi
mapfile -t snippets < <(find "$dist/snippets" -type f -path '*/vendor/paper-shaders.js')
[[ ${#snippets[@]} == 1 ]] || { echo 'Expected exactly one shader snippet.' >&2; exit 1; }
snippet="${snippets[0]}"
cmp "$bundle" "$snippet"
backup=$(mktemp -d)
cp "$source_path" "$backup/source"
cp "$bundle" "$backup/bundle"
cp "$snippet" "$backup/snippet"
cp "$dist/index.html" "$backup/index"
restore() {
  status=$?
  trap - EXIT INT TERM
  cp "$backup/source" "$source_path"
  cp "$backup/bundle" "$bundle"
  cp "$backup/snippet" "$snippet"
  cp "$backup/index" "$dist/index.html"
  cmp "$backup/source" "$source_path" && cmp "$backup/bundle" "$bundle" && cmp "$backup/snippet" "$snippet" && cmp "$backup/index" "$dist/index.html" || status=1
  if [[ -n $(git -C "$root" status --porcelain --untracked-files=normal) ]]; then
    echo 'Mutation restoration left a dirty tree.' >&2
    status=1
  fi
  rm -rf "$backup"
  exit "$status"
}
trap restore EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# Run the exact target against the baseline too. A broken test cannot count as
# a killed mutant, even if another CI job previously passed the whole suite.
(cd "$cd_path" && npx playwright test atmosphere-fallback.spec.ts --grep 'live shader stays stopped' --output="$root/e2e-artifacts/test-results/atmosphere-baseline" --reporter=json) > "$backup/baseline.json"
node - "$backup/baseline.json" <<'NODE'
const fs = require('node:fs');
const report = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const tests = [];
function walk(suite) {
  for (const spec of suite.specs ?? []) tests.push(...spec.tests);
  for (const child of suite.suites ?? []) walk(child);
}
for (const suite of report.suites ?? []) walk(suite);
if (tests.length !== 1 || report.errors?.length || tests[0].results.length !== 1 || tests[0].results[0].status !== 'passed') {
  throw Error('Baseline must run exactly one passing loss test');
}
NODE
git -C "$root" apply "$cd_path/mutations/21-atmosphere-speed-only.patch"
bash "$root/crates/fleet-ui/vendor/build.sh"
if cmp -s "$bundle" "$backup/bundle"; then
  echo 'Mutation did not change the shipped bundle.' >&2
  exit 1
fi
cp "$bundle" "$snippet"
# Trunk's modulepreload pins the snippet's bytes with SRI. Change that one
# digest alongside the mutant, preserving all other index content.
node - "$dist" "$snippet" "$backup/snippet" <<'NODE'
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const [dist, snippet, baseline] = process.argv.slice(2);
const indexPath = path.join(dist, 'index.html');
const index = fs.readFileSync(indexPath, 'utf8');
const href = '/' + path.relative(dist, snippet);
const tags = [...index.matchAll(/<link\b[^>]*>/g)].map(match => match[0]).filter(tag => {
  const value = tag.match(/\bhref=["']([^"']+)["']/)?.[1];
  return value === href;
});
if (tags.length !== 1 || !/\brel=["']modulepreload["']/.test(tags[0])) {
  throw Error('Expected one matching shader modulepreload');
}
const digest = file => 'sha384-' + crypto.createHash('sha384').update(fs.readFileSync(file)).digest('base64');
const oldIntegrity = tags[0].match(/\bintegrity=["']([^"']+)["']/)?.[1];
if (oldIntegrity !== digest(baseline)) throw Error('Baseline shader preload integrity does not match its bytes');
fs.writeFileSync(indexPath, index.replace(tags[0], tags[0].replace(oldIntegrity, digest(snippet))));
NODE
set +e
(cd "$cd_path" && npx playwright test atmosphere-fallback.spec.ts --grep 'live shader stays stopped' --output="$root/e2e-artifacts/test-results/atmosphere-mutant" --reporter=json) > "$backup/mutant.json"
mutant_status=$?
set -e
[[ "$mutant_status" == 1 ]] || { echo "Expected test failure, got $mutant_status" >&2; exit 1; }
node - "$backup/mutant.json" <<'NODE'
const fs = require('node:fs');
const report = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const specs = [];
function walk(suite) { specs.push(...(suite.specs ?? [])); for (const child of suite.suites ?? []) walk(child); }
for (const suite of report.suites ?? []) walk(suite);
const tests = specs.flatMap(s => s.tests);
if (tests.length !== 1 || report.errors?.length) throw Error('Mutation must run exactly one test without runner errors');
const results = tests[0].results;
if (results.length !== 1 || results[0].status !== 'failed') throw Error('Mutation did not fail normally');
const errors = results[0].errors ?? [];
if (errors.length !== 1 || !errors[0].message.includes('ATMOSPHERE_NO_RESTART:')) {
  console.error(JSON.stringify(errors, null, 2));
  throw Error('Mutation failed outside the no-restart assertion');
}
console.log('Mutation 21 killed by ATMOSPHERE_NO_RESTART.');
NODE
# Constructor cleanup is independent of the deleted context-loss latch.
(cd "$cd_path" && npx playwright test atmosphere-fallback.spec.ts --grep 'compile failure leaves' --output="$root/e2e-artifacts/test-results/atmosphere-control" --reporter=line)
cp "$backup/baseline.json" "$root/e2e-artifacts/test-results/atmosphere-baseline.json"
cp "$backup/mutant.json" "$root/e2e-artifacts/test-results/atmosphere-mutant.json"
echo 'Mutation 21 unaffected compile-failure control passed.'
