// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { saveSnapshotMutationKilled } from './check-save-mutation.mjs';

function target(entry, status = 'failed', message = 'Save snapshot preview must equal the exact editor buffer') {
  return {
    title: `Save captures editor buffer: ${entry} preview and POST preserve exact text without filters or range`,
    tests: [{ results: [{ status, errors: [{ message }] }] }],
  };
}
function report(...specs) { return { suites: [{ suites: [{ specs }] }] }; }

test('both preview failures count as a kill through nested suites', () => {
  assert.equal(saveSnapshotMutationKilled(report(target('editor'), target('toolbar'))), true);
});

test('exact POST assertion failures also count as a kill', () => {
  const message = 'Save snapshot POST must contain the exact editor buffer once';
  assert.equal(saveSnapshotMutationKilled(report(target('editor', 'failed', message), target('toolbar', 'failed', message))), true);
});

test('a missing or duplicate entry does not count', () => {
  assert.equal(saveSnapshotMutationKilled(report(target('editor'))), false);
  assert.equal(saveSnapshotMutationKilled(report(target('editor'), target('toolbar'), target('toolbar'))), false);
});

test('timeouts, skips and passes cannot supply a failed snapshot assertion', () => {
  for (const status of ['timedOut', 'skipped', 'passed', 'interrupted']) {
    assert.equal(saveSnapshotMutationKilled(report(target('editor'), target('toolbar', status))), false);
  }
});

test('unrelated failures and wrong test titles do not count', () => {
  assert.equal(saveSnapshotMutationKilled(report(target('editor'), target('toolbar', 'failed', 'browserType.launch: executable missing'))), false);
  assert.equal(saveSnapshotMutationKilled(report(target('editor'), target('other'))), false);
});

test('missing report structure or results fail closed', () => {
  for (const value of [null, {}, { suites: [] }, { suites: 'invalid' }]) {
    assert.equal(saveSnapshotMutationKilled(value), false);
  }
  const missing = target('toolbar');
  missing.tests[0].results = [];
  assert.equal(saveSnapshotMutationKilled(report(target('editor'), missing)), false);
});

test('the CLI rejects missing and malformed JSON reports', () => {
  const checker = fileURLToPath(new URL('./check-save-mutation.mjs', import.meta.url));
  // This Bash mutation runner targets Linux. stdin avoids a temporary report.
  for (const input of ['', '{invalid']) {
    const result = spawnSync(process.execPath, [checker, '/dev/stdin'], { input, encoding: 'utf8' });
    assert.notEqual(result.status, 0);
    assert.equal(result.signal, null);
  }
  const missing = spawnSync(process.execPath, [checker], { encoding: 'utf8' });
  assert.notEqual(missing.status, 0);
  assert.equal(missing.signal, null);
});
