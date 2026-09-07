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
