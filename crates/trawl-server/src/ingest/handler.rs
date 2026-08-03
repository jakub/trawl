// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP handler for the `POST /api/v1/ingest` endpoint.

use std::io::Read as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use fleet_auth::VerifiedKey;
use indexmap::IndexMap;

use crate::error::ServerError;
use crate::ingest::envelope::{self, EnvelopeContext, RejectReason};
use crate::ingest::pipeline::ServiceBatch;
use crate::policy::{Permission, TrawlAuthz as _};
use crate::state::AppState;
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

/// The batch key: `(env, service)` — two envs must never share a WAL
/// batch, a hot-buffer drain key, or a parquet partition (ADR-0009).
type BatchKey = (String, String);

/// Parsed ingest payload, grouped by `(env, service)`.
#[derive(Debug)]
struct ParsedEvents {
    /// Service batches keyed by `(env, service)`, insertion-order preserved.
    batches: IndexMap<BatchKey, ServiceBatch>,
    /// Per-event validation errors accumulated during parsing.
    errors: Vec<IngestEventError>,
    /// Per-reason rejection counts for prometheus labels.
    reject_counts: RejectCounts,
    /// Per-`(code, service)` repair counts (ADR-0009): accepted events the
    /// server modified, feeding `trawl_ingest_repairs_total{code,service}`.
    repairs: IndexMap<(&'static str, String), u64>,
}

impl ParsedEvents {
    fn new() -> Self {
        Self {
            batches: IndexMap::new(),
            errors: Vec::new(),
            reject_counts: RejectCounts::default(),
            repairs: IndexMap::new(),
        }
    }

    /// Canonicalize one parsed object and either batch it or record the
    /// rejection. Shared by the ndjson and JSON-array parse paths.
    fn add_event(
        &mut self,
        index: usize,
        obj: &serde_json::Map<String, serde_json::Value>,
        ctx: &EnvelopeContext<'_>,
    ) {
        match envelope::canonicalize(obj, ctx) {
            Ok(canonical) => {
                for code in &canonical.repairs {
                    *self
                        .repairs
                        .entry((code.as_str(), canonical.service.clone()))
                        .or_default() += 1;
                }
                let key = (canonical.env, canonical.service);
                self.batches.entry(key).or_default().push(canonical.obj);
            }
            Err((message, reason)) => {
                self.errors.push(IngestEventError { index, message });
                self.reject_counts.increment(reason);
            }
        }
    }

    /// Total repaired-event... repairs applied (a single event may carry
    /// several codes; this counts code applications, for the log line).
    fn total_repairs(&self) -> u64 {
        self.repairs.values().sum()
    }
}

/// `POST /api/v1/ingest` — accept ndjson events into the WAL.
///
/// Expects `Content-Type: application/x-ndjson` (or `application/json`).
/// Supports `Content-Encoding: gzip` for compressed payloads.
pub async fn ingest(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Extension(peer_addr): Extension<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IngestResponse>, ServerError> {
    if !verified.has_permission(Permission::Ingest) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let wal_writer = state
        .ingest
        .wal_writer
        .as_ref()
        .ok_or_else(|| ServerError::Internal("ingest not enabled".into()))?;

    // Decompress, parse, and WAL-write in a single blocking task to avoid
    // hogging tokio worker threads with CPU-bound gzip/JSON work.
    let compressed = is_gzip(&headers);
    let wire_bytes = body.len();
    let wal_writer = Arc::clone(wal_writer);

    // Capture request-scoped context before moving into the blocking task.
    let arrival_instant = chrono::Utc::now();
    let arrival = arrival_instant.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let peer_host = peer_addr.ip().to_string();
    let peer_is_trusted_relay = state
        .ingest
        .trusted_relays
        .iter()
        .any(|c| c.contains(peer_addr.ip()));
    let envs = Arc::clone(&state.ingest.envs);
    let default_env = Arc::clone(&state.ingest.default_env);

    let (mut parsed, wal_paths, body_bytes, decompress_ms, parse_ms, wal_ms) =
        tokio::task::spawn_blocking(move || -> Result<_, ServerError> {
            let ctx = EnvelopeContext {
                arrival: &arrival,
                arrival_instant,
                peer_host: &peer_host,
                peer_is_trusted_relay,
                envs: envs.as_ref(),
                default_env: default_env.as_ref(),
            };
            let t0 = std::time::Instant::now();
            let raw = if compressed {
                decompress_gzip(&body, body.len())?
            } else {
                body.to_vec()
            };
            let decompress_ms = t0.elapsed().as_millis();
            let body_bytes = raw.len();

            if raw.is_empty() {
                return Err(ServerError::Ingest("empty request body".into()));
            }

            let t1 = std::time::Instant::now();
            let mut parsed = parse_events(&raw, &ctx)?;
            let parse_ms = t1.elapsed().as_millis();

            let t2 = std::time::Instant::now();
            let wal_paths = write_wal_batches(&wal_writer, &mut parsed);
            let wal_ms = t2.elapsed().as_millis();

            Ok((
                parsed,
                wal_paths,
                body_bytes,
                decompress_ms,
                parse_ms,
                wal_ms,
            ))
        })
        .await
        .map_err(|e| ServerError::Internal(format!("ingest task panicked: {e}")))??;

    let result = finalize_ingest(
        &state,
        &mut parsed,
        &wal_paths,
        &verified,
        wire_bytes,
        compressed,
        body_bytes,
        decompress_ms,
        parse_ms,
        wal_ms,
    );

    Ok(Json(result))
}

/// Write one WAL file per `(env, service)` group. Partial failures are
/// reported as per-event errors — successful groups are durable. Failed
/// groups are removed from `parsed.batches` so they are never published.
///
fn write_wal_batches(
    wal_writer: &crate::ingest::wal::WalWriter,
    parsed: &mut ParsedEvents,
) -> Vec<(BatchKey, PathBuf)> {
    let mut wal_paths: Vec<(BatchKey, PathBuf)> = Vec::new();
    let mut wal_failures: Vec<BatchKey> = Vec::new();

    for (key, batch) in &parsed.batches {
        let (env, svc) = key;
        match wal_writer.write(env, svc, &batch.ndjson) {
            Ok(path) => wal_paths.push((key.clone(), path)),
            Err(e) => {
                tracing::warn!(
                    event_type = "wal_write_failed",
                    env = %env,
                    service = %svc,
                    events_lost = batch.maps.len(),
                    error = %e,
                    "WAL write failed for service group"
                );
                let event_count = batch.maps.len();
                parsed.errors.push(IngestEventError {
                    index: 0,
                    message: format!(
                        "WAL write failed for service '{svc}': {e} ({event_count} events lost)"
                    ),
                });
                parsed
                    .reject_counts
                    .increment_by(RejectReason::WalFailure, event_count as u64);
                wal_failures.push(key.clone());
            }
        }
    }

    // Remove failed service groups from batches so we don't publish them.
    for key in &wal_failures {
        parsed.batches.shift_remove(key);
    }

    wal_paths
}

/// Post-blocking-task: update metrics, publish to event bus, log, and build response.
#[allow(clippy::too_many_arguments)]
fn finalize_ingest(
    state: &AppState,
    parsed: &mut ParsedEvents,
    wal_paths: &[(BatchKey, PathBuf)],
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
        // without flooding telemetry with per-event detail.
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

    if !parsed.repairs.is_empty() {
        for ((code, service), count) in &parsed.repairs {
            // SECURITY: `service` is client-supplied and `/metrics` is
            // unauthenticated. It is a data dimension ADR-0009 publishes
            // deliberately (alerting per repair code and sender), unlike the
            // principal-derived labels `handlers.rs` forbids — but the
            // recorder never evicts a series, so the distinct label values are
            // capped and per-key attribution stays in the authenticated log.
            metrics::counter!(
                crate::metrics::INGEST_REPAIRS_TOTAL,
                "code" => *code,
                "service" => crate::metrics::repair_service_label(service)
            )
            .increment(*count);
        }
        let summary = parsed
            .repairs
            .iter()
            .map(|((code, service), count)| format!("{service}/{code}:{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            event_type = "ingest_repairs",
            user = %verified.name,
            repairs = parsed.total_repairs(),
            summary,
            "accepted events were repaired (codes recorded in _repairs)"
        );
    }

    // Publish each successfully-written batch to hot buffer + event bus
    // so events are visible to queries and SSE streams immediately.
    if let Some(pipeline) = &state.ingest.pipeline {
        for (key, wal_path) in wal_paths {
            if let Some(batch) = parsed.batches.swap_remove(key) {
                pipeline.publish(&key.0, &key.1, batch, wal_path);
            }
        }
    }

    let duration_ms = decompress_ms + parse_ms + wal_ms;
    let services = wal_paths.len();
    tracing::info!(
        event_type = "ingest_complete",
        user = %verified.name,
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
        "ingested events to WAL"
    );

    IngestResponse {
        accepted,
        rejected,
        errors: std::mem::take(&mut parsed.errors),
    }
}

/// Check if the request body is gzip-encoded.
fn is_gzip(headers: &HeaderMap) -> bool {
    headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"))
}

/// Maximum decompression ratio (compressed → decompressed). Prevents
/// gzip bombs from exhausting memory: a 16 MB payload can expand to at
/// most 160 MB.
const MAX_DECOMPRESSION_RATIO: usize = 10;

/// Decompress gzip body with a size cap to prevent decompression bombs.
fn decompress_gzip(data: &[u8], wire_bytes: usize) -> Result<Vec<u8>, ServerError> {
    let limit = wire_bytes.saturating_mul(MAX_DECOMPRESSION_RATIO);
    let decoder = flate2::read::GzDecoder::new(data);
    // Read up to limit + 1: if we get more than limit bytes, the payload
    // exceeds the cap and we reject it before allocating further.
    let mut decompressed = Vec::with_capacity(data.len().min(limit));
    decoder
        .take(u64::try_from(limit + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut decompressed)
        .map_err(|e| ServerError::Ingest(format!("gzip decompression failed: {e}")))?;
    if decompressed.len() > limit {
        return Err(ServerError::Ingest(format!(
            "decompressed body exceeds {limit} byte limit (ratio > {MAX_DECOMPRESSION_RATIO}x)",
        )));
    }
    Ok(decompressed)
}

/// Parse events from either ndjson or JSON array format.
///
/// Returns parsed events grouped by `(env, service)` with canonical maps
/// and ndjson bytes. The ndjson bytes are always ndjson regardless of
/// input format, ready for the WAL.
fn parse_events(data: &[u8], ctx: &EnvelopeContext<'_>) -> Result<ParsedEvents, ServerError> {
    let text = std::str::from_utf8(data)
        .map_err(|e| ServerError::Ingest(format!("body is not valid UTF-8: {e}")))?;

    let trimmed = text.trim_start();

    // Detect format: JSON array (vector batches) vs ndjson (line-delimited).
    if trimmed.starts_with('[') {
        parse_json_array(trimmed, ctx)
    } else {
        Ok(parse_ndjson(trimmed, ctx))
    }
}

/// Parse a JSON array of events (vector's default batch format).
///
/// Groups events by `(env, service)`, converting to ndjson per group for
/// WAL storage. Invalid events are accumulated as per-event errors rather
/// than aborting the entire batch — valid events are still accepted.
fn parse_json_array(text: &str, ctx: &EnvelopeContext<'_>) -> Result<ParsedEvents, ServerError> {
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| ServerError::Ingest(format!("invalid JSON array: {e}")))?;

    let arr = parsed
        .as_array()
        .ok_or_else(|| ServerError::Ingest("expected JSON array".into()))?;

    if arr.is_empty() {
        return Err(ServerError::Ingest("empty event array".into()));
    }

    let mut events = ParsedEvents::new();

    for (i, event) in arr.iter().enumerate() {
        let Some(obj) = event.as_object() else {
            events.errors.push(IngestEventError {
                index: i,
                message: "expected JSON object".into(),
            });
            events.reject_counts.increment(RejectReason::NotObject);
            continue;
        };
        events.add_event(i, obj, ctx);
    }

    Ok(events)
}

/// Parse ndjson (newline-delimited JSON objects).
///
/// Groups events by `(env, service)`, re-serializing to ndjson per group
/// for consistent WAL bytes (trimmed, one object per line). Invalid lines
/// are accumulated as per-event errors rather than aborting the batch.
fn parse_ndjson(text: &str, ctx: &EnvelopeContext<'_>) -> ParsedEvents {
    let mut events = ParsedEvents::new();

    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                events.errors.push(IngestEventError {
                    index: i,
                    message: format!("invalid JSON: {e}"),
                });
                events.reject_counts.increment(RejectReason::InvalidJson);
                continue;
            }
        };

        let Some(obj) = parsed.as_object() else {
            events.errors.push(IngestEventError {
                index: i,
                message: "expected JSON object".into(),
            });
            events.reject_counts.increment(RejectReason::NotObject);
            continue;
        };

        events.add_event(i, obj, ctx);
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::compaction;

    const ARRIVAL: &str = "2026-01-01T00:00:00.000000Z";

    /// Parse with a default single-env ("prod") context, no trusted relay.
    fn parse(data: &[u8]) -> Result<ParsedEvents, ServerError> {
        parse_with(data, &["prod"], false)
    }

    fn parse_with(data: &[u8], envs: &[&str], relay: bool) -> Result<ParsedEvents, ServerError> {
        let envs: Vec<String> = envs.iter().map(|s| (*s).to_string()).collect();
        let ctx = EnvelopeContext {
            arrival: ARRIVAL,
            arrival_instant: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            peer_host: "127.0.0.1",
            peer_is_trusted_relay: relay,
            envs: &envs,
            default_env: &envs[0],
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
        // `level` is consumed at ingest: derived onto the OTel ladder,
        // original spelling in severity_text.
        let data = br#"[{"service":"nginx","level":"error"},{"service":"nginx","level":"warn"}]"#;
        let parsed = parse(data).unwrap();
        let maps = batch_maps(&parsed, "prod", "nginx");
        assert_eq!(maps.len(), 2);
        assert!(!maps[0].contains_key("level"));
        assert_eq!(
            maps[0].get("severity").and_then(serde_json::Value::as_i64),
            Some(17)
        );
        assert_eq!(
            maps[1].get("severity").and_then(serde_json::Value::as_i64),
            Some(13)
        );
        assert_eq!(
            maps[1]
                .get("severity_text")
                .and_then(serde_json::Value::as_str),
            Some("warn")
        );
    }

    // --- batch-level errors (still Err, unchanged) ---

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
        // Spaces died with the verbatim-filename cutover (ADR-0009).
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
            parsed.repairs.get(&("env.defaulted", "svc".to_string())),
            Some(&1)
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
            parsed.repairs.get(&("host.from_peer", "svc".to_string())),
            Some(&1)
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
    // does NOT protect against this — that param caps schema-inference
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
        // The timestamp wire alias is consumed into a canonicalized _time.
        assert_eq!(event["_time"], "2025-12-31T12:00:00.000000Z");
        assert!(!event.contains_key("timestamp"));
        assert_eq!(event["host"], "myhost");
        assert_eq!(event["message"], "hello");
    }

    /// The repair ledger counts per (code, service) across the batch.
    #[test]
    fn repairs_counted_per_code_and_service() {
        let data = br#"{"service":"a","timestamp":"bad"}
{"service":"a","timestamp":"also-bad"}
{"service":"b","timestamp":"bad"}"#;
        let parsed = parse(data).unwrap();
        assert_eq!(
            parsed.repairs.get(&("time.from_ingest", "a".to_string())),
            Some(&2)
        );
        assert_eq!(
            parsed.repairs.get(&("time.from_ingest", "b".to_string())),
            Some(&1)
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
                .contains("time.from_ingest")
        );
        assert!(
            wal_event["_raw"].as_str().unwrap().contains("not-a-date"),
            "the malformed original is findable in _raw"
        );
    }

    /// A client-supplied `_trawl_wal_file` never reaches the WAL: compaction
    /// projects that name as its synthetic provenance column, so a row
    /// carrying it makes `read_json` fail to bind and wedges the service's
    /// whole WAL. The event itself is still accepted.
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
        // nginx gets 2 events, redis and postgres each get 1.
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
}
