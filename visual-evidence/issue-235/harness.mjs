#!/usr/bin/env node
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Bounded search for the #235 500 (`last=15m service=coredns | head 20`).
//
// Disposable infrastructure only: one owned postgres:18 container on a
// loopback port, a release trawld from this checkout, a private data
// directory. Never reads a saved CLI profile or touches a live server.
//
//   node harness.mjs setup                 # pg + keys + seeded corpus
//   node harness.mjs scenario <a..g> [--minutes 30] [--max-queries 10000]
//   node harness.mjs report                # README markdown on stdout
//   node harness.mjs teardown              # stop trawld, remove pg + secrets
//
// Build first:  CARGO_TARGET_DIR=... cargo build --release --locked \
//                 -p trawl-server -p fleet-admin
// HARNESS_DIR (default <target>/issue-235-harness) holds keys, DSNs, data
// and raw logs; it is private and removed by teardown. Result summaries
// (no secrets) land in results/ beside this script.
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../..');
const target = path.resolve(root, process.env.CARGO_TARGET_DIR || 'target');
const release = path.join(target, 'release');
const stateDir = process.env.HARNESS_DIR || path.join(target, 'issue-235-harness');
const statePath = path.join(stateDir, 'state.json');
const resultsDir = path.join(here, 'results');
const QUERY = 'last=15m service=coredns | head 20';
const WINDOW_MS = 15 * 60 * 1000;
const docker = (...argv) => execFileSync('docker', ['--host', 'unix:///var/run/docker.sock', ...argv], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();

// ---------------------------------------------------------------- scenarios
// Every scenario runs the same query loop. The knobs below are the whole
// difference between them; `compaction` is ingest.compaction_interval_secs.
const SCENARIOS = {
  a: { title: 'baseline: queries only', compaction: 300 },
  b: { title: 'coredns hour-file merge compaction publishing during queries', compaction: 5, coredns: 120 },
  c: { title: 'concurrent compaction of another service (machined)', compaction: 5, machined: 60 },
  d: { title: 'SSE live tail on service=coredns at ~100 ev/min', compaction: 300, coredns: 100 / 60, sse: 'service=coredns' },
  e: { title: 'scheduled nets on a shortened interval', compaction: 300, nets: true },
  f: { title: 'all together', compaction: 5, coredns: 120, machined: 60, sse: 'service=coredns rcode=SERVFAIL', nets: true },
  g: { title: 'postgres paused (docker pause) for 2-8 s intervals', compaction: 5, coredns: 120, pause: true },
};
const CONCURRENCY = 3;
// The event type the persisted read-back is controlled with (see scenario).
const READ_BACK_CONTROL = 'pool_acquired';
const SERVFAIL_FRACTION = 1 / 72; // 120 ev/s * 1/72 = 100 ev/min on the f tail

function config(s, name) {
  const t = JSON.stringify;
  return `[server]
http_addr = "127.0.0.1:0"
tls_cert_path = ${t(tlsCert)}
tls_key_path = ${t(tlsKey)}
max_result_rows = 25000
shutdown_drain_secs = 3
[server.rate_limit]
default_rpm = 0
ingest_rpm = 0
[data]
path = ${t(path.join(stateDir, 'data'))}
[ingest]
enabled = true
internal_telemetry = true
compaction_interval_secs = ${s.compaction}
default_env = "lab"
envs = ["lab"]
[retention]
max_age_days = 0
min_free_disk_bytes = 0
[scheduler]
enabled = ${s.nets ? 'true' : 'false'}
poll_interval_secs = 1
[syslog]
enabled = false
`;
}

// ---------------------------------------------------------------- plumbing
// The disposable trawld serves a certificate this harness generates at
// setup (IP SAN 127.0.0.1, the only host it connects to). Every request
// trusts exactly that certificate, with validation on.
const tlsDir = path.join(stateDir, 'tls');
const tlsCert = path.join(tlsDir, 'cert.pem');
const tlsKey = path.join(tlsDir, 'key.pem');
function makeServerCert() {
  fs.mkdirSync(tlsDir, { recursive: true, mode: 0o700 });
  execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes',
    '-keyout', tlsKey, '-out', tlsCert, '-days', '2', '-subj', '/CN=trawl-i235-harness',
    '-addext', 'subjectAltName=IP:127.0.0.1'], { stdio: ['ignore', 'ignore', 'pipe'] });
  fs.chmodSync(tlsKey, 0o600);
}
let pinnedCa;
const ca = () => (pinnedCa ??= fs.readFileSync(tlsCert));
let sharedAgent;
const agent = () => (sharedAgent ??= new https.Agent({ keepAlive: true, maxSockets: 32, ca: ca(), rejectUnauthorized: true }));
function request(base, route, { token, method = 'GET', body, ndjson, timeout = 40000 } = {}) {
  const encoded = ndjson ?? (body === undefined ? undefined : JSON.stringify(body));
  return new Promise((resolve) => {
    const headers = { authorization: `Bearer ${token}` };
    if (encoded !== undefined) {
      headers['content-type'] = ndjson ? 'application/x-ndjson' : 'application/json';
      headers['content-length'] = Buffer.byteLength(encoded);
    }
    // A response whose headers arrived keeps its status and request id
    // even when its body is cut, whichever of the response or request
    // error handlers sees the failure first (a timeout destroys the
    // request): `incomplete` marks it, and `status` 0 means no response
    // arrived at all.
    let got = null;
    const cut = error => (got ? { ...got(), incomplete: true, error } : { status: 0, error });
    const req = https.request(base + route, { method, headers, agent: agent() }, res => {
      const chunks = [];
      got = () => ({ status: res.statusCode, requestId: res.headers['x-request-id'], text: Buffer.concat(chunks).toString() });
      res.on('data', c => chunks.push(c));
      res.on('error', e => resolve(cut(e.message)));
      res.on('aborted', () => resolve(cut('response aborted')));
      res.on('end', () => resolve(res.complete ? got() : cut('response ended early')));
    });
    req.setTimeout(timeout, () => req.destroy(new Error('client deadline')));
    req.on('error', e => resolve(cut(e.message)));
    if (encoded !== undefined) req.write(encoded);
    req.end();
  });
}
const loadState = () => JSON.parse(fs.readFileSync(statePath, 'utf8'));
const saveState = s => fs.writeFileSync(statePath, JSON.stringify(s, null, 2), { mode: 0o600 });
function pctl(values, f) { const s = [...values].sort((x, y) => x - y); return s.length ? Math.round(s[Math.max(0, Math.ceil(s.length * f) - 1)]) : null; }

async function startTrawld(st, name, s) {
  const cfg = path.join(stateDir, `${name}.toml`);
  fs.writeFileSync(cfg, config(s, name), { mode: 0o600 });
  const out = fs.openSync(path.join(stateDir, 'logs', `${name}.stdout`), 'a', 0o600);
  const child = spawn(path.join(release, 'trawld'), ['--config', cfg, '--no-monitor'], {
    env: { PATH: process.env.PATH, HOME: process.env.HOME, NO_COLOR: '1', LD_LIBRARY_PATH: path.join(release, 'deps'),
      FLEET_DATABASE_URL: st.fleetDsn, TRAWL_DATABASE_URL: st.appDsn },
    detached: true, stdio: ['ignore', out, out] });
  st.trawldPid = child.pid; saveState(st);
  const log = path.join(stateDir, 'logs', `${name}.stdout`);
  for (let i = 0; i < 600; i++) {
    await delay(100);
    assert.ok(child.exitCode === null, `trawld exited during startup; see ${log}`);
    const m = fs.readFileSync(log, 'utf8').replace(/\x1b\[[0-9;]*m/g, '').match(/HTTPS server listening[^\n]*addr[=:]\s*"?(127\.0\.0\.1:\d+)/g);
    if (m) {
      const addr = m[m.length - 1].match(/127\.0\.0\.1:\d+/)[0];
      const base = `https://${addr}`;
      const h = await request(base, '/api/v1/health', { token: 'none' });
      if (h.status === 200) return { child, base };
    }
  }
  throw new Error('trawld readiness deadline');
}
async function stopTrawld(st, child) {
  if (!st.trawldPid) return;
  const pid = st.trawldPid;
  const alive = () => { try { process.kill(pid, 0); return fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').includes('trawld'); } catch { return false; } };
  if (alive()) process.kill(pid, 'SIGTERM');
  for (let i = 0; i < 200 && alive(); i++) await delay(100);
  if (alive()) process.kill(pid, 'SIGKILL');
  for (let i = 0; i < 50 && alive(); i++) await delay(100);
  if (alive()) return; // leave the pid recorded so teardown can report it
  if (child && child.exitCode === null) await new Promise(r => child.once('exit', r));
  delete st.trawldPid; saveState(st);
}

// ---------------------------------------------------------------- workload
let seq = 0;
const RUN = randomBytes(4).toString('hex');
const QTYPES = ['A', 'AAAA', 'PTR', 'SRV', 'TXT', 'HTTPS'];
function coredns(ts, servfail) {
  const n = seq++;
  const rcode = servfail ? 'SERVFAIL' : (n % 9 === 0 ? 'NXDOMAIN' : 'NOERROR');
  return { timestamp: new Date(ts).toISOString(), env: 'lab', service: 'coredns', host: `dns-${n % 3}`, level: rcode === 'SERVFAIL' ? 'error' : 'info',
    qname: `svc-${n % 211}.lab.internal.`, qtype: QTYPES[n % 6], rcode, duration_ms: (n * 37) % 250, client: `10.0.${n % 7}.${n % 250}`,
    harness_run: RUN, harness_seq: n, message: `[INFO] 10.0.${n % 7}.${n % 250} - ${n} "${QTYPES[n % 6]} IN svc-${n % 211}.lab.internal. udp" ${rcode}` };
}
function machined(ts) {
  const n = seq++;
  return { timestamp: new Date(ts).toISOString(), env: 'lab', service: 'machined', host: `node-${n % 4}`, level: 'info',
    controller: `ctrl-${n % 13}`, phase: n % 5, harness_run: RUN, harness_seq: n, message: `controller ctrl-${n % 13} reconciled resource ${n}` };
}
// Accepted coredns timestamps, per second, for the lower-bound content check.
const sentPerSec = new Map();
function noteSent(events) { for (const e of events) if (e.service === 'coredns') { const s = Math.floor(Date.parse(e.timestamp) / 1000); sentPerSec.set(s, (sentPerSec.get(s) || 0) + 1); } }
function sentBetween(fromMs, toMs) { let n = 0; for (const [s, c] of sentPerSec) if (s * 1000 >= fromMs && s * 1000 + 999 <= toMs) n += c; return n; }

async function ingest(base, st, events, stats, retry = false) {
  for (let attempt = 0; ; attempt++) {
    const r = await request(base, '/api/v1/ingest', { token: st.writer, method: 'POST', ndjson: events.map(e => JSON.stringify(e)).join('\n') + '\n' });
    if (r.status === 200 && !r.incomplete) { noteSent(events); if (stats) stats.accepted += events.length; return r; }
    if (stats) { stats.failed++; stats.statuses[r.status] = (stats.statuses[r.status] || 0) + 1; }
    if (!retry || attempt > 20) return r;
    await delay(500 * (attempt + 1));
  }
}
function ingestLoop(base, st, rate, make, stats, stop) {
  // One batch per second (or one event per 1/rate s when rate < 1/s).
  return (async () => {
    const period = rate >= 1 ? 1000 : 1000 / rate;
    let next = performance.now();
    while (!stop.done) {
      const now = Date.now();
      const n = rate >= 1 ? Math.round(rate) : 1;
      const batch = Array.from({ length: n }, (_, i) => make(now - (n > 1 ? Math.floor(1000 * i / n) : 0), i));
      await ingest(base, st, batch, stats);
      next += period;
      await delay(Math.max(0, next - performance.now()));
    }
  })();
}

// ---------------------------------------------------------------- checks
function checkResult(r, sentAt, recvAt) {
  const problems = [];
  let body;
  try { body = JSON.parse(r.text); } catch { return { problems: ['unparseable body'], owed: 0 }; }
  const cols = (body.result?.columns ?? body.columns ?? []).map(c => c.name);
  const rows = body.result?.rows ?? body.rows;
  if (!Array.isArray(rows)) return { problems: ['no rows array'], owed: 0 };
  if (rows.length > 20) problems.push(`rows ${rows.length} > 20`);
  if (body.pagination && body.pagination.returned !== rows.length) problems.push('pagination.returned != rows.length');
  const si = cols.indexOf('service'), ti = cols.indexOf('_time');
  if (rows.length && si < 0) problems.push('no service column');
  if (rows.length && ti < 0) problems.push('no _time column');
  for (const row of rows) {
    if (si >= 0 && row[si] !== 'coredns') { problems.push(`service ${JSON.stringify(row[si])}`); break; }
    if (ti >= 0) {
      // DuckDB renders UTC as `YYYY-MM-DD HH:MM:SS.fff` with no zone.
      const t = Date.parse(/[zZ]|[+-]\d\d:?\d\d$/.test(row[ti]) ? row[ti] : `${String(row[ti]).replace(' ', 'T')}Z`);
      if (!Number.isFinite(t)) { problems.push(`_time unparseable ${JSON.stringify(row[ti])}`); break; }
      if (t < sentAt - WINDOW_MS - 5000 || t > recvAt + 5000) { problems.push(`_time ${row[ti]} outside window`); break; }
    }
  }
  // Events this harness had accepted, strictly inside the window, before
  // the query was sent: the answer owes min(20, that many) rows.
  const owed = Math.min(20, sentBetween(recvAt - WINDOW_MS + 10000, sentAt - 2000));
  if (rows.length < owed) problems.push(`rows ${rows.length} < ${owed} owed`);
  return { problems, owed };
}

// ---------------------------------------------------------------- commands
async function setup({ seed = true } = {}) {
  assert.ok(!fs.existsSync(statePath), `${statePath} exists: run teardown first`);
  for (const bin of ['trawld', 'fleet-admin', 'deps/libduckdb.so']) assert.ok(fs.existsSync(path.join(release, bin)), `missing ${release}/${bin}: build first`);
  fs.mkdirSync(path.join(stateDir, 'logs'), { recursive: true, mode: 0o700 });
  fs.chmodSync(stateDir, 0o700);
  const container = `trawl-i235-${randomBytes(6).toString('hex')}`;
  const password = randomBytes(24).toString('hex');
  const st = { container, created: new Date().toISOString() };
  // State first, so teardown can find and remove everything after this,
  // the generated key included.
  saveState(st);
  try { makeServerCert(); } catch (e) { fs.rmSync(tlsDir, { recursive: true, force: true }); throw e; }
  const envFile = path.join(stateDir, 'pg.env');
  fs.writeFileSync(envFile, `POSTGRES_USER=h\nPOSTGRES_PASSWORD=${password}\nPOSTGRES_DB=fleet\n`, { mode: 0o600 });
  docker('run', '--detach', '--name', container, '--label', 'trawl.issue-235-harness=1', '--publish', '127.0.0.1::5432',
    '--env-file', envFile, '--memory', '2g', '--tmpfs', '/var/lib/postgresql:size=1g', 'postgres:18');
  fs.rmSync(envFile);
  const mapping = docker('port', container, '5432/tcp').split('\n')[0];
  st.fleetDsn = `postgres://h:${password}@${mapping}/fleet`;
  st.appDsn = `postgres://h:${password}@${mapping}/trawl`;
  saveState(st);
  for (let i = 0; ; i++) {
    try { docker('exec', container, 'pg_isready', '-h', '127.0.0.1', '-U', 'h', '-d', 'fleet'); break; } catch { assert.ok(i < 120, 'pg readiness'); await delay(500); }
  }
  docker('exec', container, 'createdb', '-U', 'h', 'trawl');
  const admin = (...a) => execFileSync(path.join(release, 'fleet-admin'), a, { env: { PATH: process.env.PATH, DATABASE_URL: st.fleetDsn }, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  admin('migrate');
  admin('roles', 'create', '--name', 'h-reader', '--rate-rpm', '1000000', ...['query', 'schema_read', 'validate', 'saved_query', 'stream', 'query_cancel'].flatMap(p => ['--perm', `trawl:${p}`]));
  admin('roles', 'create', '--name', 'h-ingest', '--rate-rpm', '1000000', '--perm', 'trawl:ingest');
  st.reader = admin('keys', 'create', '--name', 'h-reader', '--kind', 'human', '--role', 'h-reader', '--expires', '12h');
  st.writer = admin('keys', 'create', '--name', 'h-ingest', '--kind', 'service', '--role', 'h-ingest', '--expires', '12h');
  saveState(st);
  if (!seed) { console.log('setup done without a seed'); return; }
  // Seed: a coredns hour file near the incident's 370,488 rows and a
  // machined one, compacted with the 5 s interval. Compaction names the
  // output hour by wall clock, so a seed that straddles a UTC hour splits
  // into two files and never reaches the thresholds below. Start with at
  // least SEED_MARGIN_MS left in the hour, and fail fast on a split.
  const SEED_MARGIN_MS = 8 * 60 * 1000;
  const hourMs = 3600 * 1000;
  const left = hourMs - (Date.now() % hourMs);
  if (left < SEED_MARGIN_MS) {
    console.error(`waiting ${Math.ceil(left / 1000) + 5} s for the next UTC hour so the seed lands in one hour file`);
    await delay(left + 5000);
  }
  const seedHour = Math.floor(Date.now() / hourMs);
  const { child, base } = await startTrawld(st, 'seed', { compaction: 5 });
  const stats = { accepted: 0, failed: 0, statuses: {} };
  const now = Date.now();
  for (const [service, total] of [['coredns', 370000], ['machined', 150000]]) {
    for (let done = 0; done < total; done += 5000) {
      const batch = Array.from({ length: 5000 }, (_, i) => {
        const ts = now - 14 * 60 * 1000 + Math.floor((done + i) * (14 * 60 * 1000) / total);
        return service === 'coredns' ? coredns(ts, false) : machined(ts);
      });
      const r = await ingest(base, st, batch, stats, true);
      assert.equal(r.status, 200, `seed ingest failed: ${r.status} ${r.text?.slice(0, 200)}`);
    }
  }
  // Wait for both hour files to exist and the hot buffer to drain.
  const logFile = path.join(stateDir, 'logs', 'seed.stdout');
  for (let i = 0; i < 600; i++) {
    await delay(1000);
    const done = compactionStats(logFile);
    if ((done.coredns?.maxRows ?? 0) >= 370000 && (done.machined?.maxRows ?? 0) >= 150000) break;
    assert.equal(Math.floor(Date.now() / hourMs), seedHour, 'the seed crossed a UTC hour boundary and split its hour files: run teardown, then setup again');
    assert.ok(i < 599, 'seed compaction deadline');
  }
  await stopTrawld(st, child);
  st.sentPerSec = Object.fromEntries(sentPerSec);
  st.seed = { accepted: stats.accepted, retriedStatuses: stats.statuses, compaction: compactionStats(logFile) };
  saveState(st);
  console.log(JSON.stringify(st.seed));
}

// The target of one formatted event: after zero or more `name{fields}:`
// span segments (each optionally followed by whitespace) comes
// `target: `. Linear: each character of a segment is scanned once, a `}:`
// inside a quoted field value does not end the segment, and the target
// is tried once at each segment boundary, preferring the most segments.
const TARGET = /[\w:.]+: /y;
function eventTarget(rest) {
  const starts = [0];
  let pos = 0;
  for (;;) {
    // `name{`: at least one non-space character, then the first `{`, with
    // no whitespace between. Each character is scanned once.
    const open = rest.indexOf('{', pos + 1);
    if (open < 0) break;
    let k = pos;
    while (k < open && !/\s/.test(rest[k])) k++;
    if (k < open) break;
    // The first `}:` after it that is not inside a quoted field value
    // (tracing quotes Debug strings, with backslash escapes).
    let close = -1;
    for (let j = open + 1; j < rest.length - 1; j++) {
      if (rest[j] === '"') {
        for (j++; j < rest.length && rest[j] !== '"'; j++) if (rest[j] === '\\') j++;
      } else if (rest[j] === '}' && rest[j + 1] === ':') { close = j; break; }
    }
    if (close < 0) break;
    pos = close + 2;
    while (pos < rest.length && /\s/.test(rest[pos])) pos++;
    starts.push(pos);
  }
  for (let i = starts.length - 1; i >= 0; i--) {
    TARGET.lastIndex = starts[i];
    const m = TARGET.exec(rest);
    if (m) return m[0].slice(0, -2);
  }
  return null;
}

// trawld's stdout, in tracing-subscriber's default text format (log_file
// is not opened while internal telemetry is on). Fields only, parsed
// from `key=value` pairs; quoted values are unquoted.
function readLog(file) {
  if (!fs.existsSync(file)) return [];
  const out = [];
  for (const raw of fs.readFileSync(file, 'utf8').split('\n')) {
    const line = raw.replace(/\x1b\[[0-9;]*m/g, '');
    const m = line.match(/^(\S+)\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+(.*)$/);
    if (!m) continue;
    const fields = {};
    for (const f of m[3].matchAll(/(\w+)=("(?:[^"\\]|\\.)*"|\S+)/g)) {
      let v = f[2];
      if (v.startsWith('"')) { try { v = JSON.parse(v); } catch { v = v.slice(1, -1); } }
      fields[f[1]] = v;
    }
    out.push({ timestamp: m[1], level: m[2], target: eventTarget(m[3]), fields, line });
  }
  return out;
}
function compactionStats(file) {
  const out = {};
  for (const e of readLog(file)) {
    const f = e.fields || {};
    if (f.event_type !== 'compaction_complete') continue;
    const s = (out[f.compact_service] ??= { runs: 0, merged: 0, maxRows: 0, maxMs: 0 });
    s.runs++; if (f.merged === true || f.merged === 'true') s.merged++;
    s.maxRows = Math.max(s.maxRows, Number(f.rows) || 0);
    s.maxMs = Math.max(s.maxMs, Number(f.duration_ms) || 0);
  }
  return out;
}

async function scenario(id, minutes, maxQueries) {
  const s = SCENARIOS[id];
  assert.ok(s, `unknown scenario ${id}`);
  const st = loadState();
  // Coredns events accepted by setup and earlier scenarios still count
  // toward what a window owes.
  for (const [k, v] of Object.entries(st.sentPerSec ?? {})) sentPerSec.set(Number(k), v);
  const name = `scenario-${id}`;
  const logFile = path.join(stateDir, 'logs', `${name}.stdout`);
  fs.rmSync(logFile, { force: true });
  const { child, base } = await startTrawld(st, name, s);
  const stop = { done: false };
  const started = Date.now();
  const res = { id, title: s.title, config: { compaction_interval_secs: s.compaction, coredns_ev_per_s: s.coredns ?? 0, machined_ev_per_s: s.machined ?? 0,
    sse: s.sse ?? null, nets: !!s.nets, pg_pause: !!s.pause, query_concurrency: CONCURRENCY, bound_minutes: minutes, bound_queries: maxQueries },
    started: new Date(started).toISOString(), queries: 0, statuses: {}, fiveXX: [], contentFailures: 0, owedFull: 0, contentSamples: [], transportErrors: 0, inconclusive: 0, inconclusiveSamples: [], latencyMs: [] };
  const ingestStats = { accepted: 0, failed: 0, statuses: {} };
  const bg = [];
  if (s.coredns) bg.push(ingestLoop(base, st, s.coredns, (ts) => coredns(ts, s.sse === 'service=coredns rcode=SERVFAIL' ? Math.random() < SERVFAIL_FRACTION : false), ingestStats, stop));
  if (s.machined) bg.push(ingestLoop(base, st, s.machined, ts => machined(ts), ingestStats, stop));
  // SSE live tail: count data events; reconnect if the stream ends.
  const sse = { connects: 0, statuses: {}, events: 0, byName: {} };
  if (s.sse) bg.push((async () => {
    while (!stop.done) {
      sse.connects++;
      await new Promise(resolve => {
        const req = https.request(`${base}/api/v1/stream?query=${encodeURIComponent(s.sse)}`, { headers: { authorization: `Bearer ${st.reader}`, accept: 'text/event-stream' }, agent: false, ca: ca(), rejectUnauthorized: true }, r => {
          sse.statuses[r.statusCode] = (sse.statuses[r.statusCode] || 0) + 1;
          let buf = '';
          r.on('data', c => { buf += c; let i; while ((i = buf.indexOf('\n\n')) >= 0) { const frame = buf.slice(0, i); buf = buf.slice(i + 2); const name = frame.match(/^event:[ \t]*(\S+)/m)?.[1] ?? 'message'; sse.byName[name] = (sse.byName[name] || 0) + 1; if (name === 'data' && /^data:/m.test(frame)) sse.events++; } });
          r.on('end', resolve); r.on('error', resolve);
        });
        req.on('error', resolve);
        const t = setInterval(() => { if (stop.done) { req.destroy(); clearInterval(t); } }, 200);
        req.end();
      });
      if (!stop.done) await delay(1000);
    }
  })());
  // Nets: five saved queries on the 60 s floor, created 12 s apart, so a
  // scheduled run lands about every 12 s.
  if (s.nets) {
    st.nets ??= [];
    for (let i = st.nets.length; i < 5; i++) {
      const c = await request(base, '/api/v1/saved', { token: st.reader, method: 'POST', body: { name: `coredns-net-${i}`, query: 'last=15m service=coredns | stats count() by rcode' } });
      assert.equal(c.status, 200, `create net: ${c.status} ${c.text?.slice(0, 200)}`);
      const idn = JSON.parse(c.text).id;
      const sc = await request(base, `/api/v1/saved/${idn}/schedule`, { token: st.reader, method: 'PUT', body: { interval: '1m' } });
      assert.equal(sc.status, 200, `schedule net: ${sc.status} ${sc.text?.slice(0, 200)}`);
      st.nets.push(idn); saveState(st);
      if (i < 4) await delay(12000);
    }
  }
  const pauses = { count: 0, totalMs: 0 };
  if (s.pause) bg.push((async () => {
    while (!stop.done) {
      await delay(5000 + Math.floor(Math.random() * 5000));
      if (stop.done) break;
      const hold = 2000 + Math.floor(Math.random() * 6000);
      docker('pause', st.container);
      try { await delay(hold); } finally { docker('unpause', st.container); }
      pauses.count++; pauses.totalMs += hold;
    }
  })());
  if (s.coredns && s.coredns < 1) await delay(3000);
  // Events a previous trawld accepted but never compacted sit in the WAL
  // and are not visible again until this process's first compaction tick
  // (see restart-probe). Measure that gap by exact count over the window,
  // then start the loop, so the loop tests only this scenario.
  {
    const t0 = Date.now();
    const bound = t0 + (2 * s.compaction + 30) * 1000;
    const from = Math.ceil((t0 - WINDOW_MS + 30000) / 1000) * 1000, to = Math.floor((t0 - 2000) / 1000) * 1000;
    const iso = ms => new Date(ms).toISOString();
    const sv = { window: [iso(from), iso(to)], owed: sentBetween(from, to), firstCount: null, waitedMs: 0, satisfied: false };
    for (;;) {
      const r = await request(base, '/api/v1/query', { token: st.reader, method: 'POST', body: { query: `service=coredns earliest="${iso(from)}" latest="${iso(to)}" | stats count() as n` } });
      let n = -r.status;
      if (r.status === 200 && !r.incomplete) { const b = JSON.parse(r.text); const rows = b.result?.rows ?? b.rows; n = rows.length ? Number(rows[0][0]) : 0; }
      sv.firstCount ??= n;
      sv.lastCount = n;
      if (n >= sv.owed) { sv.satisfied = true; break; }
      if (Date.now() > bound) break;
      await delay(1000);
    }
    sv.waitedMs = Date.now() - t0;
    res.startupVisibility = sv;
  }
  // The query loop.
  const deadline = started + minutes * 60000;
  async function worker() {
    while (Date.now() < deadline && res.queries < maxQueries) {
      res.queries++;
      const sentAt = Date.now();
      const t0 = performance.now();
      const r = await request(base, '/api/v1/query', { token: st.reader, method: 'POST', body: { query: QUERY } });
      const recvAt = Date.now();
      res.latencyMs.push(performance.now() - t0);
      res.statuses[r.status] = (res.statuses[r.status] || 0) + 1;
      // A known 5xx counts even with a cut body. Anything else without a
      // complete response is inconclusive: its content was never checked.
      if (r.status >= 500) { res.fiveXX.push({ at: new Date(sentAt).toISOString(), status: r.status, requestId: r.requestId, latencyMs: recvAt - sentAt, incomplete: !!r.incomplete, body: r.text.slice(0, 300) }); continue; }
      if (r.status === 0) { res.transportErrors++; continue; }
      if (r.incomplete) { res.inconclusive++; if (res.inconclusiveSamples.length < 10) res.inconclusiveSamples.push({ at: new Date(sentAt).toISOString(), status: r.status, requestId: r.requestId, error: r.error }); continue; }
      if (r.status !== 200) continue;
      const { problems: p, owed } = checkResult(r, sentAt, recvAt);
      if (owed === 20) res.owedFull++;
      if (p.length) { res.contentFailures++; if (res.contentSamples.length < 10) res.contentSamples.push({ at: new Date(sentAt).toISOString(), requestId: r.requestId, problems: p }); }
    }
  }
  try {
    await Promise.all(Array.from({ length: CONCURRENCY }, worker));
  } finally {
    stop.done = true;
    await Promise.allSettled(bg);
    res.ended = new Date().toISOString();
    // Persisted http_failure count for the run, read back through the API,
    // after a few telemetry flushes (1 s by default). A zero means nothing
    // unless the same filter shape finds an event type that is persisted
    // in the same window: every query this run sent logged pool_acquired.
    await delay(3000);
    const latest = new Date(Date.now() + 60000).toISOString();
    const readBack = async eventType => {
      const pq = await request(base, '/api/v1/query', { token: st.reader, method: 'POST', body: { query: `service=trawld event_type="${eventType}" earliest="${res.started}" latest="${latest}" | stats count() as n` } });
      try { assert.equal(pq.status, 200); assert.ok(!pq.incomplete); const b = JSON.parse(pq.text); const rows = b.result?.rows ?? b.rows; return rows.length ? Number(rows[0][0]) : 0; } catch { return `read-back query ${pq.status}`; }
    };
    res.persistedHttpFailures = await readBack('http_failure');
    const control = await readBack(READ_BACK_CONTROL);
    res.persistedControl = { eventType: READ_BACK_CONTROL, count: control };
    if (s.nets) {
      let runs = 0; const byStatus = {};
      for (const idn of st.nets) {
        const r = await request(base, `/api/v1/saved/${idn}/runs?limit=500`, { token: st.reader });
        if (r.status !== 200) { byStatus[`list_${r.status}`] = (byStatus[`list_${r.status}`] || 0) + 1; continue; }
        for (const run of JSON.parse(r.text).runs) if (Date.parse(run.started_at) >= started) { runs++; byStatus[run.status] = (byStatus[run.status] || 0) + 1; }
      }
      res.netRuns = { runs, byStatus };
    }
    await stopTrawld(st, child);
    st.sentPerSec = Object.fromEntries([...sentPerSec].filter(([k]) => k * 1000 > Date.now() - 2 * WINDOW_MS));
    saveState(st);
  }
  const lat = res.latencyMs; delete res.latencyMs;
  res.latency = { p50: pctl(lat, 0.5), p95: pctl(lat, 0.95), p99: pctl(lat, 0.99), max: pctl(lat, 1) };
  res.durationS = Math.round((Date.parse(res.ended) - started) / 1000);
  res.ingest = ingestStats;
  if (s.sse) res.sse = sse;
  if (s.pause) res.pauses = pauses;
  res.compaction = compactionStats(logFile);
  // Every http_failure trawld logged (explicit fields only, no secrets).
  const failures = readLog(logFile).filter(e => e.fields?.event_type === 'http_failure').map(e => ({ timestamp: e.timestamp, level: e.level, target: e.target, ...e.fields }));
  res.httpFailureEvents = failures.length;
  const byKey = {};
  for (const f of failures) { const k = `${f.route} ${f.status} ${f.stage}${f.reached ? `/${f.reached}` : ''} ${f.error_class}/${f.cause_kind}`; byKey[k] = (byKey[k] || 0) + 1; }
  res.httpFailureShapes = byKey;
  const queryFailures = failures.filter(f => f.route === '/api/v1/query');
  res.queryFiveXXMatched = res.fiveXX.filter(x => queryFailures.some(f => f.request_id === x.requestId)).length;
  fs.mkdirSync(resultsDir, { recursive: true });
  fs.writeFileSync(path.join(resultsDir, `${id}.json`), JSON.stringify(res, null, 2) + '\n');
  if (failures.length) fs.writeFileSync(path.join(resultsDir, `${id}-http_failure.jsonl`), failures.map(f => JSON.stringify(f)).join('\n') + '\n');
  console.log(JSON.stringify({ id, queries: res.queries, statuses: res.statuses, fiveXX: res.fiveXX.length, contentFailures: res.contentFailures, httpFailureShapes: byKey }));
}

// Minimal reproduction of the restart visibility gap: accepted events that
// are still in the WAL at shutdown are missing from search after restart
// until the new process's first compaction tick.
async function restartProbe() {
  const st = loadState();
  const probe = { compaction: 30 };
  const svc = `probe${RUN}`;
  const q = { query: `service=${svc} last=15m | stats count() as n` };
  const count = async base => { const r = await request(base, '/api/v1/query', { token: st.reader, method: 'POST', body: q }); if (r.status !== 200) return -r.status; const rows = JSON.parse(r.text).result?.rows ?? JSON.parse(r.text).rows; return rows.length ? Number(rows[0][0]) : 0; };
  const out = { compaction_interval_secs: probe.compaction, service: svc, events: 50 };
  let { child, base } = await startTrawld(st, 'probe-1', probe);
  const r = await ingest(base, st, Array.from({ length: 50 }, (_, i) => ({ ...coredns(Date.now() - i * 100, false), service: svc })));
  assert.equal(r.status, 200);
  out.beforeRestart = await count(base);
  const stoppedAt = Date.now();
  await stopTrawld(st, child);
  out.walFilesAfterStop = fs.readdirSync(path.join(stateDir, 'data', 'wal'), { recursive: true }).filter(f => String(f).includes(svc)).length;
  ({ child, base } = await startTrawld(st, 'probe-2', probe));
  const up = Date.now();
  out.restartMs = up - stoppedAt;
  out.series = [];
  for (let i = 0; i < 120; i++) {
    const n = await count(base);
    out.series.push([Math.round((Date.now() - up) / 1000), n]);
    if (n === 50) break;
    await delay(1000);
  }
  out.visibleAfterS = out.series.at(-1)[1] === 50 ? out.series.at(-1)[0] : null;
  const comp = readLog(path.join(stateDir, 'logs', 'probe-2.stdout')).filter(e => e.fields.event_type === 'compaction_complete' && e.fields.compact_service === svc);
  out.compactionAfterRestart = comp.map(e => e.timestamp);
  await stopTrawld(st, child);
  // Compress the series to its transitions.
  out.series = out.series.filter((p, i, a) => i === 0 || i === a.length - 1 || p[1] !== a[i - 1][1]);
  fs.mkdirSync(resultsDir, { recursive: true });
  fs.writeFileSync(path.join(resultsDir, 'restart-probe.json'), JSON.stringify(out, null, 2) + '\n');
  console.log(JSON.stringify(out));
}

// A short check of the plumbing against a real trawld: a few authenticated
// queries through request() and one SSE connect, both on the pinned CA.
// Node's codes for a certificate chain it could not verify: what the
// negative control must fail with, rather than any connection error.
const CERT_VERIFY_CODES = new Set(['DEPTH_ZERO_SELF_SIGNED_CERT', 'SELF_SIGNED_CERT_IN_CHAIN', 'UNABLE_TO_VERIFY_LEAF_SIGNATURE',
  'UNABLE_TO_GET_ISSUER_CERT', 'UNABLE_TO_GET_ISSUER_CERT_LOCALLY', 'CERT_UNTRUSTED', 'CERT_SIGNATURE_FAILURE']);

// A short check of the plumbing against a real trawld: a few authenticated
// queries through request() and one SSE connect, both on the pinned CA,
// and a control request without the CA that must fail verification.
// Exits non-zero when any check fails.
async function smoke() {
  const st = loadState();
  const failures = [];
  const check = (ok, line) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${line}`); if (!ok) failures.push(line); };
  const { child, base } = await startTrawld(st, 'smoke', { compaction: 300 });
  try {
    for (const q of ['last=15m service=coredns | head 20', 'service=trawld last=15m | stats count() as n', 'last=1h | head 1']) {
      const r = await request(base, '/api/v1/query', { token: st.reader, method: 'POST', body: { query: q } });
      check(r.status === 200 && !r.incomplete, `query ${JSON.stringify(q)}: ${r.status}${r.incomplete ? ' incomplete' : ''}${r.error ? ` ${r.error}` : ''}`);
    }
    const sse = await new Promise(resolve => {
      const req = https.request(`${base}/api/v1/stream?query=${encodeURIComponent('service=coredns')}`, { headers: { authorization: `Bearer ${st.reader}`, accept: 'text/event-stream' }, agent: false, ca: ca(), rejectUnauthorized: true }, r => {
        resolve({ status: r.statusCode, type: r.headers['content-type'] }); req.destroy();
      });
      req.on('error', e => resolve({ status: 0, error: e.message }));
      req.end();
    });
    check(sse.status === 200 && /^text\/event-stream\b/.test(sse.type ?? ''), `sse: ${sse.status} ${sse.type ?? sse.error}`);
    // Negative control: the same server without the pinned CA.
    const unpinned = await new Promise(resolve => {
      const req = https.request(`${base}/api/v1/health`, { agent: false, rejectUnauthorized: true }, r => { resolve({ response: r.statusCode }); req.destroy(); });
      req.on('error', e => resolve({ code: e.code, message: e.message }));
      req.end();
    });
    check(CERT_VERIFY_CODES.has(unpinned.code), `without the pinned CA: ${unpinned.response !== undefined ? `HTTP ${unpinned.response} (validation did not refuse)` : `refused: ${unpinned.code ?? unpinned.message}`}`);
  } finally { await stopTrawld(st, child); }
  if (failures.length) throw new Error(`smoke failed ${failures.length} check(s)`);
  console.log('smoke passed');
}

async function teardown() {
  if (!fs.existsSync(statePath)) { console.log('no state'); return; }
  const st = loadState();
  const pid = st.trawldPid;
  await stopTrawld(st);
  const problems = [];
  if (pid) { try { if (fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').includes('trawld')) problems.push(`trawld ${pid} still running`); } catch { /* gone */ } }
  try { docker('unpause', st.container); } catch { /* not paused */ }
  try { docker('rm', '--force', '--volumes', st.container); } catch (e) { problems.push(`docker rm: ${e.message.split('\n')[0]}`); }
  let remaining;
  try { remaining = docker('ps', '--all', '--quiet', '--filter', `name=^${st.container}$`); } catch (e) { remaining = `docker ps failed: ${e.message.split('\n')[0]}`; }
  if (remaining) problems.push(`container ${st.container} still present`);
  if (problems.length) {
    // Keep the state (container name, pid) so a retry can finish.
    throw new Error(`teardown incomplete, state kept in ${stateDir}: ${problems.join('; ')}`);
  }
  fs.rmSync(stateDir, { recursive: true, force: true });
  console.log(`removed ${st.container} and ${stateDir}`);
}

const INCIDENT_ROWS = 370488;
// Records written before the harness split cut bodies out carry no
// `inconclusive` field; that harness counted a cut response as a
// transport error or content-checked its partial body, so none was left
// unclassified: an absent field means zero.
const inconclusive = r => r.inconclusive ?? 0;
// How far the persisted http_failure read-back can be trusted.
function readBackState(r) {
  if (r.persistedControl === undefined) return 'uncontrolled';
  const c = r.persistedControl.count;
  return typeof c === 'number' && c > 0 && typeof r.persistedHttpFailures === 'number' ? 'controlled' : 'inconclusive';
}
function readBackText(r) {
  const state = readBackState(r);
  if (state === 'uncontrolled') return `${r.persistedHttpFailures}, uncontrolled (recorded before the control existed)`;
  const control = `control: ${r.persistedControl.count} ${r.persistedControl.eventType} events read back with the same filter`;
  return state === 'controlled' ? `${r.persistedHttpFailures} (${control})` : `inconclusive: ${r.persistedHttpFailures} (${control})`;
}
// The startup wait either saw every carried-over event or hit its bound.
function visibilityText(sv, lead) {
  const s = Math.round(sv.waitedMs / 1000);
  return sv.satisfied ? `${lead} after ${s} s` : `the wait hit its bound after ${s} s with ${sv.lastCount ?? 'an unrecorded number'} of ${sv.owed} visible`;
}

function outcomeOf(r) {
  const bad = [r.fiveXX.length && `${r.fiveXX.length} 5xx`, r.contentFailures && `${r.contentFailures} content failures`,
    r.transportErrors && `${r.transportErrors} transport errors`, inconclusive(r) && `${inconclusive(r)} inconclusive`].filter(Boolean);
  return bad.length ? `**${bad.join(', ')}**` : 'clean';
}

function report() {
  const commit = fs.existsSync(path.join(resultsDir, 'commit.txt')) ? fs.readFileSync(path.join(resultsDir, 'commit.txt'), 'utf8').trim() : '(unrecorded)';
  const seed = fs.existsSync(path.join(resultsDir, 'seed.json')) ? JSON.parse(fs.readFileSync(path.join(resultsDir, 'seed.json'), 'utf8')) : null;
  const out = [];
  out.push('# #235: bounded search for the original 500', '');
  out.push(`Generated by \`node visual-evidence/issue-235/harness.mjs report\` from \`results/*.json\`. The runs used a release \`trawld\` built from commit \`${commit}\`, with disposable \`postgres:18\` in Docker and a private data directory.`, '');
  const provenance = path.join(resultsDir, 'provenance.md');
  if (fs.existsSync(provenance)) out.push(fs.readFileSync(provenance, 'utf8').trim(), '');
  out.push(`Query: \`${QUERY}\`, ${CONCURRENCY} concurrent loops, back to back. Bound per scenario: 30 minutes or 10,000 queries, whichever comes first.`, '');
  out.push('A content failure is a 200 whose body does not parse, has more than 20 rows, has a non-`coredns` row or a `_time` outside the window, or has fewer rows than the harness had accepted inside the window before it sent the query (capped at 20).', '');
  if (seed) out.push(`Seed: ${seed.accepted} events accepted. The seed built a coredns hour file of ${seed.compaction.coredns?.maxRows} rows and a machined hour file of ${seed.compaction.machined?.maxRows} rows. The incident's hour file had 370,488 rows.`, '');
  out.push('Common config: `internal_telemetry = true`, `daily_rollup` default (true), `max_concurrent_queries` default (nproc), scheduler `poll_interval_secs = 1`, rate limits off (role rpm 1,000,000). Per-scenario knobs are in the table.', '');
  out.push('A response with no status is a transport error. A non-5xx response whose body was cut is inconclusive: its content was never checked. A scenario with either is not clean.', '');
  out.push('| Scenario | Knobs | Queries | Duration | 5xx | Content failures | Transport errors / inconclusive | p50/p95/max ms | Compactions (coredns merged / machined) | Outcome |');
  out.push('| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |');
  const files = fs.existsSync(resultsDir) ? fs.readdirSync(resultsDir).filter(f => /^[a-g]\.json$/.test(f)).sort() : [];
  for (const f of files) {
    const r = JSON.parse(fs.readFileSync(path.join(resultsDir, f), 'utf8'));
    const k = r.config;
    const knobs = [`compaction ${k.compaction_interval_secs}s`, k.coredns_ev_per_s && `coredns ${+k.coredns_ev_per_s.toFixed(2)}/s`, k.machined_ev_per_s && `machined ${k.machined_ev_per_s}/s`,
      k.sse && `SSE \`${k.sse}\``, k.nets && '5 nets @1m', k.pg_pause && 'pg pause'].filter(Boolean).join(', ');
    const c = r.compaction || {};
    const comp = `${c.coredns ? `${c.coredns.merged}/${c.coredns.runs} (max ${c.coredns.maxRows} rows, ${c.coredns.maxMs} ms)` : '0'} / ${c.machined ? `${c.machined.runs} (max ${c.machined.maxMs} ms)` : '0'}`;
    out.push(`| ${r.id}: ${r.title} | ${knobs} | ${r.queries} | ${r.durationS}s | ${r.fiveXX.length} | ${r.contentFailures} | ${r.transportErrors} / ${inconclusive(r)} | ${r.latency.p50}/${r.latency.p95}/${r.latency.max} | ${comp} | ${outcomeOf(r)} |`);
  }
  out.push('');
  for (const f of files) {
    const r = JSON.parse(fs.readFileSync(path.join(resultsDir, f), 'utf8'));
    const extra = [];
    extra.push(`statuses ${JSON.stringify(r.statuses)}`);
    extra.push(`200s that owed the full 20 rows: ${r.owedFull}`);
    if (r.startupVisibility) extra.push(`carried-over coredns events visible at start: ${r.startupVisibility.firstCount}/${r.startupVisibility.owed}, ${visibilityText(r.startupVisibility, 'all visible')}`);
    extra.push(`http_failure events logged: ${r.httpFailureEvents}, persisted (read back through the API): ${readBackText(r)}`);
    if (Object.keys(r.httpFailureShapes).length) extra.push(`http_failure shapes (route status stage class/cause): ${JSON.stringify(r.httpFailureShapes)}`);
    if (r.fiveXX.length) extra.push(`query 5xx matched to an http_failure by request_id: ${r.queryFiveXXMatched}/${r.fiveXX.length}`);
    if (r.ingest.accepted || r.ingest.failed) extra.push(`ingest accepted ${r.ingest.accepted}, failed batches ${r.ingest.failed} ${JSON.stringify(r.ingest.statuses)}`);
    if (r.sse) extra.push(`SSE connects ${r.sse.connects}, statuses ${JSON.stringify(r.sse.statuses)}, data events ${r.sse.events} (${(r.sse.events / (r.durationS / 60)).toFixed(0)}/min)`);
    if (r.netRuns) extra.push(`net runs in window: ${r.netRuns.runs} ${JSON.stringify(r.netRuns.byStatus)}`);
    if (r.pauses) extra.push(`pg pauses: ${r.pauses.count}, ${Math.round(r.pauses.totalMs / 1000)} s total`);
    if (r.inconclusiveSamples?.length) extra.push(`inconclusive samples: ${JSON.stringify(r.inconclusiveSamples.slice(0, 3))}`);
    if (r.contentSamples.length) extra.push(`content failure samples: ${JSON.stringify(r.contentSamples.slice(0, 3))}`);
    out.push(`- **${r.id}**: ${extra.join('; ')}.`);
  }
  out.push('');
  const all = files.map(f => JSON.parse(fs.readFileSync(path.join(resultsDir, f), 'utf8')));
  const total = k => all.reduce((n, r) => n + k(r), 0);
  const fives = total(r => r.fiveXX.length), content = total(r => r.contentFailures);
  const transport = total(r => r.transportErrors), unsure = total(inconclusive);
  const clean = all.filter(r => outcomeOf(r) === 'clean').map(r => r.id);
  let verdict;
  if (!all.length) verdict = 'No scenario has run.';
  else if (all.some(r => r.fiveXX.some(x => x.status === 500))) verdict = 'A 500 reproduced: see the per-scenario 5xx records in results/.';
  else if (fives) verdict = 'No 500 reproduced, but other 5xx did: see the per-scenario 5xx records in results/.';
  else if (transport || unsure) verdict = 'No 5xx was returned, but some responses were incomplete, so the search is not conclusive for those requests.';
  else verdict = 'The original 500 did not reproduce: original 500 unresolved.';
  out.push(`**Outcome: ${total(r => r.queries)} queries across ${all.length} scenarios (${all.map(r => r.id).join(', ') || 'none'}), ${fives} 5xx, ${content} content failures, ${transport} transport errors, ${unsure} inconclusive. Clean: ${clean.join(', ') || 'none'}.** ${verdict}`, '');
  const limits = [];
  const fmt = n => n.toLocaleString('en-US');
  const merged = all.filter(r => r.compaction?.coredns?.merged);
  if (merged.length) {
    const at = merged.filter(r => r.compaction.coredns.maxRows >= INCIDENT_ROWS), below = merged.filter(r => r.compaction.coredns.maxRows < INCIDENT_ROWS);
    const list = rs => rs.map(r => `${r.id} (${fmt(r.compaction.coredns.maxRows)} rows, ${r.compaction.coredns.merged} merge${r.compaction.coredns.merged === 1 ? "" : "s"})`).join(', ');
    limits.push(`- The compactions column shows the largest coredns hour file merged in each scenario; the file restarts at each UTC hour. At or above the incident's ${fmt(INCIDENT_ROWS)} rows: ${list(at) || 'none'}. Below it: ${list(below) || 'none'}.${all.length > merged.length ? ` No coredns merge: ${all.filter(r => !r.compaction?.coredns?.merged).map(r => r.id).join(', ')}.` : ''}`);
  }
  for (const r of all.filter(x => x.pauses)) {
    const by = {};
    for (const x of r.fiveXX) by[x.status] = (by[x.status] || 0) + 1;
    const shapes = Object.keys(r.httpFailureShapes || {}).filter(k => / pre_admission /.test(k));
    limits.push(`- ${r.id} paused postgres ${r.pauses.count} times, ${Math.round(r.pauses.totalMs / 1000)} s in total; the longest query took ${fmt(r.latency.max)} ms. It returned ${r.fiveXX.length ? `these 5xx: ${JSON.stringify(by)}` : 'no 5xx'}, and ${shapes.length ? `logged pre-admission failures: ${shapes.join('; ')}` : 'logged no pre-admission (auth backend) failure'}.`);
  }
  limits.push('- The harness counts 5xx from HTTP status, not from logs, so a missed log line cannot hide a 5xx. It parses `http_failure` from stdout with the same parser that reads `compaction_complete`.');
  const byState = st => all.filter(r => readBackState(r) === st).map(r => r.id);
  const rbParts = [['uncontrolled', 'uncontrolled (recorded before the control existed), so its zeros are not evidence that nothing was persisted'],
    ['inconclusive', 'inconclusive (the control read back nothing)'], ['controlled', 'controlled by a nonzero read-back of the control event']]
    .filter(([k]) => byState(k).length).map(([k, text]) => `${byState(k).join(', ')}: ${text}`);
  if (rbParts.length) limits.push(`- The persisted http_failure read-back is ${rbParts.join('; ')}. The 5xx counts do not depend on it.`);
  const unsatisfied = all.filter(r => r.startupVisibility && !r.startupVisibility.satisfied).map(r => r.id);
  if (unsatisfied.length) limits.push(`- The carried-over events never all became visible before the startup wait hit its bound in: ${unsatisfied.join(', ')}. Their content checks count those events as owed.`);
  const waited = all.filter(r => (r.startupVisibility?.waitedMs ?? 0) >= 1000);
  if (waited.length) limits.push(`- Duration includes the wait for events carried over from the previous scenario (see the finding below): ${waited.map(r => `${r.id} ${Math.round(r.startupVisibility.waitedMs / 1000)} s`).join(', ')}.`);
  out.push('Limits of this record:', '', ...limits, '');
  // The certificate paths are harness plumbing, and the recorded run
  // predates them (trawld generated its own certificate then), so the
  // printed config leaves them out.
  out.push('<details><summary>trawld config excerpt (scenario f, TLS certificate paths omitted; other scenarios change only the knobs above)</summary>', '', '```toml', config(SCENARIOS.f, 'scenario-f').replaceAll(stateDir, '$HARNESS_DIR').split('\n').filter(l => !l.startsWith('tls_')).join('\n').trimEnd(), '```', '', '</details>', '');
  const probePath = path.join(resultsDir, 'restart-probe.json');
  if (fs.existsSync(probePath)) {
    const p = JSON.parse(fs.readFileSync(probePath, 'utf8'));
    out.push('## Finding: accepted events are missing from search after a restart', '');
    out.push(`\`harness.mjs restart-probe\` sets \`compaction_interval_secs = ${p.compaction_interval_secs}\`. It ingests ${p.events} events for a fresh service and counts them with \`service=<probe> last=15m | stats count()\`. Before the restart the count is ${p.beforeRestart}. After SIGTERM, ${p.walFilesAfterStop} WAL file still holds the events. After the restart, the count by seconds since startup is ${p.series.map(([t, n]) => `${n} at ${t} s`).join(', ')}. All events are visible after ${p.visibleAfterS} s, when the first compaction completes (${p.compactionAfterRestart.join(', ')}).`, '');
    out.push('Cause: the hot buffer starts empty (`crates/trawl-server/src/state.rs:645`) and nothing loads the WAL into it. The compaction loop sleeps one interval before its first tick (`crates/trawl-server/src/ingest/compaction.rs:71`), and shutdown stops the loop without a final compaction (`compaction.rs:97-99`). A query in that window gets a 200 with rows missing, not an error. The default interval is 10 s. The same code is at base `ed3a2516`. This is not the #235 500. Each scenario waits until the events carried over from the previous scenario are visible before it starts its loop, and records that wait.', '');
    const gaps = all.filter(r => r.startupVisibility && r.startupVisibility.firstCount < r.startupVisibility.owed);
    if (gaps.length) out.push(`The scenario runs show the same gap: ${gaps.map(r => `${r.id} started with ${fmt(r.startupVisibility.firstCount)} of ${fmt(r.startupVisibility.owed)} carried-over events visible, ${visibilityText(r.startupVisibility, 'all')} (compaction interval ${r.config.compaction_interval_secs} s)`).join('; ')}.`, '');
  }
  const producers = path.join(here, 'producers.md');
  if (fs.existsSync(producers)) out.push(fs.readFileSync(producers, 'utf8').trimEnd(), '');
  process.stdout.write(out.join('\n') + '\n');
}

const [cmd, ...rest] = process.argv.slice(2);
if (cmd !== 'report' && process.env.NODE_TLS_REJECT_UNAUTHORIZED === '0') {
  console.error('NODE_TLS_REJECT_UNAUTHORIZED=0 would disable certificate validation: unset it');
  process.exit(2);
}
const opt = (n, d) => { const i = rest.indexOf(`--${n}`); return i >= 0 ? Number(rest[i + 1]) : d; };
try {
  if (cmd === 'setup' && rest.includes('--no-seed')) await setup({ seed: false });
  else if (cmd === 'smoke') await smoke();
  else if (cmd === 'setup') { await setup(); const st = loadState(); fs.mkdirSync(resultsDir, { recursive: true }); fs.writeFileSync(path.join(resultsDir, 'seed.json'), JSON.stringify(st.seed, null, 2) + '\n'); }
  else if (cmd === 'scenario') await scenario(rest[0], opt('minutes', 30), opt('max-queries', 10000));
  else if (cmd === 'teardown') await teardown();
  else if (cmd === 'restart-probe') await restartProbe();
  else if (cmd === 'report') report();
  else { console.error('usage: harness.mjs setup [--no-seed] | smoke | scenario <a..g> [--minutes N] [--max-queries N] | report | teardown'); process.exit(2); }
} finally { sharedAgent?.destroy(); }
