// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Verdict for one leg of issue241-negative-leg.sh, read from Playwright's
// JSON report. A kill is structural: the single error must be located at
// the spec's health-count assertion, and its message header (ANSI stripped,
// cut before the source excerpt) must be that assertion's own failure. The
// excerpt quotes neighbouring lines, so an unrelated error one line up still
// contains the assertion's label and must not count.
//
// Usage:
//   node issue241-negative-leg.verdict.mjs REPORT fail|pass EXIT_STATUS OUT
//   node issue241-negative-leg.verdict.mjs --self-test

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SPEC = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'tests', 'signed-out-probes.spec.ts');
const LABEL = 'harness counted a health request';
const ASSERTION = `'${LABEL}').toBe(0)`;

/** 1-based line of the health-count assertion, found in the spec itself. */
export function assertionLine(source = fs.readFileSync(SPEC, 'utf8')) {
  const lines = source.split('\n').flatMap((l, i) => (l.includes(ASSERTION) ? [i + 1] : []));
  if (lines.length !== 1) throw new Error(`expected one health assertion in ${SPEC}, found ${lines.length}`);
  return lines[0];
}

/** Message text before the source excerpt or stack, ANSI codes removed. */
export function header(message) {
  const plain = message.replace(/\u001b\[[0-9;]*m/g, '');
  const out = [];
  for (const line of plain.split('\n')) {
    if (/^\s*>?\s*\d+ \|/.test(line) || /^\s+at /.test(line)) break;
    out.push(line);
  }
  return out.join('\n');
}

export function classify(report, expect, status, line = assertionLine()) {
  if (!report) return ['inconclusive', 'no JSON report'];
  const results = [];
  const walk = s => {
    for (const spec of s.specs ?? []) for (const t of spec.tests) for (const res of t.results) results.push(res);
    for (const c of s.suites ?? []) walk(c);
  };
  for (const s of report.suites ?? []) walk(s);
  const st = report.stats ?? {};
  const runErrors = (report.errors ?? []).length;
  if (results.length !== 1 || runErrors) return ['inconclusive', `${results.length} results, ${runErrors} run errors`];
  const [res] = results;
  if (expect === 'pass') {
    return status === '0' && st.expected === 1 && st.unexpected === 0 && res.status === 'passed'
      ? ['pass', 'one test passed']
      : ['inconclusive', `status=${res.status} exit=${status}`];
  }
  const errors = res.errors ?? [];
  if (status === '0' || st.unexpected !== 1 || res.status !== 'failed' || errors.length !== 1)
    return ['inconclusive', `status=${res.status} exit=${status} errors=${errors.length}`];
  const [err] = errors;
  const at = err.location ?? {};
  if (path.resolve(at.file ?? '') !== SPEC || at.line !== line)
    return ['inconclusive', `error at ${at.file}:${at.line}, not the health assertion at line ${line}`];
  const h = header(err.message ?? '');
  const received = /^Received: (\d+)$/m.exec(h);
  if (!h.startsWith(`Error: ${LABEL}\n`) || !/^Expected: 0$/m.test(h) || !received || Number(received[1]) === 0)
    return ['inconclusive', 'error at the health assertion is not its count failure'];
  return ['fail', `one test failed at line ${line} on "${LABEL}" (received ${received[1]})`];
}

function report(error) {
  const failed = { status: 'failed', errors: [error] };
  return { stats: { expected: 0, unexpected: 1 }, errors: [], suites: [{ specs: [{ tests: [{ results: [failed] }] }] }] };
}

function selfTest() {
  const line = assertionLine();
  const excerpt = (at) => [
    `  ${line - 1} |   const hits = (await (await request.get('/__ctl/state')).json()).healthHits;`,
    `${at === line ? '>' : ' '} ${line} |   expect(hits.health ?? 0, '${LABEL}').toBe(0);`,
    `    at ${SPEC}:${at}:1`,
  ].join('\n');
  const cases = [
    ['real kill (message shape from a mutant run)', 'fail', {
      location: { file: SPEC, line, column: 64 },
      message: `Error: ${LABEL}\n\n\u001b[2mexpect(\u001b[22m\u001b[31mreceived\u001b[39m\u001b[2m).\u001b[22mtoBe\u001b[2m(\u001b[22m\u001b[32mexpected\u001b[39m\u001b[2m) // Object.is equality\u001b[22m\n\nExpected: \u001b[32m0\u001b[39m\nReceived: \u001b[31m1\u001b[39m\n\n${excerpt(line)}`,
    }],
    ['state read fails one line up; excerpt quotes the label', 'inconclusive', {
      location: { file: SPEC, line: line - 1, column: 40 },
      message: `SyntaxError: Unexpected end of JSON input\n\n${excerpt(line - 1)}`,
    }],
    ['other error on the assertion line', 'inconclusive', {
      location: { file: SPEC, line, column: 15 },
      message: `TypeError: Cannot read properties of undefined (reading 'health')\n\n${excerpt(line)}`,
    }],
  ];
  let ok = true;
  for (const [name, want, error] of cases) {
    const [got, why] = classify(report(error), 'fail', '1', line);
    ok &&= got === want;
    console.log(`${got === want ? 'ok  ' : 'FAIL'} ${name}: ${got} (${why})`);
  }
  process.exit(ok ? 0 : 1);
}

if (process.argv[2] === '--self-test') selfTest();
else {
  const [file, expect, status, out] = process.argv.slice(2);
  const [verdict, why] = classify(fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, 'utf8')) : null, expect, status);
  console.log(`verdict basis: ${why}`);
  fs.writeFileSync(out, verdict);
}
