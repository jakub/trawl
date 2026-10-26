// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP handler for the `POST /api/v1/ingest` endpoint.

use std::borrow::Cow;
use std::future::{self, Future};
use std::io::Read as _;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{FromRequestParts, State};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use fleet_auth::VerifiedKey;
use indexmap::IndexMap;

use crate::error::ServerError;
use crate::hot_buffer::{Charge, Refusal, Reservation};
use crate::ingest::body::{EventOutcome, EventReport, HttpRequestContext, visit_events};
use crate::ingest::envelope::{EnvelopeContext, RejectReason, RepairClass, RepairCode};
use crate::ingest::pipeline::{BatchKey, PipelineWriter, ServiceBatch};
use crate::ingest::producer::ProducerKind;
use crate::policy::{Permission, TrawlAuthz as _};
use crate::state::AppState;
#[cfg(any(test, feature = "test-support"))]
use crate::transport::request_limit::body_work::BodyWork;
use crate::transport::request_limit::{self, RequestInProgress};
use trawl_api::{IngestEventError, IngestResponse};

/// Per-reason rejection counts for labeled prometheus metrics.
#[derive(Debug, Default)]
struct RejectCounts(IndexMap<RejectReason, u64>);

impl RejectCounts {
    fn increment(&mut self, reason: RejectReason) {
        self.increment_by(reason, 1);
    }

    fn increment_by(&mut self, reason: RejectReason, n: u64) {
        *self.0.entry(reason).or_default() += n;
    }

    #[cfg(test)]
    fn get(&self, reason: RejectReason) -> u64 {
        self.0.get(&reason).copied().unwrap_or(0)
    }

    #[cfg(test)]
    fn total(&self) -> u64 {
        self.0.values().sum()
    }

    /// Format non-zero counts as a compact summary (e.g. `"invalid_chars:3, missing_service:1"`).
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (reason, count) in &self.0 {
            if *count > 0 {
                parts.push(format!("{reason}:{count}"));
            }
        }
        parts.join(", ")
    }

    /// Emit non-zero counts as labeled prometheus counters.
    fn emit_metrics(&self) {
        for (reason, count) in &self.0 {
            if *count > 0 {
                metrics::counter!(
                    crate::metrics::INGEST_EVENTS_REJECTED_TOTAL,
                    "reason" => reason.as_str()
                )
                .increment(*count);
            }
        }
    }
}

/// How many `(code, service)` pairs a repair log line lists (ADR-0055).
/// Per-code totals are always complete; this only bounds the detail.
const REPAIR_DETAIL_PAIRS: usize = 8;

/// One `(env, service)` group's repair and unmapped-severity tallies, held
/// until that group's WAL write returns (ADR-0055).
#[derive(Debug, Default)]
struct GroupTally {
    /// Per-code repair counts: occurrences of one code on one event.
    repairs: IndexMap<RepairCode, u64>,
    /// Accepted events whose severity source maps to nothing on the `OTel`
    /// ladder (ADR-0013 §2).
    severity_unmapped: u64,
}

/// The repairs and unmapped severities of a request's acknowledged groups,
/// the only source of their metrics and log lines (ADR-0055). A group whose
/// WAL write failed never enters it.
#[derive(Debug, Default)]
struct RepairLedger {
    /// Per-`(code, service)` repair counts, feeding
    /// `trawl_ingest_repairs_total{code,service}`. The same service written
    /// under two envs shares one entry.
    repairs: IndexMap<(RepairCode, String), u64>,
    /// Per-service unmapped-severity counts, feeding
    /// `trawl_severity_unmapped_total{service}`.
    severity_unmapped: IndexMap<String, u64>,
}

/// A bounded per-service detail: the listed pairs and what was left out.
#[derive(Debug, PartialEq, Eq)]
struct RepairDetail {
    /// `service/code:n` for at most [`REPAIR_DETAIL_PAIRS`] pairs, joined
    /// with `", "`. Empty when nothing matched.
    text: String,
    /// Pairs past the bound.
    omitted_pairs: u64,
    /// Occurrences those omitted pairs held.
    omitted_occurrences: u64,
}

impl RepairLedger {
    /// Fold the acknowledged groups' tallies. `wal_paths` is the list of
    /// groups whose write succeeded, the same list publication walks, so
    /// what is counted and what is published cannot diverge.
    fn acknowledged(
        tallies: &IndexMap<BatchKey, GroupTally>,
        wal_paths: &[(BatchKey, PathBuf)],
    ) -> Self {
        let mut ledger = Self::default();
        for (key, _) in wal_paths {
            if let Some(tally) = tallies.get(key) {
                ledger.add(&key.1, tally);
            }
        }
        ledger
    }

    fn add(&mut self, service: &str, tally: &GroupTally) {
        for (code, count) in &tally.repairs {
            *self.repairs.entry((*code, service.to_owned())).or_default() += count;
        }
        if tally.severity_unmapped > 0 {
            *self
                .severity_unmapped
                .entry(service.to_owned())
                .or_default() += tally.severity_unmapped;
        }
    }

    /// The repairs of `class`, or of both classes for `None`, as
    /// `(code, service, count)`.
    fn pairs(&self, class: Option<RepairClass>) -> impl Iterator<Item = (RepairCode, &str, u64)> {
        self.repairs
            .iter()
            .filter(move |((code, _), _)| class.is_none_or(|class| code.class() == class))
            .map(|((code, service), count)| (*code, service.as_str(), *count))
    }

    /// Total repair occurrences of `class`, or of both classes for `None`.
    fn total(&self, class: Option<RepairClass>) -> u64 {
        self.pairs(class).map(|(_, _, count)| count).sum()
    }

    /// Exact non-zero per-code totals as `code:n, code:n`, in
    /// [`RepairCode::ALL`] order, the same shape as [`RejectCounts::summary`].
    fn codes(&self, class: Option<RepairClass>) -> String {
        let mut totals: IndexMap<RepairCode, u64> = IndexMap::new();
        for (code, _, count) in self.pairs(class) {
            *totals.entry(code).or_default() += count;
        }
        RepairCode::ALL
            .iter()
            .filter_map(|code| totals.get(code).map(|count| format!("{code}:{count}")))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// At most [`REPAIR_DETAIL_PAIRS`] pairs, by count, highest first, then
    /// by code, then by service, with exact counts of what was left out.
    fn detail(&self, class: Option<RepairClass>) -> RepairDetail {
        let mut pairs: Vec<_> = self.pairs(class).collect();
        pairs.sort_by(
            |(a_code, a_service, a_count), (b_code, b_service, b_count)| {
                b_count
                    .cmp(a_count)
                    .then_with(|| a_code.as_str().cmp(b_code.as_str()))
                    .then_with(|| a_service.cmp(b_service))
            },
        );
        let tail = pairs.split_off(pairs.len().min(REPAIR_DETAIL_PAIRS));
        RepairDetail {
            text: pairs
                .iter()
                .map(|(code, service, count)| format!("{service}/{code}:{count}"))
                .collect::<Vec<_>>()
                .join(", "),
            omitted_pairs: tail.len() as u64,
            omitted_occurrences: tail.iter().map(|(_, _, count)| count).sum(),
        }
    }

    /// Emit the repair and unmapped-severity counters.
    fn emit_metrics(&self) {
        for (service, count) in &self.severity_unmapped {
            // Same label discipline as the repair counter: `service` is a
            // client-chosen data dimension trawl publishes deliberately, run
            // through the same bounded label helper.
            metrics::counter!(
                crate::metrics::SEVERITY_UNMAPPED_TOTAL,
                "service" => crate::metrics::repair_service_label(service)
            )
            .increment(*count);
        }
        for ((code, service), count) in &self.repairs {
            // SECURITY: `service` is client-supplied and `/metrics` is
            // unauthenticated. It is a data dimension ADR-0009 publishes
            // deliberately (alerting per repair code and sender), unlike the
            // principal-derived labels `handlers.rs` forbids — but the
            // recorder never evicts a series, so the distinct label values are
            // capped and per-key attribution stays in the authenticated log.
            // Only acknowledged groups get here, so a failed write never
            // spends one of those capped label slots.
            metrics::counter!(
                crate::metrics::INGEST_REPAIRS_TOTAL,
                "code" => code.as_str(),
                "service" => crate::metrics::repair_service_label(service)
            )
            .increment(*count);
        }
    }
}

/// Parsed ingest payload, grouped by `(env, service)`.
#[derive(Debug)]
struct ParsedEvents {
    /// Service batches keyed by `(env, service)`, insertion-order preserved.
    batches: IndexMap<BatchKey, ServiceBatch>,
    /// Per-event validation errors accumulated during parsing.
    errors: Vec<IngestEventError>,
    /// Per-reason rejection counts for prometheus labels.
    reject_counts: RejectCounts,
    /// Repair and unmapped-severity tallies of the groups that have any,
    /// keyed like `batches`. Only the groups whose WAL write succeeds are
    /// counted ([`RepairLedger::acknowledged`]).
    tallies: IndexMap<BatchKey, GroupTally>,
}

impl ParsedEvents {
    fn new() -> Self {
        Self {
            batches: IndexMap::new(),
            errors: Vec::new(),
            reject_counts: RejectCounts::default(),
            tallies: IndexMap::new(),
        }
    }

    /// Take one position's outcome from the shared step: batch an accepted
    /// event, record a rejected one. Never stops the visit.
    fn record(&mut self, report: EventReport<'_>) -> ControlFlow<()> {
        match report.outcome {
            EventOutcome::Accepted(canonical) => {
                let severity_unmapped = canonical.severity_unmapped();
                let key = (canonical.env, canonical.service);
                if !canonical.repairs.is_empty() || severity_unmapped {
                    let tally = self.tallies.entry(key.clone()).or_default();
                    for code in &canonical.repairs {
                        *tally.repairs.entry(*code).or_default() += 1;
                    }
                    tally.severity_unmapped += u64::from(severity_unmapped);
                }
                self.batches.entry(key).or_default().push(canonical.obj);
            }
            EventOutcome::Rejected {
                reason, message, ..
            } => {
                self.errors.push(event_error(report.index, reason, message));
                self.reject_counts.increment(reason);
            }
        }
        ControlFlow::Continue(())
    }

    /// The hot-buffer charge of every valid event, across all groups: what
    /// the request reserves as a whole. Saturates on the (unreachable)
    /// overflow, which then refuses as oversized.
    fn charge(&self) -> Charge {
        self.batches
            .values()
            .map(ServiceBatch::charge)
            .fold(Charge::ZERO, |total, group| {
                total.checked_add(group).unwrap_or(Charge {
                    events: usize::MAX,
                    bytes: usize::MAX,
                })
            })
    }
}

/// The caller's key, holding `Permission::Ingest`.
///
/// A parts extractor because axum runs those before the body extractor:
/// a key without the grant is refused on its request head, before trawld
/// collects up to `[ingest] max_body_bytes` it would discard. Extractors
/// run inside the router's layers, so the rate limiter still sees the
/// request first. The refusal is a [`ServerError`] like any the handler
/// returns, so it keeps the envelope, and `failure::mark_handler`, which
/// runs before every extractor, still classifies it as the handler's.
pub struct IngestKey(VerifiedKey);

// `VerifiedKey` has no `Debug`, so neither does the key here.
impl std::fmt::Debug for IngestKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngestKey").finish_non_exhaustive()
    }
}

impl<S: Sync> FromRequestParts<S> for IngestKey {
    type Rejection = ServerError;

    // Not `async fn`: nothing here awaits.
    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        future::ready(ingest_key(parts))
    }
}

impl IngestKey {
    /// Admit `verified` to ingest, or refuse it with a 403.
    pub fn new(verified: VerifiedKey) -> Result<Self, ServerError> {
        if !verified.has_permission(Permission::Ingest) {
            return Err(ServerError::Forbidden("insufficient permissions".into()));
        }
        Ok(Self(verified))
    }
}

fn ingest_key(parts: &Parts) -> Result<IngestKey, ServerError> {
    // The bearer layer inserts the key on every request it lets through.
    let verified = parts
        .extensions
        .get::<VerifiedKey>()
        .cloned()
        .ok_or_else(|| ServerError::Internal("ingest route without a verified key".into()))?;
    IngestKey::new(verified)
}

/// `POST /api/v1/ingest` — accept ndjson events into the WAL.
///
/// Expects `Content-Type: application/x-ndjson` (or `application/json`).
/// Supports `Content-Encoding: gzip` for compressed payloads.
///
/// The request is admitted against hot-buffer capacity as a whole before
/// anything is written (ADR-0043), in this order:
///
/// 1. With no free space at all for HTTP, refuse before decompressing
///    anything: 503 `hot_buffer_full`.
/// 2. Decompress, parse and canonicalize on a blocking thread.
/// 3. Reserve the exact charge of every valid event, synchronously. A
///    request over the per-request ceiling is 413 `ingest_batch_too_large`
///    whatever the buffer holds; one that fits the ceiling but not the free
///    space is 503 `hot_buffer_full` with `Retry-After` set to the
///    compaction interval. A refusal writes nothing and never touches the
///    publication gate, which compaction needs to drain.
/// 4. Take the publication gate's ingest side, then write and publish on a
///    second blocking thread that owns the reservation and the guard.
///
/// If any `(env, service)` group's WAL write fails, the request answers a
/// redacted 500. The groups that did write are still published and
/// counted first: they are durable, and compaction will merge them. A 500
/// is therefore not an atomic rollback. The failed groups' share of the
/// reservation is released.
///
/// Both blocking tasks hold the request's body, so each keeps the
/// request's count until it ends, even after the client hangs up
/// (ADR-0054). The wait for the publication gate between them does not.
pub async fn ingest(
    State(state): State<AppState>,
    IngestKey(verified): IngestKey,
    Extension(peer_addr): Extension<SocketAddr>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<IngestResponse>, ServerError> {
    // The key may ingest (`IngestKey`): only now does the body's read
    // answer, a body past `[ingest] max_body_bytes` as 413.
    let body = crate::shape::shaped("request body", body)?;
    let count = request_limit::current();
    let pipeline = state
        .ingest
        .pipeline
        .as_ref()
        .map(Arc::clone)
        .ok_or_else(|| ServerError::Internal("ingest not enabled".into()))?;

    // Step 1: refuse a full buffer before spending CPU on the body.
    if let Some(buf) = &state.query.hot_buffer {
        buf.ensure_free_space(ProducerKind::Http)
            .map_err(|refusal| refusal_error(&state, &pipeline, refusal, None))?;
    }

    let compressed = is_gzip(&headers);
    let wire_bytes = body.len();

    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let request_span = tracing::Span::current();

    // Step 2: decompress and parse. No guard is held here.
    let parsed = parse_request(
        &state,
        peer_addr,
        compressed,
        body,
        count.clone(),
        dispatch.clone(),
        request_span.clone(),
    )
    .await?;
    let Parsed {
        events: mut parsed,
        body_bytes,
        decompress_ms,
        parse_ms,
    } = parsed;

    // Step 3: admit the whole request, or none of it.
    let requested = parsed.charge();
    let reservation = match pipeline.reserve(ProducerKind::Http, requested) {
        Ok(reservation) => reservation,
        Err(refusal) => {
            record_refused(&state, &mut parsed, refusal);
            return Err(refusal_error(&state, &pipeline, refusal, Some(requested)));
        }
    };

    // Step 4: wait for the gate before dispatching blocking work. Requests
    // waiting behind a compactor must leave blocking threads available to
    // existing readers. Nothing waits for capacity past this point.
    let publication = state.query.hot_buffer.as_ref().map(|buf| buf.publication());
    let ingest_guard = match publication {
        Some(gate) => Some(gate.ingest().await),
        None => None,
    };

    #[cfg(any(test, feature = "test-support"))]
    let hold = state
        .ingest
        .body_work_holds
        .take(BodyWork::IngestWrite, count.as_ref());
    let result = tokio::task::spawn_blocking(move || -> Result<_, ServerError> {
        let _count = count;
        #[cfg(any(test, feature = "test-support"))]
        if let Some(hold) = hold {
            hold.wait();
        }
        // Cancellation cannot split durability from insertion once this
        // task starts. The guard and the reservation stay here until both
        // steps finish.
        let _ingest = ingest_guard;
        let mut reservation = reservation;
        let _dispatch = tracing::dispatcher::set_default(&dispatch);
        let _span = request_span.enter();

        let t2 = std::time::Instant::now();
        let (wal_paths, wal_failed) = write_wal_batches(pipeline.wal_writer(), &mut parsed);
        let wal_ms = t2.elapsed().as_millis();

        // Groups that did reach the WAL are acknowledged on disk, so they
        // are published and counted before any failure answers the request.
        let response = finalize_ingest(
            &state,
            &pipeline,
            &mut parsed,
            &wal_paths,
            &mut reservation,
            &verified,
            wire_bytes,
            compressed,
            body_bytes,
            decompress_ms,
            parse_ms,
            wal_ms,
        );
        // What is left is the failed groups' share: released here.
        drop(reservation);
        if wal_failed {
            // A failed group was never acknowledged, so the request fails
            // as a whole and the sender retries it. The filesystem error
            // stays in the `wal_write_failed` log, never in the response.
            return Err(ServerError::Internal("WAL write failed".into()));
        }
        Ok(response)
    })
    .await
    .map_err(|e| ServerError::from_join("ingest", e))??;

    Ok(Json(result))
}

/// What the parse task hands back: the events and its timings.
#[derive(Debug)]
struct Parsed {
    events: ParsedEvents,
    body_bytes: usize,
    decompress_ms: u128,
    parse_ms: u128,
}

/// Decompress and parse the body on a blocking thread, so CPU-bound gzip
/// and JSON work does not hog tokio workers. The thread keeps `count`, the
/// request's count, for as long as it holds the body.
async fn parse_request(
    state: &AppState,
    peer_addr: SocketAddr,
    compressed: bool,
    body: Bytes,
    count: Option<RequestInProgress>,
    dispatch: tracing::Dispatch,
    span: tracing::Span,
) -> Result<Parsed, ServerError> {
    // One arrival instant and one peer reading for the whole request,
    // captured before moving into the blocking task.
    let request = HttpRequestContext::new(&state.ingest, peer_addr.ip());
    let max_body_bytes = state.ingest.max_body_bytes;
    #[cfg(any(test, feature = "test-support"))]
    let hold = state
        .ingest
        .body_work_holds
        .take(BodyWork::IngestParse, count.as_ref());
    tokio::task::spawn_blocking(move || -> Result<_, ServerError> {
        let _count = count;
        #[cfg(any(test, feature = "test-support"))]
        if let Some(hold) = hold {
            hold.wait();
        }
        let _dispatch = tracing::dispatcher::set_default(&dispatch);
        let _span = span.enter();
        let ctx = request.envelope();
        let t0 = std::time::Instant::now();
        // An uncompressed body is parsed in place: copying it would double
        // the request's footprint at up to `[ingest] max_body_bytes`.
        let raw: Cow<'_, [u8]> = if compressed {
            Cow::Owned(decompress_gzip(&body, max_body_bytes)?)
        } else {
            Cow::Borrowed(&body)
        };
        let decompress_ms = t0.elapsed().as_millis();

        let t1 = std::time::Instant::now();
        let events = parse_events(&raw, &ctx)?;
        Ok(Parsed {
            events,
            body_bytes: raw.len(),
            decompress_ms,
            parse_ms: t1.elapsed().as_millis(),
        })
    })
    .await
    .map_err(|e| ServerError::from_join("ingest", e))?
}

/// The response for a refused request. `requested` is `None` for the early
/// refusal, which runs before the body is parsed.
fn refusal_error(
    state: &AppState,
    pipeline: &PipelineWriter,
    refusal: Refusal,
    requested: Option<Charge>,
) -> ServerError {
    match refusal {
        Refusal::Full => ServerError::HotBufferFull {
            retry_after_secs: state.ingest.retry_after_secs,
        },
        Refusal::Oversized => ServerError::IngestBatchTooLarge {
            requested,
            ceiling: pipeline.ceiling(ProducerKind::Http),
        },
    }
}

/// Account a parsed request the hot buffer refused: its valid events count
/// under the refusal's reject reason, its per-event validation rejects
/// under their own. Nothing was written, so nothing counts as accepted,
/// repaired or severity-unmapped. No log line: the failure observer's one
/// `http_failure` is the record of a 503, and a 413 is the client's.
fn record_refused(state: &AppState, parsed: &mut ParsedEvents, refusal: Refusal) {
    let reason = match refusal {
        Refusal::Full => RejectReason::HotBufferFull,
        Refusal::Oversized => RejectReason::IngestBatchTooLarge,
    };
    let refused = parsed.charge().events as u64;
    parsed.reject_counts.increment_by(reason, refused);
    parsed.reject_counts.emit_metrics();
    state.ingest.total_rejected.fetch_add(
        refused + parsed.errors.len() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Write one WAL file per `(env, service)` group. Every group is attempted,
/// and successful groups are durable. Failed groups are counted as
/// `wal_failure` rejections and removed from `parsed.batches` so they are
/// never published. The returned flag is true when any group failed, which
/// fails the request after the successful groups are finalized.
fn write_wal_batches(
    wal_writer: &crate::ingest::wal::WalWriter,
    parsed: &mut ParsedEvents,
) -> (Vec<(BatchKey, PathBuf)>, bool) {
    let mut wal_paths: Vec<(BatchKey, PathBuf)> = Vec::new();
    let mut wal_failures: Vec<BatchKey> = Vec::new();

    for (key, batch) in &parsed.batches {
        let (env, svc) = key;
        match wal_writer.write(env, svc, &batch.ndjson) {
            Ok(path) => wal_paths.push((key.clone(), path)),
            Err(e) => {
                tracing::warn!(
                    event_type = "wal_write_failed",
                    batch_env = %env,
                    batch_service = %svc,
                    events_lost = batch.maps.len(),
                    left_visible = e.left_visible(),
                    error = %e,
                    "WAL write failed for service group"
                );
                let event_count = batch.maps.len();
                // The filesystem error stays in the log line above: this
                // message is client-facing text and names no path.
                parsed.errors.push(event_error(
                    0,
                    RejectReason::WalFailure,
                    format!("WAL write failed for service '{svc}' ({event_count} events lost)"),
                ));
                parsed
                    .reject_counts
                    .increment_by(RejectReason::WalFailure, event_count as u64);
                wal_failures.push(key.clone());
            }
        }
    }

    for key in &wal_failures {
        parsed.batches.shift_remove(key);
    }

    (wal_paths, !wal_failures.is_empty())
}

/// Finish within the blocking task while its publication read guard is held.
/// Update metrics, publish to the hot buffer and bus, and build the response.
/// Each published group moves its charge out of `reservation`; what is left
/// belongs to groups that were not written.
#[allow(clippy::too_many_arguments)]
fn finalize_ingest(
    state: &AppState,
    pipeline: &PipelineWriter,
    parsed: &mut ParsedEvents,
    wal_paths: &[(BatchKey, PathBuf)],
    reservation: &mut Reservation,
    verified: &VerifiedKey,
    wire_bytes: usize,
    compressed: bool,
    body_bytes: usize,
    decompress_ms: u128,
    parse_ms: u128,
    wal_ms: u128,
) -> IngestResponse {
    let accepted: usize = parsed.batches.values().map(|b| b.maps.len()).sum();
    let rejected = parsed.errors.len();

    metrics::counter!(crate::metrics::INGEST_EVENTS_TOTAL).increment(accepted as u64);
    state
        .ingest
        .total_events
        .fetch_add(accepted as u64, std::sync::atomic::Ordering::Relaxed);
    if rejected > 0 {
        parsed.reject_counts.emit_metrics();
        state
            .ingest
            .total_rejected
            .fetch_add(rejected as u64, std::sync::atomic::Ordering::Relaxed);

        let reasons = parsed.reject_counts.summary();
        // Sample up to 5 error messages so rejection causes are queryable
        // without flooding telemetry with per-event detail. A message may
        // quote a bounded client value (`Rejection::message`), which is why
        // this is the one log line allowed to carry one.
        let samples: Vec<&str> = parsed
            .errors
            .iter()
            .take(5)
            .map(|e| e.message.as_str())
            .collect();
        let sample_text = samples.join("; ");
        tracing::warn!(
            event_type = "ingest_rejections",
            user = %verified.name,
            rejected,
            reasons,
            samples = sample_text,
            "events rejected during ingest"
        );
    }

    // Only the groups whose WAL write succeeded count. Fold them before
    // publication drains the batches.
    let ledger = RepairLedger::acknowledged(&parsed.tallies, wal_paths);
    ledger.emit_metrics();
    let repairs = ledger.total(None);
    let repairs_routine = ledger.total(Some(RepairClass::Routine));
    let repairs_actionable = ledger.total(Some(RepairClass::Actionable));
    if repairs_actionable > 0 {
        let actionable = ledger.detail(Some(RepairClass::Actionable));
        // Not "lost": `time.out_of_range` keeps the value it flags.
        tracing::warn!(
            event_type = "ingest_repairs",
            user = %verified.name,
            key_id = verified.id,
            repairs_actionable,
            repairs_routine,
            actionable_codes = ledger.codes(Some(RepairClass::Actionable)),
            actionable_detail = actionable.text,
            actionable_detail_omitted_pairs = actionable.omitted_pairs,
            actionable_detail_omitted_occurrences = actionable.omitted_occurrences,
            "accepted events have repairs that need inspection (codes in _repairs)"
        );
    }

    // Publish each successfully-written batch to hot buffer + event bus
    // so events are visible to queries and SSE streams immediately. Each
    // takes its own share of the request's reservation.
    for (key, wal_path) in wal_paths {
        if let Some(batch) = parsed.batches.swap_remove(key) {
            let share = reservation.take(batch.charge());
            pipeline.publish(&key.0, &key.1, batch, wal_path, share);
        }
    }

    let duration_ms = decompress_ms + parse_ms + wal_ms;
    let services = wal_paths.len();
    let detail = ledger.detail(None);
    // The per-key record of every repair, routine ones included (ADR-0055).
    tracing::info!(
        event_type = "ingest_complete",
        user = %verified.name,
        key_id = verified.id,
        services,
        accepted,
        rejected,
        body_bytes,
        wire_bytes,
        compressed,
        duration_ms,
        decompress_ms,
        parse_ms,
        wal_ms,
        repairs,
        repairs_routine,
        repairs_actionable,
        repair_codes = ledger.codes(None),
        repair_detail = detail.text,
        repair_detail_omitted_pairs = detail.omitted_pairs,
        repair_detail_omitted_occurrences = detail.omitted_occurrences,
        "ingested events to WAL"
    );

    IngestResponse {
        accepted,
        rejected,
        errors: std::mem::take(&mut parsed.errors),
    }
}

fn is_gzip(headers: &HeaderMap) -> bool {
    headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"))
}

/// Decompress a gzip body, refusing one that decompresses past
/// `max_body_bytes` (`[ingest] max_body_bytes`, the same limit the wire
/// body is held to).
///
/// The bound is on decompressed size, not on the wire-to-decompressed
/// ratio: repetitive log batches routinely expand past 20x, and a ratio cap
/// refused them with a 400 the sender does not retry. Decoding reads at
/// most `max_body_bytes + 1` bytes, so a gzip bomb costs no more memory
/// than the largest body trawld accepts.
fn decompress_gzip(data: &[u8], max_body_bytes: usize) -> Result<Vec<u8>, ServerError> {
    let decoder = flate2::read::GzDecoder::new(data);
    // Read up to limit + 1: if we get more than limit bytes, the payload
    // exceeds the cap and we reject it before allocating further. The
    // buffer grows with what is actually decoded; nothing is sized from
    // the request up front.
    let mut decompressed = Vec::new();
    decoder
        .take(u64::try_from(max_body_bytes.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut decompressed)
        .map_err(|e| ServerError::Ingest(format!("gzip decompression failed: {e}")))?;
    if decompressed.len() > max_body_bytes {
        tracing::warn!(
            event_type = "ingest_body_too_large",
            wire_bytes = data.len(),
            max_body_bytes,
            "refused a gzip ingest body that decompresses past max_body_bytes"
        );
        return Err(ServerError::IngestBodyTooLarge {
            limit: max_body_bytes,
        });
    }
    Ok(decompressed)
}

/// One per-event error, as both ingest routes report it: the position, the
/// reason's wire code and the human-readable message.
fn event_error(index: usize, reason: RejectReason, message: String) -> IngestEventError {
    IngestEventError {
        index,
        reason: reason.as_str().to_owned(),
        message,
    }
}

/// Parse and canonicalize a body through the shared step, grouping the
/// accepted events by `(env, service)` with canonical maps and ndjson
/// bytes. The ndjson bytes are always ndjson regardless of input format,
/// ready for the WAL.
fn parse_events(data: &[u8], ctx: &EnvelopeContext<'_>) -> Result<ParsedEvents, ServerError> {
    let mut events = ParsedEvents::new();
    // `record` never breaks, so the visit always reaches the end.
    let _: ControlFlow<()> = visit_events(data, ctx, |report| events.record(report))?;
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::compaction;
    use crate::ingest::producer::Producer;

    const ARRIVAL: &str = "2026-01-01T00:00:00.000000Z";

    /// Parse with a default single-env ("prod") context, no trusted relay.
    fn parse(data: &[u8]) -> Result<ParsedEvents, ServerError> {
        parse_with(data, &["prod"], false)
    }

    fn parse_with(data: &[u8], envs: &[&str], relay: bool) -> Result<ParsedEvents, ServerError> {
        let envs: Vec<String> = envs.iter().map(|s| (*s).to_string()).collect();
        let derivation = crate::ingest::producer::Derivation::defaults();
        let ctx = EnvelopeContext {
            arrival: ARRIVAL,
            arrival_instant: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            envs: &envs,
            default_env: &envs[0],
            producer: Producer::Http {
                peer_host: "127.0.0.1",
                peer_is_trusted_relay: relay,
            },
            derivation: &derivation,
        };
        parse_events(data, &ctx)
    }

    /// Total accepted event count across all service batches.
    fn total_accepted(parsed: &ParsedEvents) -> usize {
        parsed.batches.values().map(|b| b.maps.len()).sum()
    }

    /// Get the maps for `(env, service)`, panicking if absent.
    fn batch_maps<'a>(
        parsed: &'a ParsedEvents,
        env: &str,
        svc: &str,
    ) -> &'a [serde_json::Map<String, serde_json::Value>] {
        &parsed
            .batches
            .get(&(env.to_string(), svc.to_string()))
            .unwrap_or_else(|| panic!("no batch for ({env}, {svc})"))
            .maps
    }

    fn has_batch(parsed: &ParsedEvents, env: &str, svc: &str) -> bool {
        parsed
            .batches
            .contains_key(&(env.to_string(), svc.to_string()))
    }

    /// The `(env, service)` group's tally, panicking if it has none.
    fn tally<'a>(parsed: &'a ParsedEvents, env: &str, svc: &str) -> &'a GroupTally {
        parsed
            .tallies
            .get(&(env.to_string(), svc.to_string()))
            .unwrap_or_else(|| panic!("no tally for ({env}, {svc})"))
    }

    /// One code's count in the `(env, service)` group's tally.
    fn repairs_of(parsed: &ParsedEvents, env: &str, svc: &str, code: RepairCode) -> u64 {
        parsed
            .tallies
            .get(&(env.to_string(), svc.to_string()))
            .and_then(|tally| tally.repairs.get(&code))
            .copied()
            .unwrap_or(0)
    }

    // --- happy path tests ---

    #[test]
    fn parse_ndjson_format() {
        let data = br#"{"service":"nginx","message":"ok"}
{"service":"nginx","message":"error"}
"#;
        let parsed = parse(data).unwrap();
        assert!(has_batch(&parsed, "prod", "nginx"));
        assert_eq!(total_accepted(&parsed), 2);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parse_json_array_format() {
        let data = br#"[{"service":"nginx","message":"ok"},{"service":"nginx","message":"error"}]"#;
        let parsed = parse(data).unwrap();
        assert!(has_batch(&parsed, "prod", "nginx"));
        assert_eq!(total_accepted(&parsed), 2);
        assert!(parsed.errors.is_empty());
        // WAL output should be ndjson, not a JSON array.
        let ndjson = &parsed.batches[&("prod".to_string(), "nginx".to_string())].ndjson;
        let text = std::str::from_utf8(ndjson).unwrap();
        assert!(!text.starts_with('['));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn parse_ndjson_empty_lines_skipped() {
        let data = br#"
{"service":"test","message":"hello"}

{"service":"test","message":"world"}
"#;
        let parsed = parse(data).unwrap();
        assert!(has_batch(&parsed, "prod", "test"));
        assert_eq!(total_accepted(&parsed), 2);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parse_accepts_valid_service_names() {
        for name in ["nginx", "my-app", "app_v2", "host.name.prod", "A1-B2_c3.d"] {
            let data = format!(r#"{{"service":"{name}","message":"ok"}}"#);
            let parsed = parse(data.as_bytes()).unwrap();
            assert!(has_batch(&parsed, "prod", name));
            assert_eq!(total_accepted(&parsed), 1);
            assert!(parsed.errors.is_empty());
        }
    }

    #[test]
    fn parse_ndjson_retains_maps() {
        let data = br#"{"service":"nginx","message":"hello","status":200}
{"service":"nginx","message":"world","status":404}
"#;
        let parsed = parse(data).unwrap();
        let maps = batch_maps(&parsed, "prod", "nginx");
        assert_eq!(maps.len(), 2);
        assert_eq!(
            maps[0].get("message").and_then(serde_json::Value::as_str),
            Some("hello")
        );
        assert_eq!(
            maps[1].get("status").and_then(serde_json::Value::as_u64),
            Some(404)
        );
    }

    #[test]
    fn parse_json_array_derives_severity() {
        // `level` is observed, never consumed: the source stays as the
        // sender's own column and `_severity` is derived beside it.
        let data = br#"[{"service":"nginx","level":"error"},{"service":"nginx","level":"warn"}]"#;
        let parsed = parse(data).unwrap();
        let maps = batch_maps(&parsed, "prod", "nginx");
        assert_eq!(maps.len(), 2);
        assert_eq!(
            maps[0].get("level").and_then(serde_json::Value::as_str),
            Some("error")
        );
        assert_eq!(
            maps[1].get("level").and_then(serde_json::Value::as_str),
            Some("warn")
        );
        let sev = |m: &serde_json::Map<String, serde_json::Value>| {
            m.get(trawl_core::schema::SEVERITY)
                .and_then(serde_json::Value::as_i64)
        };
        assert_eq!(sev(&maps[0]), Some(17));
        assert_eq!(sev(&maps[1]), Some(13));
    }

    /// A sender whose severity source maps to nothing is counted, never
    /// repaired (ADR-0013 §2) — the counter's input, per service.
    #[test]
    fn unmappable_severity_sources_are_counted_per_service() {
        let data = br#"[{"service":"game","level":"gold"},{"service":"game","level":"silver"},{"service":"nginx","level":"error"}]"#;
        let parsed = parse(data).unwrap();
        assert_eq!(tally(&parsed, "prod", "game").severity_unmapped, 2);
        assert_eq!(tally(&parsed, "prod", "nginx").severity_unmapped, 0);
        // Counted, never repaired: no code on any event says so.
        let codes: Vec<&str> = parsed
            .tallies
            .values()
            .flat_map(|tally| tally.repairs.keys().map(|code| code.as_str()))
            .collect();
        assert!(
            !codes.iter().any(|code| code.starts_with("severity.")),
            "the deleted repair code must not come back: {codes:?}"
        );
        let ledger = RepairLedger::acknowledged(
            &parsed.tallies,
            &[
                (("prod".to_owned(), "game".to_owned()), PathBuf::new()),
                (("prod".to_owned(), "nginx".to_owned()), PathBuf::new()),
            ],
        );
        assert_eq!(
            ledger.severity_unmapped,
            IndexMap::from([("game".to_owned(), 2)])
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one scenario, stated in order
    fn http_wal_failure_rejects_only_failed_group_and_preserves_successes() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let writer = crate::ingest::wal::WalWriter::new(tmp.path().join("wal"));
            writer.ensure_dir().unwrap();
            std::fs::write(writer.dir().join("blocked"), b"keep").unwrap();
            let mut parsed = ParsedEvents::new();
            // `shared` sits in both a written env and the failed one.
            for (env, service, count) in [
                ("prod", "before", 1),
                ("blocked", "shared", 3),
                ("prod", "shared", 2),
            ] {
                let mut batch = ServiceBatch::default();
                for id in 0..count {
                    batch.push(
                        serde_json::json!({"env":env,"service":service,"id":id})
                            .as_object()
                            .unwrap()
                            .clone(),
                    );
                }
                let key = (env.to_owned(), service.to_owned());
                parsed.batches.insert(key.clone(), batch);
                let code = if env == "blocked" {
                    RepairCode::FieldNameTooLong
                } else {
                    RepairCode::EnvDefaulted
                };
                parsed.tallies.insert(
                    key,
                    GroupTally {
                        repairs: IndexMap::from([(code, count)]),
                        severity_unmapped: count,
                    },
                );
            }
            let (written, failed) = write_wal_batches(&writer, &mut parsed);
            assert!(failed, "a failed group fails the request");
            assert_eq!(parsed.reject_counts.get(RejectReason::WalFailure), 3);
            assert_eq!(parsed.errors.len(), 1);
            assert_eq!(parsed.batches.len(), 2);
            assert!(
                !parsed
                    .batches
                    .contains_key(&("blocked".to_owned(), "shared".to_owned()))
            );

            // Only the written groups reach the ledger: the failed group's
            // repairs and unmapped severities are unacknowledged.
            let ledger = RepairLedger::acknowledged(&parsed.tallies, &written);
            assert_eq!(
                ledger.repairs,
                IndexMap::from([
                    ((RepairCode::EnvDefaulted, "before".to_owned()), 1),
                    ((RepairCode::EnvDefaulted, "shared".to_owned()), 2),
                ])
            );
            assert_eq!(
                ledger.severity_unmapped,
                IndexMap::from([("before".to_owned(), 1), ("shared".to_owned(), 2)])
            );
            assert_eq!(ledger.total(Some(RepairClass::Actionable)), 0);
            ledger.emit_metrics();
            assert_eq!(
                sample(
                    &handle,
                    "trawl_ingest_repairs_total{code=\"env.defaulted\",service=\"shared\"}"
                ),
                2
            );
            assert_eq!(
                sample(&handle, "trawl_severity_unmapped_total{service=\"shared\"}"),
                2
            );
            assert!(
                !handle.render().contains("field.name_too_long"),
                "{}",
                handle.render()
            );

            // finalize_ingest emits this same per-reason accounting once.
            parsed.reject_counts.emit_metrics();
            assert_eq!(
                sample(
                    &handle,
                    "trawl_ingest_events_rejected_total{reason=\"wal_failure\"}"
                ),
                3
            );
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                0
            );
            assert_eq!(written.len(), 2);
            for ((env, service), path) in written {
                assert_eq!(env, "prod");
                let output = std::fs::read_to_string(path).unwrap();
                assert_eq!(
                    output.lines().count(),
                    if service == "before" { 1 } else { 2 }
                );
            }
            assert_eq!(
                std::fs::read(writer.dir().join("blocked")).unwrap(),
                b"keep"
            );
        });
    }

    /// Every group failed: nothing reaches the ledger.
    #[test]
    fn a_ledger_over_no_written_group_counts_nothing() {
        let mut tallies = IndexMap::new();
        tallies.insert(
            ("prod".to_owned(), "svc".to_owned()),
            GroupTally {
                repairs: IndexMap::from([(RepairCode::TimeOutOfRange, 4)]),
                severity_unmapped: 4,
            },
        );
        let ledger = RepairLedger::acknowledged(&tallies, &[]);
        assert!(ledger.repairs.is_empty());
        assert!(ledger.severity_unmapped.is_empty());
        assert_eq!(ledger.total(None), 0);
    }

    // --- the repair ledger's log text (ADR-0055) ---

    fn ledger(pairs: &[(RepairCode, &str, u64)]) -> RepairLedger {
        RepairLedger {
            repairs: pairs
                .iter()
                .map(|(code, service, count)| ((*code, (*service).to_owned()), *count))
                .collect(),
            severity_unmapped: IndexMap::new(),
        }
    }

    /// A clean request: zero totals and empty strings, never a missing field.
    #[test]
    fn a_clean_ledger_reports_zeroes_and_empty_detail() {
        let ledger = RepairLedger::default();
        for class in [
            None,
            Some(RepairClass::Routine),
            Some(RepairClass::Actionable),
        ] {
            assert_eq!(ledger.total(class), 0);
            assert_eq!(ledger.codes(class), "");
            assert_eq!(
                ledger.detail(class),
                RepairDetail {
                    text: String::new(),
                    omitted_pairs: 0,
                    omitted_occurrences: 0,
                }
            );
        }
    }

    /// Ties on count break by code, then by service, both ascending.
    #[test]
    fn repair_detail_orders_by_count_then_code_then_service() {
        let ledger = ledger(&[
            (RepairCode::HostFromPeer, "b", 5),
            (RepairCode::EnvDefaulted, "b", 5),
            (RepairCode::EnvDefaulted, "a", 5),
            (RepairCode::FieldNameCaseFolded, "z", 9),
            (RepairCode::TimeOutOfRange, "c", 1),
        ]);
        assert_eq!(
            ledger.detail(None).text,
            "z/field.name_case_folded:9, a/env.defaulted:5, b/env.defaulted:5, \
             b/host.from_peer:5, c/time.out_of_range:1"
        );
    }

    /// At most eight pairs; the rest are counted, never listed.
    #[test]
    fn repair_detail_lists_eight_pairs_and_counts_the_rest() {
        let services: Vec<String> = (0..11).map(|i| format!("s{i:02}")).collect();
        let pairs: Vec<(RepairCode, &str, u64)> = services
            .iter()
            .enumerate()
            .map(|(i, service)| (RepairCode::EnvDefaulted, service.as_str(), 20 - i as u64))
            .collect();
        let detail = ledger(&pairs).detail(None);
        let listed: Vec<&str> = detail.text.split(", ").collect();
        assert_eq!(listed.len(), REPAIR_DETAIL_PAIRS);
        assert_eq!(listed[0], "s00/env.defaulted:20");
        assert_eq!(listed[7], "s07/env.defaulted:13");
        // s08..s10 hold 12 + 11 + 10.
        assert_eq!(detail.omitted_pairs, 3);
        assert_eq!(detail.omitted_occurrences, 33);
    }

    /// Per-code totals are exact over the whole ledger, in declaration
    /// order, and the class filter keeps each line to its own codes.
    #[test]
    fn repair_codes_are_exact_and_filtered_by_class() {
        let ledger = ledger(&[
            (RepairCode::FieldNameTooLong, "a", 1),
            (RepairCode::EnvDefaulted, "a", 50),
            (RepairCode::EnvDefaulted, "b", 7),
            (RepairCode::TimeOutOfRange, "b", 2),
            (RepairCode::FieldNameCaseFolded, "c", 3),
        ]);
        assert_eq!(
            ledger.codes(None),
            "env.defaulted:57, time.out_of_range:2, field.name_too_long:1, \
             field.name_case_folded:3"
        );
        assert_eq!(
            ledger.codes(Some(RepairClass::Routine)),
            "env.defaulted:57, field.name_case_folded:3"
        );
        assert_eq!(
            ledger.codes(Some(RepairClass::Actionable)),
            "time.out_of_range:2, field.name_too_long:1"
        );
        assert_eq!(ledger.total(None), 63);
        assert_eq!(ledger.total(Some(RepairClass::Routine)), 60);
        assert_eq!(ledger.total(Some(RepairClass::Actionable)), 3);
        assert_eq!(
            ledger.detail(Some(RepairClass::Actionable)).text,
            "b/time.out_of_range:2, a/field.name_too_long:1"
        );
        assert!(
            !ledger
                .detail(Some(RepairClass::Routine))
                .text
                .contains("time.out_of_range")
        );
    }

    // --- batch-level errors ---

    #[test]
    fn parse_empty_json_array_rejected() {
        let data = b"[]";
        let err = parse(data).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    // --- per-event errors ---

    #[test]
    fn parse_missing_service() {
        let data = br#"{"message":"no service field"}"#;
        let parsed = parse(data).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("service"));
        assert_eq!(parsed.reject_counts.get(RejectReason::MissingService), 1);
    }

    #[test]
    fn parse_non_string_service_typed_reason() {
        for data in [
            br#"{"service":{"name":"x"}}"#.as_slice(),
            br#"{"service":42}"#.as_slice(),
        ] {
            let parsed = parse(data).unwrap();
            assert!(parsed.batches.is_empty());
            assert_eq!(
                parsed.reject_counts.get(RejectReason::ServiceNotString),
                1,
                "non-string service must carry the type-specific reason"
            );
            assert_eq!(parsed.reject_counts.get(RejectReason::MissingService), 0);
        }
    }

    #[test]
    fn parse_invalid_json() {
        let data = b"not json at all";
        let parsed = parse(data).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid JSON"));
    }

    #[test]
    fn parse_rejects_path_traversal_service() {
        let data = br#"{"service":"../../etc/passwd","message":"pwned"}"#;
        let parsed = parse(data).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    #[test]
    fn parse_rejects_slash_in_service() {
        let data = br#"{"service":"foo/bar","message":"nope"}"#;
        let parsed = parse(data).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    #[test]
    fn parse_rejects_space_in_service() {
        // The service name is written to the path verbatim (ADR-0009), so
        // the charset excludes spaces.
        let data = br#"{"service":"Activity Monitor","message":"nope"}"#;
        let parsed = parse(data).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.reject_counts.get(RejectReason::InvalidChars), 1);
    }

    #[test]
    fn parse_rejects_empty_service_name() {
        let data = br#"{"service":"","message":"empty"}"#;
        let parsed = parse(data).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("cannot be empty"));
    }

    #[test]
    fn parse_rejects_long_service_name() {
        let name = "a".repeat(129);
        let data = format!(r#"{{"service":"{name}","message":"long"}}"#);
        let parsed = parse(data.as_bytes()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("too long"));
    }

    #[test]
    fn parse_json_array_rejects_bad_service() {
        let data = br#"[{"service":"../evil","message":"nope"}]"#;
        let parsed = parse(data).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    // --- env allowlist (ADR-0009) ---

    #[test]
    fn parse_unlisted_env_rejected_per_event() {
        // The sibling with a good env still lands; the batch never gains a
        // (nope, svc) key, so no data/nope/ path can ever be created.
        let data = br#"{"service":"svc","env":"nope","message":"bad"}
{"service":"svc","env":"lab","message":"good"}"#;
        let parsed = parse_with(data, &["prod", "lab"], false).unwrap();
        assert_eq!(total_accepted(&parsed), 1);
        assert!(has_batch(&parsed, "lab", "svc"));
        assert!(!has_batch(&parsed, "nope", "svc"));
        assert_eq!(parsed.reject_counts.get(RejectReason::EnvNotAllowed), 1);
    }

    #[test]
    fn parse_missing_env_defaults_with_repair() {
        let data = br#"{"service":"svc","message":"ok"}"#;
        let parsed = parse_with(data, &["prod", "lab"], false).unwrap();
        assert!(has_batch(&parsed, "prod", "svc"));
        let event = &batch_maps(&parsed, "prod", "svc")[0];
        assert_eq!(event["env"], "prod");
        assert_eq!(
            repairs_of(&parsed, "prod", "svc", RepairCode::EnvDefaulted),
            1
        );
    }

    #[test]
    fn parse_same_service_in_two_envs_separate_batches() {
        let data = br#"{"service":"svc","env":"prod","message":"a"}
{"service":"svc","env":"lab","message":"b"}"#;
        let parsed = parse_with(data, &["prod", "lab"], false).unwrap();
        assert_eq!(parsed.batches.len(), 2);
        assert!(has_batch(&parsed, "prod", "svc"));
        assert!(has_batch(&parsed, "lab", "svc"));
    }

    // --- trusted relay ---

    #[test]
    fn parse_hostless_event_from_trusted_relay_rejected() {
        let data = br#"{"service":"svc","message":"no host"}"#;
        let parsed = parse_with(data, &["prod"], true).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(
            parsed.reject_counts.get(RejectReason::HostMissingFromRelay),
            1
        );

        // The same event from a non-relay peer lands with host.from_peer.
        let parsed = parse_with(data, &["prod"], false).unwrap();
        assert_eq!(total_accepted(&parsed), 1);
        assert_eq!(
            repairs_of(&parsed, "prod", "svc", RepairCode::HostFromPeer),
            1
        );
    }

    // --- partial-success tests ---

    #[test]
    fn parse_partial_ndjson_bad_middle() {
        let data = b"{\"service\":\"nginx\",\"message\":\"one\"}\nnot json\n{\"service\":\"nginx\",\"message\":\"three\"}";
        let parsed = parse(data).unwrap();
        assert_eq!(total_accepted(&parsed), 2);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 1);
        assert!(parsed.errors[0].message.contains("invalid JSON"));
    }

    #[test]
    fn parse_partial_array_non_object() {
        let data = br#"[{"service":"nginx","message":"ok"},"just a string",{"service":"nginx","message":"also ok"}]"#;
        let parsed = parse(data).unwrap();
        assert_eq!(total_accepted(&parsed), 2);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 1);
        assert!(parsed.errors[0].message.contains("expected JSON object"));
    }

    #[test]
    fn parse_mixed_services_accepted() {
        let data = b"{\"service\":\"nginx\",\"message\":\"one\"}\n{\"service\":\"apache\",\"message\":\"two\"}";
        let parsed = parse(data).unwrap();
        assert_eq!(total_accepted(&parsed), 2);
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.batches.len(), 2);
        assert!(has_batch(&parsed, "prod", "nginx"));
        assert!(has_batch(&parsed, "prod", "apache"));
    }

    #[test]
    fn parse_first_event_bad_scans_forward() {
        let data = b"{\"message\":\"no service\"}\n{\"service\":\"nginx\",\"message\":\"good\"}";
        let parsed = parse(data).unwrap();
        assert_eq!(total_accepted(&parsed), 1);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 0);
        assert!(has_batch(&parsed, "prod", "nginx"));
    }

    #[test]
    fn parse_all_events_bad() {
        let data = b"not json\nalso not json\n{\"no_service\":true}";
        let parsed = parse(data).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 3);
    }

    #[test]
    fn parse_ndjson_mixed_validity() {
        // 5 lines: index 0 good, 1 bad json, 2 good, 3 missing service, 4 good
        let data = b"{\"service\":\"nginx\",\"message\":\"a\"}\nnot json\n{\"service\":\"nginx\",\"message\":\"c\"}\n{\"no_service\":true}\n{\"service\":\"nginx\",\"message\":\"e\"}";
        let parsed = parse(data).unwrap();
        assert_eq!(total_accepted(&parsed), 3);
        assert_eq!(parsed.errors.len(), 2);
        assert_eq!(parsed.errors[0].index, 1);
        assert_eq!(parsed.errors[1].index, 3);
    }

    // --- depth-limit regression tests ---

    // DuckDB's `read_json` is a recursive-descent parser: JSON nested past
    // ~500 levels overflows the (2 MB) `spawn_blocking` stack and the C++
    // frames can leap the guard page into a raw SIGSEGV. `maximum_depth=2`
    // does not protect against this — that param caps schema-inference
    // flattening, not parse recursion. What actually keeps adversarially-deep
    // JSON away from `read_json` is that ingest re-serializes every event
    // through `serde_json`, whose default recursion limit (128) rejects it at
    // the door, before it is ever written to the WAL. These tests pin that
    // invariant: if a refactor ever calls `Deserializer::disable_recursion_limit()`
    // (or enables serde_json's `unbounded_depth`), they break loudly.

    /// A pathologically-deep ndjson line is rejected per-line and never reaches
    /// a WAL batch; sibling good lines on the same request still survive.
    #[test]
    fn parse_ndjson_rejects_deeply_nested_event() {
        const DEPTH: usize = 1000; // well past serde_json's 128 limit
        let deep = format!("{}1{}", "{\"a\":".repeat(DEPTH), "}".repeat(DEPTH));
        let data = format!("{deep}\n{{\"service\":\"nginx\",\"message\":\"ok\"}}");
        let parsed = parse(data.as_bytes()).unwrap();
        // Deep line (index 0) rejected; good line (index 1) accepted.
        assert_eq!(total_accepted(&parsed), 1);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 0);
        assert!(
            parsed.errors[0].message.contains("recursion"),
            "expected a serde recursion-limit rejection, got: {}",
            parsed.errors[0].message
        );
    }

    /// A deeply-nested event in a JSON array aborts the whole request at parse
    /// time (the array path has no per-event isolation), so nothing is written.
    #[test]
    fn parse_json_array_rejects_deeply_nested_event() {
        const DEPTH: usize = 1000;
        let deep = format!("{}1{}", "{\"a\":".repeat(DEPTH), "}".repeat(DEPTH));
        let data = format!("[{deep}]");
        let err = parse(data.as_bytes()).expect_err("deeply-nested array event must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("recursion"),
            "expected a serde recursion-limit rejection, got: {msg}"
        );
    }

    // --- envelope stamping through the parse paths ---

    #[test]
    fn envelope_stamped_when_missing_ndjson() {
        let data = br#"{"service":"test"}"#;
        let parsed = parse(data).unwrap();
        let event = &batch_maps(&parsed, "prod", "test")[0];
        assert_eq!(event["_time"], ARRIVAL);
        assert_eq!(event["_ingested"], ARRIVAL);
        assert_eq!(event["host"], "127.0.0.1");
        assert_eq!(event["env"], "prod");
        assert!(event.contains_key("_raw"));
        assert!(
            !event.contains_key("message"),
            "message is convention, not server-filled (ADR-0009)"
        );
    }

    #[test]
    fn envelope_stamped_when_missing_json_array() {
        let data = br#"[{"service":"test"}]"#;
        let parsed = parse(data).unwrap();
        let event = &batch_maps(&parsed, "prod", "test")[0];
        assert_eq!(event["_time"], ARRIVAL);
        assert_eq!(event["_ingested"], ARRIVAL);
        assert_eq!(event["host"], "127.0.0.1");
    }

    #[test]
    fn client_values_not_overwritten() {
        let data = br#"{"service":"test","timestamp":"2025-12-31T12:00:00Z","host":"myhost","message":"hello"}"#;
        let parsed = parse(data).unwrap();
        let event = &batch_maps(&parsed, "prod", "test")[0];
        // The `timestamp` source is read for `_time` and stored verbatim
        // as the sender's own column (ADR-0013 §2).
        assert_eq!(event["_time"], "2025-12-31T12:00:00.000000Z");
        assert_eq!(event["timestamp"], "2025-12-31T12:00:00Z");
        assert_eq!(event["host"], "myhost");
        assert_eq!(event["message"], "hello");
    }

    /// Repairs are tallied per code within each `(env, service)` group, one
    /// per event that earned the code.
    #[test]
    fn repairs_counted_per_code_and_service() {
        let data = br#"{"service":"a"}
{"service":"a","Mixed":1}
{"service":"b","env":"lab","host":"h","_time":"2026-01-01T00:00:00Z"}
{"service":"b"}"#;
        let parsed = parse_with(data, &["prod", "lab"], false).unwrap();
        assert_eq!(
            repairs_of(&parsed, "prod", "a", RepairCode::TimeFromIngest),
            2
        );
        assert_eq!(
            repairs_of(&parsed, "prod", "a", RepairCode::EnvDefaulted),
            2
        );
        assert_eq!(
            repairs_of(&parsed, "prod", "a", RepairCode::HostFromPeer),
            2
        );
        assert_eq!(
            repairs_of(&parsed, "prod", "a", RepairCode::FieldNameCaseFolded),
            1
        );
        assert_eq!(
            repairs_of(&parsed, "prod", "b", RepairCode::TimeFromIngest),
            1
        );
        assert!(
            !parsed
                .tallies
                .contains_key(&("lab".to_owned(), "b".to_owned())),
            "a clean group holds no tally"
        );
        assert_eq!(parsed.errors.len(), 0, "repair is not a rejection");
    }

    /// The substituted time and canonical `_raw` reach the WAL bytes.
    #[test]
    fn substituted_time_appears_in_wal_ndjson() {
        let data = br#"{"service":"test","timestamp":"not-a-date"}"#;
        let parsed = parse(data).unwrap();
        let ndjson = &parsed.batches[&("prod".to_string(), "test".to_string())].ndjson;
        let wal_text = std::str::from_utf8(ndjson).unwrap();
        let wal_event: serde_json::Value = serde_json::from_str(wal_text.trim()).unwrap();
        assert_eq!(wal_event["_time"], ARRIVAL);
        assert!(
            wal_event["_repairs"]
                .as_str()
                .unwrap()
                .contains("time.unparseable")
        );
        assert!(
            wal_event["_raw"].as_str().unwrap().contains("not-a-date"),
            "the malformed original is findable in _raw"
        );
    }

    /// A client-supplied `_trawl_wal_file` never reaches the WAL: compaction
    /// projects that name as its synthetic provenance column, so a row
    /// carrying it collides with `read_json`'s `filename=` option and costs
    /// the batch a retry under a renamed provenance column. The event
    /// itself is still accepted.
    #[test]
    fn reserved_wal_file_key_stripped_from_events() {
        let data = br#"{"service":"test","timestamp":"2025-12-31T12:00:00Z","_trawl_wal_file":"x","keep":"me"}"#;
        let parsed = parse(data).unwrap();
        assert!(parsed.errors.is_empty(), "stripping is not a rejection");
        assert_eq!(total_accepted(&parsed), 1);

        let event = &batch_maps(&parsed, "prod", "test")[0];
        assert!(
            !event.contains_key(compaction::WAL_FILE_COL),
            "reserved key must be stripped, got {event:?}"
        );
        assert_eq!(
            event.get("keep").and_then(serde_json::Value::as_str),
            Some("me"),
            "other user fields are untouched"
        );

        let wal_text =
            std::str::from_utf8(&parsed.batches[&("prod".to_string(), "test".to_string())].ndjson)
                .unwrap();
        // The stripped key survives only inside the _raw string (escaped),
        // never as a JSON key of the WAL event.
        let wal_event: serde_json::Value = serde_json::from_str(wal_text.trim()).unwrap();
        assert!(wal_event.get(compaction::WAL_FILE_COL).is_none());
    }

    #[test]
    fn envelope_appears_in_wal_ndjson() {
        let data = br#"{"service":"test"}"#;
        let parsed = parse(data).unwrap();
        let ndjson = &parsed.batches[&("prod".to_string(), "test".to_string())].ndjson;
        let wal_text = std::str::from_utf8(ndjson).unwrap();
        let wal_event: serde_json::Value = serde_json::from_str(wal_text.trim()).unwrap();
        assert_eq!(wal_event["_time"], ARRIVAL);
        assert_eq!(wal_event["_ingested"], ARRIVAL);
        assert_eq!(wal_event["host"], "127.0.0.1");
        assert_eq!(wal_event["env"], "prod");
    }

    // --- mixed-service tests ---

    #[test]
    fn parse_three_services_ndjson() {
        let data = b"{\"service\":\"nginx\",\"message\":\"a\"}\n{\"service\":\"redis\",\"message\":\"b\"}\n{\"service\":\"postgres\",\"message\":\"c\"}\n{\"service\":\"nginx\",\"message\":\"d\"}";
        let parsed = parse(data).unwrap();
        assert_eq!(parsed.batches.len(), 3);
        assert_eq!(total_accepted(&parsed), 4);
        assert!(parsed.errors.is_empty());
        assert_eq!(batch_maps(&parsed, "prod", "nginx").len(), 2);
        assert_eq!(batch_maps(&parsed, "prod", "redis").len(), 1);
        assert_eq!(batch_maps(&parsed, "prod", "postgres").len(), 1);
    }

    #[test]
    fn parse_three_services_json_array() {
        let data = br#"[{"service":"a","m":"1"},{"service":"b","m":"2"},{"service":"c","m":"3"}]"#;
        let parsed = parse(data).unwrap();
        assert_eq!(parsed.batches.len(), 3);
        assert_eq!(total_accepted(&parsed), 3);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parse_insertion_order_preserved() {
        let data = b"{\"service\":\"charlie\",\"message\":\"1\"}\n{\"service\":\"alpha\",\"message\":\"2\"}\n{\"service\":\"bravo\",\"message\":\"3\"}";
        let parsed = parse(data).unwrap();
        let keys: Vec<&str> = parsed.batches.keys().map(|(_, s)| s.as_str()).collect();
        assert_eq!(keys, &["charlie", "alpha", "bravo"]);
    }

    #[test]
    fn parse_ndjson_per_service_wal_bytes() {
        let data = b"{\"service\":\"a\",\"x\":1}\n{\"service\":\"b\",\"x\":2}\n{\"service\":\"a\",\"x\":3}";
        let parsed = parse(data).unwrap();
        // Each service batch's ndjson should only contain its own events.
        let a_lines: Vec<_> =
            std::str::from_utf8(&parsed.batches[&("prod".to_string(), "a".to_string())].ndjson)
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .collect();
        let b_lines: Vec<_> =
            std::str::from_utf8(&parsed.batches[&("prod".to_string(), "b".to_string())].ndjson)
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .collect();
        assert_eq!(a_lines.len(), 2);
        assert_eq!(b_lines.len(), 1);
    }

    #[test]
    fn reject_counts_accuracy() {
        // 1 invalid json, 1 missing service, 1 empty service, 1 bad chars,
        // 1 non-string service, 1 valid
        let data = b"not json\n{\"no_svc\":1}\n{\"service\":\"\",\"m\":\"x\"}\n{\"service\":\"a/b\",\"m\":\"x\"}\n{\"service\":7,\"m\":\"x\"}\n{\"service\":\"ok\",\"m\":\"x\"}";
        let parsed = parse(data).unwrap();
        assert_eq!(parsed.reject_counts.get(RejectReason::InvalidJson), 1);
        assert_eq!(parsed.reject_counts.get(RejectReason::MissingService), 1);
        assert_eq!(parsed.reject_counts.get(RejectReason::EmptyService), 1);
        assert_eq!(parsed.reject_counts.get(RejectReason::InvalidChars), 1);
        assert_eq!(parsed.reject_counts.get(RejectReason::ServiceNotString), 1);
        assert_eq!(parsed.reject_counts.total(), 5);
        assert_eq!(total_accepted(&parsed), 1);
    }

    // --- gzip decompression bound ---

    /// `len` bytes of ndjson lines of one repetitive shape: journald-like JSON
    /// compresses far past 10x, as Vector's batches do.
    fn repetitive_ndjson(len: usize) -> Vec<u8> {
        let line = br#"{"service":"sshd","message":"session opened for user root by (uid=0)","_SYSTEMD_UNIT":"ssh.service"}"#;
        let mut body = Vec::with_capacity(len);
        while body.len() < len {
            body.extend_from_slice(line);
            body.push(b'\n');
        }
        body.truncate(len);
        body
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// A body that expands ~25x is accepted while its decompressed size
    /// stays under the limit: the wire-to-decompressed ratio is not a limit.
    #[test]
    fn gzip_expanding_far_past_ten_times_is_accepted_under_the_limit() {
        let raw = repetitive_ndjson(64 * 1024);
        let wire = gzip(&raw);
        assert!(
            raw.len() >= 25 * wire.len(),
            "fixture expands only {}x",
            raw.len() / wire.len()
        );
        let decompressed = decompress_gzip(&wire, 1024 * 1024).unwrap();
        assert_eq!(decompressed, raw);
    }

    /// A body that decompresses to exactly the limit is accepted.
    #[test]
    fn gzip_decompressing_to_exactly_the_limit_is_accepted() {
        let raw = repetitive_ndjson(4096);
        let decompressed = decompress_gzip(&gzip(&raw), raw.len()).unwrap();
        assert_eq!(decompressed.len(), raw.len());
    }

    /// One byte past the limit is 413 `ingest_error`, and decompression
    /// stops at limit + 1: the stream below is truncated after its first
    /// 64 KiB, so reading it to the end would fail as a broken stream (400)
    /// instead.
    #[tokio::test]
    async fn gzip_decompressing_past_the_limit_is_413_and_stops_early() {
        use axum::response::IntoResponse as _;
        use std::io::Write as _;

        let limit = 4096;
        let raw = repetitive_ndjson(1024 * 1024);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&raw[..64 * 1024]).unwrap();
        encoder.flush().unwrap();
        let prefix = encoder.get_ref().len();
        encoder.write_all(&raw[64 * 1024..]).unwrap();
        let mut wire = encoder.finish().unwrap();
        wire.truncate(prefix);
        // The truncated stream is broken when read to the end.
        assert!(decompress_gzip(&wire, usize::MAX / 2).is_err());

        // Exactly limit + 1 decompressed bytes, from a complete stream.
        let over = decompress_gzip(&gzip(&raw[..=limit]), limit).unwrap_err();
        let response = over.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "ingest_error");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(&limit.to_string()), "{message}");

        let early = decompress_gzip(&wire, limit).unwrap_err().into_response();
        assert_eq!(early.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }
}
