// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// Shared only by the experiment runner and its attached evidence scenario.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import fs from 'node:fs/promises';
import path from 'node:path';

export const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
export async function fileHash(file) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest('hex');
}

// Keep the preparation tree hash unchanged. Reject links so a run-owned copy
// cannot continue reading assets from the checkout after another build.
export async function spaHash(dir, origin, relative = '') {
  assert.ok((await fs.lstat(dir)).isDirectory(), 'SPA directory must not be a symlink');
  const hash = createHash('sha256');
  for (const entry of (await fs.readdir(dir, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name))) {
    const file = path.join(dir, entry.name);
    const name = relative ? `${relative}/${entry.name}` : entry.name;
    hash.update(entry.name);
    if (entry.isDirectory()) hash.update(await spaHash(file, origin, name));
    else {
      assert.ok(entry.isFile(), 'SPA assets must be regular files');
      const bytes = await fs.readFile(file);
      hash.update(bytes);
      if (origin) {
        const url = `${origin}/${name.split('/').map(encodeURIComponent).join('/')}`;
        const response = await fetch(url, { redirect: 'error', signal: AbortSignal.timeout(15000),
          headers: { 'Accept-Encoding': 'identity', 'Cache-Control': 'no-cache' } });
        assert.equal(response.status, 200, 'served SPA asset failed');
        assert.ok([null, 'identity'].includes(response.headers.get('content-encoding')), 'served SPA encoding changed');
        const served = createHash('sha256');
        let size = 0;
        for await (const chunk of response.body) {
          size += chunk.length;
          assert.ok(size <= bytes.length, 'served SPA asset exceeds snapshot size');
          served.update(chunk);
        }
        assert.equal(size, bytes.length, 'served SPA asset size differs from snapshot');
        assert.equal(served.digest('hex'), sha256(bytes), 'served SPA asset differs from snapshot');
      }
    }
  }
  return hash.digest('hex');
}

async function processStart(pid) {
  const stat = await fs.readFile(`/proc/${pid}/stat`, 'utf8');
  // comm is parenthesized and may itself contain spaces or parentheses.
  const fields = stat.slice(stat.lastIndexOf(')') + 2).trim().split(/\s+/);
  assert.ok(!['Z', 'X'].includes(fields[0]), 'recorded process has exited');
  assert.match(fields[19], /^\d+$/, 'invalid process start ticks');
  return { bootId: (await fs.readFile('/proc/sys/kernel/random/boot_id', 'utf8')).trim(), startTicks: fields[19] };
}

export async function processIdentity(pid, config, executableHash, spaDirectory) {
  assert.ok(Number.isSafeInteger(pid) && pid > 1, 'invalid recorded PID');
  const before = await processStart(pid);
  const argv = (await fs.readFile(`/proc/${pid}/cmdline`, 'utf8')).split('\0');
  const flag = argv.indexOf('--config');
  assert.ok(flag >= 0 && argv[flag + 1] === config, 'recorded process does not own this run config');
  if (spaDirectory) {
    // The environment contains credentials. Select this one nonsecret entry
    // in memory and never include the environment in a report or assertion.
    const entries = (await fs.readFile(`/proc/${pid}/environ`, 'utf8')).split('\0');
    assert.ok(entries.filter(e => e.startsWith('TRAWL_WEB_SPA_DIR=')).length === 1
      && entries.includes(`TRAWL_WEB_SPA_DIR=${spaDirectory}`), 'process does not serve the owned SPA directory');
  }
  // Read the kernel's executable handle, not the current build pathname.
  // A rebuild may replace that pathname while the old process stays alive.
  const executableSHA256 = await fileHash(`/proc/${pid}/exe`);
  assert.equal(executableSHA256, executableHash, 'running executable differs from run build');
  const configSHA256 = await fileHash(config);
  assert.deepEqual(await processStart(pid), before, 'process changed during identity check');
  return { pid, ...before, executableSHA256, configSHA256 };
}

export async function readRunBuild(run, instance) {
  const file = path.join(run, 'build.json');
  assert.ok((await fs.lstat(file)).isFile(), 'run build record must be a regular file');
  const bytes = await fs.readFile(file);
  assert.equal(sha256(bytes), instance.buildSHA256, 'run build record digest differs from instance');
  const record = JSON.parse(bytes);
  assert.equal(record.schema, 1, 'unsupported run build record');
  assert.equal(record.runId, instance.runId, 'build record belongs to another run');
  assert.equal(record.runId, path.basename(run), 'build record differs from selected directory');
  return record;
}

export async function verifyRunIdentity(run, instance, record, { served = true } = {}) {
  assert.deepEqual(await readRunBuild(run, instance), record, 'run build record changed');
  const privateDir = path.join(run, 'private');
  assert.equal(await fs.realpath(privateDir), privateDir, 'private directory must not be a symlink');
  const spaDirectory = path.join(privateDir, 'spa');
  for (const [name, config, binary] of [['trawld', 'trawld.toml', 'trawld'], ['web', 'web.toml', 'trawl-web']]) {
    const file = path.join(privateDir, config);
    assert.equal(await fs.realpath(file), file, 'run config must not be a symlink');
    const actual = await processIdentity(instance.pids[name], file, record.build.binaries[binary], name === 'web' ? spaDirectory : undefined);
    assert.deepEqual(actual, instance.processIdentity[name], 'recorded process identity changed');
  }
  const config = await fs.readFile(path.join(privateDir, 'web.toml'), 'utf8');
  const origin = new URL(instance.browserOrigin);
  assert.equal(origin.protocol, 'http:');
  assert.equal(origin.hostname, '127.0.0.1');
  assert.equal(origin.origin, instance.browserOrigin);
  assert.ok(config.includes(`bind_addr = "127.0.0.1:${origin.port}"`), 'run browser origin differs from config');
  assert.ok(config.includes(`upstream_url = "${instance.upstream}"`), 'run upstream differs from config');
  assert.equal(await spaHash(spaDirectory), record.build.spaHash, 'owned SPA differs from run build');
  if (served) assert.equal(await spaHash(spaDirectory, origin.origin), record.build.spaHash, 'served SPA differs from run build');
  // Bracket the HTTP requests with a process-generation check too. These
  // checks detect drift; they are not isolation from a hostile local user.
  for (const name of ['trawld', 'web']) {
    const expected = instance.processIdentity[name];
    assert.deepEqual(await processStart(expected.pid), { bootId: expected.bootId, startTicks: expected.startTicks }, 'process changed during SPA verification');
  }
}
