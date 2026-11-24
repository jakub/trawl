// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The failure observer: one `http_failure` event for every server 5xx,
//! naming its request, route, stage and cause (ADR-0040).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use super::http::RequestId;
use crate::error::{CauseKind, ServerError};

pub const FAILURE_TARGET: &str = "trawl_server::transport::failure";

pub const UNMETERED_FAILURE_TARGET: &str = "trawl_server::transport::failure::unmetered";

pub const UNMATCHED_ROUTE: &str = "<unmatched>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureStage {
    PreAdmission,
    Admitted,
    HandlerError,
    /// A panic was caught: by the outer panic catcher, or by the pool's own
    /// catch on a blocking worker.
    Panicked,
    Unrecorded,
}

impl FailureStage {
    pub const ALL: [Self; 5] = [
        Self::PreAdmission,
        Self::Admitted,
        Self::HandlerError,
        Self::Panicked,
        Self::Unrecorded,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreAdmission => "pre_admission",
            Self::Admitted => "admitted",
            Self::HandlerError => "handler_error",
            Self::Panicked => "panicked",
            Self::Unrecorded => "unrecorded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Progress {
    PreAdmission = 0,
    Admitted = 1,
    Handler = 2,
}

impl Progress {
    pub const ALL: [Self; 3] = [Self::PreAdmission, Self::Admitted, Self::Handler];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreAdmission => "pre_admission",
            Self::Admitted => "admitted",
            Self::Handler => "handler",
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::PreAdmission,
            1 => Self::Admitted,
            _ => Self::Handler,
        }
    }

    const fn failure_stage(self) -> FailureStage {
        match self {
            Self::PreAdmission => FailureStage::PreAdmission,
            Self::Admitted => FailureStage::Admitted,
            Self::Handler => FailureStage::HandlerError,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Recorded {
    class: &'static str,
    cause: CauseKind,
    at: Progress,
}

#[derive(Debug)]
pub struct RequestRecord {
    progress: AtomicU8,
    key_id: OnceLock<i64>,
    query_id: OnceLock<u64>,
    failure: parking_lot::Mutex<Option<Recorded>>,
    panicked: AtomicBool,
}

impl Default for RequestRecord {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestRecord {
    #[must_use]
    pub fn new() -> Self {
        Self {
            progress: AtomicU8::new(Progress::PreAdmission as u8),
            key_id: OnceLock::new(),
            query_id: OnceLock::new(),
            failure: parking_lot::Mutex::new(None),
            panicked: AtomicBool::new(false),
        }
    }

    fn advance(&self, to: Progress) {
        self.progress.fetch_max(to as u8, Ordering::AcqRel);
    }

    fn progress(&self) -> Progress {
        Progress::from_u8(self.progress.load(Ordering::Acquire))
    }

    #[must_use]
    pub fn metered(&self) -> bool {
        self.key_id.get().is_some()
    }

    #[must_use]
    pub fn stage(&self) -> FailureStage {
        if self.panicked.load(Ordering::Acquire) {
            return FailureStage::Panicked;
        }
        let recorded = *self.failure.lock();
        recorded.map_or(FailureStage::Unrecorded, |recorded| {
            recorded.at.failure_stage()
        })
    }

    fn cause(&self) -> Option<(&'static str, CauseKind)> {
        if self.panicked.load(Ordering::Acquire) {
            return Some(("panic", CauseKind::None));
        }
        let recorded = *self.failure.lock();
        recorded.map(|recorded| (recorded.class, recorded.cause))
    }

    fn record_failure(&self, class: &'static str, cause: CauseKind) {
        let at = self.progress();
        *self.failure.lock() = Some(Recorded { class, cause, at });
    }
}

tokio::task_local! {
    static RECORD: Arc<RequestRecord>;
}

fn with_record(f: impl FnOnce(&RequestRecord)) {
    let _ = RECORD.try_with(|record| f(record));
}

/// Called for every verified key the limiter lets through, a key whose
/// limit is disabled (`rpm` 0) included: the operator chose to lift that
/// bound, and the request still passed the limiter.
pub(crate) fn record_metered(key_id: i64) {
    with_record(|record| {
        let _ = record.key_id.set(key_id);
        record.advance(Progress::Admitted);
    });
}

pub(crate) fn record_query_id(query_id: u64) {
    with_record(|record| {
        let _ = record.query_id.set(query_id);
    });
}

pub(crate) fn record_error(err: &ServerError) {
    with_record(|record| {
        if matches!(err, ServerError::Panicked(_)) {
            record.panicked.store(true, Ordering::Release);
        }
        record.record_failure(err.error_class(), err.cause_kind());
    });
}

/// Record a failure whose producer holds no [`ServerError`]: a response
/// another crate built, rewritten at trawl's boundary.
pub(crate) fn record_class(class: &'static str, cause: CauseKind) {
    with_record(|record| record.record_failure(class, cause));
}

pub(crate) fn record_panic() {
    with_record(|record| record.panicked.store(true, Ordering::Release));
}

/// Mounted with `route_layer` innermost on every router, so it runs after
/// authentication and rate limiting and directly before the handler's
/// extractors. A failure recorded from here on is the handler's.
pub async fn mark_handler(request: Request, next: Next) -> Response {
    with_record(|record| record.advance(Progress::Handler));
    next.run(request).await
}

/// The `method` a failure event carries: a standard method's name, or
/// `OTHER`. An extension method is the caller's text.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::CONNECT => "CONNECT",
        Method::PATCH => "PATCH",
        Method::TRACE => "TRACE",
        _ => "OTHER",
    }
}

struct Failure {
    request_id: String,
    method: &'static str,
    route: String,
    status: u16,
    latency_ms: u64,
    stage: &'static str,
    reached: Option<&'static str>,
    error_class: &'static str,
    cause_kind: &'static str,
    query_id: Option<u64>,
    key_id: Option<i64>,
    peer_addr: Option<SocketAddr>,
}

/// Mounted between `request_id_middleware` and the `TraceLayer`, so the
/// request id already exists and every other layer, the panic catcher,
/// authentication, the rate limiter and the handlers, runs inside the
/// record's scope.
pub async fn failure_observer(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let record = Arc::new(RequestRecord::new());
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map_or_else(|| "unknown".to_owned(), |id| id.0.clone());
    let route = request.extensions().get::<MatchedPath>().map_or_else(
        || UNMATCHED_ROUTE.to_owned(),
        |path| path.as_str().to_owned(),
    );
    let method = method_label(request.method());
    let peer_addr = request.extensions().get::<SocketAddr>().copied();

    let response = RECORD.scope(Arc::clone(&record), next.run(request)).await;

    let status = response.status();
    if status.is_server_error() {
        let stage = record.stage();
        let (error_class, cause_kind) = record
            .cause()
            .map_or(("unknown", "none"), |(class, cause)| {
                (class, cause.as_str())
            });
        emit(&Failure {
            request_id,
            method,
            route,
            status: status.as_u16(),
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            stage: stage.as_str(),
            reached: (stage == FailureStage::Unrecorded).then(|| record.progress().as_str()),
            error_class,
            cause_kind,
            query_id: record.query_id.get().copied(),
            key_id: record.key_id.get().copied(),
            peer_addr: (!record.metered()).then_some(peer_addr).flatten(),
        });
    }
    response
}

const fn is_pressure(status: u16) -> bool {
    status == StatusCode::SERVICE_UNAVAILABLE.as_u16()
        || status == StatusCode::GATEWAY_TIMEOUT.as_u16()
}

/// `parent: None` keeps every field explicit, so the event inherits no
/// span's path or user agent.
fn emit(f: &Failure) {
    match (f.key_id, is_pressure(f.status)) {
        (Some(key_id), true) => tracing::warn!(
            target: FAILURE_TARGET,
            parent: None,
            event_type = "http_failure",
            request_id = f.request_id.as_str(),
            method = f.method,
            route = f.route.as_str(),
            status = f.status,
            latency_ms = f.latency_ms,
            stage = f.stage,
            reached = f.reached,
            error_class = f.error_class,
            cause_kind = f.cause_kind,
            query_id = f.query_id,
            key_id,
            "request failed"
        ),
        (Some(key_id), false) => tracing::error!(
            target: FAILURE_TARGET,
            parent: None,
            event_type = "http_failure",
            request_id = f.request_id.as_str(),
            method = f.method,
            route = f.route.as_str(),
            status = f.status,
            latency_ms = f.latency_ms,
            stage = f.stage,
            reached = f.reached,
            error_class = f.error_class,
            cause_kind = f.cause_kind,
            query_id = f.query_id,
            key_id,
            "request failed"
        ),
        (None, true) => tracing::warn!(
            target: UNMETERED_FAILURE_TARGET,
            parent: None,
            event_type = "http_failure",
            request_id = f.request_id.as_str(),
            method = f.method,
            route = f.route.as_str(),
            status = f.status,
            latency_ms = f.latency_ms,
            stage = f.stage,
            reached = f.reached,
            error_class = f.error_class,
            cause_kind = f.cause_kind,
            query_id = f.query_id,
            peer_addr = f.peer_addr.map(tracing::field::display),
            "request failed"
        ),
        (None, false) => tracing::error!(
            target: UNMETERED_FAILURE_TARGET,
            parent: None,
            event_type = "http_failure",
            request_id = f.request_id.as_str(),
            method = f.method,
            route = f.route.as_str(),
            status = f.status,
            latency_ms = f.latency_ms,
            stage = f.stage,
            reached = f.reached,
            error_class = f.error_class,
            cause_kind = f.cause_kind,
            query_id = f.query_id,
            peer_addr = f.peer_addr.map(tracing::field::display),
            "request failed"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_are_a_closed_snake_case_set() {
        let literals: Vec<_> = FailureStage::ALL.iter().map(|s| s.as_str()).collect();
        let unique: std::collections::HashSet<_> = literals.iter().collect();
        assert_eq!(unique.len(), literals.len(), "duplicate stage literal");
        for literal in literals {
            assert!(
                literal.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{literal} is not snake_case"
            );
        }
    }

    #[test]
    fn progress_marks_are_a_closed_snake_case_set() {
        let literals: Vec<_> = Progress::ALL.iter().map(|p| p.as_str()).collect();
        let unique: std::collections::HashSet<_> = literals.iter().collect();
        assert_eq!(unique.len(), literals.len(), "duplicate progress literal");
        for literal in literals {
            assert!(
                literal.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{literal} is not snake_case"
            );
        }
        for progress in Progress::ALL {
            assert_eq!(Progress::from_u8(progress as u8), progress);
        }
    }

    #[test]
    fn a_record_nobody_wrote_is_unrecorded() {
        let record = RequestRecord::new();
        assert_eq!(record.stage(), FailureStage::Unrecorded);
        assert!(record.cause().is_none());
        assert!(!record.metered());
    }

    #[test]
    fn a_failure_is_staged_where_it_was_recorded() {
        let record = RequestRecord::new();
        record.record_failure("service_unavailable", CauseKind::Unknown);
        assert_eq!(record.stage(), FailureStage::PreAdmission);

        let _ = record.key_id.set(7);
        record.advance(Progress::Admitted);
        record.record_failure("internal", CauseKind::Unknown);
        assert_eq!(record.stage(), FailureStage::Admitted);

        record.advance(Progress::Handler);
        record.record_failure("database", CauseKind::DuckdbFailure);
        assert_eq!(record.stage(), FailureStage::HandlerError);
        assert_eq!(record.cause(), Some(("database", CauseKind::DuckdbFailure)));
    }

    #[test]
    fn progress_never_moves_backwards() {
        let record = RequestRecord::new();
        record.advance(Progress::Handler);
        record.advance(Progress::Admitted);
        assert_eq!(record.progress(), Progress::Handler);
    }

    #[test]
    fn a_panic_outranks_every_recorded_failure() {
        let record = RequestRecord::new();
        record.advance(Progress::Handler);
        record.record_failure("database", CauseKind::DuckdbFailure);
        record.panicked.store(true, Ordering::Release);
        assert_eq!(record.stage(), FailureStage::Panicked);
        assert_eq!(record.cause(), Some(("panic", CauseKind::None)));
    }

    #[test]
    fn extension_methods_are_other() {
        assert_eq!(method_label(&Method::POST), "POST");
        let custom = Method::from_bytes(b"ZZSENTINEL").unwrap();
        assert_eq!(method_label(&custom), "OTHER");
    }

    #[test]
    fn only_503_and_504_are_pressure() {
        assert!(is_pressure(503));
        assert!(is_pressure(504));
        assert!(!is_pressure(500));
        assert!(!is_pressure(502));
    }

    #[tokio::test]
    async fn producers_outside_a_request_are_no_ops() {
        record_metered(1);
        record_query_id(2);
        record_panic();
        record_class("internal", CauseKind::Unknown);
        record_error(&ServerError::Internal("x".into()));
    }
}
