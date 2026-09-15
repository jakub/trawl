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
    pub page: usize,
    pub generation: u64,
    pub intent: u64,
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
/// The returned intent revision advances independently of the serialized
/// fetcher, including while an older request is still in flight.
pub fn rows_resource(
    effective_q: Memo<String>,
    page: Memo<usize>,
    pending: WriteSignal<bool>,
    generation: RwSignal<u64>,
    base: Memo<String>,
    filters: Memo<Vec<Filter>>,
    range: Memo<RangeSpec>,
) -> (LocalResource<Result<ExecutedResponse, ApiError>>, Memo<u64>) {
    let intent = Memo::new(move |previous: Option<&u64>| {
        effective_q.with(|_| ());
        page.with(|_| ());
        base.with(|_| ());
        filters.with(|_| ());
        range.with(|_| ());
        previous.map_or(0, |revision| revision.wrapping_add(1))
    });
    // LocalResource awaits one request before processing its next dependency
    // notification. Observe intermediate intents now, so A -> empty -> A
    // cannot accept the first A or suppress the newly requested execution.
    Effect::new(move |_| {
        intent.get();
    });
    // A subscriber can consume LocalResource's dependency invalidation while
    // its old future is waiting. A stale completion must rearm the fetcher
    // itself so the latest intent still gets its serialized execution.
    let rearm = RwSignal::new(0_u64);
    let rows = LocalResource::new(move || {
        rearm.track();
        let request_intent = intent.get();
        let q = effective_q.get();
        let p = page.get();
        let query = ExecutedQuery {
            effective: q.clone(),
            base: base.get(),
            filters: filters.get(),
            range: range.get(),
        };
        // Capture ownership before polling the future, including synthetic
        // empty requests. Intent ownership rejects results that the serialized
        // resource can publish before it starts the latest queued request.
        let request_generation = generation.get_untracked().wrapping_add(1);
        generation.set(request_generation);
        pending.set(!q.trim().is_empty());
        async move {
            if q.trim().is_empty() {
                rearm_if_stale(intent, request_intent, rearm);
                return Ok(ExecutedResponse {
                    query,
                    response: empty_response(),
                    page: p,
                    generation: request_generation,
                    intent: request_intent,
                });
            }
            let response = api::query(&q, p).await;
            // A response can finish after route teardown disposed the signal.
            if generation.try_get_untracked() == Some(request_generation)
                && intent.try_get_untracked() == Some(request_intent)
            {
                let _ = pending.try_set(false);
            }
            rearm_if_stale(intent, request_intent, rearm);
            response.map(|response| ExecutedResponse {
                query,
                response,
                page: p,
                generation: request_generation,
                intent: request_intent,
            })
        }
    });
    (rows, intent)
}

/// Rearm from the completing future, before the resource driver checks its
/// dependencies again. An eager effect alone can have its invalidation
/// consumed while the request waits. Signals may be disposed by route teardown.
fn rearm_if_stale(intent: Memo<u64>, started: u64, rearm: RwSignal<u64>) {
    if intent
        .try_get_untracked()
        .is_some_and(|latest| latest != started)
    {
        let _ = rearm.try_update(|revision| *revision = revision.wrapping_add(1));
    }
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
