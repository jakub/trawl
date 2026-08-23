// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Canned response payloads, shaped after the real wire types the SPA
// decodes. Sources (re-verify on drift):
//   MeResponse       crates/trawl-web/src/routes/auth.rs, crates/trawl-web-ui/src/api/mod.rs
//   QueryResponse     crates/trawl-api/src/lib.rs (QueryResponse/QueryResult/PaginationMeta)
//   HistoryResponse   crates/trawl-api/src/lib.rs (HistoryResponse/HistoryEntryResponse)
//   HealthResponse    crates/trawl-api/src/lib.rs (HealthResponse/HealthStatus)

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
  return { status: 'ok' };
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
  return { services: [] };
}
