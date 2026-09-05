#!/usr/bin/env node
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, randomBytes, randomInt } from 'node:crypto';
import fs from 'node:fs/promises';
import { createWriteStream } from 'node:fs';
import https from 'node:https';
import http from 'node:http';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { parseArgs } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';
import { corpus, verifyRows, percentile } from './workload.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const { values: args } = parseArgs({ options: {
  help: { type: 'boolean' }, 'skip-build': { type: 'boolean' },
  events: { type: 'string', default: '1000' }, seed: { type: 'string', default: '42' },
  rate: { type: 'string', default: '200' }, 'batch-size': { type: 'string', default: '50' },
  'hold-seconds': { type: 'string', default: '0' },
  'web-port': { type: 'string' },
} });
if (args.help) {
  console.log(`Usage: bin/app-experiment [--events 1000] [--seed 42] [--rate 200]
       [--batch-size 50] [--hold-seconds 0] [--web-port PORT] [--skip-build]

Runs a disposable real Postgres + trawld + trawl-web + Chromium experiment.
Builds this checkout, sends seeded HTTP logs, verifies browser search/live tail,
compaction and restart, then removes owned processes, container and secrets.
Retains a private report directory under target/app-experiments/.
--rate is the target events/second, paced by batch; sends are never retried.
--hold-seconds keeps the verified instance open for bounded interactive use.
--skip-build requires this checkout's matching preparation manifest.
Requires Linux, Docker, Rust, Trunk, Node, and Playwright Chromium dependencies.
CARGO_TARGET_DIR selects Cargo's artifact directory; no dev profile is read.`);
  process.exit(0);
}
function integer(name, min, max) {
  const n = Number(args[name]);
  assert.ok(Number.isSafeInteger(n) && n >= min && n <= max, `--${name} must be ${min}..${max}`);
  return n;
}
const count = integer('events', 10, 20000);
const seed = integer('seed', 0, 4294967295);
const rate = integer('rate', 1, 100000);
const batchSize = integer('batch-size', 1, 1000);
const holdSeconds = integer('hold-seconds', 0, 3600);
assert.ok(batchSize < count, '--batch-size must be smaller than --events for live-tail verification');
assert.ok(count / rate <= 600, 'workload duration must not exceed 600 seconds');
// No bind-and-release port probe. The proxy itself claims this port, and a
// collision is a startup failure. We never accept another process's health.
const webPort = args['web-port'] ? integer('web-port', 1024, 65535) : randomInt(20000, 60000);
const target = path.resolve(root, process.env.CARGO_TARGET_DIR || 'target');
const artifacts = path.join(root, 'target/app-experiments');
const runId = `run-${Date.now()}-${randomBytes(5).toString('hex')}`;
const runDir = path.join(artifacts, runId);
process.umask(0o077);
await fs.mkdir(runDir, { recursive: true, mode: 0o700 });
const privateDir = path.join(runDir, 'private');
await fs.mkdir(privateDir, { mode: 0o700 });
const browserOrigin = `http://127.0.0.1:${webPort}`;
const container = `trawl-experiment-${randomBytes(12).toString('hex')}`;
const report = { schema: 1, runId, status: 'running', seed, count, rate, batchSize,
  host: { platform: os.platform(), architecture: os.arch(), cpuModel: os.cpus()[0]?.model, logicalCpus: os.cpus().length, totalMemoryBytes: os.totalmem() },
  build: {}, phases: [], ingest: { sent: 0, accepted: 0, rejected: 0, ambiguousBatches: 0 },
  cleanup: { processes: false, container: false, secrets: false } };
let interrupted = false;
let browser;
let containerAttempted = false;
const children = new Set();
const secretValues = [];
const baseEnv = Object.fromEntries(['PATH', 'HOME', 'USER', 'LANG', 'TMPDIR', 'RUSTUP_HOME', 'CARGO_HOME']
  .filter(k => process.env[k]).map(k => [k, process.env[k]]));
baseEnv.CARGO_TARGET_DIR = target;
baseEnv.NO_COLOR = 'true';
baseEnv.CARGO_BUILD_JOBS = process.env.CARGO_BUILD_JOBS || '4';
const clean = text => secretValues.reduce((s, secret) => s.split(secret).join('[redacted]'), String(text));
const writeJSON = (name, value) => fs.writeFile(path.join(runDir, name), `${JSON.stringify(value, null, 2)}\n`);
function checkInterrupted() { if (interrupted) throw new Error('interrupted'); }
async function pacedWait(ms) {
  const until = performance.now() + ms;
  while (performance.now() < until) {
    checkInterrupted();
    await delay(Math.min(200, Math.max(0, until - performance.now())));
  }
}
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => { interrupted = true; });

// Every command has a deadline. stdout is private until the caller elects to
// use it; tokens emitted by fleet-admin never reach logs or terminal output.
async function command(exe, argv, { cwd = root, env = {}, timeout = 120000, log } = {}) {
  // Always use the local daemon, even if the user's Docker context is remote.
  if (exe === 'docker') argv = ['--host', 'unix:///var/run/docker.sock', ...argv];
  const logStream = log ? createWriteStream(path.join(runDir, log), { mode: 0o600 }) : null;
  const child = spawn(exe, argv, { cwd, env: { ...baseEnv, ...env }, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  children.add(child);
  let stdout = '', stderr = '', exceeded = false, logError;
  logStream?.once('error', error => { logError = error; killGroup(child, 'SIGKILL'); });
  const append = (current, chunk) => (current + chunk.toString()).slice(-2 * 1024 * 1024);
  child.stdout.on('data', c => { stdout = append(stdout, c); logStream?.write(clean(c)); });
  child.stderr.on('data', c => { stderr = append(stderr, c); logStream?.write(clean(c)); });
  const timer = setTimeout(() => { exceeded = true; killGroup(child, 'SIGKILL'); }, timeout);
  const interruptTimer = setInterval(() => { if (interrupted) killGroup(child, 'SIGKILL'); }, 200);
  try {
    const code = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('close', resolve);
    });
    if (logError) throw logError;
    if (logStream) await new Promise((resolve, reject) => { logStream.once('error', reject); logStream.end(resolve); });
    checkInterrupted();
    assert.equal(code, 0, `${path.basename(exe)} failed${exceeded ? ' (deadline)' : ''}${log ? `; see ${log}` : `: ${clean(stderr).slice(-1500)}`}`);
    return stdout.trim();
  } finally { logStream?.end(); clearTimeout(timer); clearInterval(interruptTimer); children.delete(child); }
}
function killGroup(child, signal) {
  if (!child.pid) return;
  try { process.kill(-child.pid, signal); } catch (error) { if (error.code !== 'ESRCH') throw error; }
}
async function stop(child) {
  if (!child) return;
  if (child.exitCode !== null || child.signalCode !== null) { children.delete(child); return; }
  killGroup(child, 'SIGTERM');
  const deadline = Date.now() + 15000;
  while (child.exitCode === null && child.signalCode === null && Date.now() < deadline) await delay(100);
  if (child.exitCode === null && child.signalCode === null) {
    killGroup(child, 'SIGKILL');
    await new Promise(resolve => child.once('close', resolve));
  }
  children.delete(child);
}
async function daemon(exe, argv, name, env, readyPattern) {
  checkInterrupted();
  const logFile = path.join(runDir, `${name}.log`);
  const output = await fs.open(logFile, 'a', 0o600);
  const child = spawn(exe, argv, { cwd: root, env: { ...baseEnv, ...env, RUST_LOG: 'trawl_server=info,trawl_web=info,fleet_auth=info' }, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  children.add(child);
  report.processes ??= [];
  report.processes.push({ name, pid: child.pid });
  let text = '', error;
  child.once('error', e => { error = e; });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => {
    const value = clean(chunk.toString().replace(/\x1b\[[0-9;]*m/g, ''));
    text = (text + value).slice(-100000);
    // A dedicated descriptor keeps a long-lived daemon's logs off the heap.
    output.write(value).catch(e => { error = e; });
  });
  child.once('close', () => { output.close().catch(() => {}); });
  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) {
    checkInterrupted();
    if (error) throw error;
    assert.ok(child.exitCode === null && child.signalCode === null, `${name} exited during startup; see ${name}.log`);
    const match = text.match(readyPattern);
    if (match) return { child, match };
    await delay(100);
  }
  throw new Error(`${name} readiness deadline; see ${name}.log`);
}
async function request(url, { token, method = 'GET', body, origin } = {}) {
  checkInterrupted();
  const encoded = body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body);
  return new Promise((resolve, reject) => {
    const headers = { ...(token ? { authorization: `Bearer ${token}` } : {}),
      ...(origin ? { origin } : {}), ...(encoded ? { 'content-type': typeof body === 'string' ? 'application/x-ndjson' : 'application/json', 'content-length': Buffer.byteLength(encoded) } : {}) };
    const transport = url.startsWith('https:') ? https : http;
    const req = transport.request(url, { method, headers, rejectUnauthorized: false }, res => {
      let text = '';
      res.on('data', c => { text += c; if (text.length > 32 * 1024 * 1024) req.destroy(new Error('response exceeded 32 MiB')); });
      res.on('error', reject);
      res.on('end', () => {
        if (res.statusCode < 200 || res.statusCode >= 300) reject(new Error(`HTTP ${res.statusCode} ${new URL(url).pathname}: ${text.slice(0, 500)}`));
        else resolve({ text, status: res.statusCode });
      });
    });
    const timer = setTimeout(() => req.destroy(new Error('request deadline')), 30000);
    req.on('close', () => clearTimeout(timer));
    req.on('error', reject);
    if (encoded) req.write(encoded);
    req.end();
  });
}
async function poll(label, fn, timeout = 60000) {
  const start = performance.now();
  while (performance.now() - start < timeout) {
    checkInterrupted();
    if (await fn()) return;
    await delay(200);
  }
  throw new Error(`${label} deadline`);
}
async function fingerprint() {
  const tracked = (await command('git', ['ls-files', '--cached', '--others', '--exclude-standard', '-z'])).split('\0').filter(Boolean);
  const hash = createHash('sha256');
  for (const file of tracked.filter(f => f.startsWith('crates/') || f.startsWith('.cargo/') || f.startsWith('Cargo.') || f === 'rust-toolchain.toml' || f === 'bin/trawld-dev').sort()) { hash.update(file); hash.update(await fs.readFile(path.join(root, file))); }
  return hash.digest('hex');
}
async function prepare() {
  const sourceHash = await fingerprint();
  const stampPath = path.join(root, 'target/app-experiment-build.json');
  const binaryNames = ['trawld', 'trawl-web', 'fleet-admin', 'deps/libduckdb.so'];
  const hashes = async () => Object.fromEntries(await Promise.all(binaryNames.map(async name =>
    [name, createHash('sha256').update(await fs.readFile(path.join(target, 'debug', name))).digest('hex')])));
  if (args['skip-build']) {
    const stamp = JSON.parse(await fs.readFile(stampPath, 'utf8'));
    assert.equal(stamp.sourceHash, sourceHash, 'checkout changed: run without --skip-build');
    assert.equal(stamp.checkout, root, 'worktree moved: run without --skip-build');
    assert.equal(stamp.commit, await command('git', ['rev-parse', 'HEAD']), 'commit changed: run without --skip-build');
    assert.equal(stamp.target, target, 'Cargo target changed: run without --skip-build');
    assert.deepEqual(stamp.binaries, await hashes(), 'binaries changed: run without --skip-build');
    report.build = stamp;
  } else {
    console.log('Building daemons and SPA; build output goes into the run directory.');
    await command('cargo', ['build', '--locked', '--no-default-features', '-p', 'trawl-server', '-p', 'trawl-web', '-p', 'fleet-admin'], { timeout: 1800000, log: 'build.log' });
    await command('trunk', ['build', '--release'], { cwd: path.join(root, 'crates/trawl-web-ui'), timeout: 1800000, log: 'spa-build.log' });
    await command('npm', ['ci', '--no-audit', '--no-fund'], { cwd: path.join(root, 'crates/trawl-web-ui/e2e'), log: 'npm.log' });
    await command('node', ['node_modules/@playwright/test/cli.js', 'install', 'chromium'], { cwd: path.join(root, 'crates/trawl-web-ui/e2e'), timeout: 180000, log: 'browser-install.log' });
    report.build = { sourceHash, target, checkout: root, binaries: await hashes(), profile: 'debug server, release SPA, downloaded DuckDB',
      commit: await command('git', ['rev-parse', 'HEAD']), rustc: await command('rustc', ['--version']), node: process.version };
    await fs.mkdir(path.dirname(stampPath), { recursive: true });
    await fs.writeFile(stampPath, JSON.stringify(report.build));
  }
  // Record the bytes actually served, including JS/Wasm, so SPA identity is
  // checkable in every result and skip-build cannot silently reuse changed dist.
  async function treeHash(dir) {
    const hash = createHash('sha256');
    for (const entry of (await fs.readdir(dir, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name))) {
      hash.update(entry.name);
      hash.update(entry.isDirectory() ? await treeHash(path.join(dir, entry.name)) : await fs.readFile(path.join(dir, entry.name)));
    }
    return hash.digest('hex');
  }
  const spaHash = await treeHash(path.join(root, 'crates/trawl-web-ui/dist'));
  if (args['skip-build']) assert.equal(report.build.spaHash, spaHash, 'SPA changed: run without --skip-build');
  report.build.spaHash = spaHash;
  await fs.writeFile(stampPath, JSON.stringify(report.build));
}

async function experiment() {
  await prepare();
  report.runnerHash = createHash('sha256').update(await fs.readFile(fileURLToPath(import.meta.url))).update(await fs.readFile(path.join(root, 'scripts/app-experiment/workload.mjs'))).digest('hex');
  checkInterrupted();
  await command('docker', ['info', '--format', '{{.ServerVersion}}']);
  const password = randomBytes(24).toString('hex');
  secretValues.push(password);
  const pgEnvFile = path.join(privateDir, 'postgres.env');
  await fs.writeFile(pgEnvFile, `POSTGRES_USER=experiment\nPOSTGRES_PASSWORD=${password}\nPOSTGRES_DB=fleet\n`);
  containerAttempted = true;
  // No host volume and no shared container name. Docker owns the ephemeral
  // loopback port until the container is removed. Keep fsync enabled.
  await command('docker', ['run', '--detach', '--name', container, '--label', `trawl.experiment=${runId}`,
    '--publish', '127.0.0.1::5432', '--env-file', pgEnvFile, '--memory', '1g', '--cpus', '2', '--tmpfs', '/var/lib/postgresql:size=512m', 'postgres:18'], { log: 'postgres-start.log' });
  report.postgresImage = await command('docker', ['inspect', '--format', '{{.Image}}', container]);
  const mapping = await command('docker', ['port', container, '5432/tcp']);
  assert.match(mapping, /^127\.0\.0\.1:\d+$/);
  const fleetDsn = `postgres://experiment:${password}@${mapping}/fleet`;
  const appDsn = `postgres://experiment:${password}@${mapping}/trawl`;
  secretValues.push(fleetDsn, appDsn);
  await poll('Postgres readiness', async () => {
    try { await command('docker', ['exec', container, 'pg_isready', '-U', 'experiment', '-d', 'fleet'], { timeout: 5000 }); return true; }
    catch (e) { checkInterrupted(); return false; }
  });
  await command('docker', ['exec', container, 'createdb', '-U', 'experiment', 'trawl']);
  const admin = path.join(target, 'debug/fleet-admin');
  const fleetEnv = { DATABASE_URL: fleetDsn };
  await command(admin, ['migrate'], { env: fleetEnv });
  for (const [role, perms] of [['experiment-reader', ['query', 'schema_read', 'validate', 'saved_query', 'export', 'stream', 'query_cancel']], ['experiment-ingest', ['ingest']]]) {
    await command(admin, ['roles', 'create', '--name', role, '--rate-rpm', '100000', ...perms.flatMap(p => ['--perm', `trawl:${p}`])], { env: fleetEnv });
  }
  const reader = await command(admin, ['keys', 'create', '--name', 'experiment-browser', '--kind', 'human', '--role', 'experiment-reader', '--expires', '2h'], { env: fleetEnv });
  secretValues.push(reader);
  const writer = await command(admin, ['keys', 'create', '--name', 'experiment-sender', '--kind', 'service', '--role', 'experiment-ingest', '--expires', '2h'], { env: fleetEnv });
  secretValues.push(writer);
  const sessionKey = await command(admin, ['generate-session-key']);
  secretValues.push(sessionKey);
  await fs.writeFile(path.join(privateDir, 'browser-key'), `${reader}\n`);
  const configPath = path.join(privateDir, 'trawld.toml');
  const dataDir = path.join(privateDir, 'data');
  const toml = value => JSON.stringify(value);
  const serverConfig = `[server]\nhttp_addr = "127.0.0.1:0"\nmax_result_rows = 25000\nmax_concurrent_queries = 2\nshutdown_drain_secs = 3\nquery_log = ${toml(path.join(runDir, 'queries.ndjson'))}\n[data]\npath = ${toml(dataDir)}\n[ingest]\nenabled = true\ninternal_telemetry = false\ndaily_rollup = false\ncompaction_interval_secs = 10\ncompaction_chunk_size = 500\ndefault_env = "experiment"\nenvs = ["experiment"]\n[retention]\nmax_age_days = 0\nmin_free_disk_bytes = 0\n[scheduler]\nenabled = false\n[syslog]\nenabled = false\n`;
  await fs.writeFile(configPath, serverConfig);
  await fs.writeFile(path.join(runDir, 'config.toml'), serverConfig);
  const daemonEnv = { FLEET_DATABASE_URL: fleetDsn, TRAWL_DATABASE_URL: appDsn };
  let server;
  let upstream;
  async function startServer(name) {
    server = await daemon(path.join(root, 'bin/trawld-dev'), ['--config', configPath], name, daemonEnv,
      /HTTPS server listening[^\n]*addr[=:]\s*"?(127\.0\.0\.1:\d+)/);
    upstream = `https://${server.match[1]}`;
    await request(`${upstream}/api/v1/health`);
  }
  await startServer('trawld');
  const webConfig = path.join(privateDir, 'web.toml');
  async function startWeb(name) {
    await fs.writeFile(webConfig, `${serverConfig}\n[web]\nbind_addr = "127.0.0.1:${webPort}"\nupstream_url = ${toml(upstream)}\nallow_insecure_cookies = true\npublic_origins = [${toml(browserOrigin)}]\ncookie_secret_env = "FLEET_SESSION_AEAD_KEY"\n`);
    return daemon(path.join(target, 'debug/trawl-web'), ['--config', webConfig], name,
      { FLEET_SESSION_AEAD_KEY: sessionKey, TRAWL_WEB_INSECURE_UPSTREAM: '1', TRAWL_WEB_SPA_DIR: path.join(root, 'crates/trawl-web-ui/dist') }, /trawl-web listening/);
  }
  let web = await startWeb('trawl-web');
  await writeJSON('instance.json', { runId, browserOrigin, upstream, container, pids: { trawld: server.child.pid, web: web.child.pid }, browserKeyFile: path.join(privateDir, 'browser-key') });
  console.log(`Instance ready: ${browserOrigin}. Artifacts: ${runDir}`);
  const events = corpus(seed, count, runId);
  const ndjson = events.map(e => JSON.stringify(e)).join('\n') + '\n';
  await fs.writeFile(path.join(runDir, 'corpus.ndjson'), ndjson);
  report.corpusHash = createHash('sha256').update(ndjson).digest('hex');
  const queryText = `experiment_run="${runId}" earliest="2026-01-01T00:00:00Z" latest="2026-01-02T00:00:00Z" | fields experiment_seq, status | sort experiment_seq`;
  const latencies = [];
  async function query() {
    const start = performance.now();
    const response = JSON.parse((await request(`${upstream}/api/v1/query`, { token: reader, method: 'POST', body: { query: queryText, limit: 25000 } })).text);
    latencies.push(performance.now() - start);
    return response;
  }
  async function verify(label, expected) {
    const response = await query();
    await writeJSON(`${label}.json`, response);
    verifyRows(response, expected);
    const digest = createHash('sha256').update(JSON.stringify({ columns: response.columns, rows: response.rows })).digest('hex');
    const procStatus = await fs.readFile(`/proc/${server.child.pid}/status`, 'utf8');
    const memory = Object.fromEntries(procStatus.split('\n').filter(line => /^(VmRSS|VmHWM|Threads):/.test(line)).map(line => line.split(':').map(s => s.trim())));
    report.phases.push({ name: label, verifiedEvents: expected.length, resultHash: digest, process: memory });
    await fs.writeFile(path.join(runDir, `${label}.prom`), (await request(`${upstream}/metrics`)).text);
    console.log(`${label}: verified ${expected.length} exact event IDs and values.`);
  }
  async function send(batch) {
    report.ingest.sent += batch.length;
    let response;
    try {
      response = JSON.parse((await request(`${upstream}/api/v1/ingest`, { token: writer, method: 'POST', body: batch.map(e => JSON.stringify(e)).join('\n') + '\n' })).text);
    } catch (error) { report.ingest.ambiguousBatches++; throw error; }
    report.ingest.accepted += response.accepted;
    report.ingest.rejected += response.rejected || 0;
    assert.equal(response.accepted, batch.length, 'batch was not fully accepted');
    assert.equal(response.rejected || 0, 0, 'unexpected rejected events');
  }
  // The first batch proves visibility before the first periodic compaction.
  const firstCount = Math.min(batchSize, count);
  await send(events.slice(0, firstCount));
  await verify('initial', events.slice(0, firstCount));
  const require = createRequire(path.join(root, 'crates/trawl-web-ui/e2e/package.json'));
  const { chromium } = require('@playwright/test');
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, locale: 'en-US', timezoneId: 'UTC', reducedMotion: 'reduce' });
  await context.addInitScript(() => {
    window.experimentStreams = [];
    const Original = window.EventSource;
    window.EventSource = class extends Original {
      constructor(...args) {
        super(...args);
        const record = { url: String(args[0]), closed: false, ids: [], lagged: false };
        window.experimentStreams.push(record);
        this.addEventListener('data', event => {
          const row = JSON.parse(event.data);
          if (row.experiment_seq !== undefined) record.ids.push(row.experiment_seq);
        });
        this.addEventListener('lagged', () => { record.lagged = true; });
        const close = this.close.bind(this);
        this.close = () => { record.closed = true; close(); };
      }
    };
  });
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  const pageErrors = [];
  page.on('pageerror', e => pageErrors.push(clean(e.message)));
  try {
    await page.goto(`${browserOrigin}/login`);
    await page.getByLabel('API key').fill(reader);
    await page.getByRole('button', { name: 'Sign In', exact: true }).click();
    await page.waitForURL('**/search');
    // Start tracing only after login; a login trace would retain the API key.
    await context.tracing.start({ screenshots: true, snapshots: true });
    const editor = page.locator('.dsl-editor .cm-content');
    await editor.click();
    await page.keyboard.press('Control+a');
    await page.keyboard.insertText(queryText);
    const rendered = page.waitForResponse(r => r.url().endsWith('/api/v1/query') && r.request().method() === 'POST');
    await page.keyboard.press('Control+Enter');
    const initialResponse = await rendered;
    const initialPage = await initialResponse.json();
    await writeJSON('browser-initial-response.json', initialPage);
    assert.equal(initialResponse.status(), 200, 'browser query failed; see browser-initial-response.json');
    assert.ok(initialPage.pagination.returned > 0, 'browser search returned no rows');
    verifyRows(initialPage, events.slice(0, Math.min(firstCount, initialPage.pagination.returned)));
    await page.locator('.results table tbody tr').first().waitFor();
    const firstCells = await page.locator('.results table tbody tr').first().locator('td').allTextContents();
    assert.deepEqual(firstCells.slice(1).map(c => c.trim()), initialPage.rows[0].map(String), 'rendered cells differ from the query response');
    await page.screenshot({ path: path.join(runDir, 'search.png') });
    report.phases.push({ name: 'browser-login-search', verifiedEvents: initialPage.pagination.returned });
    // Drive real UI controls to open live tail, then stream the remainder.
    await editor.click();
    await page.keyboard.press('Control+a');
    await page.keyboard.insertText(queryText.split(' | ')[0]);
    await page.locator('.daterange .dr-trigger').click();
    await page.locator('.dr-pop').getByText('Real-time', { exact: true }).click();
    const streamReady = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/stream');
    await page.locator('.rt-hint button').click();
    assert.equal((await streamReady).status(), 200);
    const ingestStart = performance.now();
    for (let offset = firstCount; offset < count; offset += batchSize) {
      const due = (Math.min(offset + batchSize, count) - firstCount) * 1000 / rate;
      await pacedWait(Math.max(0, due - (performance.now() - ingestStart)));
      await send(events.slice(offset, offset + batchSize));
      // Concurrent query work checks the partial corpus while ingest continues.
      verifyRows(await query(), events.slice(0, Math.min(offset + batchSize, count)));
    }
    report.ingest.durationMs = performance.now() - ingestStart;
    report.ingest.pacedEvents = count - firstCount;
    const sentinel = `synthetic-event-${count - 1}`;
    await page.getByText(new RegExp(`${sentinel}\\b`)).first().waitFor();
    await page.screenshot({ path: path.join(runDir, 'live-tail.png') });
    const streams = await page.evaluate(() => window.experimentStreams.filter(s => s.url.includes('/api/v1/stream')));
    assert.equal(streams.length, 1, 'expected exactly one live-tail stream');
    assert.equal(streams[0].lagged, false, 'live tail lagged');
    assert.deepEqual(streams[0].ids.sort((a, b) => a - b), events.slice(firstCount).map(e => e.experiment_seq), 'live tail lost or duplicated events');
    report.phases.push({ name: 'browser-live-tail', verifiedEvents: count - firstCount });
    await page.locator('nav.rail a[title="History"]').click();
    await page.getByRole('heading', { name: 'Search history', exact: true }).waitFor();
    await page.waitForFunction(() => window.experimentStreams.filter(s => s.url.includes('/api/v1/stream')).every(s => s.closed));
    report.phases.push({ name: 'browser-live-tail-closed' });
    await verify('ingested', events);
    await poll('compaction and hot-buffer drain', async () => {
      const metrics = (await request(`${upstream}/metrics`)).text;
      return /^trawl_hot_buffer_events 0$/m.test(metrics) && (await fs.readdir(path.join(dataDir, 'experiment'), { recursive: true })).some(p => p.endsWith('.parquet'));
    });
    await verify('compacted', events);
    // Stop the proxy first so it cannot send requests into the restarting daemon.
    await stop(web.child);
    await stop(server.child);
    await startServer('trawld-restart');
    web = await startWeb('trawl-web-restart');
    await verify('restarted', events);
    await page.goto(`${browserOrigin}/search`);
    await editor.click();
    await page.keyboard.press('Control+a');
    await page.keyboard.insertText(queryText);
    const restartedResponse = page.waitForResponse(r => r.url().endsWith('/api/v1/query') && r.request().method() === 'POST');
    await page.keyboard.press('Control+Enter');
    const afterRestartResponse = await restartedResponse;
    const restartedPage = await afterRestartResponse.json();
    await writeJSON('browser-restarted-response.json', restartedPage);
    assert.equal(afterRestartResponse.status(), 200, 'browser query failed; see browser-restarted-response.json');
    assert.ok(restartedPage.pagination.returned > 0, 'browser search returned no rows after restart');
    verifyRows(restartedPage, events.slice(0, restartedPage.pagination.returned));
    await page.locator('.results table tbody tr').first().waitFor();
    const restartedCells = await page.locator('.results table tbody tr').first().locator('td').allTextContents();
    assert.deepEqual(restartedCells.slice(1).map(c => c.trim()), restartedPage.rows[0].map(String), 'rendered cells differ after restart');
    await page.screenshot({ path: path.join(runDir, 'restarted.png') });
    assert.deepEqual(pageErrors, [], 'browser JavaScript errors');
    report.phases.push({ name: 'browser-session-after-restart', verifiedEvents: restartedPage.pagination.returned });
    report.queryLatencyMs = { values: latencies, samples: latencies.length, p50: percentile(latencies, 0.5), p95: percentile(latencies, 0.95), max: Math.max(...latencies) };
    await writeJSON('instance.json', { runId, browserOrigin, upstream, container, pids: { trawld: server.child.pid, web: web.child.pid }, browserKeyFile: path.join(privateDir, 'browser-key') });
    if (holdSeconds) {
      console.log(`Verified instance held for ${holdSeconds}s at ${browserOrigin}; login key is in ${privateDir}/browser-key.`);
      const until = Date.now() + holdSeconds * 1000;
      while (Date.now() < until) { checkInterrupted(); await delay(200); }
    }
  } catch (error) {
    await page.screenshot({ path: path.join(runDir, 'failure.png') }).catch(() => {});
    throw error;
  } finally {
    await context.tracing.stop({ path: path.join(runDir, 'browser-trace.zip') }).catch(() => {});
    await writeJSON('browser-errors.json', pageErrors);
    await browser.close();
    browser = undefined;
  }
}

console.log(`Experiment ${runId}. Private artifacts: ${runDir}`);
try {
  await experiment();
  report.status = 'passed';
} catch (error) {
  report.status = interrupted ? 'interrupted' : 'failed';
  report.error = clean(error.stack || error.message);
  console.error(clean(error.message));
  process.exitCode = 1;
} finally {
  // Cleanup runs even after a failed startup or a signal. No database sweep,
  // volume prune, or PID-file based kill ever touches someone else's instance.
  if (browser) await browser.close().catch(() => {});
  for (const child of [...children].reverse()) await stop(child);
  report.cleanup.processes = [...children].every(c => c.exitCode !== null || c.signalCode !== null);
  const wasInterrupted = interrupted;
  interrupted = false;
  if (containerAttempted) {
    try {
      // run may have created the container before its client failed. Reconcile
      // that exact random name and ownership label before removing anything.
      const owner = await command('docker', ['inspect', '--format', '{{index .Config.Labels "trawl.experiment"}}', container]);
      assert.equal(owner, runId, 'container owner mismatch');
      try {
        await command('docker', ['logs', container], { log: 'postgres.log', timeout: 10000 });
      } catch (error) {
        // Diagnostic failure must never skip removal of an owned container.
        report.diagnosticError = clean(error.message);
        report.status = 'failed';
        process.exitCode = 1;
      }
      await command('docker', ['rm', '--force', container], { timeout: 30000 });
      report.cleanup.container = true;
    } catch (error) {
      report.cleanup.error = clean(error.message);
      report.status = 'failed';
      process.exitCode = 1;
    }
  } else report.cleanup.container = true;
  await fs.rm(privateDir, { recursive: true, force: true });
  report.cleanup.secrets = true;
  report.endedAt = new Date().toISOString();
  report.interrupted = wasInterrupted;
  await writeJSON('report.json', report);
  console.log(`${report.status}: ${path.join(runDir, 'report.json')}`);
}
