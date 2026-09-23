#!/usr/bin/env node
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { parseArgs } from 'node:util';
import { sha256, readRunBuild, verifyRunIdentity } from './identity.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const { values } = parseArgs({ options: { run: { type: 'string' } } });
assert.ok(values.run, 'Usage: node scripts/app-experiment/issue233-evidence.mjs --run EXACT_RUN_DIRECTORY');
const run = await fs.realpath(values.run);
const ownedRoot = await fs.realpath(path.join(root, 'target/app-experiments'));
assert.equal(path.dirname(run), ownedRoot, 'run must be a direct child of this checkout experiment directory');
const inside = async (file, dir) => {
  const actual = await fs.realpath(file);
  assert.ok(actual.startsWith(`${dir}${path.sep}`), 'file escapes its owned directory');
  return actual;
};
const readJSON = async file => JSON.parse(await fs.readFile(await inside(file, run), 'utf8'));
const instance = await readJSON(path.join(run, 'instance.json'));
assert.equal(instance.runId, path.basename(run));
assert.match(instance.runId, /^run-\d+-[a-f0-9]+$/);
const origin = new URL(instance.browserOrigin);
assert.equal(origin.protocol, 'http:');
assert.equal(origin.hostname, '127.0.0.1');
assert.equal(origin.origin, instance.browserOrigin);
assert.ok(Number(origin.port) >= 1024);
const upstream = new URL(instance.upstream);
assert.equal(upstream.protocol, 'https:');
assert.equal(upstream.hostname, '127.0.0.1');
assert.match(instance.container, /^trawl-experiment-[a-f0-9]+$/);
const privateDir = await inside(path.join(run, 'private'), run);
assert.equal(privateDir, path.join(run, 'private'));
const keyFile = await inside(instance.browserKeyFile, privateDir);
assert.equal(keyFile, path.join(privateDir, 'browser-key'));
// The selected run owns its build record. The checkout-wide preparation cache
// may belong to a later run, even while these processes remain alive.
const buildRecord = await readRunBuild(run, instance);
const build = buildRecord.build;
const head = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim();
assert.equal(build.checkout, root);
assert.equal(build.commit, head, 'build commit differs from current HEAD');
const scenarioPath = 'scripts/app-experiment/issue233-evidence.mjs';
const scenarioBytes = await fs.readFile(fileURLToPath(import.meta.url));
const committedScenario = execFileSync('git', ['show', `HEAD:${scenarioPath}`], { cwd: root });
assert.ok(scenarioBytes.equals(committedScenario), 'scenario differs from committed HEAD; commit it before collecting evidence');
const scenarioSHA256 = createHash('sha256').update(scenarioBytes).digest('hex');
const identityPath = 'scripts/app-experiment/identity.mjs';
const identityBytes = await fs.readFile(new URL('./identity.mjs', import.meta.url));
const committedIdentity = execFileSync('git', ['show', `HEAD:${identityPath}`], { cwd: root });
assert.ok(identityBytes.equals(committedIdentity), 'identity helper differs from committed HEAD; commit it before collecting evidence');
const identityHelperSHA256 = sha256(identityBytes);
assert.equal(identityHelperSHA256, build.identityHelperSHA256, 'identity helper differs from selected run');
// Keep this filter identical to run.mjs fingerprint(). The commit alone does
// not detect uncommitted product edits made after preparation.
const tracked = execFileSync('git', ['ls-files', '--cached', '--others', '--exclude-standard', '-z'], { cwd: root, encoding: 'utf8' }).split('\0').filter(Boolean);
const sourceHash = createHash('sha256');
for (const file of tracked.filter(f => f.startsWith('crates/') || f.startsWith('.cargo/') || f.startsWith('Cargo.') || f === 'rust-toolchain.toml' || f === 'bin/trawld-dev').sort()) {
  sourceHash.update(file);
  sourceHash.update(await fs.readFile(path.join(root, file)));
}
assert.equal(sourceHash.digest('hex'), build.sourceHash, 'source changed since experiment preparation');
// Reject wrong build/process/SPA associations before creating evidence or
// reading a credential. These checks also verify unauthenticated asset bytes.
await verifyRunIdentity(run, instance, buildRecord);
const output = path.join(run, 'issue233-evidence');
await fs.mkdir(output, { mode: 0o700 }); // Refuse accidental overwrite/reseed.
// Issue 233 (ADR-0039): the query error bodies the real daemon writes, and
// what the SPA it serves shows for them. Each case types a query into the
// real editor under the 15-minute range, sends it with Ctrl+Enter, and reads
// the daemon's answer from the browser's own response. The bodies are the
// oracle for the e2e suite's route fixtures: each must equal its committed
// file under crates/trawl-web-ui/e2e/harness/wire/.
const wireDir = path.join(root, 'crates/trawl-web-ui/e2e/harness/wire');
const cases = [
  { label: 'parse-error', query: 'service=kubelet | stats count( by host', sent: 'last=15m service=kubelet | stats count( by host',
    code: 'parse_error', wire: 'query-parse-error.json', lead: /^Couldn't run the query: found 'h', expected /, excerpt: true },
  { label: 'parse-errors-two', query: 'f=#a,#b', sent: 'last=15m f=#a,#b',
    code: 'parse_error', wire: 'query-parse-errors-two.json', lead: /^Couldn't run the query: 2 errors$/, excerpt: true },
  { label: 'validation-hint', query: '* | stats countt(x) by host', sent: 'last=15m * | stats countt(x) by host',
    code: 'validation_error', wire: 'query-validation-hint.json',
    lead: /^Couldn't run the query: unknown function: countt — did you mean 'count'\?$/, excerpt: false },
  { label: 'validation-no-hint', query: '* | stats nosuchfunc(x) by host', sent: 'last=15m * | stats nosuchfunc(x) by host',
    code: 'validation_error', wire: 'query-validation-no-hint.json',
    lead: /^Couldn't run the query: unknown function: nosuchfunc$/, excerpt: false },
];
const report = { schema: 1, status: 'running', runId: instance.runId, sourceHead: head, scenarioSHA256, identityHelperSHA256,
  build: { commit: build.commit, sourceHash: build.sourceHash, sourceFingerprintVerified: true,
    runBuildSHA256: instance.buildSHA256,
    manifestArtifacts: { spaHash: build.spaHash, binaries: build.binaries },
    artifactVerification: 'Selected run record, running executable bytes, process starts, configs, and owned/served SPA verified before evidence and again before pass.' },
  command: `node ${scenarioPath} --run target/app-experiments/${instance.runId}`,
  cases: [], captures: [], cleanup: { browser: false },
  limits: ['Refusals only: parse and validation errors are decided before DuckDB runs, so no corpus row is read.',
    'Live mode is not exercised: EventSource cannot read a refusal body, and the stub suite covers the live notice.',
    'Identity checks detect drift and stale processes; they do not isolate against a hostile local user.'] };
let browser;
let context;
let phase = 'initialize';
let interrupted = false;
const deadline = Date.now() + 5 * 60 * 1000;
const check = () => { assert.ok(!interrupted && Date.now() < deadline, 'scenario interrupted or deadline exceeded'); };
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => { interrupted = true; void browser?.close(); });
try {
  const require = createRequire(path.join(root, 'crates/trawl-web-ui/e2e/package.json'));
  const { chromium } = require('@playwright/test');
  browser = await chromium.launch({ headless: true });
  context = await browser.newContext({ viewport: { width: 1440, height: 900 }, timezoneId: 'UTC', locale: 'en-US', reducedMotion: 'reduce', serviceWorkers: 'block' });
  // Block all traffic outside this owned browser origin, including redirects.
  await context.route('**/*', route => new URL(route.request().url()).origin === origin.origin ? route.continue() : route.abort());
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  await page.goto(`${origin.origin}/login`);
  phase = 'login';
  await verifyRunIdentity(run, instance, buildRecord, { served: false });
  let key = (await fs.readFile(keyFile, 'utf8')).trim();
  const loginStatus = await page.evaluate(async api_key => (await fetch('/api/auth/login', { method: 'POST', redirect: 'error', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ api_key }) })).status, key);
  key = undefined;
  assert.equal(loginStatus, 200, 'login failed');
  await page.goto(`${origin.origin}/search?r=15m`);
  for (const c of cases) {
    phase = `case-${c.label}`;
    check();
    await page.locator('.dsl-editor .cm-content').click();
    await page.keyboard.press('Control+a');
    await page.keyboard.insertText(c.query);
    const pending = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/query' && r.request().method() === 'POST');
    await page.keyboard.press('Control+Enter');
    const response = await pending;
    assert.equal(response.request().postDataJSON().query, c.sent, 'the page sent a different effective query');
    assert.equal(response.status(), 400);
    const body = await response.json();
    assert.equal(body.error.code, c.code);
    const wire = JSON.parse(await fs.readFile(path.join(wireDir, c.wire), 'utf8'));
    assert.deepEqual(body, wire, `${c.wire} differs from the daemon's body`);
    if (c.code === 'validation_error') {
      for (const detail of body.error.details) assert.equal(detail.span, undefined, 'a validation detail carries a span');
    }
    const notice = page.locator('#search-results .query-error[role="alert"]');
    await notice.waitFor();
    const lead = await notice.locator('.query-error-lead').innerText();
    assert.match(lead, c.lead);
    const excerpts = await notice.locator('figure.query-error-excerpt').count();
    assert.equal(excerpts > 0, c.excerpt);
    const quoted = await notice.locator('pre.query-error-text').evaluateAll(pres => pres.map(p => p.textContent.split('\n')[0]));
    for (const line of quoted) assert.equal(line, c.sent, 'an excerpt quotes text other than the sent query');
    assert.equal(await page.locator('#search-results').getByRole('button').count(), 0, 'the notice offers a control');
    const leadCount = lead.split('did you mean').length - 1;
    assert.ok(leadCount <= 1 && (await notice.innerText()).split('did you mean').length - 1 === leadCount, 'the hint repeats');
    await page.screenshot({ path: path.join(output, `${c.label}.png`) });
    report.captures.push(`${c.label}.png`);
    report.cases.push({ label: c.label, sent: c.sent, status: response.status(), body, wireFixture: c.wire, wireFixtureEqual: true,
      notice: { lead, excerpts, quoted } });
  }
  phase = 'final-identity';
  await verifyRunIdentity(run, instance, buildRecord);
  report.status = 'passed';
} catch (error) {
  // Browser/assertion errors can embed request data. Publish only the phase.
  report.status = interrupted ? 'interrupted' : 'failed';
  report.failure = { phase, message: 'Assertion or operation failed; rerun this phase under local inspection. Raw errors intentionally omitted.' };
  if (error instanceof assert.AssertionError) {
    const primitive = value => typeof value === 'boolean' || (typeof value === 'number' && Number.isFinite(value));
    report.failure.kind = 'assertion';
    if (primitive(error.actual)) report.failure.actual = error.actual;
    if (primitive(error.expected)) report.failure.expected = error.expected;
    report.failure.assertionLine = error.stack?.match(/issue233-evidence\.mjs:(\d+):/)?.[1];
  } else report.failure.kind = 'operation';
  process.exitCode = 1;
} finally {
  try { await context?.close(); await browser?.close(); report.cleanup.browser = true; }
  catch { report.status = 'failed'; report.cleanup.browser = false; process.exitCode = 1; }
  await fs.writeFile(path.join(output, 'report.json'), `${JSON.stringify(report, null, 2)}\n`, { mode: 0o600 });
  console.log(`${report.status}: ${path.join(output, 'report.json')}`);
}
