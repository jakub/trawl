// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's one count of requests in progress (ADR-0054).

use std::sync::Arc;

use axum::extract::{MatchedPath, Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;

use crate::deadline::Deadline;
use crate::error::ServerError;
use crate::metrics::{REQUEST_ALLOWANCE, REQUESTS_IN_PROGRESS, REQUESTS_REFUSED_TOTAL};

/// Also the `allowance` label on the three request-count metrics, so the
/// set is closed and carries nothing from the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Allowance {
    Regular,
    Probe,
    Control,
}

impl Allowance {
    pub const ALL: [Self; 3] = [Self::Regular, Self::Probe, Self::Control];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Probe => "probe",
            Self::Control => "control",
        }
    }
}

pub const PROBE_ALLOWANCE: usize = 3;

pub const CONTROL_ALLOWANCE: usize = 4;

static PROBE_ROUTES: [(Method, &str); 4] = [
    (Method::GET, "/api/v1/health"),
    (Method::HEAD, "/api/v1/health"),
    (Method::GET, "/metrics"),
    (Method::HEAD, "/metrics"),
];

static CONTROL_ROUTES: [(Method, &str); 3] = [
    (Method::GET, "/api/v1/queries"),
    (Method::HEAD, "/api/v1/queries"),
    (Method::DELETE, "/api/v1/queries/{id}"),
];

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

/// Built once per router by [`RequestLimit::new`], before any layer is
/// added: axum clones a layer once per route and per method, so a count
/// built inside a layer would be one count per route.
pub(crate) struct RequestLimit {
    regular: Arc<Semaphore>,
    probe: Arc<Semaphore>,
    control: Arc<Semaphore>,
}

impl RequestLimit {
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

#[derive(Clone)]
pub(crate) struct RequestInProgress {
    _counted: Arc<Counted>,
}

tokio::task_local! {
    static CURRENT: RequestInProgress;
}

pub(crate) fn current() -> Option<RequestInProgress> {
    CURRENT.try_with(Clone::clone).ok()
}

/// A request's count lent to a task that can outlive the request, back no
/// later than a deadline (ADR-0059).
pub(crate) struct Lease {
    count: Arc<Mutex<Option<RequestInProgress>>>,
    expiry: Option<AbortHandle>,
}

impl Lease {
    pub(crate) fn until(count: Option<RequestInProgress>, deadline: Deadline) -> Self {
        let Some(count) = count else {
            return Self {
                count: Arc::default(),
                expiry: None,
            };
        };
        let count = Arc::new(Mutex::new(Some(count)));
        let expiry = tokio::spawn({
            let count = Arc::clone(&count);
            async move {
                tokio::time::sleep_until(deadline.instant()).await;
                let taken = count.lock().take();
                drop(taken);
            }
        })
        .abort_handle();
        Self {
            count,
            expiry: Some(expiry),
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(expiry) = &self.expiry {
            expiry.abort();
        }
        let taken = self.count.lock().take();
        drop(taken);
    }
}

/// A test seam that holds body work after it has taken the request's
/// count, so a test can watch the count outlive the request.
#[cfg(any(test, feature = "test-support"))]
pub mod body_work {
    use std::collections::HashMap;
    use std::sync::{Arc, Weak, mpsc};

    use super::{Counted, RequestInProgress};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum BodyWork {
        /// Ingest decompress and parse.
        IngestParse,
        /// Ingest WAL write and finalize.
        IngestWrite,
        /// The ingest preview's report.
        Preview,
    }

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

    #[derive(Debug)]
    pub(crate) struct Pause {
        entered: mpsc::Sender<HoldProbe>,
        release: mpsc::Receiver<()>,
        probe: HoldProbe,
    }

    impl Pause {
        pub(crate) fn wait(self) {
            let _ = self.entered.send(self.probe);
            let _ = self.release.recv();
        }
    }
}

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

    /// The expected rows are written out here, not read from the tables, so
    /// moving a row between tables fails this test.
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

    #[tokio::test(start_paused = true)]
    async fn a_lease_dropped_before_its_deadline_gives_the_count_back_once() {
        let limit = RequestLimit::new(1);
        let deadline = Deadline::after(std::time::Duration::from_secs(5));
        let lease = Lease::until(limit.try_count(Allowance::Regular), deadline);
        assert!(limit.try_count(Allowance::Regular).is_none());
        drop(lease);
        let next = limit
            .try_count(Allowance::Regular)
            .expect("the lease gave it back");
        tokio::time::sleep_until(deadline.instant() + std::time::Duration::from_secs(1)).await;
        assert!(
            limit.try_count(Allowance::Regular).is_none(),
            "the deadline took back a count the lease no longer held"
        );
        drop(next);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lease_gives_the_count_back_at_its_deadline_whatever_its_holder_awaits() {
        let limit = RequestLimit::new(1);
        let deadline = Deadline::after(std::time::Duration::from_secs(5));
        let lease = Lease::until(limit.try_count(Allowance::Regular), deadline);
        tokio::time::sleep_until(deadline.instant() - std::time::Duration::from_millis(1)).await;
        assert!(limit.try_count(Allowance::Regular).is_none(), "back early");
        tokio::time::sleep_until(deadline.instant() + std::time::Duration::from_millis(1)).await;
        let next = limit
            .try_count(Allowance::Regular)
            .expect("the deadline took the count back");
        drop(lease);
        assert!(
            limit.try_count(Allowance::Regular).is_none(),
            "the lease's drop gave back a count it no longer held"
        );
        drop(next);
    }

    #[test]
    fn a_lease_of_no_count_lends_nothing() {
        let deadline = Deadline::after(std::time::Duration::from_secs(5));
        drop(Lease::until(None, deadline));
    }

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
