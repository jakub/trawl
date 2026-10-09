// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's one count of requests in progress (ADR-0054).
//!
//! `[server] max_concurrent_requests` is the number of requests in progress
//! on the HTTPS listener, across every route and method. A further four are
//! the control allowance: health, `/metrics`, query listing and query
//! cancellation, which neither use the regular count nor lend to it. A
//! request over either count is refused at once with a 503
//! `request_limit_reached`.
//!
//! [`count_request`] takes the count with a try-acquire and never waits.
//! The request keeps it while its future runs: until the response head is
//! produced, the future is dropped, or a panic is caught inside it. The
//! response body is not counted. Blocking work that holds the request's
//! body takes a clone of the count from [`current`] and keeps the count
//! until that work ends.

use std::sync::Arc;

use axum::extract::{MatchedPath, Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::ServerError;
use crate::metrics::{REQUEST_ALLOWANCE, REQUESTS_IN_PROGRESS, REQUESTS_REFUSED_TOTAL};

/// Which count a request in progress is charged to.
///
/// Also the `allowance` label on the three request-count metrics, so the
/// set is closed and carries nothing from the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Allowance {
    /// `[server] max_concurrent_requests`: every route not in the control
    /// table.
    Regular,
    /// The fixed control allowance of [`CONTROL_ALLOWANCE`].
    Control,
}

impl Allowance {
    /// Both allowances, for metric registration and tests that enumerate.
    pub const ALL: [Self; 2] = [Self::Regular, Self::Control];

    /// The fixed literal this allowance is labelled with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Control => "control",
        }
    }
}

/// Requests in progress reserved for the control routes, fixed in code
/// (ADR-0054): liveness and readiness probes, one scrape and one operator
/// request.
pub const CONTROL_ALLOWANCE: usize = 4;

/// The control routes: trawld's matched route template and the method.
///
/// Membership is read off the route the router matched, never the raw
/// path, the query string or a header, so no spelling of a request can
/// claim the allowance. A route added later is regular unless it is added
/// here.
static CONTROL_ROUTES: [(Method, &str); 7] = [
    (Method::GET, "/api/v1/health"),
    (Method::HEAD, "/api/v1/health"),
    (Method::GET, "/metrics"),
    (Method::HEAD, "/metrics"),
    (Method::GET, "/api/v1/queries"),
    (Method::HEAD, "/api/v1/queries"),
    (Method::DELETE, "/api/v1/queries/{id}"),
];

/// The allowance a request is charged to. A request that matched no route
/// carries no [`MatchedPath`] and is regular.
fn allowance_of(request: &Request) -> Allowance {
    let Some(matched) = request.extensions().get::<MatchedPath>() else {
        return Allowance::Regular;
    };
    let control = CONTROL_ROUTES
        .iter()
        .any(|(method, route)| method == request.method() && *route == matched.as_str());
    if control {
        Allowance::Control
    } else {
        Allowance::Regular
    }
}

/// The two counts one listener shares.
///
/// Built once per router by [`RequestLimit::new`], before any layer is
/// added: axum clones a layer once per route and per method, so a count
/// built inside a layer would be one count per route.
pub(crate) struct RequestLimit {
    regular: Arc<Semaphore>,
    control: Arc<Semaphore>,
}

impl RequestLimit {
    /// A limit of `max_concurrent_requests` regular requests in progress
    /// and [`CONTROL_ALLOWANCE`] control ones. Publishes both sizes and
    /// both refusal counters at zero.
    #[allow(clippy::cast_precision_loss)] // gauge values are f64; sizes stay far below 2^52
    pub(crate) fn new(max_concurrent_requests: usize) -> Arc<Self> {
        for (allowance, size) in [
            (Allowance::Regular, max_concurrent_requests),
            (Allowance::Control, CONTROL_ALLOWANCE),
        ] {
            let label = allowance.label();
            metrics::gauge!(REQUEST_ALLOWANCE, "allowance" => label).set(size as f64);
            metrics::gauge!(REQUESTS_IN_PROGRESS, "allowance" => label).increment(0.0);
            metrics::counter!(REQUESTS_REFUSED_TOTAL, "allowance" => label).increment(0);
        }
        Arc::new(Self {
            regular: Arc::new(Semaphore::new(max_concurrent_requests)),
            control: Arc::new(Semaphore::new(CONTROL_ALLOWANCE)),
        })
    }

    /// Take one request in progress from `allowance`, or `None` when it has
    /// none free. One atomic try-acquire: never a wait, and never a check
    /// followed by a separate take.
    fn try_count(&self, allowance: Allowance) -> Option<RequestInProgress> {
        let semaphore = match allowance {
            Allowance::Regular => &self.regular,
            Allowance::Control => &self.control,
        };
        let permit = Arc::clone(semaphore).try_acquire_owned().ok()?;
        Some(RequestInProgress {
            _counted: Arc::new(Counted::new(permit, allowance)),
        })
    }
}

/// One request in progress: its share of a count and its gauge.
///
/// The gauge goes up when this is built and down when it drops, so it
/// follows ownership and never an await point. The permit goes back when
/// the last owner drops.
pub(crate) struct Counted {
    allowance: Allowance,
    _permit: OwnedSemaphorePermit,
}

impl Counted {
    fn new(permit: OwnedSemaphorePermit, allowance: Allowance) -> Self {
        metrics::gauge!(REQUESTS_IN_PROGRESS, "allowance" => allowance.label()).increment(1.0);
        Self {
            allowance,
            _permit: permit,
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        metrics::gauge!(REQUESTS_IN_PROGRESS, "allowance" => self.allowance.label()).decrement(1.0);
    }
}

/// An owned handle on the current request's count.
///
/// The request holds one for as long as its future runs. Blocking work
/// that holds the request's body moves a clone into its closure, so the
/// count outlives a client that hangs up until that work ends (ADR-0054).
#[derive(Clone)]
pub(crate) struct RequestInProgress {
    /// Held for its `Drop`: the last owner gives the count back.
    _counted: Arc<Counted>,
}

tokio::task_local! {
    /// The count of the request this task is serving, when
    /// [`count_request`] admitted it.
    static CURRENT: RequestInProgress;
}

/// The current request's count, or `None` outside a request the count
/// admitted: a background task, a unit test, a spawned task.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "ingest and preview body work takes the count from here"
    )
)]
pub(crate) fn current() -> Option<RequestInProgress> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Axum middleware: count the request in progress, or refuse it at once.
///
/// A refusal is [`ServerError::RequestLimitReached`]: no inner layer runs,
/// the handler is never entered and the body is never polled. trawld does
/// not drain it. An admitted request runs inside [`CURRENT`]'s scope, so
/// the count ends with its future: when the response head is produced,
/// when the future is dropped, or when the panic catcher inside this layer
/// answers a panic.
pub(crate) async fn count_request(
    State(limit): State<Arc<RequestLimit>>,
    request: Request,
    next: Next,
) -> Response {
    let allowance = allowance_of(&request);
    let Some(in_progress) = limit.try_count(allowance) else {
        metrics::counter!(REQUESTS_REFUSED_TOTAL, "allowance" => allowance.label()).increment(1);
        return ServerError::RequestLimitReached(allowance).into_response();
    };
    CURRENT.scope(in_progress, next.run(request)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: Method, matched: Option<&str>) -> Request {
        let mut request = Request::builder()
            .method(method)
            .uri("/whatever")
            .body(axum::body::Body::empty())
            .unwrap();
        if let Some(matched) = matched {
            // `MatchedPath` has no public constructor; route a request to
            // read the one axum builds.
            let path = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(matched_path(matched));
            request.extensions_mut().insert(path);
        }
        request
    }

    async fn matched_path(template: &str) -> MatchedPath {
        use tower::ServiceExt as _;
        let (tx, rx) = std::sync::mpsc::channel();
        let concrete = template.replace("{id}", "7");
        let app = axum::Router::new().route(
            template,
            axum::routing::any(move |path: MatchedPath| async move {
                tx.send(path).unwrap();
            }),
        );
        app.oneshot(
            Request::builder()
                .uri(concrete)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        rx.recv().unwrap()
    }

    /// The table, row by row, and what falls outside it.
    #[test]
    fn control_membership_is_the_matched_route_and_method() {
        for (method, route) in &CONTROL_ROUTES {
            assert_eq!(
                allowance_of(&request(method.clone(), Some(route))),
                Allowance::Control,
                "{method} {route}"
            );
        }
        for (method, matched) in [
            (Method::POST, Some("/api/v1/health")),
            (Method::DELETE, Some("/api/v1/queries")),
            (Method::GET, Some("/api/v1/queries/{id}")),
            (Method::GET, Some("/api/v1/query")),
            (Method::GET, None),
        ] {
            assert_eq!(
                allowance_of(&request(method.clone(), matched)),
                Allowance::Regular,
                "{method} {matched:?}"
            );
        }
    }

    /// The count and the gauge follow ownership: a clone keeps the count
    /// after the request's own handle drops, and the last drop frees it.
    #[test]
    fn a_clone_keeps_the_count_until_the_last_owner_drops() {
        let limit = RequestLimit::new(1);
        let first = limit.try_count(Allowance::Regular).expect("one is free");
        assert!(limit.try_count(Allowance::Regular).is_none());
        let clone = first.clone();
        drop(first);
        assert!(limit.try_count(Allowance::Regular).is_none());
        drop(clone);
        assert!(limit.try_count(Allowance::Regular).is_some());
    }

    /// `current` answers inside the scope and nowhere else.
    #[tokio::test]
    async fn current_is_the_scoped_request() {
        assert!(current().is_none());
        let limit = RequestLimit::new(1);
        let in_progress = limit.try_count(Allowance::Regular).unwrap();
        CURRENT
            .scope(in_progress, async {
                assert!(current().is_some(), "inside the scope");
            })
            .await;
        assert!(current().is_none());
        assert!(limit.try_count(Allowance::Regular).is_some());
    }
}
