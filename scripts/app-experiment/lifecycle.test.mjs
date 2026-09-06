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
