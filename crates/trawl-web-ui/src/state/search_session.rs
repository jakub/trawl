// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snapshot query resource: re-fetches on `(effective_q, page)` tuple change.

use leptos::prelude::*;
use trawl_api::QueryResponse;

use crate::api::{self, ApiError};
use crate::query_merge::{Filter, RangeSpec};

/// Query provenance captured with one response. Keep structured URL state
/// separate from effective DSL so Include preserves the original navigation.
#[derive(Clone)]
pub struct ExecutedQuery {
    pub effective: String,
    pub base: String,
    pub filters: Vec<Filter>,
    pub range: RangeSpec,
}

#[derive(Clone)]
pub struct ExecutedResponse {
    pub query: ExecutedQuery,
    pub response: QueryResponse,
}
impl std::ops::Deref for ExecutedResponse {
    type Target = QueryResponse;
    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

/// Build a Leptos `LocalResource` that runs `api::query(q, page)` whenever
/// the effective-query or page signals change.
///
/// Callers should feed an "effective" query (base DSL + filters + range
/// merged via `state::query::effective_query`), not the user's raw editor
/// buffer. The resource doesn't care where the string came from — it just
/// re-fires on value changes.
///
/// Empty `q` short-circuits to an `Ok(empty result)` without a network
/// round-trip so the first page load doesn't fire a POST with `?q=`.
///
/// Uses `LocalResource` (CSR-only, no Serialize/Deserialize bounds on the
/// result type) because this crate doesn't do SSR hydration.
/// `pending` tracks the request itself: a resource retains its previous
/// `Some` response during a reload, so presence cannot report loading.
pub fn rows_resource(
    effective_q: Memo<String>,
    page: Memo<usize>,
    pending: WriteSignal<bool>,
    base: Memo<String>,
    filters: Memo<Vec<Filter>>,
    range: Memo<RangeSpec>,
) -> LocalResource<Result<ExecutedResponse, ApiError>> {
    LocalResource::new(move || {
        let q = effective_q.get();
        let p = page.get();
        let query = ExecutedQuery {
            effective: q.clone(),
            base: base.get(),
            filters: filters.get(),
            range: range.get(),
        };
        async move {
            if q.trim().is_empty() {
                return Ok(ExecutedResponse {
                    query,
                    response: empty_response(),
                });
            }
            let _ = pending.try_set(true);
            let response = api::query(&q, p).await;
            // A response can finish after route teardown disposed the signal.
            let _ = pending.try_set(false);
            response.map(|response| ExecutedResponse { query, response })
        }
    })
}

fn empty_response() -> QueryResponse {
    QueryResponse {
        execution: None,
        result: trawl_api::value::QueryResult::empty(),
        truncated: false,
        pagination: trawl_api::PaginationMeta {
            limit: api::PAGE_SIZE,
            offset: 0,
            returned: 0,
        },
        // A placeholder for "no query yet" — no execution, no notice,
        // and no columns to render as anything.
        degraded_fields: Vec::new(),
        severity_columns: Vec::new(),
    }
}
