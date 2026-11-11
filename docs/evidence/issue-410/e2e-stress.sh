#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #410, the browser rows: premise checkpoint 5
# and the e2e acceptance criterion. Runs a named Playwright selection
# `--repeat-each N --retries 0` under a fixed synthetic load and prints a
# transcript to stdout.
#
# What a run proves: at the commit it names, each selected test passed or
# failed the stated number of times, under 16 CPU spinners and 4
# `dd oflag=dsync` writers on the disk the run writes to, with no retry.
# Every failure is listed with its title, its failing assertion and the
# trace Playwright kept for it.
#
# What it trusts: the stub harness (harness/server.mjs) and Playwright's
# JSON report. The SPA is built from the same commit with `trunk build`
# before the load starts; the transcript names the hash of the built
# index.html, not of the wasm it loads.
#
# Modes:
#   premise  a failing repetition is data. Exits nonzero only on a harness
#            error.
#   accept   exits nonzero on any repetition that did not pass.
# A harness error is a run that did not report every repetition: a
# Playwright error outside a test, a cut-off run, a repetition that did not
# run, or a selection that matched a different number of tests than the
# caller expects. It exits 2 in both modes.
#
# Usage:
#   docs/evidence/issue-410/e2e-stress.sh MODE REPEAT TESTS GREP SPEC...
#     MODE    premise | accept
#     REPEAT  --repeat-each count
#     TESTS   how many distinct tests GREP must select
#     GREP    Playwright --grep pattern (a regular expression on titles)
#     SPEC    spec files, relative to crates/trawl-web-ui/e2e
#
# Environment:
#   E2E_STRESS_WORK_DIR  logs, the Playwright output (traces) and the
#                        load's files (default: <repo>/target/issue-410-e2e).
#                        Not tmpfs: the writers must hit a real disk.
#   E2E_PORT             first of the four harness ports (default: the
#                        first free run of four from 47310).
#
# Refuses a dirty tree, because the transcript names the commit.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
UI_DIR="$ROOT_DIR/crates/trawl-web-ui"
E2E_DIR="$UI_DIR/e2e"

if [[ $# -lt 5 ]]; then
  sed -n '/^# Usage:/,/^# Environment:/p' "${BASH_SOURCE[0]}" >&2
  exit 2
fi
MODE="$1" REPEAT="$2" TESTS="$3" GREP="$4"
shift 4
SPECS=("$@")
if [[ $MODE != premise && $MODE != accept ]]; then
  echo "e2e-stress: MODE must be premise or accept" >&2
  exit 2
fi

SPINNERS=16
WRITERS=4
WORKERS=4

if [[ -n "$(git -C "$ROOT_DIR" status --porcelain)" ]]; then
  echo "e2e-stress: working tree is dirty; commit first." >&2
  git -C "$ROOT_DIR" status --short >&2
  exit 2
fi
if [[ ! -d "$E2E_DIR/node_modules/@playwright/test" ]]; then
  echo "e2e-stress: run npm ci --no-audit --no-fund in $E2E_DIR first" >&2
  exit 2
fi

WORK_DIR="${E2E_STRESS_WORK_DIR:-$ROOT_DIR/target/issue-410-e2e}"
mkdir -p "$WORK_DIR"
if [[ "$(stat -f -c %T "$WORK_DIR")" == tmpfs ]]; then
  echo "e2e-stress: $WORK_DIR is on tmpfs; point it at a disk-backed directory" >&2
  exit 2
fi
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$MODE"
RUN_DIR="$WORK_DIR/$RUN_ID"
LOAD_DIR="$WORK_DIR/load-$RUN_ID"
mkdir -p "$RUN_DIR" "$LOAD_DIR"

port_free() { ! ss -ltnH "sport = :$1" | grep -q .; }
if [[ -z "${E2E_PORT:-}" ]]; then
  for base in $(seq 47310 10 47990); do
    if port_free "$base" && port_free $((base + 1)) && port_free $((base + 2)) && port_free $((base + 3)); then
      E2E_PORT=$base
      break
    fi
  done
fi
if [[ -z "${E2E_PORT:-}" ]]; then
  echo "e2e-stress: no free run of four ports from 47310" >&2
  exit 2
fi
export E2E_PORT

LOAD_PIDS=()
stop_load() {
  local pid
  for pid in "${LOAD_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${LOAD_PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
  LOAD_PIDS=()
  rm -rf "$LOAD_DIR"
}
trap stop_load EXIT
trap 'stop_load; exit 130' INT TERM

loadavg() { cut -d' ' -f1-3 /proc/loadavg; }

# Build before the load, so the build neither adds to it nor suffers it.
if ! (cd "$UI_DIR" && env -u NO_COLOR trunk build) >"$RUN_DIR/trunk.log" 2>&1; then
  echo "e2e-stress: trunk build failed; see $RUN_DIR/trunk.log" >&2
  exit 2
fi

CMD=(npx playwright test "${SPECS[@]}" --grep "$GREP" --repeat-each "$REPEAT" --retries 0
  --workers "$WORKERS" --trace retain-on-failure --global-timeout 0
  --output "$RUN_DIR/output" --reporter list,json)

echo "## $MODE run: $REPEAT repetitions of $TESTS test(s)"
echo "commit:        $(git -C "$ROOT_DIR" rev-parse HEAD)"
echo "subject:       $(git -C "$ROOT_DIR" log -1 --format=%s)"
echo "started:       $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "playwright:    $(cd "$E2E_DIR" && npx playwright --version)"
echo "spa build:     trunk build (dev), dist/index.html sha256 $(sha256sum "$UI_DIR/dist/index.html" | cut -d' ' -f1)"
echo "cpus:          $(nproc)"
echo "load:          $SPINNERS CPU spinners, $WRITERS dd oflag=dsync writers on $(stat -f -c %T "$LOAD_DIR") (${LOAD_DIR#"$ROOT_DIR"/})"
echo "load average before the load: $(loadavg)"

for _ in $(seq "$SPINNERS"); do
  (while :; do :; done) &
  LOAD_PIDS+=($!)
done
for i in $(seq "$WRITERS"); do
  (while :; do
    dd if=/dev/zero of="$LOAD_DIR/writer-$i" bs=64k count=256 oflag=dsync status=none
  done) &
  LOAD_PIDS+=($!)
done
# Let the load average show the load before the first repetition.
sleep 30
echo "load average after 30 s of load: $(loadavg)"
echo "command:       (cd crates/trawl-web-ui/e2e && E2E_PORT=$E2E_PORT $(printf '%q ' "${CMD[@]}"))" | sed "s|$ROOT_DIR|<repo>|g"

set +e
(cd "$E2E_DIR" && PLAYWRIGHT_JSON_OUTPUT_FILE="$RUN_DIR/report.json" "${CMD[@]}") >"$RUN_DIR/playwright.log" 2>&1
status=$?
set -e
echo "load average at the end of the run: $(loadavg)"
stop_load
echo "playwright exit: $status"

# Summarize the JSON report. Prints the transcript lines, and on its last
# line "VERDICT <harness-ok 0|1> <failures>".
summary="$(node - "$RUN_DIR/report.json" "$REPEAT" "$TESTS" "$ROOT_DIR" <<'EOF'
const fs = require('fs');
const path = require('path');
const [file, repeat, tests, root] = process.argv.slice(2);
const out = [];
let report;
try {
  report = JSON.parse(fs.readFileSync(file, 'utf8'));
} catch (e) {
  console.log(`harness error: no JSON report (${e.message})`);
  console.log('VERDICT 0 0');
  process.exit(0);
}
const byTitle = new Map();
const failures = [];
let attempts = 0;
let harnessOk = true;
const walk = (suite, titles) => {
  for (const spec of suite.specs || []) {
    for (const t of spec.tests || []) {
      const title = [...titles, spec.title].filter(Boolean).join(' › ');
      const key = `${spec.file} › ${title}`;
      const row = byTitle.get(key) || { pass: 0, fail: 0, other: 0 };
      byTitle.set(key, row);
      if ((t.results || []).length === 0) {
        row.other += 1;
        harnessOk = false;
      }
      for (const r of t.results || []) {
        attempts += 1;
        if (r.status === 'passed') {
          row.pass += 1;
        } else if (r.status === 'failed' || r.status === 'timedOut') {
          row.fail += 1;
          const err = (r.errors && r.errors[0]) || r.error || {};
          const msg = (err.message || '(no message)').replace(/\x1b\[[0-9;]*m/g, '');
          const at = err.location ? `${path.basename(err.location.file)}:${err.location.line}` : '?';
          const trace = (r.attachments || []).find((a) => a.name === 'trace');
          failures.push({
            key,
            status: r.status,
            at,
            msg,
            trace: trace && trace.path ? path.relative(root, trace.path) : '(none)',
          });
        } else {
          row.other += 1;
          harnessOk = false;
        }
      }
    }
  }
  for (const s of suite.suites || []) walk(s, s.title && !s.title.endsWith('.ts') ? [...titles, s.title] : titles);
};
for (const s of report.suites || []) walk(s, []);
for (const e of report.errors || []) {
  harnessOk = false;
  out.push(`harness error: ${(e.message || '').replace(/\x1b\[[0-9;]*m/g, '').split('\n')[0]}`);
}
if (byTitle.size !== Number(tests)) {
  harnessOk = false;
  out.push(`harness error: the selection matched ${byTitle.size} test(s), expected ${tests}`);
}
const want = Number(repeat) * Number(tests);
if (attempts !== want) {
  harnessOk = false;
  out.push(`harness error: ${attempts} attempts reported, expected ${want}`);
}
const passes = [...byTitle.values()].reduce((n, r) => n + r.pass, 0);
out.push(`attempts:      ${attempts}`);
out.push(`passes:        ${passes}`);
out.push(`failures:      ${failures.length}`);
for (const [key, r] of byTitle) {
  out.push(`  ${String(r.pass).padStart(4)} passed ${String(r.fail).padStart(4)} failed ${String(r.other).padStart(3)} other  ${key}`);
}
for (const f of failures) {
  out.push('');
  out.push(`failure: ${f.key} (${f.status})`);
  out.push(`  at:    ${f.at}`);
  out.push(`  trace: ${f.trace}`);
  for (const line of f.msg.split('\n').slice(0, 12)) out.push(`  | ${line}`);
}
console.log(out.join('\n'));
console.log(`VERDICT ${harnessOk ? 1 : 0} ${failures.length}`);
EOF
)"
printf '%s\n' "$summary" | sed '$d'
read -r _ harness_ok failed <<<"$(printf '%s\n' "$summary" | tail -1)"
echo "finished:      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "load average after the load stopped: $(loadavg)"
echo "log:           ${RUN_DIR#"$ROOT_DIR"/}/playwright.log"

if [[ $harness_ok -ne 1 ]]; then
  echo "verdict:       harness error"
  exit 2
fi
# Playwright exits 1 for failed tests; any other nonzero exit is the harness.
if [[ $status -ne 0 && $failed -eq 0 ]]; then
  echo "verdict:       harness error (exit $status with no failed repetition)"
  exit 2
fi
if [[ $MODE == accept ]]; then
  if [[ $failed -eq 0 ]]; then
    echo "verdict:       all $REPEAT repetitions of each test passed"
    exit 0
  fi
  echo "verdict:       NOT met, $failed failed repetition(s)"
  exit 1
fi
if [[ $failed -eq 0 ]]; then
  echo "verdict:       did not reproduce: 0 failures in $REPEAT repetitions of each test"
else
  echo "verdict:       reproduced: $failed failed repetition(s); see the failures above"
fi
exit 0
