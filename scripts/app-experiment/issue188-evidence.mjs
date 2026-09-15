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
import { randomBytes, createHash } from 'node:crypto';
import { parseArgs } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const { values } = parseArgs({ options: { run: { type: 'string' } } });
assert.ok(values.run, 'Usage: node scripts/app-experiment/issue188-evidence.mjs --run EXACT_RUN_DIRECTORY');
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
// A stale descriptor must not authorize sending its key to a reused port.
for (const [name, config] of [['trawld', 'trawld.toml'], ['web', 'web.toml']]) {
  const pid = instance.pids[name];
  assert.ok(Number.isSafeInteger(pid) && pid > 1);
  const argv = (await fs.readFile(`/proc/${pid}/cmdline`, 'utf8')).split('\0');
  assert.ok(argv.includes(path.join(privateDir, config)), 'recorded process does not own this run config');
}
const config = await fs.readFile(await inside(path.join(privateDir, 'web.toml'), privateDir), 'utf8');
assert.ok(config.includes(`bind_addr = "127.0.0.1:${origin.port}"`));
assert.ok(config.includes(`upstream_url = "${upstream.origin}"`));
const build = JSON.parse(await fs.readFile(path.join(root, 'target/app-experiment-build.json'), 'utf8'));
const head = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim();
assert.equal(build.checkout, root);
assert.equal(build.commit, head, 'build commit differs from current HEAD');
const scenarioPath = 'scripts/app-experiment/issue188-evidence.mjs';
const scenarioBytes = await fs.readFile(fileURLToPath(import.meta.url));
const committedScenario = execFileSync('git', ['show', `HEAD:${scenarioPath}`], { cwd: root });
assert.ok(scenarioBytes.equals(committedScenario), 'scenario differs from committed HEAD; commit it before collecting evidence');
const scenarioSHA256 = createHash('sha256').update(scenarioBytes).digest('hex');
// Keep this filter identical to run.mjs fingerprint(). The commit alone does
// not detect uncommitted product edits made after preparation.
const tracked = execFileSync('git', ['ls-files', '--cached', '--others', '--exclude-standard', '-z'], { cwd: root, encoding: 'utf8' }).split('\0').filter(Boolean);
const sourceHash = createHash('sha256');
for (const file of tracked.filter(f => f.startsWith('crates/') || f.startsWith('.cargo/') || f.startsWith('Cargo.') || f === 'rust-toolchain.toml' || f === 'bin/trawld-dev').sort()) {
  sourceHash.update(file);
  sourceHash.update(await fs.readFile(path.join(root, file)));
}
assert.equal(sourceHash.digest('hex'), build.sourceHash, 'source changed since experiment preparation');
const output = path.join(run, 'issue188-evidence');
await fs.mkdir(output, { mode: 0o700 }); // Refuse accidental overwrite/reseed.
const report = { schema: 1, status: 'running', runId: instance.runId, sourceHead: head, scenarioSHA256,
  build: { commit: build.commit, sourceHash: build.sourceHash, sourceFingerprintVerified: true,
    manifestArtifacts: { spaHash: build.spaHash, binaries: build.binaries },
    artifactVerification: 'Hashes copied from preparation manifest; default runner owns artifact verification.' },
  command: `node ${scenarioPath} --run target/app-experiments/${instance.runId}`,
  search: [], receipts: [], orders: [], ui: [], captures: [], cleanup: { browser: false },
  limits: ['Real successful runs; null/status/timestamp tie diversity belongs to PostgreSQL fixture tests.',
    'Custom success must be paired with the default report exit 0, passed, and successful cleanup.'] };
let browser;
let context;
let phase = 'initialize';
let interrupted = false;
const deadline = Date.now() + 10 * 60 * 1000;
const check = () => { assert.ok(!interrupted && Date.now() < deadline, 'scenario interrupted or deadline exceeded'); };
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => { interrupted = true; void browser?.close(); });
const duration = ms => ms < 10 ? `${ms}ms` : `${Math.floor(ms / 1000)}.${String(ms % 1000).padStart(3, '0')}s`;
const safeReceipt = r => Object.fromEntries(['id', 'net_id', 'net_name', 'status', 'started_at', 'finished_at', 'duration_ms', 'row_count'].filter(k => r[k] !== undefined).map(k => [k, r[k]]));
// RFC3339 timestamps retain microseconds for ordering; Date.parse alone loses ties.
const instant = value => {
  const match = value.match(/^(.*T\d\d:\d\d:\d\d)(?:\.(\d+))?(Z|\+00:00)$/);
  assert.ok(match, 'expected UTC timestamp');
  return BigInt(Date.parse(`${match[1]}Z`)) * 1000000n + BigInt((match[2] || '').padEnd(9, '0'));
};
const cmp = (a, b) => a < b ? -1 : a > b ? 1 : 0;
function compare(key, dir) {
  const sign = dir === 'asc' ? 1 : -1;
  return (a, b) => {
    const av = key === 'net' ? a.net_name.toLowerCase() : key === 'started' ? instant(a.started_at) : a[{ status: 'status', duration: 'duration_ms', rows: 'row_count' }[key]];
    const bv = key === 'net' ? b.net_name.toLowerCase() : key === 'started' ? instant(b.started_at) : b[{ status: 'status', duration: 'duration_ms', rows: 'row_count' }[key]];
    const primary = av == null ? (bv == null ? 0 : 1) : bv == null ? -1 : sign * cmp(av, bv);
    return primary || (key === 'started' ? sign * cmp(a.id, b.id) : cmp(instant(b.started_at), instant(a.started_at)) || cmp(b.id, a.id));
  };
}
try {
  const corpusText = await fs.readFile(await inside(path.join(run, 'corpus.ndjson'), run), 'utf8');
  const corpus = corpusText.trim().split('\n').map(JSON.parse);
  assert.ok(corpus.length >= 11);
  corpus.forEach((e, i) => { assert.equal(e.experiment_run, instance.runId); assert.equal(e.experiment_seq, i); assert.equal(e.timestamp, '2026-01-01T12:00:00Z'); });
  report.corpus = { count: corpus.length, sha256: createHash('sha256').update(corpusText).digest('hex'), fieldsChecked: ['experiment_run', 'experiment_seq', 'timestamp'] };
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
  let key = (await fs.readFile(keyFile, 'utf8')).trim();
  const loginStatus = await page.evaluate(async api_key => (await fetch('/api/auth/login', { method: 'POST', redirect: 'error', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ api_key }) })).status, key);
  key = undefined;
  assert.equal(loginStatus, 200, 'login failed');
  await page.goto(`${origin.origin}/search`);
  const api = async (url, method = 'GET', body) => {
    check();
    assert.ok(url.startsWith('/api/v1/') && !url.startsWith('//'));
    const r = await page.evaluate(async ({ url, method, body }) => {
      const response = await fetch(url, { method, redirect: 'error', signal: AbortSignal.timeout(15000), headers: { 'Content-Type': 'application/json' }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
      return { status: response.status, body: response.ok ? await response.json() : null };
    }, { url, method, body });
    assert.ok(r.status >= 200 && r.status < 300, `API ${method} failed with ${r.status}`);
    return r.body;
  };
  const capture = async name => { await page.screenshot({ path: path.join(output, `${name}.png`), fullPage: true }); report.captures.push(`${name}.png`); };
  const base = `experiment_run="${instance.runId}" earliest="2026-01-01T00:00:00Z" latest="2026-01-02T00:00:00Z"`;
  const marker = randomBytes(5).toString('hex');
  const logPaths = (await fs.readdir(run)).filter(n => /^trawld.*\.log$/.test(n));
  assert.ok(logPaths.length);
  for (const [label, predicate, expected] of [['nonempty', 'experiment_seq<3', 3], ['empty', 'experiment_seq<0', 0]]) {
    phase = `search-${label}`;
    // The harmless inequality marks this DSL uniquely without excluding corpus rows.
    const query = `${base} ${predicate} message!="issue188-${marker}-${label}" | fields experiment_seq, status | sort experiment_seq`;
    const offsets = new Map(await Promise.all(logPaths.map(async n => [n, (await fs.stat(await inside(path.join(run, n), run))).size])));
    await page.locator('.dsl-editor .cm-content').click();
    await page.keyboard.press('Control+a');
    await page.keyboard.insertText(query);
    const pending = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/query' && r.request().method() === 'POST' && r.request().postDataJSON()?.query === query);
    const sent = Date.now();
    await page.keyboard.press('Control+Enter');
    const response = await pending;
    const received = Date.now();
    assert.equal(response.status(), 200);
    const result = await response.json();
    const request = response.request().postDataJSON();
    assert.equal(result.rows.length, expected);
    assert.equal(result.pagination.returned, expected);
    assert.equal(result.truncated, false);
    assert.ok(result.execution);
    const started = Number(instant(result.execution.started_at) / 1000000n);
    assert.ok(started >= sent - 2 && started <= received + 2, 'execution start outside browser request bounds');
    assert.ok(Number.isSafeInteger(result.execution.duration_ms) && result.execution.duration_ms >= 0);
    phase = `search-${label}-lifecycle`;
    let starts = [], ends = [];
    const flushDeadline = Date.now() + 5000;
    do {
    check();
    const lines = [];
    for (const name of logPaths) {
      const handle = await fs.open(await inside(path.join(run, name), run));
      try { const buffer = Buffer.alloc((await handle.stat()).size - offsets.get(name)); await handle.read(buffer, 0, buffer.length, offsets.get(name)); lines.push(...buffer.toString().split('\n').slice(0, -1)); } finally { await handle.close(); }
    }
    const events = lines.filter(line => /event_type="?query_(start|complete)\b/.test(line)).map(line => {
      const fields = Object.fromEntries([...line.matchAll(/\b(event_type|user|query_id|query_len|limit|offset|duration_ms|total_rows|returned_rows)=(?:"([^"]*)"|([^\s]+))/g)].map(m => [m[1], m[2] ?? m[3]]));
      fields.timestamp = line.match(/\d{4}-\d\d-\d\dT[\d:.]+(?:Z|\+00:00)/)?.[0];
      return fields;
    });
    starts = events.filter(e => e.event_type === 'query_start' && e.user === 'experiment-browser' && Number(e.query_len) === Buffer.byteLength(query) && Number(e.limit) === result.pagination.limit && Number(e.offset) === (request.offset ?? 0) && Date.parse(e.timestamp) >= sent - 2 && Date.parse(e.timestamp) <= received + 2);
    assert.ok(starts.length <= 1, 'ambiguous lifecycle start');
    ends = starts.length === 1 ? events.filter(e => e.event_type === 'query_complete' && e.query_id === starts[0].query_id && e.user === starts[0].user && e.query_len === starts[0].query_len) : [];
    assert.ok(ends.length <= 1, 'ambiguous lifecycle completion');
    if (starts.length === 1 && ends.length === 1) break;
    await delay(100);
    } while (Date.now() < flushDeadline);
    assert.equal(starts.length, 1, 'ambiguous or absent lifecycle start');
    assert.equal(ends.length, 1, 'ambiguous or absent lifecycle completion');
    assert.equal(Number(ends[0].duration_ms), result.execution.duration_ms);
    assert.equal(Number(ends[0].returned_rows), expected);
    assert.equal(Number(ends[0].total_rows), expected);
    assert.ok(started >= Date.parse(starts[0].timestamp) - 2 && started <= Date.parse(ends[0].timestamp) + 2);
    const utc = new Date(started).toISOString().slice(0, 19).replace('T', ' ') + ' UTC';
    await page.locator('.scope-count').filter({ hasText: `${expected} rows returned` }).waitFor();
    await page.locator('.scope-execution').filter({ hasText: duration(result.execution.duration_ms) }).waitFor();
    await page.locator('.scope-started').filter({ hasText: utc }).waitFor();
    // Extract only the three execution facts. The surrounding strip includes
    // filter chips whose text must not enter the sanitized JSON report.
    const visibleFacts = {
      count: (await page.locator('.scope-count').innerText()).match(/\b\d+ rows returned\b/)?.[0],
      execution: (await page.locator('.scope-execution').innerText()).match(/\b(?:\d+ms|\d+\.\d{3}s)\b/)?.[0],
      started: (await page.locator('.scope-started').innerText()).match(/\b\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} UTC\b/)?.[0],
    };
    assert.equal(visibleFacts.count, `${expected} rows returned`);
    assert.equal(visibleFacts.execution, duration(result.execution.duration_ms));
    assert.equal(visibleFacts.started, utc);
    report.search.push({ label, querySha256: createHash('sha256').update(query).digest('hex'), request: { queryLength: Buffer.byteLength(query), limit: request.limit, offset: request.offset ?? 0 }, sent, received, expectedRows: expected, execution: result.execution, observedLifecycleTotal: Number(ends[0].total_rows), returned: result.pagination.returned, lifecycle: [starts[0], ends[0]], visibleFacts });
    await capture(`search-${label}`);
  }
  phase = 'seed-nets';
  assert.equal((await api('/api/v1/runs?limit=20&offset=0')).total, 0, 'requires fresh run without existing saved runs');
  const nets = [];
  for (const [name, count] of [['Zulu', 2], ['alpha', 11], ['Bravo', 5]]) {
    const net = await api('/api/v1/saved', 'POST', { name: `${name}-issue188-${marker}`, query: `${base} experiment_seq<${count} | fields experiment_seq, status | sort experiment_seq` });
    await api(`/api/v1/saved/${net.id}/schedule`, 'PUT', { interval: '1h', enabled: true });
    nets.push({ id: net.id, name: net.name, count });
  }
  phase = 'individual-receipts';
  for (let i = 0; i < 21; i++) {
    const net = nets[i % nets.length];
    const trigger = await api(`/api/v1/saved/${net.id}/run`, 'POST');
    let receipt;
    const until = Date.now() + 30000;
    do { receipt = await api(`/api/v1/saved/${net.id}/runs/${trigger.id}`); if (receipt.status !== 'running') break; await delay(100); } while (Date.now() < until);
    assert.equal(receipt.status, 'success');
    assert.equal(receipt.row_count, net.count);
    assert.equal(receipt.result.rows.length, net.count);
    const seq = receipt.result.columns.findIndex(c => c.name === 'experiment_seq');
    assert.ok(seq >= 0);
    assert.deepEqual(receipt.result.rows.map(row => row[seq]), corpus.slice(0, net.count).map(e => e.experiment_seq));
    report.receipts.push(safeReceipt({ ...receipt, net_id: net.id, net_name: net.name }));
  }
  assert.equal(new Set(report.receipts.map(r => r.id)).size, 21);
  phase = 'global-orders';
  for (const sort of ['net', 'status', 'started', 'duration', 'rows']) for (const dir of ['asc', 'desc']) {
    const expected = [...report.receipts].sort(compare(sort, dir));
    const observed = [];
    for (const offset of [0, 20]) {
      const response = await api(`/api/v1/runs?limit=20&offset=${offset}&sort=${sort}&dir=${dir}`);
      assert.equal(response.total, 21);
      assert.deepEqual(response.runs.map(safeReceipt), expected.slice(offset, offset + 20));
      observed.push(...response.runs.map(r => r.id));
    }
    assert.equal(new Set(observed).size, 21);
    report.orders.push({ sort, dir, total: 21, expectedIds: expected.map(r => r.id), observedIds: observed });
  }
  phase = 'runs-ui';
  for (const [label, sort, first] of [['Net', 'net', 'asc'], ['Rows', 'rows', 'desc']]) {
    await page.goto(`${origin.origin}/jobs/runs`);
    const header = page.getByRole('columnheader', { name: new RegExp(`^${label}`) });
    await header.getByRole('button').click();
    for (const dir of [first, first === 'asc' ? 'desc' : 'asc']) {
      if (dir !== first) await header.getByRole('button').click();
      const expected = [...report.receipts].sort(compare(sort, dir));
      for (const offset of [0, 20]) {
        await page.waitForFunction(({ expected }) => {
          const rows = [...document.querySelectorAll('tr.tbl-row')].filter(r => r.querySelector('a[href*="/jobs/runs?"]'));
          return JSON.stringify(rows.map(r => Number(new URL(r.querySelector('a').href).searchParams.get('run')))) === JSON.stringify(expected);
        }, { expected: expected.slice(offset, offset + 20).map(r => r.id) });
        assert.equal(await header.getAttribute('aria-sort'), dir === 'asc' ? 'ascending' : 'descending');
        const rows = await page.locator('tr.tbl-row').filter({ has: page.locator('a[href*="/jobs/runs?"]') }).evaluateAll(rows => rows.map(row => ({ href: row.querySelector('a').getAttribute('href'), cells: [...row.querySelectorAll('td')].map(td => td.textContent.trim()) })));
        rows.forEach((row, i) => { const r = expected[offset + i]; const url = new URL(row.href, origin); assert.equal(Number(url.searchParams.get('net')), r.net_id); assert.equal(row.cells[0], r.net_name); assert.equal(row.cells[1].toLowerCase(), r.status); assert.equal(row.cells[3], duration(r.duration_ms)); assert.equal(row.cells[4], String(r.row_count)); });
        report.ui.push({ sort, dir, offset, expectedIds: expected.slice(offset, offset + 20).map(r => r.id), observed: rows });
        await capture(`runs-${sort}-${dir}-${offset}`);
        if (offset === 0) await page.getByRole('button', { name: /Next/ }).click();
      }
    }
  }
  report.status = 'passed';
} catch (error) {
  // Browser/assertion errors can embed request data. Publish only the phase.
  report.status = interrupted ? 'interrupted' : 'failed';
  report.failure = { phase, message: 'Assertion or operation failed; rerun this phase under local inspection. Raw errors intentionally omitted.' };
  if (error instanceof assert.AssertionError) {
    // Only numeric/boolean primitives are safe by construction. Strings and
    // objects may contain credentials or browser request details.
    const primitive = value => typeof value === 'boolean' || (typeof value === 'number' && Number.isFinite(value));
    report.failure.kind = 'assertion';
    if (primitive(error.actual)) report.failure.actual = error.actual;
    if (primitive(error.expected)) report.failure.expected = error.expected;
    report.failure.assertionLine = error.stack?.match(/issue188-evidence\.mjs:(\d+):/)?.[1];
  } else report.failure.kind = 'operation';
  process.exitCode = 1;
} finally {
  try { await context?.close(); await browser?.close(); report.cleanup.browser = true; }
  catch { report.status = 'failed'; report.cleanup.browser = false; process.exitCode = 1; }
  await fs.writeFile(path.join(output, 'report.json'), `${JSON.stringify(report, null, 2)}\n`, { mode: 0o600 });
  console.log(`${report.status}: ${path.join(output, 'report.json')}`);
}
