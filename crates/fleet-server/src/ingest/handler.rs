//! HTTP handler for the `POST /api/v1/ingest` endpoint.

use std::io::Read as _;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use fleet_auth::keys::VerifiedKey;
use fleet_auth::roles::Permission;
use serde_json::json;

use crate::bus::{EventBus as _, IngestBatch};
use crate::error::ServerError;
use crate::state::AppState;
use fleet_api::{IngestEventError, IngestResponse};

/// Parsed ingest payload ready for WAL write and bus publishing.
#[derive(Debug)]
struct ParsedEvents {
    /// Service name extracted from the first valid event, or `None` if all
    /// events were rejected.
    service: Option<String>,
    /// Parsed event objects for in-memory consumers (hot buffer, streaming).
    maps: Vec<serde_json::Map<String, serde_json::Value>>,
    /// Serialized ndjson bytes for WAL write.
    ndjson: Vec<u8>,
    /// Per-event validation errors accumulated during parsing.
    errors: Vec<IngestEventError>,
}

/// Maximum service name length.
const MAX_SERVICE_NAME_LEN: usize = 128;

/// Validate a service name, returning a human-readable error string on failure.
///
/// Pure function used by per-event validation — no `ServerError` dependency.
/// Only alphanumeric, dash, underscore, and dot are allowed.
/// Prevents path traversal in WAL directory structure and log injection.
fn validate_service_name_str(service: &str) -> Result<(), String> {
    if service.is_empty() {
        return Err("service name cannot be empty".into());
    }
    if service.len() > MAX_SERVICE_NAME_LEN {
        return Err(format!(
            "service name too long ({} chars, max {MAX_SERVICE_NAME_LEN})",
            service.len()
        ));
    }
    if !service
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err("service name contains invalid characters \
             (only alphanumeric, dash, underscore, dot allowed)"
            .into());
    }
    Ok(())
}

/// Validate a single event object, returning the service name or an error string.
///
/// Checks: is object (caller guarantees), has `service` field, service passes
/// charset/length validation, service matches batch service (if established).
fn validate_event(
    obj: &serde_json::Map<String, serde_json::Value>,
    batch_service: Option<&str>,
) -> Result<String, String> {
    let svc = obj
        .get("service")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "missing 'service' field".to_owned())?;

    validate_service_name_str(svc)?;

    if let Some(expected) = batch_service
        && svc != expected
    {
        return Err(format!(
            "service mismatch: expected '{expected}', got '{svc}'"
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
    if !verified.role.has_permission(Permission::Ingest) {
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

    let (parsed, wal_path, body_bytes, ndjson_byte_size, decompress_ms, parse_ms, wal_ms) =
        tokio::task::spawn_blocking(move || -> Result<_, ServerError> {
            let t0 = std::time::Instant::now();
            let raw = if compressed {
                decompress_gzip(&body)?
            } else {
                body.to_vec()
            };
            let decompress_ms = t0.elapsed().as_millis();
            let body_bytes = raw.len();

            if raw.is_empty() {
                return Err(ServerError::Ingest("empty request body".into()));
            }

            let t1 = std::time::Instant::now();
            let parsed = parse_events(&raw, &defaults)?;
            let parse_ms = t1.elapsed().as_millis();
            let ndjson_byte_size = parsed.ndjson.len();

            // Only write to WAL if we have accepted events.
            let wal_path = if let Some(ref svc) = parsed.service {
                let t2 = std::time::Instant::now();
                let path = wal_writer
                    .write(svc, &parsed.ndjson)
                    .map_err(|e| ServerError::Internal(format!("WAL write failed: {e}")))?;
                let wal_ms_inner = t2.elapsed().as_millis();
                Some((path, wal_ms_inner))
            } else {
                None
            };

            let (wal_path, wal_ms) = match wal_path {
                Some((path, ms)) => (Some(path), ms),
                None => (None, 0),
            };

            Ok((
                parsed,
                wal_path,
                body_bytes,
                ndjson_byte_size,
                decompress_ms,
                parse_ms,
                wal_ms,
            ))
        })
        .await
        .map_err(|e| ServerError::Internal(format!("ingest task panicked: {e}")))??;

    let result = finalize_ingest(
        &state,
        parsed,
        wal_path.as_ref(),
        ndjson_byte_size,
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
    parsed: ParsedEvents,
    wal_path: Option<&std::path::PathBuf>,
    ndjson_byte_size: usize,
    verified: &VerifiedKey,
    wire_bytes: usize,
    compressed: bool,
    body_bytes: usize,
    decompress_ms: u128,
    parse_ms: u128,
    wal_ms: u128,
) -> IngestResponse {
    let accepted = parsed.maps.len();
    let rejected = parsed.errors.len();

    metrics::counter!(crate::metrics::INGEST_EVENTS_TOTAL).increment(accepted as u64);
    state
        .ingest
        .total_events
        .fetch_add(accepted as u64, std::sync::atomic::Ordering::Relaxed);
    if rejected > 0 {
        metrics::counter!(crate::metrics::INGEST_EVENTS_REJECTED_TOTAL).increment(rejected as u64);
        state
            .ingest
            .total_rejected
            .fetch_add(rejected as u64, std::sync::atomic::Ordering::Relaxed);
    }

    // Publish to event bus (best-effort — WAL is the durability guarantee).
    if let (Some(bus), Some(wal_path)) = (&state.ingest.event_bus, wal_path) {
        let batch_id: Arc<str> = wal_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .into();
        let service = parsed.service.as_deref().unwrap_or("unknown");
        let batch = Arc::new(IngestBatch {
            batch_id,
            service: Arc::from(service),
            byte_size: ndjson_byte_size,
            events: parsed.maps,
            draining: std::sync::atomic::AtomicBool::new(false),
        });
        let subscribers = bus.publish(batch);
        tracing::debug!(subscribers, "published batch to event bus");
    }

    let duration_ms = decompress_ms + parse_ms + wal_ms;
    let ingest_service = parsed.service.as_deref().unwrap_or("<none>");
    tracing::info!(
        event_type = "ingest_complete",
        user = %verified.name,
        ingest_service,
        accepted,
        rejected,
        body_bytes,
        wire_bytes,
        compressed,
        duration_ms,
        decompress_ms,
        parse_ms,
        wal_ms,
        path = wal_path.as_ref().map_or("<skipped>", |p| p.to_str().unwrap_or("<non-utf8>")),
        "ingested events to WAL"
    );

    IngestResponse {
        accepted,
        rejected,
        errors: parsed.errors,
    }
}

/// Check if the request body is gzip-encoded.
fn is_gzip(headers: &HeaderMap) -> bool {
    headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"))
}

/// Decompress gzip body.
fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, ServerError> {
    let mut decoder = flate2::read::GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| ServerError::Ingest(format!("gzip decompression failed: {e}")))?;
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
/// Converts to ndjson for WAL storage and retains parsed maps for the bus.
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

    let mut service: Option<String> = None;
    let mut maps = Vec::with_capacity(arr.len());
    let mut ndjson = Vec::new();
    let mut errors = Vec::new();

    for (i, event) in arr.iter().enumerate() {
        let Some(obj) = event.as_object() else {
            errors.push(IngestEventError {
                index: i,
                message: "expected JSON object".into(),
            });
            continue;
        };

        match validate_event(obj, service.as_deref()) {
            Ok(svc) => {
                if service.is_none() {
                    service = Some(svc);
                }
            }
            Err(msg) => {
                errors.push(IngestEventError {
                    index: i,
                    message: msg,
                });
                continue;
            }
        }

        let mut obj = obj.clone();
        fill_defaults(&mut obj, defaults);

        serde_json::to_writer(&mut ndjson, &obj)
            .map_err(|e| ServerError::Internal(format!("failed to serialize event: {e}")))?;
        ndjson.push(b'\n');

        maps.push(obj);
    }

    Ok(ParsedEvents {
        service,
        maps,
        ndjson,
        errors,
    })
}

/// Parse ndjson (newline-delimited JSON objects).
///
/// Retains parsed maps for the bus and re-serializes to ndjson for
/// consistent WAL bytes (trimmed, one object per line).  Invalid lines
/// are accumulated as per-event errors rather than aborting the batch.
fn parse_ndjson(text: &str, defaults: &IngestDefaults) -> Result<ParsedEvents, ServerError> {
    let mut service: Option<String> = None;
    let mut maps = Vec::new();
    let mut ndjson = Vec::new();
    let mut errors = Vec::new();

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
                continue;
            }
        };

        let Some(obj) = parsed.as_object() else {
            errors.push(IngestEventError {
                index: i,
                message: "expected JSON object".into(),
            });
            continue;
        };

        match validate_event(obj, service.as_deref()) {
            Ok(svc) => {
                if service.is_none() {
                    service = Some(svc);
                }
            }
            Err(msg) => {
                errors.push(IngestEventError {
                    index: i,
                    message: msg,
                });
                continue;
            }
        }

        let mut obj = obj.clone();
        fill_defaults(&mut obj, defaults);

        serde_json::to_writer(&mut ndjson, &obj)
            .map_err(|e| ServerError::Internal(format!("failed to serialize event: {e}")))?;
        ndjson.push(b'\n');

        maps.push(obj);
    }

    Ok(ParsedEvents {
        service,
        maps,
        ndjson,
        errors,
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

    // --- happy path tests (unchanged semantics) ---

    #[test]
    fn parse_ndjson_format() {
        let data = br#"{"service":"nginx","message":"ok"}
{"service":"nginx","message":"error"}
"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.service.as_deref(), Some("nginx"));
        assert_eq!(parsed.maps.len(), 2);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parse_json_array_format() {
        let data = br#"[{"service":"nginx","message":"ok"},{"service":"nginx","message":"error"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.service.as_deref(), Some("nginx"));
        assert_eq!(parsed.maps.len(), 2);
        assert!(parsed.errors.is_empty());
        // WAL output should be ndjson, not a JSON array.
        let text = std::str::from_utf8(&parsed.ndjson).unwrap();
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
        assert_eq!(parsed.service.as_deref(), Some("test"));
        assert_eq!(parsed.maps.len(), 2);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parse_accepts_valid_service_names() {
        let defaults = test_defaults();
        for name in ["nginx", "my-app", "app_v2", "host.name.prod", "A1-B2_c3.d"] {
            let data = format!(r#"{{"service":"{name}","message":"ok"}}"#);
            let parsed = parse_events(data.as_bytes(), &defaults).unwrap();
            assert_eq!(parsed.service.as_deref(), Some(name));
            assert_eq!(parsed.maps.len(), 1);
            assert!(parsed.errors.is_empty());
        }
    }

    #[test]
    fn parse_ndjson_retains_maps() {
        let data = br#"{"service":"nginx","message":"hello","status":200}
{"service":"nginx","message":"world","status":404}
"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.maps.len(), 2);
        assert_eq!(
            parsed.maps[0].get("message").and_then(|v| v.as_str()),
            Some("hello")
        );
        assert_eq!(
            parsed.maps[1]
                .get("status")
                .and_then(serde_json::Value::as_u64),
            Some(404)
        );
    }

    #[test]
    fn parse_json_array_retains_maps() {
        let data = br#"[{"service":"nginx","level":"error"},{"service":"nginx","level":"warn"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.maps.len(), 2);
        assert_eq!(
            parsed.maps[0].get("level").and_then(|v| v.as_str()),
            Some("error")
        );
        assert_eq!(
            parsed.maps[1].get("level").and_then(|v| v.as_str()),
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

    // --- per-event errors (now Ok with errors, changed from Err) ---

    #[test]
    fn parse_missing_service() {
        let data = br#"{"message":"no service field"}"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.maps.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("missing 'service'"));
        assert!(parsed.service.is_none());
    }

    #[test]
    fn parse_invalid_json() {
        let data = b"not json at all";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.maps.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid JSON"));
        assert!(parsed.service.is_none());
    }

    #[test]
    fn parse_rejects_path_traversal_service() {
        let data = br#"{"service":"../../etc/passwd","message":"pwned"}"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.maps.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    #[test]
    fn parse_rejects_slash_in_service() {
        let data = br#"{"service":"foo/bar","message":"nope"}"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.maps.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    #[test]
    fn parse_rejects_empty_service_name() {
        let data = br#"{"service":"","message":"empty"}"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.maps.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("cannot be empty"));
    }

    #[test]
    fn parse_rejects_long_service_name() {
        let name = "a".repeat(129);
        let data = format!(r#"{{"service":"{name}","message":"long"}}"#);
        let parsed = parse_events(data.as_bytes(), &test_defaults()).unwrap();
        assert!(parsed.maps.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("too long"));
    }

    #[test]
    fn parse_json_array_rejects_bad_service() {
        let data = br#"[{"service":"../evil","message":"nope"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.maps.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("invalid characters"));
    }

    // --- new partial-success tests ---

    #[test]
    fn parse_partial_ndjson_bad_middle() {
        let data = b"{\"service\":\"nginx\",\"message\":\"one\"}\nnot json\n{\"service\":\"nginx\",\"message\":\"three\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.maps.len(), 2);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 1);
        assert!(parsed.errors[0].message.contains("invalid JSON"));
        assert_eq!(parsed.service.as_deref(), Some("nginx"));
    }

    #[test]
    fn parse_partial_array_non_object() {
        let data = br#"[{"service":"nginx","message":"ok"},"just a string",{"service":"nginx","message":"also ok"}]"#;
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.maps.len(), 2);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 1);
        assert!(parsed.errors[0].message.contains("expected JSON object"));
    }

    #[test]
    fn parse_service_mismatch_rejected() {
        let data = b"{\"service\":\"nginx\",\"message\":\"one\"}\n{\"service\":\"apache\",\"message\":\"two\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.maps.len(), 1);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 1);
        assert!(parsed.errors[0].message.contains("service mismatch"));
        assert_eq!(parsed.service.as_deref(), Some("nginx"));
    }

    #[test]
    fn parse_first_event_bad_scans_forward() {
        let data = b"{\"message\":\"no service\"}\n{\"service\":\"nginx\",\"message\":\"good\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.maps.len(), 1);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 0);
        assert!(parsed.errors[0].message.contains("missing 'service'"));
        assert_eq!(parsed.service.as_deref(), Some("nginx"));
    }

    #[test]
    fn parse_all_events_bad() {
        let data = b"not json\nalso not json\n{\"no_service\":true}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert!(parsed.maps.is_empty());
        assert_eq!(parsed.errors.len(), 3);
        assert!(parsed.service.is_none());
    }

    #[test]
    fn parse_bad_service_name_scans_forward() {
        let data = b"{\"service\":\"../../evil\",\"message\":\"bad\"}\n{\"service\":\"nginx\",\"message\":\"good\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.maps.len(), 1);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].index, 0);
        assert!(parsed.errors[0].message.contains("invalid characters"));
        assert_eq!(parsed.service.as_deref(), Some("nginx"));
    }

    #[test]
    fn parse_ndjson_mixed_validity() {
        // 5 lines: index 0 good, 1 bad json, 2 good, 3 missing service, 4 good
        let data = b"{\"service\":\"nginx\",\"message\":\"a\"}\nnot json\n{\"service\":\"nginx\",\"message\":\"c\"}\n{\"no_service\":true}\n{\"service\":\"nginx\",\"message\":\"e\"}";
        let parsed = parse_events(data, &test_defaults()).unwrap();
        assert_eq!(parsed.maps.len(), 3);
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
        let event = &parsed.maps[0];
        assert_eq!(
            event.get("timestamp").and_then(|v| v.as_str()),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(
            event.get("host").and_then(|v| v.as_str()),
            Some("127.0.0.1")
        );
        assert_eq!(event.get("message").and_then(|v| v.as_str()), Some(""));
    }

    #[test]
    fn defaults_filled_when_missing_json_array() {
        let data = br#"[{"service":"test"}]"#;
        let defaults = test_defaults();
        let parsed = parse_events(data, &defaults).unwrap();
        let event = &parsed.maps[0];
        assert_eq!(
            event.get("timestamp").and_then(|v| v.as_str()),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(
            event.get("host").and_then(|v| v.as_str()),
            Some("127.0.0.1")
        );
        assert_eq!(event.get("message").and_then(|v| v.as_str()), Some(""));
    }

    #[test]
    fn defaults_not_overwritten_when_present() {
        let data = br#"{"service":"test","timestamp":"2025-06-01T12:00:00Z","host":"myhost","message":"hello"}"#;
        let defaults = test_defaults();
        let parsed = parse_events(data, &defaults).unwrap();
        let event = &parsed.maps[0];
        assert_eq!(
            event.get("timestamp").and_then(|v| v.as_str()),
            Some("2025-06-01T12:00:00Z")
        );
        assert_eq!(event.get("host").and_then(|v| v.as_str()), Some("myhost"));
        assert_eq!(event.get("message").and_then(|v| v.as_str()), Some("hello"));
    }

    #[test]
    fn defaults_appear_in_wal_ndjson() {
        let data = br#"{"service":"test"}"#;
        let defaults = test_defaults();
        let parsed = parse_events(data, &defaults).unwrap();
        let wal_text = std::str::from_utf8(&parsed.ndjson).unwrap();
        let wal_event: serde_json::Value = serde_json::from_str(wal_text.trim()).unwrap();
        assert_eq!(wal_event["timestamp"], "2026-01-01T00:00:00.000Z");
        assert_eq!(wal_event["host"], "127.0.0.1");
        assert_eq!(wal_event["message"], "");
    }
}
