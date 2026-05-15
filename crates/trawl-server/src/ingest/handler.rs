// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP handler for the `POST /api/v1/ingest` endpoint.

use std::fmt;
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use indexmap::IndexMap;
use serde_json::json;
use trawl_auth::keys::VerifiedKey;
use trawl_auth::roles::Permission;

use crate::error::ServerError;
use crate::ingest::pipeline::{self, ServiceBatch};
use crate::state::AppState;
use trawl_api::{IngestEventError, IngestResponse};

/// Why an event was rejected — used as a prometheus label value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectReason {
    MissingService,
    EmptyService,
    ServiceTooLong,
    InvalidChars,
    NotObject,
    InvalidJson,
    WalFailure,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingService => "missing_service",
            Self::EmptyService => "empty_service",
            Self::ServiceTooLong => "service_too_long",
            Self::InvalidChars => "invalid_chars",
            Self::NotObject => "not_object",
            Self::InvalidJson => "invalid_json",
            Self::WalFailure => "wal_failure",
        })
    }
}

/// Per-reason rejection counts for labeled prometheus metrics.
#[derive(Debug, Default)]
struct RejectCounts {
    missing_service: u64,
    empty_service: u64,
    service_too_long: u64,
    invalid_chars: u64,
    not_object: u64,
    invalid_json: u64,
    wal_failure: u64,
}

impl RejectCounts {
    fn increment(&mut self, reason: RejectReason) {
        self.increment_by(reason, 1);
    }

    fn increment_by(&mut self, reason: RejectReason, n: u64) {
        match reason {
            RejectReason::MissingService => self.missing_service += n,
            RejectReason::EmptyService => self.empty_service += n,
            RejectReason::ServiceTooLong => self.service_too_long += n,
            RejectReason::InvalidChars => self.invalid_chars += n,
            RejectReason::NotObject => self.not_object += n,
            RejectReason::InvalidJson => self.invalid_json += n,
            RejectReason::WalFailure => self.wal_failure += n,
        }
    }

    #[cfg(test)]
    fn total(&self) -> u64 {
        self.missing_service
            + self.empty_service
            + self.service_too_long
            + self.invalid_chars
            + self.not_object
            + self.invalid_json
            + self.wal_failure
    }

    /// Format non-zero counts as a compact summary (e.g. `"invalid_chars:3, missing_service:1"`).
    fn summary(&self) -> String {
        let pairs: &[(u64, &str)] = &[
            (self.missing_service, "missing_service"),
            (self.empty_service, "empty_service"),
            (self.service_too_long, "service_too_long"),
            (self.invalid_chars, "invalid_chars"),
            (self.not_object, "not_object"),
            (self.invalid_json, "invalid_json"),
            (self.wal_failure, "wal_failure"),
        ];
        let mut parts = Vec::new();
        for &(count, reason) in pairs {
            if count > 0 {
                parts.push(format!("{reason}:{count}"));
            }
        }
        parts.join(", ")
    }

    /// Emit non-zero counts as labeled prometheus counters.
    fn emit_metrics(&self) {
        let pairs: &[(u64, &str)] = &[
            (self.missing_service, "missing_service"),
            (self.empty_service, "empty_service"),
            (self.service_too_long, "service_too_long"),
            (self.invalid_chars, "invalid_chars"),
            (self.not_object, "not_object"),
            (self.invalid_json, "invalid_json"),
            (self.wal_failure, "wal_failure"),
        ];
        for &(count, reason) in pairs {
            if count > 0 {
                metrics::counter!(crate::metrics::INGEST_EVENTS_REJECTED_TOTAL, "reason" => reason)
                    .increment(count);
            }
        }
    }
}

/// Parsed ingest payload, grouped by service.
#[derive(Debug)]
struct ParsedEvents {
    /// Service batches keyed by service name, insertion-order preserved.
    batches: IndexMap<String, ServiceBatch>,
    /// Per-event validation errors accumulated during parsing.
    errors: Vec<IngestEventError>,
    /// Per-reason rejection counts for prometheus labels.
    reject_counts: RejectCounts,
}

/// Validate a single event object, returning the service name or a typed error.
///
/// Checks: has `service` field, service passes charset/length validation.
/// No cross-event validation — mixed services are accepted.
fn validate_event(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, (String, RejectReason)> {
    let svc = obj
        .get("service")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            (
                "missing 'service' field".to_owned(),
                RejectReason::MissingService,
            )
        })?;

    if svc.is_empty() {
        return Err((
            "service name cannot be empty".into(),
            RejectReason::EmptyService,
        ));
    }
    if svc.len() > pipeline::MAX_SERVICE_NAME_LEN {
        return Err((
            format!(
                "service name too long ({} chars, max {})",
                svc.len(),
                pipeline::MAX_SERVICE_NAME_LEN,
            ),
            RejectReason::ServiceTooLong,
        ));
    }
    if !svc.bytes().all(pipeline::is_valid_service_char) {
        return Err((
            format!(
                "service '{svc}' contains invalid characters \
                 (only alphanumeric, dash, underscore, dot, space allowed)"
            ),
            RejectReason::InvalidChars,
        ));
    }

    Ok(svc.to_owned())
}

/// Per-request defaults for fields that must always be present in WAL events.
///
/// Constructed once per ingest request (before `spawn_blocking`) so all events
/// in the batch share the same request-scoped timestamp and peer address.
struct IngestDefaults {
    /// RFC 3339 timestamp with millisecond precision (from request arrival).
    timestamp: String,
    /// Peer IP address as a string (from the TCP connection).
    host: String,
}

/// Fill mandatory fields on an event map if they are missing.
///
/// Events without these fields are effectively invisible to most queries
/// (time filters, bare text search, host grouping), so we fill sensible
/// defaults at ingest time rather than silently dropping them.
fn fill_defaults(obj: &mut serde_json::Map<String, serde_json::Value>, defaults: &IngestDefaults) {
    if !obj.contains_key("timestamp") {
        obj.insert("timestamp".into(), json!(&defaults.timestamp));
    }
    if !obj.contains_key("host") {
        obj.insert("host".into(), json!(&defaults.host));
    }
    if !obj.contains_key("message") {
        obj.insert("message".into(), json!(""));
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

    // Capture request-scoped defaults before moving into blocking task.
    let defaults = IngestDefaults {
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        host: peer_addr.ip().to_string(),
    };

    let (mut parsed, wal_paths, body_bytes, decompress_ms, parse_ms, wal_ms) =
        tokio::task::spawn_blocking(move || -> Result<_, ServerError> {
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
            let mut parsed = parse_events(&raw, &defaults)?;
            let parse_ms = t1.elapsed().as_millis();

            // Write one WAL file per service group. Partial failures are
            // reported as per-event errors — successful services are durable.
            let t2 = std::time::Instant::now();
            let mut wal_paths: Vec<(String, PathBuf)> = Vec::new();
            let mut wal_failures: Vec<(String, usize)> = Vec::new();

            for (svc, batch) in &parsed.batches {
                match wal_writer.write(svc, &batch.ndjson) {
                    Ok(path) => wal_paths.push((svc.clone(), path)),
                    Err(e) => {
                        tracing::warn!(
                            event_type = "wal_write_failed",
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
                        wal_failures.push((svc.clone(), event_count));
                    }
                }
            }
            let wal_ms = t2.elapsed().as_millis();

            // Remove failed service groups from batches so we don't publish them.
            for (svc, _) in &wal_failures {
                parsed.batches.shift_remove(svc);
            }

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

/// Post-blocking-task: update metrics, publish to event bus, log, and build response.
#[allow(clippy::too_many_arguments)]
fn finalize_ingest(
    state: &AppState,
    parsed: &mut ParsedEvents,
    wal_paths: &[(String, PathBuf)],
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

    // Publish each successfully-written batch to hot buffer + event bus
    // so events are visible to queries and SSE streams immediately.
    if let Some(pipeline) = &state.ingest.pipeline {
        for (svc, wal_path) in wal_paths {
            if let Some(batch) = parsed.batches.swap_remove(svc) {
                pipeline.publish(svc, batch, wal_path);
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
/// Returns parsed events with service name, JSON maps, and ndjson bytes.
/// The ndjson bytes are always ndjson regardless of input format, ready
/// for the WAL.
fn parse_events(data: &[u8], defaults: &IngestDefaults) -> Result<ParsedEvents, ServerError> {
    let text = std::str::from_utf8(data)
        .map_err(|e| ServerError::Ingest(format!("body is not valid UTF-8: {e}")))?;

    let trimmed = text.trim_start();

    // Detect format: JSON array (vector batches) vs ndjson (line-delimited).
    if trimmed.starts_with('[') {
        parse_json_array(trimmed, defaults)
    } else {
        parse_ndjson(trimmed, defaults)
    }
}

/// Parse a JSON array of events (vector's default batch format).
///
/// Groups events by service, converting to ndjson per service for WAL storage.
/// Invalid events are accumulated as per-event errors rather than aborting
/// the entire batch — valid events are still accepted.
fn parse_json_array(text: &str, defaults: &IngestDefaults) -> Result<ParsedEvents, ServerError> {
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| ServerError::Ingest(format!("invalid JSON array: {e}")))?;

    let arr = parsed
        .as_array()
        .ok_or_else(|| ServerError::Ingest("expected JSON array".into()))?;

    if arr.is_empty() {
        return Err(ServerError::Ingest("empty event array".into()));
    }

    let mut batches: IndexMap<String, ServiceBatch> = IndexMap::new();
    let mut errors = Vec::new();
    let mut reject_counts = RejectCounts::default();

    for (i, event) in arr.iter().enumerate() {
        let Some(obj) = event.as_object() else {
            errors.push(IngestEventError {
                index: i,
                message: "expected JSON object".into(),
            });
            reject_counts.increment(RejectReason::NotObject);
            continue;
        };

        let svc = match validate_event(obj) {
            Ok(svc) => svc,
            Err((msg, reason)) => {
                errors.push(IngestEventError {
                    index: i,
                    message: msg,
                });
                reject_counts.increment(reason);
                continue;
            }
        };

        let mut obj = obj.clone();
        fill_defaults(&mut obj, defaults);

        let batch = batches.entry(svc).or_default();
        serde_json::to_writer(&mut batch.ndjson, &obj)
            .map_err(|e| ServerError::Internal(format!("failed to serialize event: {e}")))?;
        batch.ndjson.push(b'\n');
        batch.maps.push(obj);
    }

    Ok(ParsedEvents {
        batches,
        errors,
        reject_counts,
    })
}

/// Parse ndjson (newline-delimited JSON objects).
///
/// Groups events by service, re-serializing to ndjson per service for
/// consistent WAL bytes (trimmed, one object per line). Invalid lines
/// are accumulated as per-event errors rather than aborting the batch.
fn parse_ndjson(text: &str, defaults: &IngestDefaults) -> Result<ParsedEvents, ServerError> {
    let mut batches: IndexMap<String, ServiceBatch> = IndexMap::new();
    let mut errors = Vec::new();
    let mut reject_counts = RejectCounts::default();

    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                errors.push(IngestEventError {
                    index: i,
                    message: format!("invalid JSON: {e}"),
                });
                reject_counts.increment(RejectReason::InvalidJson);
                continue;
            }
        };

        let Some(obj) = parsed.as_object() else {
            errors.push(IngestEventError {
                index: i,
                message: "expected JSON object".into(),
            });
            reject_counts.increment(RejectReason::NotObject);
            continue;
        };

        let svc = match validate_event(obj) {
            Ok(svc) => svc,
            Err((msg, reason)) => {
                errors.push(IngestEventError {
                    index: i,
                    message: msg,
                });
                reject_counts.increment(reason);
                continue;
            }
        };

        let mut obj = obj.clone();
        fill_defaults(&mut obj, defaults);

        let batch = batches.entry(svc).or_default();
        serde_json::to_writer(&mut batch.ndjson, &obj)
            .map_err(|e| ServerError::Internal(format!("failed to serialize event: {e}")))?;
        batch.ndjson.push(b'\n');
        batch.maps.push(obj);
    }

    Ok(ParsedEvents {
        batches,
        errors,
        reject_counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct test defaults for use in parse tests.
    fn test_defaults() -> IngestDefaults {
        IngestDefaults {
            timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
            host: "127.0.0.1".to_owned(),
        }
    }

    /// Total accepted event count across all service batches.
    fn total_accepted(parsed: &ParsedEvents) -> usize {
        parsed.batches.values().map(|b| b.maps.len()).sum()
    }

    /// Get the maps for a specific service, panicking if absent.
    fn service_maps<'a>(
        parsed: &'a ParsedEvents,
        svc: &str,
    ) -> &'a [serde_json::Map<String, serde_json::Value>] {
        &parsed
            .batches
            .get(svc)
            .unwrap_or_else(|| panic!("no batch for service '{svc}'"))
            .maps
    }

    // --- happy path tests ---

    #[test]
    fn parse_ndjson_format() {
        let data = br#"{"service":"nginx","message":"ok"}
{"service":"nginx","message":"error"}
"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.contains_key("nginx"));
        assert_eq!(total_accepted(&parsed), 2);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parse_json_array_format() {
        let data = br#"[{"service":"nginx","message":"ok"},{"service":"nginx","message":"error"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.contains_key("nginx"));
        assert_eq!(total_accepted(&parsed), 2);
        assert!(parsed.errors.is_empty());
        // WAL output should be ndjson, not a JSON array.
        let ndjson = &parsed.batches["nginx"].ndjson;
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
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.contains_key("test"));
        assert_eq!(total_accepted(&parsed), 2);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parse_accepts_valid_service_names() {
        let defaults = test_defaults();
        for name in [
            "nginx",
            "my-app",
            "app_v2",
            "host.name.prod",
            "A1-B2_c3.d",
            "Activity Monitor",
        ] {
            let data = format!(r#"{{"service":"{name}","message":"ok"}}"#);
            let parsed = parse_events(data.as_bytes(), &defaults).unwrap();
            assert!(parsed.batches.contains_key(name));
            assert_eq!(total_accepted(&parsed), 1);
            assert!(parsed.errors.is_empty());
        }
    }

    #[test]
    fn parse_ndjson_retains_maps() {
        let data = br#"{"service":"nginx","message":"hello","status":200}
{"service":"nginx","message":"world","status":404}
"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        let maps = service_maps(&parsed, "nginx");
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
    fn parse_json_array_retains_maps() {
        let data = br#"[{"service":"nginx","level":"error"},{"service":"nginx","level":"warn"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        let maps = service_maps(&parsed, "nginx");
        assert_eq!(maps.len(), 2);
        assert_eq!(
            maps[0].get("level").and_then(serde_json::Value::as_str),
            Some("error")
        );
        assert_eq!(
            maps[1].get("level").and_then(serde_json::Value::as_str),
            Some("warn")
        );
    }

    // --- batch-level errors (still Err, unchanged) ---

    #[test]
    fn parse_empty_json_array_rejected() {
        let data = b"[]";
        let err = parse_events(data, &test_defaults()).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    // --- per-event errors ---

    #[test]
    fn parse_missing_service() {
        let data = br#"{"message":"no service field"}"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("missing 'service'"));
    }

    #[test]
    fn parse_invalid_json() {
        let data = b"not json at all";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid JSON"));
    }

    #[test]
    fn parse_rejects_path_traversal_service() {
        let data = br#"{"service":"../../etc/passwd","message":"pwned"}"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    #[test]
    fn parse_rejects_slash_in_service() {
        let data = br#"{"service":"foo/bar","message":"nope"}"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    #[test]
    fn parse_rejects_empty_service_name() {
        let data = br#"{"service":"","message":"empty"}"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("cannot be empty"));
    }

    #[test]
    fn parse_rejects_long_service_name() {
        let name = "a".repeat(129);
        let data = format!(r#"{{"service":"{name}","message":"long"}}"#);
        let parsed = parse_events(data.as_bytes(), &test_defaults()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("too long"));
    }

    #[test]
    fn parse_json_array_rejects_bad_service() {
        let data = br#"[{"service":"../evil","message":"nope"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    // --- partial-success tests ---

    #[test]
    fn parse_partial_ndjson_bad_middle() {
        let data = b"{\"service\":\"nginx\",\"message\":\"one\"}\nnot json\n{\"service\":\"nginx\",\"message\":\"three\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(total_accepted(&parsed), 2);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 1);
        assert!(parsed.errors[0].message.contains("invalid JSON"));
    }

    #[test]
    fn parse_partial_array_non_object() {
        let data = br#"[{"service":"nginx","message":"ok"},"just a string",{"service":"nginx","message":"also ok"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(total_accepted(&parsed), 2);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 1);
        assert!(parsed.errors[0].message.contains("expected JSON object"));
    }

    #[test]
    fn parse_mixed_services_accepted() {
        let data = b"{\"service\":\"nginx\",\"message\":\"one\"}\n{\"service\":\"apache\",\"message\":\"two\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(total_accepted(&parsed), 2);
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.batches.len(), 2);
        assert!(parsed.batches.contains_key("nginx"));
        assert!(parsed.batches.contains_key("apache"));
        assert_eq!(service_maps(&parsed, "nginx").len(), 1);
        assert_eq!(service_maps(&parsed, "apache").len(), 1);
    }

    #[test]
    fn parse_first_event_bad_scans_forward() {
        let data = b"{\"message\":\"no service\"}\n{\"service\":\"nginx\",\"message\":\"good\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(total_accepted(&parsed), 1);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 0);
        assert!(parsed.errors[0].message.contains("missing 'service'"));
        assert!(parsed.batches.contains_key("nginx"));
    }

    #[test]
    fn parse_all_events_bad() {
        let data = b"not json\nalso not json\n{\"no_service\":true}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.batches.is_empty());
        assert_eq!(parsed.errors.len(), 3);
    }

    #[test]
    fn parse_bad_service_name_scans_forward() {
        let data = b"{\"service\":\"../../evil\",\"message\":\"bad\"}\n{\"service\":\"nginx\",\"message\":\"good\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(total_accepted(&parsed), 1);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 0);
        assert!(parsed.errors[0].message.contains("invalid characters"));
        assert!(parsed.batches.contains_key("nginx"));
    }

    #[test]
    fn parse_ndjson_mixed_validity() {
        // 5 lines: index 0 good, 1 bad json, 2 good, 3 missing service, 4 good
        let data = b"{\"service\":\"nginx\",\"message\":\"a\"}\nnot json\n{\"service\":\"nginx\",\"message\":\"c\"}\n{\"no_service\":true}\n{\"service\":\"nginx\",\"message\":\"e\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(total_accepted(&parsed), 3);
        assert_eq!(parsed.errors.len(), 2);
        assert_eq!(parsed.errors[0].index, 1);
        assert_eq!(parsed.errors[1].index, 3);
    }

    // --- defaults tests ---

    #[test]
    fn defaults_filled_when_missing_ndjson() {
        let data = br#"{"service":"test"}"#;
        let defaults = test_defaults();
        let parsed = parse_events(data, &defaults).unwrap();
        let maps = service_maps(&parsed, "test");
        let event = &maps[0];
        assert_eq!(
            event.get("timestamp").and_then(serde_json::Value::as_str),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(
            event.get("host").and_then(serde_json::Value::as_str),
            Some("127.0.0.1")
        );
        assert_eq!(
            event.get("message").and_then(serde_json::Value::as_str),
            Some("")
        );
    }

    #[test]
    fn defaults_filled_when_missing_json_array() {
        let data = br#"[{"service":"test"}]"#;
        let defaults = test_defaults();
        let parsed = parse_events(data, &defaults).unwrap();
        let maps = service_maps(&parsed, "test");
        let event = &maps[0];
        assert_eq!(
            event.get("timestamp").and_then(serde_json::Value::as_str),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(
            event.get("host").and_then(serde_json::Value::as_str),
            Some("127.0.0.1")
        );
        assert_eq!(
            event.get("message").and_then(serde_json::Value::as_str),
            Some("")
        );
    }

    #[test]
    fn defaults_not_overwritten_when_present() {
        let data = br#"{"service":"test","timestamp":"2025-06-01T12:00:00Z","host":"myhost","message":"hello"}"#;
        let defaults = test_defaults();
        let parsed = parse_events(data, &defaults).unwrap();
        let maps = service_maps(&parsed, "test");
        let event = &maps[0];
        assert_eq!(
            event.get("timestamp").and_then(serde_json::Value::as_str),
            Some("2025-06-01T12:00:00Z")
        );
        assert_eq!(
            event.get("host").and_then(serde_json::Value::as_str),
            Some("myhost")
        );
        assert_eq!(
            event.get("message").and_then(serde_json::Value::as_str),
            Some("hello")
        );
    }

    #[test]
    fn defaults_appear_in_wal_ndjson() {
        let data = br#"{"service":"test"}"#;
        let defaults = test_defaults();
        let parsed = parse_events(data, &defaults).unwrap();
        let ndjson = &parsed.batches["test"].ndjson;
        let wal_text = std::str::from_utf8(ndjson).unwrap();
        let wal_event: serde_json::Value = serde_json::from_str(wal_text.trim()).unwrap();
        assert_eq!(wal_event["timestamp"], "2026-01-01T00:00:00.000Z");
        assert_eq!(wal_event["host"], "127.0.0.1");
        assert_eq!(wal_event["message"], "");
    }

    // --- mixed-service tests (new) ---

    #[test]
    fn parse_three_services_ndjson() {
        let data = b"{\"service\":\"nginx\",\"message\":\"a\"}\n{\"service\":\"redis\",\"message\":\"b\"}\n{\"service\":\"postgres\",\"message\":\"c\"}\n{\"service\":\"nginx\",\"message\":\"d\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.batches.len(), 3);
        assert_eq!(total_accepted(&parsed), 4);
        assert!(parsed.errors.is_empty());
        // nginx gets 2 events, redis and postgres each get 1.
        assert_eq!(service_maps(&parsed, "nginx").len(), 2);
        assert_eq!(service_maps(&parsed, "redis").len(), 1);
        assert_eq!(service_maps(&parsed, "postgres").len(), 1);
    }

    #[test]
    fn parse_three_services_json_array() {
        let data = br#"[{"service":"a","m":"1"},{"service":"b","m":"2"},{"service":"c","m":"3"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.batches.len(), 3);
        assert_eq!(total_accepted(&parsed), 3);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parse_mixed_valid_invalid_across_services() {
        // nginx valid, bad json, apache valid, missing service, nginx valid
        let data = b"{\"service\":\"nginx\",\"message\":\"ok\"}\nnot json\n{\"service\":\"apache\",\"message\":\"ok\"}\n{\"no_svc\":true}\n{\"service\":\"nginx\",\"message\":\"ok2\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.batches.len(), 2);
        assert_eq!(total_accepted(&parsed), 3);
        assert_eq!(parsed.errors.len(), 2);
        assert_eq!(service_maps(&parsed, "nginx").len(), 2);
        assert_eq!(service_maps(&parsed, "apache").len(), 1);
    }

    #[test]
    fn parse_insertion_order_preserved() {
        let data = b"{\"service\":\"charlie\",\"message\":\"1\"}\n{\"service\":\"alpha\",\"message\":\"2\"}\n{\"service\":\"bravo\",\"message\":\"3\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        let keys: Vec<&String> = parsed.batches.keys().collect();
        assert_eq!(keys, &["charlie", "alpha", "bravo"]);
    }

    #[test]
    fn parse_ndjson_per_service_wal_bytes() {
        let data = b"{\"service\":\"a\",\"x\":1}\n{\"service\":\"b\",\"x\":2}\n{\"service\":\"a\",\"x\":3}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        // Each service batch's ndjson should only contain its own events.
        let a_lines: Vec<_> = std::str::from_utf8(&parsed.batches["a"].ndjson)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        let b_lines: Vec<_> = std::str::from_utf8(&parsed.batches["b"].ndjson)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(a_lines.len(), 2);
        assert_eq!(b_lines.len(), 1);
    }

    #[test]
    fn reject_counts_accuracy() {
        // 1 invalid json, 1 missing service, 1 empty service, 1 bad chars, 1 valid
        let data = b"not json\n{\"no_svc\":1}\n{\"service\":\"\",\"m\":\"x\"}\n{\"service\":\"a/b\",\"m\":\"x\"}\n{\"service\":\"ok\",\"m\":\"x\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.reject_counts.invalid_json, 1);
        assert_eq!(parsed.reject_counts.missing_service, 1);
        assert_eq!(parsed.reject_counts.empty_service, 1);
        assert_eq!(parsed.reject_counts.invalid_chars, 1);
        assert_eq!(parsed.reject_counts.total(), 4);
        assert_eq!(total_accepted(&parsed), 1);
    }
}
