// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Canned response payloads, shaped after the real wire types the SPA
// decodes. Sources (re-verify on drift):
//   MeResponse       crates/trawl-web/src/routes/auth.rs, crates/trawl-web-ui/src/api/mod.rs
//   QueryResponse     crates/trawl-api/src/lib.rs (QueryResponse/QueryResult/PaginationMeta)
//   HistoryResponse   crates/trawl-api/src/lib.rs (HistoryResponse/HistoryEntryResponse)
//   HealthResponse    crates/trawl-api/src/lib.rs (HealthResponse/HealthStatus)
//
// The payloads written inline below are hand-kept. The ones under
// `wire/` are drift-GUARDED: `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs`
// decodes each of those files into the very trawl-api struct the SPA
// decodes it with, so a fixture that lost a required key fails a native
// `cargo nextest` run instead of quietly rendering an error arm in the
// browser. Prefer a `wire/` file for anything new.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const WIRE_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), 'wire');

// Read text once, parse per call: every caller gets its own object, so a
// route handler that stamps a field into a response (the repin status
// route stamps `field`) cannot mutate the next caller's copy.
const wireText = new Map();

/** One drift-guarded wire body, freshly parsed. */
export function wire(name) {
  if (!wireText.has(name)) {
    wireText.set(name, fs.readFileSync(path.join(WIRE_DIR, `${name}.json`), 'utf8'));
  }
  return JSON.parse(wireText.get(name));
}

/** `GET /api/v1/schema/field?name=` — one field's pin, its per-service
 * observations and its conflict evidence. The `name` a route answers with
 * is the one that was asked for; this body carries the rest. */
export function catalogFieldResponse() {
  return wire('catalog-field');
}

/** `GET /api/v1/schema/repin/status` while a job is still running. The
 * case file adopts this at mount and starts its 3s poll from it. */
export function repinStatusRunningResponse() {
  return wire('repin-status-running');
}

/** The same job once it has finished. Same id on purpose: the case file
 * tracks a job by id and reads a different one as the install-wide
 * one-running slot having moved on. */
export function repinStatusSucceededResponse() {
  return wire('repin-status-succeeded');
}

/** `GET /api/v1/schema/repin/status` with nothing to report. */
export function repinStatusNoJobResponse() {
  return { job: null };
}

/** Non-admin identity: no `server_manage`, so AuthShell never opens the
 * admin stats SSE stream and the only /api/v1/stream open in these tests
 * is the one the live-tail toggle opens itself. */
export function meResponse() {
  return {
    name: 'e2e',
    roles: ['viewer'],
    permissions: ['query'],
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

/** Minimal valid QueryResponse: zero rows, zero columns — the results
 * table renders its "No events match this query" empty state, which is
 * fine for specs that only care about the request being made and the
 * page not crashing. */
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

/** `GET /api/v1/saved` under the `populated` scenario: one Net, so the
 * Nets table renders a row with direct action buttons and `?net=1`
 * opens the drawer. The default stays empty — specs that assert a
 * pristine page count on it. */
export function populatedListSavedResponse() {
  return wire('saved-queries');
}

/** `GET /api/v1/schema/services` under the `populated` scenario: one
 * service, which is what makes `?svc=nginx` mount the service drawer and
 * its three-tab strip. */
export function populatedServiceSchemaResponse() {
  return wire('service-schema-populated');
}

// ---- the `corpus` scenario ------------------------------------------------
//
// `corpus` is `populated` plus data: query rows, history, and the runs
// surfaces. It exists because the row controls, the facet rail and the
// results table are only reachable once something answers with rows, and
// `default`/`populated` deliberately answer empty.

/** The DSL substrings the stub dispatches `/api/v1/query` on.
 *
 * These are SHAPES, not whole queries: the service drawer composes its
 * reads with a field or service name in them, so the stub can only
 * recognise the stage. The first two are built in `src/drawer_query.rs`
 * (`top_values_query` writes `| top 10 <field>`, `cardinality_query`
 * writes `| stats dc(<f>) as …`); the third is built inline in the
 * overview pane of `src/components/service_drawer.rs`.
 * `tests/e2e_wire_fixture_contract.rs` calls the two builders and greps
 * the drawer source for the third, so a rewrite of any of them fails a
 * native test instead of silently routing a drawer read to the rows
 * fixture or to the 500 arm.
 */
export const QUERY_SHAPES = {
  topValues: '| top 10 ',
  cardinality: '| stats dc(',
  timechart: '| timechart span=1h count()',
  statsBy: '| stats count() by ',
};

/** `POST /api/v1/query` under `corpus` for a plain search: 8 events over
 * `_time, host, status, message`. `host` carries 6 distinct values, one
 * more than the facet rail shows before it offers "+ 1 more", and no two
 * sort orders of these rows agree, so a sort spec can tell ascending from
 * descending by reading the first row. */
export function corpusQueryRowsResponse() {
  return wire('query-rows');
}

/** `POST /api/v1/query` under `corpus` for a `stats count() by <field>`
 * pipeline: the one shape the categorical chart draws. Five groups over
 * `status` and `count`, of which one count is NEGATIVE so the signed
 * (two-sided) track is exercised, and one is NULL so the "no bar, no
 * value" arm is too. The exact table offers a search on `status` and
 * nothing on `count`, which is the F02 fix rendered. */
export function corpusStatsByResponse() {
  return wire('query-stats-by');
}

/** The service drawer's cardinality read. Its columns are `c0`, `c1` and
 * `c2`, because the drawer builds `dc(<field>) as c<i>` per column of the
 * service it mounted and reads the answer back BY POSITION — a field name
 * cannot be an alias, since `_time` is reserved. So the columns stand for
 * `service-schema-corpus.json`'s columns in order: `_time`, `status`,
 * `duration`. The third count is null on purpose: a field with no count is
 * skipped by `top_cardinality_rows`, so the overview card lists two rows
 * and the Fields tab shows `duration` as unknown. */
export function corpusCardinalityResponse() {
  return wire('query-cardinality');
}

/** The service drawer's top-values read. Its value column is named
 * `value`, which `parse_top_values` accepts for ANY field, so one fixture
 * serves whichever field the spec opened. */
export function corpusTopValuesResponse() {
  return wire('query-top-values');
}

/** `GET /api/v1/history` under `corpus`: one ordinary entry, and one
 * whose query text is 32769 ASCII bytes — one over `MAX_SEARCH_BYTES`
 * (`src/search_url.rs`), so rerunning it is refused by the navigator
 * rather than navigated. The length is pinned natively in
 * `tests/e2e_wire_fixture_contract.rs`. */
export function corpusHistoryResponse() {
  return wire('history');
}

/** `GET /api/v1/saved/{id}/runs` — two runs of net 1, one success and one
 * error, so the net drawer's Runs tab renders rows to expand. */
export function corpusNetRunsResponse() {
  return wire('net-runs');
}

/** `GET /api/v1/saved/{id}/runs/{run_id}` — run 501 with its result, which
 * is what a run row's expansion fetches. Answered for every run id: the
 * expansion is the thing under test, not the id routing. */
export function corpusRunResultResponse() {
  return wire('run-result');
}

/** `GET /api/v1/runs` — the same two runs, enriched with the owning net,
 * for the global Runs page. */
export function corpusAllRunsResponse() {
  return wire('runs-all');
}

/** `GET /api/v1/runs/stats` — the aggregates over those two runs. */
export function corpusRunsStatsResponse() {
  return wire('runs-stats');
}

/** The service drawer's overview histogram read. Built inline in
 * `src/components/service_drawer.rs` (`last=24h | timechart span=1h
 * count()`) rather than in `drawer_query.rs`, so the drift test pins the
 * substring against that source file instead of calling a builder.
 *
 * `build_histogram` finds its time column by name (`_time`) and its
 * counts by a column named `count` or prefixed with it, so those two
 * names are the contract. The rows are hourly buckets on 2026-09-01,
 * newest 23:00. The grid anchors on the newest row, so all six land
 * inside the 24 slots it lays out. */
export function corpusTimechartResponse() {
  return wire('query-timechart');
}

/** `GET /api/v1/schema/services` under `corpus` only. The `populated`
 * copy stays exactly as it is: this one adds a third column, `duration`,
 * and names it in the service's `degraded_fields`, which is the ONLY
 * thing that renders a field row's `.deg-btn` badge
 * (`service_card_fmt::is_degraded_column`). The name matches
 * `catalog-field.json`, so the case file the badge opens describes the
 * same field the drawer sent it to. */
export function corpusServiceSchemaResponse() {
  return wire('service-schema-corpus');
}

// ---- the `schedule` scenario ----------------------------------------------
//
// `schedule` is the drawer's schedule form with something to edit: four
// nets, one with no schedule and one per schedule mode (a tiling
// window, a fixed span, and a paused query-mode schedule), plus a run
// whose stored result is longer than one preview page. It is a separate
// scenario rather than a change to `populated`/`corpus` because those
// two are pinned as one service and one net, and the specs written
// against them count rows.

/** `GET /api/v1/saved` under `schedule`: the `populated` net verbatim
 * (no schedule), a net whose schedule tiles (`window: "since_last"`, a
 * 5m lag and a coverage watermark), a fixed-span net and a paused
 * query-mode net. One drawer per case, so a spec can put every schedule
 * shape beside the unscheduled net without a second scenario. */
export function windowedListSavedResponse() {
  return wire('saved-queries-windowed');
}

/** `PUT /api/v1/saved/{id}/schedule` — what a successful save answers.
 * The body is only decoded, never read for content: the assertion a
 * schedule spec makes is about the REQUEST. */
export function scheduleSavedResponse() {
  return wire('schedule-saved');
}

/** The 400 `/__ctl/schedule/refuse` arms. The message is the server's
 * own `WindowPolicyError::TimeClause` sentence, which is what the form
 * renders verbatim in its inline error. */
export function scheduleConflictResponse() {
  return wire('schedule-conflict');
}

/** `POST /api/v1/saved/{id}/run` under `schedule`: the claimed manual
 * run of the tiling net, with the window the server resolved at claim
 * time (from its watermark to the claim instant less its 5m lag). The
 * toast states these bounds, never ones the browser computed. */
export function runStartedResponse() {
  return wire('run-started');
}

/** The 409 `/__ctl/run/refuse` arms: an empty since_last window, in the
 * server's own words. The toast quotes the message verbatim. */
export function runRefusedResponse() {
  return wire('run-refused');
}

/** `GET /api/v1/saved/{id}/runs` under `schedule`: the two `corpus` runs
 * plus a newest third whose result is 45 rows. A separate file, so
 * `net-runs.json` and every `corpus` count written against it stay as
 * they are. */
export function scheduleNetRunsResponse() {
  return wire('schedule-net-runs');
}

/** `GET /api/v1/saved/{id}/runs/503` — 45 rows over `seq` and `message`,
 * where `seq` is the row's 1-based position. Two and a bit preview pages
 * at `PREVIEW_PAGE_SIZE` = 20, and every row names its own index, so a
 * paging spec can say WHICH rows it is looking at. */
export function pagedRunResultResponse() {
  return wire('run-result-paged');
}

/** `GET /api/v1/saved/{id}/runs/{run_id}` for a run whose stored result
 * file is gone: trawld's 409 envelope, carrying the sentence
 * `from_saved::unavailable_run_message` writes (issue #227). Built rather
 * than filed, because the sentence names the run. */
export function unavailableRunResponse(runId) {
  return {
    error: {
      code: 'bad_request',
      message: `report run ${runId} succeeded, but its stored result is unavailable; `
        + 'no older run was substituted',
    },
  };
}

/** Global ordering matches the API's closed token set. Fixtures use ASCII
 * names, so Buffer.compare implements the database's C collation. */
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

/** A full authorized fixture set; callers sort this before slicing a page.
 * The ordinary three-row fixture remains useful for local-filter tests. */
export function paginationGlobalRuns(total) {
  const source = wire(total <= 3 ? 'pagination-runs-all' : 'runs-sort').runs;
  return Array.from({ length: total }, (_, i) => ({
    ...source[i % source.length],
    id: source[i % source.length].id + Math.floor(i / source.length) * 1000,
  }));
}

// ---- the `aggregate` scenario ---------------------------------------------
//
// One aggregation, answered at whatever size the test asks for, so a
// spec can watch the whole-result fetch (ADR-0037) without a fixture per
// size. The rows are generated rather than pinned in `wire/` for that
// reason: what these specs read is the COUNT of rows and the window the
// request asked for, never a particular bucket's value.
//
// `total` is what the execution produced, measured BEFORE the window was
// cut from it — a slice never renames itself the result.

/** Bucket instants, two minutes apart from a fixed epoch. Fixed so a
 * chart of N buckets is the same picture on every run. */
const AGGREGATE_EPOCH = Date.parse('2026-09-01T00:00:00Z');
const AGGREGATE_BUCKET_MS = 2 * 60 * 1000;

/** Deterministic, non-negative and not constant: a flat line would hide
 * a chart that plotted the same point N times.
 *
 * Row 0 is a deliberate spike, well above the 13 the cycle reaches. It
 * puts the result's largest count on the FIRST page only, which is what
 * lets a width assertion tell the two scales apart: a bar measured
 * against the whole result keeps its width on page 2, while one
 * measured against its own page stretches against a page maximum of 13.
 * Without the spike both scales read 13 and the assertion passes on the
 * defect (ADR-0037). Every count stays positive so the track is
 * one-sided and the largest fills it. */
function aggregateCount(i) {
  return i === 0 ? 47 : ((i * 7) % 13) + 1;
}

/** A `_time` cell as a snapshot response actually spells it: UTC wall
 * clock, a space between the date and the time, and no zone suffix. The
 * web UI sends `timezone: None` and the server defaults the offset to
 * zero, so there is nothing for the cell to carry (ADR-0038). The LIVE
 * lane is the one that writes RFC 3339 with a `+00:00` offset. */
function aggregateInstant(i) {
  return new Date(AGGREGATE_EPOCH + i * AGGREGATE_BUCKET_MS)
    .toISOString()
    .replace('T', ' ')
    .replace('.000Z', '');
}

/** `POST /api/v1/query` under `aggregate` for a `| timechart` pipeline:
 * `buckets` rows of `_time, count`, sliced to the posted window. */
export function aggregateTimechartResponse(buckets, total, offset, limit) {
  const rows = Array.from({ length: buckets }, (_, i) => [aggregateInstant(i), aggregateCount(i)]);
  return aggregateBody([{ name: '_time' }, { name: 'count' }], rows, total, offset, limit);
}

/** The same pipeline with a `by host`: `buckets` × `hosts` rows of
 * `_time, host, count`, one series per host on the same bucket grid.
 * Rows come out bucket-major, the order the server's ORDER BY produces.
 *
 * The window is cut from the flattened rows, so a `total` larger than
 * what one fetch carries is a cut GROUPED result — the shape that
 * draws, and so the one whose coverage rung is worth a test. */
export function aggregateGroupedTimechartResponse(buckets, hosts, total, offset, limit) {
  const rows = [];
  for (let i = 0; i < buckets; i++) {
    for (let h = 0; h < hosts; h++) {
      rows.push([aggregateInstant(i), `web-${String(h + 1).padStart(2, '0')}`, aggregateCount(i + h)]);
    }
  }
  return aggregateBody(
    [{ name: '_time' }, { name: 'host' }, { name: 'count' }],
    rows,
    total,
    offset,
    limit,
  );
}

/** `POST /api/v1/query` under `aggregate` for a `| pivot` pipeline: a
 * shape the chart refuses whatever the window, which is what makes it
 * the case for "the shape sentence comes before the coverage one". */
export function aggregatePivotResponse(groups, total, offset, limit) {
  const rows = Array.from({ length: groups }, (_, i) => [
    `web-${String(i + 1).padStart(2, '0')}`,
    aggregateCount(i),
    aggregateCount(i + 1),
  ]);
  return aggregateBody(
    [{ name: 'host' }, { name: '200' }, { name: '500' }],
    rows,
    total,
    offset,
    limit,
  );
}

/** The ranked services the quick-start "Rank services by errors" query
 * answers, already in its `sort -errors` order. */
export const RANKED_SERVICES = [
  ['api', 412],
  ['checkout', 208],
  ['web', 97],
  ['auth', 31],
  ['worker', 4],
];

/** `POST /api/v1/query` under `aggregate` for the ranked
 * `stats count() as errors by service | sort -errors | head 10`
 * pipeline: `service, errors`, descending, never more than the ten rows
 * its `head` keeps. A group search prepends `service="<name>"` ahead of
 * the first pipe, and that follow-up answers the one matching row. */
export function aggregateRankedResponse(dsl, offset, limit) {
  const only = /(?:^|\s)service\s*=\s*"([^"]*)"/.exec(dsl.split('|')[0])?.[1];
  const rows = RANKED_SERVICES
    .filter(([service]) => only === undefined || service === only)
    .slice(0, 10)
    .map(([service, errors]) => [service, errors]);
  return aggregateBody([{ name: 'service' }, { name: 'errors' }], rows, null, offset, limit);
}

/** `POST /api/v1/query` under `aggregate` for a `| stats count() by`
 * pipeline: `groups` rows of `status, count`, sliced the same way. */
export function aggregateStatsByResponse(groups, total, offset, limit) {
  const rows = Array.from({ length: groups }, (_, i) => [`${200 + i}`, aggregateCount(i)]);
  return aggregateBody([{ name: 'status' }, { name: 'count' }], rows, total, offset, limit);
}

function aggregateBody(columns, rows, total, offset, limit) {
  // `total` before the slice, `returned` after it.
  const produced = total ?? rows.length;
  const window = rows.slice(offset, offset + limit);
  return {
    columns,
    rows: window,
    pagination: { limit, offset, returned: window.length, total: produced },
    degraded_fields: [],
    severity_columns: [],
    execution: { started_at: '2026-09-15T12:34:56Z', duration_ms: 125 },
  };
}
