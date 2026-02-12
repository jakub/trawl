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

use crate::error::ServerError;
use crate::ingest::types::IngestResponse;
use crate::state::AppState;

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
        .wal_writer
        .as_ref()
        .ok_or_else(|| ServerError::Internal("ingest not enabled".into()))?;

    // Decompress gzip if Content-Encoding header is set.
    let raw = if is_gzip(&headers) {
        decompress_gzip(&body)?
    } else {
        body.to_vec()
    };

    if raw.is_empty() {
        return Err(ServerError::Ingest("empty request body".into()));
    }

    // Validate ndjson and extract service name from first line.
    let (service, line_count) = validate_ndjson(&raw)?;

    // Write to WAL atomically.
    let service_clone = service.clone();
    let wal_path = tokio::task::spawn_blocking({
        let wal_writer = Arc::clone(wal_writer);
        move || wal_writer.write(&service_clone, &raw)
    })
    .await
    .map_err(|e| ServerError::Internal(format!("WAL write task panicked: {e}")))?
    .map_err(|e| ServerError::Internal(format!("WAL write failed: {e}")))?;

    tracing::info!(
        user = %verified.name,
        service = %service,
        events = line_count,
        path = %wal_path.display(),
        "ingested events to WAL"
    );

    Ok(Json(IngestResponse {
        accepted: line_count,
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

/// Validate ndjson: each line must parse as JSON with a `service` field.
///
/// Returns the service name (from the first line) and total line count.
fn validate_ndjson(data: &[u8]) -> Result<(String, usize), ServerError> {
    let text = std::str::from_utf8(data)
        .map_err(|e| ServerError::Ingest(format!("body is not valid UTF-8: {e}")))?;

    let mut service: Option<String> = None;
    let mut count = 0;

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
            service = Some(svc.to_owned());
        }

        count += 1;
    }

    let service = service.ok_or_else(|| ServerError::Ingest("no valid events in body".into()))?;
    Ok((service, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ndjson_valid() {
        let data = br#"{"service":"nginx","message":"ok"}
{"service":"nginx","message":"error"}
"#;
        let (service, count) = validate_ndjson(data).unwrap();
        assert_eq!(service, "nginx");
        assert_eq!(count, 2);
    }

    #[test]
    fn validate_ndjson_missing_service() {
        let data = br#"{"message":"no service field"}"#;
        let err = validate_ndjson(data).unwrap_err();
        assert!(err.to_string().contains("missing 'service'"));
    }

    #[test]
    fn validate_ndjson_invalid_json() {
        let data = b"not json at all";
        let err = validate_ndjson(data).unwrap_err();
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[test]
    fn validate_ndjson_empty_lines_skipped() {
        let data = br#"
{"service":"test","message":"hello"}

{"service":"test","message":"world"}
"#;
        let (service, count) = validate_ndjson(data).unwrap();
        assert_eq!(service, "test");
        assert_eq!(count, 2);
    }
}
