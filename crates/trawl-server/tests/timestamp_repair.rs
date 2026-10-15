// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for malformed-time handling (ADR-0008): every bad-time
//! variant ingests through the real handler, is queryable from the hot
//! buffer, compacts to parquet without wedging, and stays queryable, with
//! `_time` set to the arrival time, the original value findable in `_raw`,
//! and `_repairs` naming why (ADR-0013, 2026-10-14 amendment):
//! `time.unparseable` when the first present source failed to parse, a
//! JSON `null` included, and `time.from_ingest` when no source was
//! present. Canonicalization of a valid timestamp is covered at the API
//! level too.

mod common;

use std::time::Duration;

use common::{setup, setup_in_dir_with_data};
use serde_json::json;
use trawl_api::value::Value;
use trawl_client::HttpClient;
use trawl_server::config::RateLimitConfig;

fn first_row_string(result: &trawl_api::value::QueryResult, column: &str) -> Option<String> {
    let idx = result.columns.iter().position(|c| c.name == column)?;
    match result.rows.first()?.get(idx)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(format!("{other:?}")),
    }
}

/// The four unparseable trigger variants, a `_time: null` that hides a
/// valid `timestamp`, and an event with no time source at all: ingested
/// via the handler, visible in the hot buffer, compacted without errors,
/// and returned by a `last=1h` query afterwards — original preserved,
/// timestamp at arrival, and the code telling the two causes apart.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // the sqlx macro used to hide the body in an inner fn
async fn trigger_variants_survive_ingest_compact_query() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let wal_dir = root.join("wal");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_path = data_dir.to_str().unwrap().to_owned();

    let server = setup_in_dir_with_data(&root, data_path, RateLimitConfig::default()).await;
    // Leak the tempdir so it survives the server (cleaned up by OS).
    std::mem::forget(tmp);

    let ingest_client = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let query_client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // (time fields, the repair code, a substring of the original that
    // must be findable in _raw)
    let variants: Vec<(serde_json::Value, &str, &str)> = vec![
        (
            json!({"timestamp": "not-a-date"}),
            "time.unparseable",
            "not-a-date",
        ),
        (
            json!({"timestamp": "2026-13-45T99:99:99Z"}),
            "time.unparseable",
            "2026-13-45T99:99:99Z",
        ),
        (
            json!({"timestamp": {"nested": 1}}),
            "time.unparseable",
            r#"{"nested":1}"#,
        ),
        (json!({"timestamp": 12345}), "time.unparseable", "12345"),
        // A present null claims the derivation and hides the valid
        // lower-precedence source, which stays an ordinary column.
        (
            json!({"_time": null, "timestamp": "2026-01-01T00:00:00Z"}),
            "time.unparseable",
            r#""_time":null"#,
        ),
        // No source at all.
        (json!({}), "time.from_ingest", "variant 5"),
    ];

    let before_ingest = chrono::Utc::now() - chrono::Duration::minutes(1);
    for (i, (time_fields, _, _)) in variants.iter().enumerate() {
        let mut record = json!({
            "service": format!("svc{i}"),
            "env": "prod",
            "host": "web01",
            "message": format!("variant {i}"),
        });
        let fields = record.as_object_mut().unwrap();
        for (k, v) in time_fields.as_object().unwrap() {
            fields.insert(k.clone(), v.clone());
        }
        let resp = ingest_client.ingest(&[record]).await.expect("ingest ok");
        assert_eq!(resp.accepted, 1, "variant {i} must be accepted");
        assert_eq!(resp.rejected, 0, "variant {i} must not be rejected");
    }
    let after_ingest = chrono::Utc::now() + chrono::Duration::minutes(1);

    // Phase 1: hot buffer, visible to a last=1h query before compaction.
    for (i, (bad, code, preserved)) in variants.iter().enumerate() {
        let result = query_client
            .query_paginated(&format!("service=svc{i} last=1h"), None, None)
            .await
            .unwrap_or_else(|e| panic!("hot query for variant {bad} failed: {e}"));
        assert_eq!(
            result.result.row_count(),
            1,
            "variant {bad} must be hot-queryable within last=1h"
        );
        assert_eq!(
            first_row_string(&result.result, "_repairs").as_deref(),
            Some(*code),
            "variant {bad}: the repair must be recorded (hot)"
        );
        let raw = first_row_string(&result.result, "_raw")
            .unwrap_or_else(|| panic!("variant {bad}: _raw present (hot)"));
        assert!(
            raw.contains(preserved),
            "variant {bad}: original must be findable in _raw (hot): {raw}"
        );
    }

    // Phase 2: compaction tick over the server's WAL — no errors, WAL drains.
    let hot_buffer = server
        .state
        .query
        .hot_buffer
        .as_ref()
        .expect("ingest-enabled server has a hot buffer");
    let errors = trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO,
        false,
        Some(hot_buffer),
        500,
        "2GB",
        None,
    )
    .await
    .expect("compaction tick must succeed");
    assert_eq!(errors, 0, "no compaction errors for repaired timestamps");
    assert_eq!(hot_buffer.event_count(), 0, "hot buffer drained");

    let leftover: Vec<_> = std::fs::read_dir(wal_dir.join("prod"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "ndjson"))
        .collect();
    assert!(leftover.is_empty(), "WAL directory must be empty");

    // Phase 3: parquet — still returned by a last=1h query, original still
    // preserved, timestamp at arrival time.
    for (i, (bad, code, preserved)) in variants.iter().enumerate() {
        let result = query_client
            .query_paginated(&format!("service=svc{i} last=1h"), None, None)
            .await
            .unwrap_or_else(|e| panic!("post-compaction query for variant {bad} failed: {e}"));
        assert_eq!(
            result.result.row_count(),
            1,
            "variant {bad} must remain queryable from parquet within last=1h"
        );
        assert_eq!(
            first_row_string(&result.result, "_repairs").as_deref(),
            Some(*code),
            "variant {bad}: the repair must be recorded (parquet)"
        );
        let raw = first_row_string(&result.result, "_raw")
            .unwrap_or_else(|| panic!("variant {bad}: _raw present (parquet)"));
        assert!(
            raw.contains(preserved),
            "variant {bad}: original must be findable in _raw (parquet): {raw}"
        );
        let ts = first_row_string(&result.result, "_time")
            .unwrap_or_else(|| panic!("variant {bad}: _time column present"));
        // Engine formats timestamps as "YYYY-MM-DD HH:MM:SS[.frac]" (UTC).
        let parsed = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%d %H:%M:%S%.f")
            .unwrap_or_else(|e| panic!("variant {bad}: timestamp {ts} must parse: {e}"))
            .and_utc();
        assert!(
            parsed > before_ingest && parsed < after_ingest,
            "variant {bad}: _time must be the arrival time, got {ts}"
        );
    }
}

/// Canonicalization acceptance criterion at the API level: an RFC 3339
/// value with a +05:30 offset lands as the same instant in UTC with
/// sub-second precision preserved, and an offset-less ISO 8601 value keeps
/// its event time instead of being repaired to arrival time.
#[tokio::test(flavor = "multi_thread")]
async fn offset_timestamp_canonicalized_to_utc_instant() {
    let server = setup().await;
    let ingest_client = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let query_client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let record = json!({
        "service": "canon",
        "env": "prod",
        "host": "web01",
        "timestamp": "2026-01-01T10:00:00.123456+05:30",
        "message": "offset input",
    });
    let resp = ingest_client.ingest(&[record]).await.expect("ingest ok");
    assert_eq!(resp.accepted, 1);

    let result = query_client
        .query_paginated("service=canon", None, None)
        .await
        .expect("query ok");
    assert_eq!(result.result.row_count(), 1);
    assert_eq!(
        first_row_string(&result.result, "_time").as_deref(),
        Some("2026-01-01 04:30:00.123456"),
        "the +05:30 instant must land as UTC with microsecond precision"
    );
    assert_eq!(
        first_row_string(&result.result, "_repairs"),
        None,
        "a valid offset timestamp is canonicalized, not repaired"
    );

    // Offset-less ISO 8601 (Python `datetime.isoformat()`, Java
    // `LocalDateTime`) is read as UTC, not demoted to arrival time.
    let record = json!({
        "service": "canon-naive",
        "env": "prod",
        "host": "web01",
        "timestamp": "2026-01-01T04:30:00.123456",
        "message": "offset-less input",
    });
    let resp = ingest_client.ingest(&[record]).await.expect("ingest ok");
    assert_eq!(resp.accepted, 1);

    let result = query_client
        .query_paginated("service=canon-naive", None, None)
        .await
        .expect("query ok");
    assert_eq!(result.result.row_count(), 1);
    assert_eq!(
        first_row_string(&result.result, "_time").as_deref(),
        Some("2026-01-01 04:30:00.123456"),
        "an offset-less ISO 8601 value must keep its event time, read as UTC"
    );
    assert_eq!(
        first_row_string(&result.result, "_repairs"),
        None,
        "an offset-less ISO 8601 value is canonicalized, not repaired"
    );
}
