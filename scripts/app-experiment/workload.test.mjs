// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { corpus, verifyRows } from './workload.mjs';

test('seeded traffic is stable, varied, and carries consecutive IDs', () => {
  const rows = corpus(42, 1000, 'test');
  assert.deepEqual(rows, corpus(42, 1000, 'test'));
  assert.notDeepEqual(rows, corpus(43, 1000, 'test'));
  assert.deepEqual(new Set(rows.map(r => r.service)), new Set(['web', 'auth', 'app']));
  assert.deepEqual(new Set(rows.map(r => r.status)), new Set([200, 503]));
  assert.equal(rows.at(-1).experiment_seq, 999);
});

test('the oracle detects loss, duplication, value corruption, and truncation', () => {
  const events = corpus(42, 3, 'test');
  const result = { truncated: false,
    columns: [{ name: 'status' }, { name: 'experiment_seq' }],
    rows: events.map(e => [e.status, e.experiment_seq]).reverse(),
  };
  verifyRows(result, events);
  for (const mutate of [
    r => r.rows.pop(),
    r => r.rows.push(r.rows[0]),
    r => { r.rows[0][0] = 999; },
    r => { r.truncated = true; },
  ]) {
    const broken = structuredClone(result);
    mutate(broken);
    assert.throws(() => verifyRows(broken, events));
  }
});
