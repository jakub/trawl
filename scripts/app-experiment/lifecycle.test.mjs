// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// Opt-in real-process tests. First prepare a build with bin/app-experiment.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { sha256, fileHash } from './identity.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
function start(args, env = {}) {
  const child = spawn(path.join(root, 'bin/app-experiment'), ['--skip-build', '--events', '100', '--rate', '1000', ...args], { cwd: root, env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  for (const stream of [child.stdout, child.stderr]) stream.on('data', c => { output += c; });
  const done = new Promise((resolve, reject) => { child.once('error', reject); child.once('close', resolve); });
  return { child, done, output: () => output };
}
async function waitFor(run, predicate, timeout = 90000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (predicate(run.output())) return;
    assert.equal(run.child.exitCode, null, run.output());
    await delay(100);
  }
  throw new Error(`deadline: ${run.output()}`);
}
async function finish(run) {
  const timer = setTimeout(() => run.child.kill('SIGTERM'), 90000);
  try { return await run.done; } finally { clearTimeout(timer); }
}
async function readReport(run) {
  const match = run.output().match(/Private artifacts: (.+)/);
  assert.ok(match, run.output());
  const dir = match[1].trim();
  const report = JSON.parse(await fs.readFile(path.join(dir, 'report.json'), 'utf8'));
  return { dir, report };
}
async function verifyResources(report) {
  const containers = execFileSync('docker', ['--host', 'unix:///var/run/docker.sock', 'ps', '--all', '--quiet', '--filter', `label=trawl.experiment=${report.runId}`], { encoding: 'utf8', timeout: 10000, killSignal: 'SIGKILL' });
  assert.equal(containers.trim(), '', 'owned container remained after exit');
  for (const { name, pid } of report.processes || []) {
    assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' }, `${name} remained alive after exit`);
  }
}
async function verifyCleanup(run) {
  const { dir, report } = await readReport(run);
  assert.deepEqual(report.cleanup, { processes: true, container: true, secrets: true });
  await assert.rejects(fs.stat(path.join(dir, 'private')), { code: 'ENOENT' });
  await verifyResources(report);
  return report;
}

test('a proxy-port collision fails without reusing or stopping the existing listener', { timeout: 120000 }, async () => {
  const listener = http.createServer((req, res) => res.end('owned-by-test'));
  await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
  const port = listener.address().port;
  const run = start(['--web-port', String(port)]);
  try {
    assert.equal(await finish(run), 1, run.output());
    const report = await verifyCleanup(run);
    assert.equal(report.status, 'failed');
    assert.match(report.error, /trawl-web exited during startup/);
    assert.equal(await (await fetch(`http://127.0.0.1:${port}`)).text(), 'owned-by-test');
  } finally {
    if (run.child.exitCode === null) { run.child.kill('SIGTERM'); await run.done; }
    await new Promise(resolve => listener.close(resolve));
  }
});

test('SIGTERM during an interactive hold cleans the verified instance', { timeout: 120000 }, async () => {
  const run = start(['--hold-seconds', '60']);
  try {
    await waitFor(run, output => output.includes('Verified instance held'));
    run.child.kill('SIGTERM');
    assert.equal(await finish(run), 1, run.output());
    const report = await verifyCleanup(run);
    assert.equal(report.status, 'interrupted');
    assert.equal(report.interrupted, true);
    assert.ok(report.phases.some(p => p.name === 'browser-session-after-restart'));
  } finally {
    if (run.child.exitCode === null) { run.child.kill('SIGTERM'); await run.done; }
  }
});

async function injectFilesystemFailure(method, suffix, code) {
  const dir = await fs.mkdtemp(path.join(root, 'target/lifecycle-fault-'));
  const module = path.join(dir, 'inject.mjs');
  const marker = path.join(dir, 'injected');
  await fs.writeFile(module, `import fs from 'node:fs/promises';
const original = fs[${JSON.stringify(method)}];
let injected = false;
fs[${JSON.stringify(method)}] = async function(target, ...args) {
  if (!injected && String(target).endsWith(${JSON.stringify(suffix)})) {
    injected = true;
    await fs.writeFile(${JSON.stringify(marker)}, 'injected');
    throw Object.assign(new Error('lifecycle injected ${code}'), { code: ${JSON.stringify(code)} });
  }
  return original.call(this, target, ...args);
};
`);
  return { dir, marker, env: { NODE_OPTIONS: `--import=${pathToFileURL(module).href}` } };
}

for (const code of ['ENOENT', 'ENOTDIR']) {
  test(`compaction polling ${code === 'ENOENT' ? 'retries ENOENT' : 'reports ENOTDIR'}`, { timeout: 120000 }, async () => {
    const fault = await injectFilesystemFailure('readdir', '/private/data/experiment', code);
    const run = start([], fault.env);
    try {
      assert.equal(await finish(run), code === 'ENOENT' ? 0 : 1, run.output());
      assert.equal(await fs.readFile(fault.marker, 'utf8'), 'injected');
      const report = await verifyCleanup(run);
      assert.equal(report.status, code === 'ENOENT' ? 'passed' : 'failed');
      if (code === 'ENOTDIR') assert.match(report.error, /lifecycle injected ENOTDIR/);
      else assert.ok(report.phases.some(p => p.name === 'browser-session-after-restart'));
    } finally {
      if (run.child.exitCode === null) { run.child.kill('SIGTERM'); await run.done; }
      await fs.rm(fault.dir, { recursive: true, force: true });
    }
  });
}

test('private-directory removal failure still writes the final report', { timeout: 120000 }, async () => {
  const fault = await injectFilesystemFailure('rm', '/private', 'EACCES');
  const listener = http.createServer((req, res) => res.end('owned-by-test'));
  await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
  const run = start(['--web-port', String(listener.address().port)], fault.env);
  try {
    assert.equal(await finish(run), 1, run.output());
    assert.equal(await fs.readFile(fault.marker, 'utf8'), 'injected');
    const { dir, report } = await readReport(run);
    assert.equal(report.status, 'failed');
    assert.match(report.error, /trawl-web exited during startup/);
    assert.deepEqual(report.cleanup, {
      processes: true, container: true, secrets: false,
      secretsError: 'lifecycle injected EACCES',
    });
    await verifyResources(report);
    assert.ok((await fs.stat(path.join(dir, 'private'))).isDirectory());
    // The parent removes this test-owned residue only after checking ownership
    // and proving the runner stopped its processes and container.
    await fs.rm(path.join(dir, 'private'), { recursive: true, force: true });
  } finally {
    if (run.child.exitCode === null) { run.child.kill('SIGTERM'); await run.done; }
    await new Promise(resolve => listener.close(resolve));
    await fs.rm(fault.dir, { recursive: true, force: true });
  }
});

async function preflightProbe() {
  const dir = await fs.mkdtemp(path.join(root, 'target/lifecycle-identity-'));
  const module = path.join(dir, 'probe.mjs');
  const credential = path.join(dir, 'credential-read');
  const complete = path.join(dir, 'preflight-complete');
  await fs.writeFile(module, `import fs from 'node:fs/promises';
const read = fs.readFile, mkdir = fs.mkdir, write = fs.writeFile;
fs.readFile = async function(file, ...args) {
  if (String(file).endsWith('/private/browser-key')) {
    await write(${JSON.stringify(credential)}, 'attempted');
    throw new Error('lifecycle credential read forbidden');
  }
  return read.call(this, file, ...args);
};
fs.mkdir = async function(dir, ...args) {
  if (String(dir).endsWith('/issue188-evidence')) {
    await write(${JSON.stringify(complete)}, 'reached');
    throw new Error('lifecycle identity preflight complete');
  }
  return mkdir.call(this, dir, ...args);
};
`);
  return { dir, credential, complete, env: { NODE_OPTIONS: `--import=${pathToFileURL(module).href}` } };
}

async function customPreflight(dir, probe, failure) {
  await fs.rm(probe.complete, { force: true });
  const child = spawn(process.execPath, [path.join(root, 'scripts/app-experiment/issue188-evidence.mjs'), '--run', dir],
    { cwd: root, env: { ...process.env, ...probe.env }, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  for (const stream of [child.stdout, child.stderr]) stream.on('data', c => { output += c; });
  const done = new Promise((resolve, reject) => { child.once('error', reject); child.once('close', resolve); });
  assert.equal(await finish({ child, done }), 1, output);
  assert.match(output, failure || /lifecycle identity preflight complete/);
  await assert.rejects(fs.stat(probe.credential), { code: 'ENOENT' }, 'preflight read a credential');
  await assert.rejects(fs.stat(path.join(dir, 'issue188-evidence')), { code: 'ENOENT' }, 'preflight created evidence');
  if (failure) await assert.rejects(fs.stat(probe.complete), { code: 'ENOENT' }, 'invalid identity passed preflight');
  else assert.equal(await fs.readFile(probe.complete, 'utf8'), 'reached');
}

test('attached evidence binds the selected run, actual processes, and owned SPA before credentials', { timeout: 360000 }, async () => {
  const probe = await preflightProbe();
  const run = start(['--hold-seconds', '300']);
  let restore;
  try {
    await waitFor(run, output => output.includes('Verified instance held'));
    const dir = run.output().match(/Private artifacts: (.+)/)[1].trim();
    const buildFile = path.join(dir, 'build.json');
    const instanceFile = path.join(dir, 'instance.json');
    const stampFile = path.join(root, 'target/app-experiment-build.json');
    const globalIndex = path.join(root, 'crates/trawl-web-ui/dist/index.html');
    const ownedIndex = path.join(dir, 'private/spa/index.html');
    const originals = new Map(await Promise.all([buildFile, instanceFile, stampFile, globalIndex, ownedIndex]
      .map(async file => [file, await fs.readFile(file)])));
    restore = async () => { for (const [file, bytes] of originals) await fs.writeFile(file, bytes); };
    const record = JSON.parse(originals.get(buildFile));
    const instance = JSON.parse(originals.get(instanceFile));
    const replaceRecord = async next => {
      const bytes = `${JSON.stringify(next, null, 2)}\n`;
      await fs.writeFile(buildFile, bytes);
      await fs.writeFile(instanceFile, JSON.stringify({ ...instance, buildSHA256: sha256(bytes) }));
    };

    // Positive preflight control also proves the committed-script guards pass.
    // The probe stops before mkdir and never permits a browser-key read.
    await customPreflight(dir, probe);

    // A later preparation record must neither relabel nor invalidate this run.
    await fs.writeFile(stampFile, JSON.stringify({ commit: '0'.repeat(40), sourceHash: '0'.repeat(64) }));
    await customPreflight(dir, probe);
    await restore();

    // A later Trunk build must not change the held proxy's served frontend.
    await fs.appendFile(globalIndex, '\n<!-- lifecycle global SPA replacement -->\n');
    await customPreflight(dir, probe);
    await restore();

    // With a valid global stamp, an old selected run must still be refused.
    await replaceRecord({ ...record, build: { ...record.build, commit: '0'.repeat(40) } });
    await customPreflight(dir, probe, /build commit differs from current HEAD/);
    await restore();

    await replaceRecord({ ...record, runId: 'run-0-0123456789' });
    await customPreflight(dir, probe, /build record belongs to another run/);
    await restore();

    await fs.appendFile(buildFile, ' ');
    await customPreflight(dir, probe, /run build record digest differs from instance/);
    await restore();

    await replaceRecord({ ...record, build: { ...record.build, identityHelperSHA256: '0'.repeat(64) } });
    await customPreflight(dir, probe, /identity helper differs from selected run/);
    await restore();

    await replaceRecord({ ...record, build: { ...record.build, binaries: { ...record.build.binaries, 'trawl-web': '0'.repeat(64) } } });
    await customPreflight(dir, probe, /running executable differs from run build/);
    await restore();

    const stale = structuredClone(instance);
    stale.processIdentity.web.startTicks = String(BigInt(stale.processIdentity.web.startTicks) + 1n);
    await fs.writeFile(instanceFile, JSON.stringify(stale));
    await customPreflight(dir, probe, /recorded process identity changed/);
    await restore();

    await fs.appendFile(ownedIndex, '\n<!-- lifecycle owned SPA mutation -->\n');
    await customPreflight(dir, probe, /owned SPA differs from run build/);
    await restore();
    await customPreflight(dir, probe);

    restore = undefined;
    run.child.kill('SIGTERM');
    assert.equal(await finish(run), 1, run.output());
    assert.equal((await verifyCleanup(run)).status, 'interrupted');
  } finally {
    try { if (restore) await restore(); }
    finally {
      if (run.child.exitCode === null) { run.child.kill('SIGTERM'); await run.done; }
      await fs.rm(probe.dir, { recursive: true, force: true });
    }
  }
});

test('an executable substituted after preparation starts but fails actual-process identity', { timeout: 120000 }, async () => {
  const dir = await fs.mkdtemp(path.join(root, 'target/lifecycle-executable-'));
  const target = path.resolve(root, process.env.CARGO_TARGET_DIR || 'target');
  const binary = path.join(target, 'debug/trawl-web');
  const alternate = path.join(dir, 'trawl-web');
  const module = path.join(dir, 'substitute.mjs');
  const marker = path.join(dir, 'substituted');
  // An ELF trailing payload changes the identity without replacing the real
  // application. Its readiness log below is required proof that it can start.
  await fs.copyFile(binary, alternate);
  await fs.appendFile(alternate, '\nlifecycle alternate executable\n');
  await fs.chmod(alternate, 0o700);
  assert.notEqual(await fileHash(alternate), await fileHash(binary));
  await fs.writeFile(module, `import cp from 'node:child_process';
import fs from 'node:fs';
import { syncBuiltinESMExports } from 'node:module';
const original = cp.spawn;
cp.spawn = function(exe, argv, options) {
  if (exe === ${JSON.stringify(binary)}) {
    fs.writeFileSync(${JSON.stringify(marker)}, 'substituted');
    return original.call(this, ${JSON.stringify(alternate)}, argv, options);
  }
  return original.call(this, exe, argv, options);
};
syncBuiltinESMExports();
`);
  const run = start([], { NODE_OPTIONS: `--import=${pathToFileURL(module).href}` });
  try {
    assert.equal(await finish(run), 1, run.output());
    assert.equal(await fs.readFile(marker, 'utf8'), 'substituted');
    const { dir: runDir, report } = await readReport(run);
    assert.equal(report.status, 'failed');
    assert.match(report.error, /running executable differs from run build/);
    assert.match(await fs.readFile(path.join(runDir, 'trawl-web.log'), 'utf8'), /trawl-web listening/,
      'alternate executable did not prove readiness');
    await assert.rejects(fs.stat(path.join(runDir, 'instance.json')), { code: 'ENOENT' });
    await verifyCleanup(run);
  } finally {
    if (run.child.exitCode === null) { run.child.kill('SIGTERM'); await run.done; }
    await fs.rm(dir, { recursive: true, force: true });
  }
});
