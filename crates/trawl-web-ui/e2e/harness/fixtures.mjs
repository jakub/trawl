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
    },
  };
}

/** Minimal valid QueryResponse: zero rows, zero columns — the results
 * table renders its "No fish in this net yet" empty state, which is
 * fine for specs that only care about the request being made and the
 * page not crashing. */
export function queryResponse() {
  return {
    columns: [],
    rows: [],
    truncated: false,
    pagination: { limit: 50, offset: 0, returned: 0 },
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
// `schedule` is the drawer's schedule form with something to edit: TWO
// nets, one of them carrying a windowed schedule, plus a run whose
// stored result is longer than one preview page. It is a separate
// scenario rather than a change to `populated`/`corpus` because those
// two are pinned as one service and one net, and the specs written
// against them count rows.

/** `GET /api/v1/saved` under `schedule`: the `populated` net verbatim,
 * plus a second net whose schedule tiles (`window: "since_last"`, a 5m
 * lag and a coverage watermark). One drawer per case, so a spec can put
 * the windowed and the unwindowed side by side without a second
 * scenario. */
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
