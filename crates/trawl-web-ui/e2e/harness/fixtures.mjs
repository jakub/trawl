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

/** `GET /api/v1/saved` under the `populated` scenario: one net, so the
 * nets table renders a row with an `ActionsMenu` in it and `?net=1`
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
};

/** `POST /api/v1/query` under `corpus` for a plain search: 8 events over
 * `_time, host, status, message`. `host` carries 6 distinct values, one
 * more than the facet rail shows before it offers "+ 1 more", and no two
 * sort orders of these rows agree, so a sort spec can tell ascending from
 * descending by reading the first row. */
export function corpusQueryRowsResponse() {
  return wire('query-rows');
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
