// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's one count of requests in progress (ADR-0054).
//!
//! `[server] max_concurrent_requests` is the number of requests in progress
//! on the HTTPS listener, across every route and method. Two fixed
//! allowances sit beside it: the probe allowance of three for health and
//! `/metrics`, and the control allowance of four for query listing and
//! query cancellation. No count borrows from another or lends to one. A
//! request over its count is refused at once with a 503
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
    /// `[server] max_concurrent_requests`: every route in neither table.
    Regular,
    /// The fixed probe allowance of [`PROBE_ALLOWANCE`]: health and
    /// `/metrics`.
    Probe,
    /// The fixed control allowance of [`CONTROL_ALLOWANCE`]: query listing
    /// and cancellation.
    Control,
}

impl Allowance {
    /// Every allowance, for metric registration and tests that enumerate.
    pub const ALL: [Self; 3] = [Self::Regular, Self::Probe, Self::Control];

    /// The fixed literal this allowance is labelled with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Probe => "probe",
            Self::Control => "control",
        }
    }
}

/// Requests in progress reserved for the probe routes, fixed in code
/// (ADR-0054): the liveness probe, the readiness probe and one scrape.
pub const PROBE_ALLOWANCE: usize = 3;

/// Requests in progress reserved for the control routes, fixed in code
/// (ADR-0054): two operators listing or cancelling queries at once do not
/// refuse each other.
pub const CONTROL_ALLOWANCE: usize = 4;

/// The probe routes: trawld's matched route template and the method.
///
/// Membership is read off the route the router matched, never the raw
/// path, the query string or a header, so no spelling of a request can
/// claim an allowance. A route added later is regular unless it is added
/// to this table or to [`CONTROL_ROUTES`].
static PROBE_ROUTES: [(Method, &str); 4] = [
    (Method::GET, "/api/v1/health"),
    (Method::HEAD, "/api/v1/health"),
    (Method::GET, "/metrics"),
    (Method::HEAD, "/metrics"),
];

/// The control routes, matched as [`PROBE_ROUTES`] are.
static CONTROL_ROUTES: [(Method, &str); 3] = [
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
    let member = |table: &[(Method, &str)]| {
        table
            .iter()
            .any(|(method, route)| method == request.method() && *route == matched.as_str())
    };
    if member(&PROBE_ROUTES) {
        Allowance::Probe
    } else if member(&CONTROL_ROUTES) {
        Allowance::Control
    } else {
        Allowance::Regular
    }
}

/// The three counts one listener shares.
///
/// Built once per router by [`RequestLimit::new`], before any layer is
/// added: axum clones a layer once per route and per method, so a count
/// built inside a layer would be one count per route.
pub(crate) struct RequestLimit {
    regular: Arc<Semaphore>,
    probe: Arc<Semaphore>,
    control: Arc<Semaphore>,
}

impl RequestLimit {
    /// A limit of `max_concurrent_requests` regular requests in progress,
    /// [`PROBE_ALLOWANCE`] probe ones and [`CONTROL_ALLOWANCE`] control
    /// ones. Publishes every size, and every in-progress gauge and refusal
    /// counter at zero.
    #[allow(clippy::cast_precision_loss)] // gauge values are f64; sizes stay far below 2^52
    pub(crate) fn new(max_concurrent_requests: usize) -> Arc<Self> {
        for (allowance, size) in [
            (Allowance::Regular, max_concurrent_requests),
            (Allowance::Probe, PROBE_ALLOWANCE),
            (Allowance::Control, CONTROL_ALLOWANCE),
        ] {
            let label = allowance.label();
            metrics::gauge!(REQUEST_ALLOWANCE, "allowance" => label).set(size as f64);
            metrics::gauge!(REQUESTS_IN_PROGRESS, "allowance" => label).increment(0.0);
            metrics::counter!(REQUESTS_REFUSED_TOTAL, "allowance" => label).increment(0);
        }
        Arc::new(Self {
            regular: Arc::new(Semaphore::new(max_concurrent_requests)),
            probe: Arc::new(Semaphore::new(PROBE_ALLOWANCE)),
            control: Arc::new(Semaphore::new(CONTROL_ALLOWANCE)),
        })
    }

    /// Take one request in progress from `allowance`, or `None` when it has
    /// none free. One atomic try-acquire on that allowance's own count:
    /// never a wait, never a check followed by a separate take, and never
    /// a second count to fall back on.
    fn try_count(&self, allowance: Allowance) -> Option<RequestInProgress> {
        let semaphore = match allowance {
            Allowance::Regular => &self.regular,
            Allowance::Probe => &self.probe,
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
pub(crate) fn current() -> Option<RequestInProgress> {
    CURRENT.try_with(Clone::clone).ok()
}

/// A test seam that holds body work after it has taken the request's
/// count, so a test can watch the count outlive the request.
///
/// One [`Holds`](body_work::Holds) lives on each `IngestState`, never in a
/// process global, and none of this is built into a release binary.
#[cfg(any(test, feature = "test-support"))]
pub mod body_work {
    use std::collections::HashMap;
    use std::sync::{Arc, Weak, mpsc};

    use super::{Counted, RequestInProgress};

    /// The blocking work that keeps a request's count (ADR-0054).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum BodyWork {
        /// Ingest decompress and parse.
        IngestParse,
        /// Ingest WAL write and finalize.
        IngestWrite,
        /// The ingest preview's report.
        Preview,
    }

    /// What held work reports when it is entered: a weak handle on the
    /// count it keeps, which never keeps the count itself.
    #[derive(Debug)]
    pub struct HoldProbe(Weak<Counted>);

    impl HoldProbe {
        /// How many owners the request's count has: the request while its
        /// future runs, the handler's own handle, and each closure that
        /// keeps a clone. 1 once only the held work keeps it; 0 once the
        /// count is back. Work that had no count reports 0 throughout.
        #[must_use]
        pub fn holders(&self) -> usize {
            self.0.strong_count()
        }
    }

    type Hold = (mpsc::Sender<HoldProbe>, mpsc::Receiver<()>);

    /// The next hold for each kind of body work.
    #[derive(Debug, Default)]
    pub struct Holds(parking_lot::Mutex<HashMap<BodyWork, Hold>>);

    impl Holds {
        /// Hold the next `work` once it has taken its count. It sends a
        /// [`HoldProbe`] on the returned receiver, then waits until the
        /// returned sender sends or is dropped.
        pub fn hold_next(&self, work: BodyWork) -> (mpsc::Receiver<HoldProbe>, mpsc::Sender<()>) {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            self.0.lock().insert(work, (entered_tx, release_rx));
            (entered_rx, release_tx)
        }

        /// The pending hold for `work`, if a test set one, aimed at
        /// `count`. Taken on the request's task, waited on inside the
        /// closure that keeps `count`.
        #[expect(
            clippy::used_underscore_binding,
            reason = "the probe watches the handle `RequestInProgress` holds for its Drop"
        )]
        pub(crate) fn take(
            &self,
            work: BodyWork,
            count: Option<&RequestInProgress>,
        ) -> Option<Pause> {
            let (entered, release) = self.0.lock().remove(&work)?;
            let probe =
                HoldProbe(count.map_or_else(Weak::new, |count| Arc::downgrade(&count._counted)));
            Some(Pause {
                entered,
                release,
                probe,
            })
        }
    }

    /// One taken hold.
    #[derive(Debug)]
    pub(crate) struct Pause {
        entered: mpsc::Sender<HoldProbe>,
        release: mpsc::Receiver<()>,
        probe: HoldProbe,
    }

    impl Pause {
        /// Report entry and block this thread until the test releases it.
        pub(crate) fn wait(self) {
            let _ = self.entered.send(self.probe);
            let _ = self.release.recv();
        }
    }
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

    /// Each table, row by row, and what falls outside both. The expected
    /// rows are written out here, not read from the tables, so moving a
    /// row between tables fails this test.
    #[test]
    fn allowance_membership_is_the_matched_route_and_method() {
        for (method, route, allowance) in [
            (Method::GET, "/api/v1/health", Allowance::Probe),
            (Method::HEAD, "/api/v1/health", Allowance::Probe),
            (Method::GET, "/metrics", Allowance::Probe),
            (Method::HEAD, "/metrics", Allowance::Probe),
            (Method::GET, "/api/v1/queries", Allowance::Control),
            (Method::HEAD, "/api/v1/queries", Allowance::Control),
            (Method::DELETE, "/api/v1/queries/{id}", Allowance::Control),
        ] {
            assert_eq!(
                allowance_of(&request(method.clone(), Some(route))),
                allowance,
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
