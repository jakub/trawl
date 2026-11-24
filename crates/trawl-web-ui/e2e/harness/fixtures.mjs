// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The payloads written inline below are hand-kept. The ones under
// `wire/` are drift-GUARDED: `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs`
// decodes each of those files into the very trawl-api struct the SPA
// decodes it with. Prefer a `wire/` file for anything new.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const WIRE_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), 'wire');

// Read text once, parse per call: every caller gets its own object, so a
// route handler that stamps a field into a response (the repin status
// route stamps `field`) cannot mutate the next caller's copy.
const wireText = new Map();

export function wire(name) {
  if (!wireText.has(name)) {
    wireText.set(name, fs.readFileSync(path.join(WIRE_DIR, `${name}.json`), 'utf8'));
  }
  return JSON.parse(wireText.get(name));
}

// A generated result goes through these, never through a hand-spelled
// cell: the SPA refuses a whole response for one cell its column type
// refuses.

const DECIMAL = /^-?(0|[1-9][0-9]*)$/;

/** How the server writes a `timestamp` cell. */
const WIRE_INSTANT = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}(Z|[+-]\d{2}:\d{2})$/;

export function wireInstant(at) {
  return new Date(at).toISOString().replace(/\.(\d{3})Z$/, '.$1000Z');
}

/** A number past 2^53 has already lost digits, so it throws instead of
 * being spelled. */
export function wireCell(type, value) {
  if (value === null) return null;
  if (type === 'integer') {
    if (typeof value === 'number' && Number.isSafeInteger(value)) return String(value);
    if (typeof value === 'string' && DECIMAL.test(value) && value !== '-0') return value;
    throw new Error(`${JSON.stringify(value)} is not an integer cell`);
  }
  if (type === 'timestamp' && !(typeof value === 'string' && WIRE_INSTANT.test(value))) {
    throw new Error(`${JSON.stringify(value)} is not a timestamp cell`);
  }
  return value;
}

export function typedResult(columns, rows) {
  return {
    columns,
    rows: rows.map((row) => {
      if (row.length !== columns.length) throw new Error(`a ${row.length}-cell row for ${columns.length} columns`);
      return row.map((cell, i) => wireCell(columns[i].type, cell));
    }),
  };
}

export function dataFrame(fields) {
  return typedResult(
    fields.map(([name, type]) => ({ name, type })),
    [fields.map(([, , cell]) => cell)],
  );
}

export function catalogFieldResponse() {
  return wire('catalog-field');
}

export function repinStatusRunningResponse() {
  return wire('repin-status-running');
}

/** Same id as the running job on purpose: the case file tracks a job by
 * id and reads a different one as the install-wide one-running slot
 * having moved on. */
export function repinStatusSucceededResponse() {
  return wire('repin-status-succeeded');
}

export function repinStatusNoJobResponse() {
  return { job: null };
}

/** No `server_manage`, so AuthShell never opens the admin stats SSE
 * stream and the only /api/v1/stream open in these tests is the one the
 * live-tail toggle opens itself. */
export function meResponse() {
  return {
    name: 'e2e',
    roles: ['viewer'],
    permissions: ['query', 'saved_query'],
    exp: Math.floor(Date.now() / 1000) + 3600,
  };
}

export function healthResponse() {
  // HealthStatus is #[serde(rename_all = "lowercase")] — "ok", not "Ok".
  return {
    status: 'ok',
    checks: {
      duckdb: 'ok',
      auth_db: 'ok',
      storage_db: 'ok',
      data_path: 'ok',
      ingest_capacity: 'ok',
    },
  };
}

export function queryResponse() {
  return {
    columns: [],
    rows: [],
    pagination: { limit: 50, offset: 0, returned: 0, total: 0 },
    degraded_fields: [],
    severity_columns: [],
    execution: { started_at: '2026-09-15T12:34:56Z', duration_ms: 125 },
  };
}

export function historyResponse() {
  return { entries: [], total: 0 };
}

export function listSavedResponse() {
  return { queries: [] };
}

export function serviceSchemaResponse() {
  return wire('service-schema');
}

/** The default stays empty — specs that assert a pristine page count on
 * it. */
export function populatedListSavedResponse() {
  return wire('saved-queries');
}

export function populatedServiceSchemaResponse() {
  return wire('service-schema-populated');
}

/** These are SHAPES, not whole queries: the service drawer composes its
 * reads with a field or service name in them, so the stub can only
 * recognise the stage. `tests/e2e_wire_fixture_contract.rs` pins the
 * first three against the drawer's builders.
 */
export const QUERY_SHAPES = {
  topValues: '| top 10 ',
  cardinality: '| stats dc(',
  timechart: '| timechart span=1h count()',
  statsBy: '| stats count() by ',
};

/** No two sort orders of these rows agree, so a sort spec can tell
 * ascending from descending by reading the first row. */
export function corpusQueryRowsResponse() {
  return wire('query-rows');
}

/** One count is NEGATIVE so the signed (two-sided) track is exercised,
 * and one is NULL so the "no bar, no value" arm is too. */
export function corpusStatsByResponse() {
  return wire('query-stats-by');
}

/** The drawer builds `dc(<field>) as c<i>` per column of the service it
 * mounted and reads the answer back BY POSITION, so the columns stand for
 * `service-schema-corpus.json`'s columns in order. The third count is
 * null on purpose. */
export function corpusCardinalityResponse() {
  return wire('query-cardinality');
}

/** Its value column is named `value`, which `parse_top_values` accepts
 * for ANY field, so one fixture serves whichever field the spec opened. */
export function corpusTopValuesResponse() {
  return wire('query-top-values');
}

export function corpusHistoryResponse() {
  return wire('history');
}

export function corpusNetRunsResponse() {
  return wire('net-runs');
}

export function corpusRunResultResponse() {
  return wire('run-result');
}

export function corpusAllRunsResponse() {
  return wire('runs-all');
}

export function corpusRunsStatsResponse() {
  return wire('runs-stats');
}

/** `build_histogram` finds its time column by name (`_time`) and its
 * counts by a column named `count` or prefixed with it, so those two
 * names are the contract. */
export function corpusTimechartResponse() {
  return wire('query-timechart');
}

export function corpusServiceSchemaResponse() {
  return wire('service-schema-corpus');
}

// `schedule` is a separate scenario rather than a change to
// `populated`/`corpus` because those two are pinned as one service and
// one net, and the specs written against them count rows.

export function windowedListSavedResponse() {
  return wire('saved-queries-windowed');
}

export function spentListSavedResponse() {
  return wire('saved-queries-spent');
}

export function scheduleSavedResponse() {
  return wire('schedule-saved');
}

/** The message is the server's own `WindowPolicyError::TimeClause`
 * sentence. */
export function scheduleConflictResponse() {
  return wire('schedule-conflict');
}

export function runStartedResponse() {
  return wire('run-started');
}

export function runRefusedResponse() {
  return wire('run-refused');
}

export function scheduleNetRunsResponse() {
  return wire('schedule-net-runs');
}

/** `seq` is the row's 1-based position. */
export function pagedRunResultResponse() {
  return wire('run-result-paged');
}

/** The sentence `from_saved::unavailable_run_message` writes. */
export function unavailableRunResponse(runId) {
  return {
    error: {
      code: 'bad_request',
      message: `report run ${runId} succeeded, but its stored result is unavailable; `
        + 'no older run was substituted',
    },
  };
}

/** Fixtures use ASCII names, so Buffer.compare implements the database's
 * C collation. */
export function globalRunsOrder(params) {
  if (params.getAll('sort').length > 1 || params.getAll('dir').length > 1) return null;
  const sort = params.get('sort') ?? 'started';
  const dir = params.get('dir') ?? 'desc';
  if (!['net', 'status', 'started', 'duration', 'rows'].includes(sort) ||
      !['asc', 'desc'].includes(dir)) return null;
  return { sort, dir };
}

export function sortedGlobalRuns(runs, { sort, dir }) {
  const sign = dir === 'asc' ? 1 : -1;
  const compare = (a, b) => a < b ? -1 : a > b ? 1 : 0;
  const text = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
  const started = (a, b) => compare(Date.parse(a.started_at), Date.parse(b.started_at));
  return [...runs].sort((a, b) => {
    let primary;
    if (sort === 'net') primary = text(a.net_name.toLowerCase(), b.net_name.toLowerCase());
    else if (sort === 'status') primary = text(a.status, b.status);
    else if (sort === 'started') primary = started(a, b);
    else {
      const key = sort === 'duration' ? 'duration_ms' : 'row_count';
      // Nulls stay last in both directions, independently of tie-breakers.
      if (a[key] == null && b[key] != null) return 1;
      if (a[key] != null && b[key] == null) return -1;
      primary = a[key] == null ? 0 : compare(a[key], b[key]);
    }
    if (primary) return primary * sign;
    if (sort === 'started') return compare(a.id, b.id) * sign;
    return -started(a, b) || -compare(a.id, b.id);
  });
}

/** Callers sort this before slicing a page. */
export function paginationGlobalRuns(total) {
  const source = wire(total <= 3 ? 'pagination-runs-all' : 'runs-sort').runs;
  return Array.from({ length: total }, (_, i) => ({
    ...source[i % source.length],
    id: source[i % source.length].id + Math.floor(i / source.length) * 1000,
  }));
}

// `total` is what the execution produced, measured BEFORE the window was
// cut from it.

const AGGREGATE_EPOCH = Date.parse('2026-09-01T00:00:00Z');
const AGGREGATE_BUCKET_MS = 2 * 60 * 1000;

/** Not constant: a flat line would hide a chart that plotted the same
 * point N times.
 *
 * Row 0 is a deliberate spike, well above the 13 the cycle reaches. It
 * puts the result's largest count on the FIRST page only, which is what
 * lets a width assertion tell a whole-result scale from a per-page one.
 * Every count stays positive so the track is one-sided. */
function aggregateCount(i) {
  return i === 0 ? 47 : ((i * 7) % 13) + 1;
}

function aggregateInstant(i) {
  return wireInstant(AGGREGATE_EPOCH + i * AGGREGATE_BUCKET_MS);
}

export function aggregateTimechartResponse(buckets, total, offset, limit) {
  const rows = Array.from({ length: buckets }, (_, i) => [aggregateInstant(i), aggregateCount(i)]);
  return aggregateBody([TIME, COUNT], rows, total, offset, limit);
}

/** Rows come out bucket-major, the order the server's ORDER BY produces. */
export function aggregateGroupedTimechartResponse(buckets, hosts, total, offset, limit) {
  const rows = [];
  for (let i = 0; i < buckets; i++) {
    for (let h = 0; h < hosts; h++) {
      rows.push([aggregateInstant(i), `web-${String(h + 1).padStart(2, '0')}`, aggregateCount(i + h)]);
    }
  }
  return aggregateBody(
    [TIME, { name: 'host', type: 'string' }, COUNT],
    rows,
    total,
    offset,
    limit,
  );
}

/** A shape the chart refuses whatever the window. */
export function aggregatePivotResponse(groups, total, offset, limit) {
  const rows = Array.from({ length: groups }, (_, i) => [
    `web-${String(i + 1).padStart(2, '0')}`,
    aggregateCount(i),
    aggregateCount(i + 1),
  ]);
  return aggregateBody(
    [{ name: 'host', type: 'string' }, { name: '200', type: 'integer' }, { name: '500', type: 'integer' }],
    rows,
    total,
    offset,
    limit,
  );
}

/** Already in the quick-start query's `sort -errors` order. */
export const RANKED_SERVICES = [
  ['api', 412],
  ['checkout', 208],
  ['web', 97],
  ['auth', 31],
  ['worker', 4],
];

/** A group search prepends `service="<name>"` ahead of the first pipe,
 * and that follow-up answers the one matching row. */
export function aggregateRankedResponse(dsl, offset, limit) {
  const only = /(?:^|\s)service\s*=\s*"([^"]*)"/.exec(dsl.split('|')[0])?.[1];
  const rows = RANKED_SERVICES
    .filter(([service]) => only === undefined || service === only)
    .slice(0, 10)
    .map(([service, errors]) => [service, errors]);
  return aggregateBody(
    [{ name: 'service', type: 'string' }, { name: 'errors', type: 'integer' }],
    rows,
    null,
    offset,
    limit,
  );
}

export function aggregateStatsByResponse(groups, total, offset, limit) {
  const rows = Array.from({ length: groups }, (_, i) => [`${200 + i}`, aggregateCount(i)]);
  return aggregateBody([{ name: 'status', type: 'string' }, COUNT], rows, total, offset, limit);
}

const TIME = { name: '_time', type: 'timestamp' };
const COUNT = { name: 'count', type: 'integer' };

function aggregateBody(columns, rows, total, offset, limit) {
  const produced = total ?? rows.length;
  const window = rows.slice(offset, offset + limit);
  return {
    ...typedResult(columns, window),
    pagination: { limit, offset, returned: window.length, total: produced },
    degraded_fields: [],
    severity_columns: [],
    execution: { started_at: '2026-09-15T12:34:56Z', duration_ms: 125 },
  };
}
