// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snapshot query resource: re-fetches on `(effective_q, plan)` tuple change.

use leptos::prelude::*;
use trawl_api::QueryResponse;

use crate::api::{self, ApiError};
use crate::fetch_plan::FetchPlan;
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
    /// How much of the result this response was asked for. Under
    /// `Whole` it is the same value on every page of one effective
    /// query, which is what makes a page turn post nothing.
    pub plan: FetchPlan,
    pub generation: u64,
    pub intent: u64,
}
impl std::ops::Deref for ExecutedResponse {
    type Target = QueryResponse;
    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

/// A request that failed, with the same identity a success carries.
///
/// The query error notice quotes the text a refusal's spans index, which
/// is the effective query this request sent, not whatever the URL or the
/// editor holds by the time the answer lands (ADR-0039). The intent and
/// generation let the page tell this failure from a newer request for
/// the same text: a resubmitted query must not show the old verdict.
#[derive(Clone)]
pub struct ExecutedFailure {
    pub query: ExecutedQuery,
    pub error: ApiError,
    pub generation: u64,
    pub intent: u64,
}

/// A failure prints as its error, so every reader that only renders the
/// message (the tables' `Loaded`) reads what it read before.
impl std::fmt::Display for ExecutedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

/// Build a Leptos `LocalResource` that runs `api::query(q, plan)` whenever
/// the effective-query or fetch-plan signals change.
///
/// Callers should feed the DSL the snapshot runs, not the user's raw editor
/// buffer: `search_url::mode_query` with `Mode::Snapshot`, which is the base
/// DSL with the filters and the range merged in. The resource doesn't care
/// where the string came from — it just re-fires on value changes.
///
/// The request is the plan, not the page (ADR-0037): an aggregation's
/// plan is `Whole` on every page, so turning a page under one leaves
/// the input tuple unchanged, the intent where it was, and sends no
/// POST. The `inputs` memo below is the only owner of that rule.
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
    plan: Memo<FetchPlan>,
    pending: WriteSignal<bool>,
    generation: RwSignal<u64>,
    base: Memo<String>,
    filters: Memo<Vec<Filter>>,
    range: Memo<RangeSpec>,
) -> (
    LocalResource<Result<ExecutedResponse, ExecutedFailure>>,
    Memo<u64>,
) {
    // Compare the complete request before advancing its intent. Related URL
    // memos can notify through several paths for the same captured values.
    let inputs = Memo::new(move |_| {
        (
            effective_q.get(),
            plan.get(),
            base.get(),
            filters.get(),
            range.get(),
        )
    });
    let intent = Memo::new(move |previous: Option<&u64>| {
        inputs.with(|_| ());
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
        // Reading intent first resolves the complete input tuple. Subscribe
        // only through intent: tracking its inputs too can dirty the resource
        // during that resolution and execute the same request a second time.
        let (q, plan, base, filters, range) = inputs.get_untracked();
        let query = ExecutedQuery {
            effective: q.clone(),
            base,
            filters,
            range,
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
                    plan,
                    generation: request_generation,
                    intent: request_intent,
                });
            }
            let response = api::query(&q, plan).await;
            // A response can finish after route teardown disposed the signal.
            if generation.try_get_untracked() == Some(request_generation)
                && intent.try_get_untracked() == Some(request_intent)
            {
                let _ = pending.try_set(false);
            }
            rearm_if_stale(intent, request_intent, rearm);
            match response {
                Ok(response) => Ok(ExecutedResponse {
                    query,
                    response,
                    plan,
                    generation: request_generation,
                    intent: request_intent,
                }),
                Err(error) => Err(ExecutedFailure {
                    query,
                    error,
                    generation: request_generation,
                    intent: request_intent,
                }),
            }
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
        pagination: trawl_api::PaginationMeta {
            limit: api::PAGE_SIZE,
            offset: 0,
            returned: 0,
            total: 0,
        },
        // A placeholder for "no query yet" — no execution, no notice,
        // and no columns to render as anything.
        degraded_fields: Vec::new(),
        severity_columns: Vec::new(),
    }
}
