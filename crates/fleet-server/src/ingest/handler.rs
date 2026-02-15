//! HTTP handler for the `POST /api/v1/ingest` endpoint.

use std::io::Read as _;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use fleet_auth::keys::VerifiedKey;
use fleet_auth::roles::Permission;

use crate::bus::{EventBus as _, IngestBatch};
use crate::error::ServerError;
use crate::state::AppState;
use fleet_api::IngestResponse;

/// Parsed ingest payload ready for WAL write and bus publishing.
#[derive(Debug)]
struct ParsedEvents {
    /// Service name extracted from events.
    service: String,
    /// Parsed event objects for in-memory consumers (hot buffer, streaming).
    maps: Vec<serde_json::Map<String, serde_json::Value>>,
    /// Serialized ndjson bytes for WAL write.
    ndjson: Vec<u8>,
}

/// Maximum service name length.
const MAX_SERVICE_NAME_LEN: usize = 128;

/// Validate a service name from an ingest event.
///
/// Only alphanumeric, dash, underscore, and dot are allowed.
/// Prevents path traversal in WAL directory structure and log injection.
fn validate_service_name(service: &str) -> Result<(), ServerError> {
    if service.is_empty() {
        return Err(ServerError::Ingest("service name cannot be empty".into()));
    }
    if service.len() > MAX_SERVICE_NAME_LEN {
        return Err(ServerError::Ingest(format!(
            "service name too long ({} chars, max {MAX_SERVICE_NAME_LEN})",
            service.len()
        )));
    }
    if !service
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(ServerError::Ingest(
            "service name contains invalid characters \
             (only alphanumeric, dash, underscore, dot allowed)"
                .into(),
        ));
    }
    Ok(())
}

/// `POST /api/v1/ingest` — accept ndjson events into the WAL.
///
/// Expects `Content-Type: application/x-ndjson` (or `application/json`).
/// Supports `Content-Encoding: gzip` for compressed payloads.
pub async fn ingest(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
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

    // Decompress gzip if Content-Encoding header is set.
    let compressed = is_gzip(&headers);
    let wire_bytes = body.len();
    let raw = if compressed {
        decompress_gzip(&body)?
    } else {
        body.to_vec()
    };
    let body_bytes = raw.len();

    if raw.is_empty() {
        return Err(ServerError::Ingest("empty request body".into()));
    }

    // Parse events from either ndjson or JSON array format.
    // Always produces ndjson bytes for the WAL regardless of input format.
    let parsed = parse_events(&raw)?;
    let event_count = parsed.maps.len();

    // Write ndjson to WAL atomically.
    let service_clone = parsed.service.clone();
    let ndjson = parsed.ndjson;
    let wal_path = tokio::task::spawn_blocking({
        let wal_writer = Arc::clone(wal_writer);
        move || wal_writer.write(&service_clone, &ndjson)
    })
    .await
    .map_err(|e| ServerError::Internal(format!("WAL write task panicked: {e}")))?
    .map_err(|e| ServerError::Internal(format!("WAL write failed: {e}")))?;

    // Publish to event bus (best-effort — WAL is the durability guarantee).
    if let Some(bus) = &state.ingest.event_bus {
        let batch_id: Arc<str> = wal_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .into();
        let batch = Arc::new(IngestBatch {
            batch_id,
            service: Arc::from(parsed.service.as_str()),
            events: parsed.maps,
        });
        let subscribers = bus.publish(batch);
        tracing::debug!(subscribers, "published batch to event bus");
    }

    tracing::info!(
        event_type = "ingest_complete",
        user = %verified.name,
        ingest_service = %parsed.service,
        events = event_count,
        body_bytes,
        wire_bytes,
        compressed,
        path = %wal_path.display(),
        "ingested events to WAL"
    );

    Ok(Json(IngestResponse {
        accepted: event_count,
    }))
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
fn parse_events(data: &[u8]) -> Result<ParsedEvents, ServerError> {
    let text = std::str::from_utf8(data)
        .map_err(|e| ServerError::Ingest(format!("body is not valid UTF-8: {e}")))?;

    let trimmed = text.trim_start();

    // Detect format: JSON array (vector batches) vs ndjson (line-delimited).
    if trimmed.starts_with('[') {
        parse_json_array(trimmed)
    } else {
        parse_ndjson(trimmed)
    }
}

/// Parse a JSON array of events (vector's default batch format).
///
/// Converts to ndjson for WAL storage and retains parsed maps for the bus.
fn parse_json_array(text: &str) -> Result<ParsedEvents, ServerError> {
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

    for (i, event) in arr.iter().enumerate() {
        let obj = event
            .as_object()
            .ok_or_else(|| ServerError::Ingest(format!("event {}: expected JSON object", i + 1)))?;

        let svc = obj
            .get("service")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ServerError::Ingest(format!("event {}: missing 'service' field", i + 1))
            })?;

        if service.is_none() {
            validate_service_name(svc)?;
            service = Some(svc.to_owned());
        }

        maps.push(obj.clone());

        // Write each event as a ndjson line.
        serde_json::to_writer(&mut ndjson, event)
            .map_err(|e| ServerError::Internal(format!("failed to serialize event: {e}")))?;
        ndjson.push(b'\n');
    }

    let service = service.expect("non-empty array guarantees at least one service");
    Ok(ParsedEvents {
        service,
        maps,
        ndjson,
    })
}

/// Parse ndjson (newline-delimited JSON objects).
///
/// Retains parsed maps for the bus and re-serializes to ndjson for
/// consistent WAL bytes (trimmed, one object per line).
fn parse_ndjson(text: &str) -> Result<ParsedEvents, ServerError> {
    let mut service: Option<String> = None;
    let mut maps = Vec::new();
    let mut ndjson = Vec::new();

    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parsed: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| ServerError::Ingest(format!("line {}: invalid JSON: {e}", i + 1)))?;

        let obj = parsed
            .as_object()
            .ok_or_else(|| ServerError::Ingest(format!("line {}: expected JSON object", i + 1)))?;

        let svc = obj
            .get("service")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ServerError::Ingest(format!("line {}: missing 'service' field", i + 1))
            })?;

        if service.is_none() {
            validate_service_name(svc)?;
            service = Some(svc.to_owned());
        }

        maps.push(obj.clone());

        // Re-serialize for consistent ndjson in WAL.
        serde_json::to_writer(&mut ndjson, &parsed)
            .map_err(|e| ServerError::Internal(format!("failed to serialize event: {e}")))?;
        ndjson.push(b'\n');
    }

    let service = service.ok_or_else(|| ServerError::Ingest("no valid events in body".into()))?;
    Ok(ParsedEvents {
        service,
        maps,
        ndjson,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ndjson_format() {
        let data = br#"{"service":"nginx","message":"ok"}
{"service":"nginx","message":"error"}
"#;
        let parsed = parse_events(data).unwrap();
        assert_eq!(parsed.service, "nginx");
        assert_eq!(parsed.maps.len(), 2);
    }

    #[test]
    fn parse_json_array_format() {
        let data = br#"[{"service":"nginx","message":"ok"},{"service":"nginx","message":"error"}]"#;
        let parsed = parse_events(data).unwrap();
        assert_eq!(parsed.service, "nginx");
        assert_eq!(parsed.maps.len(), 2);
        // WAL output should be ndjson, not a JSON array.
        let text = std::str::from_utf8(&parsed.ndjson).unwrap();
        assert!(!text.starts_with('['));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn parse_missing_service() {
        let data = br#"{"message":"no service field"}"#;
        let err = parse_events(data).unwrap_err();
        assert!(err.to_string().contains("missing 'service'"));
    }

    #[test]
    fn parse_invalid_json() {
        let data = b"not json at all";
        let err = parse_events(data).unwrap_err();
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[test]
    fn parse_ndjson_empty_lines_skipped() {
        let data = br#"
{"service":"test","message":"hello"}

{"service":"test","message":"world"}
"#;
        let parsed = parse_events(data).unwrap();
        assert_eq!(parsed.service, "test");
        assert_eq!(parsed.maps.len(), 2);
    }

    #[test]
    fn parse_empty_json_array_rejected() {
        let data = b"[]";
        let err = parse_events(data).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn parse_rejects_path_traversal_service() {
        let data = br#"{"service":"../../etc/passwd","message":"pwned"}"#;
        let err = parse_events(data).unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn parse_rejects_slash_in_service() {
        let data = br#"{"service":"foo/bar","message":"nope"}"#;
        let err = parse_events(data).unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn parse_rejects_empty_service_name() {
        let data = br#"{"service":"","message":"empty"}"#;
        let err = parse_events(data).unwrap_err();
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[test]
    fn parse_rejects_long_service_name() {
        let name = "a".repeat(129);
        let data = format!(r#"{{"service":"{name}","message":"long"}}"#);
        let err = parse_events(data.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("too long"));
    }

    #[test]
    fn parse_accepts_valid_service_names() {
        for name in ["nginx", "my-app", "app_v2", "host.name.prod", "A1-B2_c3.d"] {
            let data = format!(r#"{{"service":"{name}","message":"ok"}}"#);
            let parsed = parse_events(data.as_bytes()).unwrap();
            assert_eq!(parsed.service, name);
            assert_eq!(parsed.maps.len(), 1);
        }
    }

    #[test]
    fn parse_json_array_rejects_bad_service() {
        let data = br#"[{"service":"../evil","message":"nope"}]"#;
        let err = parse_events(data).unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn parse_ndjson_retains_maps() {
        let data = br#"{"service":"nginx","message":"hello","status":200}
{"service":"nginx","message":"world","status":404}
"#;
        let parsed = parse_events(data).unwrap();
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
        let parsed = parse_events(data).unwrap();
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
}
