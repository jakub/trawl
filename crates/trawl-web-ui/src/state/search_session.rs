// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snapshot query resource: re-fetches on `(executed_q, page)` tuple change.

use leptos::prelude::*;
use trawl_api::QueryResponse;

use crate::api::{self, ApiError};

/// Build a Leptos `LocalResource` that runs `api::query(q, page)` whenever
/// the executed-query or page signals change.
///
/// Empty `q` short-circuits to an `Ok(empty result)` without a network
/// round-trip so the first page load doesn't fire a POST with `?q=`.
///
/// Uses `LocalResource` (CSR-only, no Serialize/Deserialize bounds on the
/// result type) because this crate doesn't do SSR hydration.
pub fn rows_resource(
    executed_q: Memo<String>,
    page: Memo<usize>,
) -> LocalResource<Result<QueryResponse, ApiError>> {
    LocalResource::new(move || {
        let q = executed_q.get();
        let p = page.get();
        async move {
            if q.trim().is_empty() {
                return Ok(empty_response());
            }
            api::query(&q, p).await
        }
    })
}

fn empty_response() -> QueryResponse {
    QueryResponse {
        result: trawl_api::value::QueryResult::empty(),
        truncated: false,
        pagination: trawl_api::PaginationMeta {
            limit: api::PAGE_SIZE,
            offset: 0,
            returned: 0,
        },
    }
}
