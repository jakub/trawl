// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { corpus, verifyWholeResult, verifyBrowserPage } from './workload.mjs';

test('seeded traffic is stable, varied, and carries consecutive IDs', () => {
  const rows = corpus(42, 1000, 'test');
  assert.deepEqual(rows, corpus(42, 1000, 'test'));
  assert.notDeepEqual(rows, corpus(43, 1000, 'test'));
  assert.deepEqual(new Set(rows.map(r => r.service)), new Set(['web', 'auth', 'app']));
  assert.deepEqual(new Set(rows.map(r => r.status)), new Set([200, 503]));
  assert.equal(rows.at(-1).experiment_seq, 999);
});

test('the oracle detects loss, duplication, value corruption, and a cut result', () => {
  const events = corpus(42, 3, 'test');
  const result = { pagination: { total: 3 },
    columns: [{ name: 'status', type: 'integer' }, { name: 'experiment_seq', type: 'integer' }],
    rows: events.map(e => [String(e.status), String(e.experiment_seq)]).reverse(),
  };
  verifyWholeResult(result, events);
  for (const mutate of [
    r => r.rows.pop(),
    r => r.rows.push(r.rows[0]),
    r => { r.rows[0][0] = '999'; },
    r => { r.pagination.total = r.rows.length + 1; },
    // The typed wire: a JSON number under `integer`, a non-canonical
    // spelling, and an untyped column are wire faults, not values.
    r => { r.rows[0][0] = Number(r.rows[0][0]); },
    r => { r.rows[0][1] = `0${r.rows[0][1]}`; },
    r => { delete r.columns[1].type; },
  ]) {
    const broken = structuredClone(result);
    mutate(broken);
    assert.throws(() => verifyWholeResult(broken, events));
  }
});

test('traffic avoids fixed eight-event host cycles and single-parity errors', () => {
  for (const seed of [42, 43, 44]) {
    const rows = corpus(seed, 1000, 'test');
    assert.notDeepEqual(rows.slice(0, 8).map(r => r.host), rows.slice(8, 16).map(r => r.host));
    assert.equal(new Set(rows.filter(r => r.status === 503).map(r => r.experiment_seq % 2)).size, 2);
  }
});

test('browser oracle refuses a short page even when returned count agrees with it', () => {
  const events = corpus(42, 100, 'test');
  const response = { pagination: { returned: 50, total: 100 },
    columns: [{ name: 'experiment_seq', type: 'integer' }, { name: 'status', type: 'integer' }],
    rows: events.slice(0, 50).map(e => [String(e.experiment_seq), String(e.status)]),
  };
  verifyBrowserPage(response, events);
  response.rows = response.rows.slice(0, 10);
  response.pagination.returned = 10;
  assert.throws(() => verifyBrowserPage(response, events));
  verifyBrowserPage(response, events.slice(0, 10));
});
