// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end HTTPS API tests for trawld.
//!
//! Starts a real TLS server on a random port with a self-signed cert,
//! creates API keys, and verifies the full request lifecycle.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{roles, setup, setup_with_query_timeout, setup_with_rate_limit};
use fleet_auth::{KeyStore, PrincipalKind};
use trawl_client::HttpClient;
use trawl_server::config::RateLimitConfig;

#[tokio::test(flavor = "multi_thread")]
async fn health_returns_ok() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "unused").unwrap();
    let health = client.health().await.unwrap();
    assert_eq!(health.status, trawl_api::HealthStatus::Ok);
    assert_eq!(
        health.checks,
        Some(std::collections::HashMap::from([
            ("duckdb".to_owned(), "ok".to_owned()),
            ("auth_db".to_owned(), "ok".to_owned()),
            ("storage_db".to_owned(), "ok".to_owned()),
            ("data_path".to_owned(), "ok".to_owned()),
        ]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn query_returns_results() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client.query_paginated("*", None, None).await.unwrap();
    assert_eq!(result.result.row_count(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn query_with_filter() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client
        .query_paginated("service=nginx", None, None)
        .await
        .unwrap();
    assert_eq!(result.result.row_count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn query_with_stats_pipeline() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client
        .query_paginated("* | stats count() by service", None, None)
        .await
        .unwrap();
    // nginx: 2, postgres: 1 → 2 rows
    assert_eq!(result.result.row_count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn query_rejects_missing_auth() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "").unwrap();

    let result = client.query_paginated("*", None, None).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected Server error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn query_rejects_invalid_token() {
    let server = setup().await;
    let client =
        HttpClient::new_insecure(&server.url, "flt_ZZZZZZZZ_totally_fake_token_here1234").unwrap();

    let result = client.query_paginated("*", None, None).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected Server error with 401, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn query_rejects_bad_dsl() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = client.query_paginated("| | | broken {{{", None, None).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 400);
        }
        other => panic!("expected 400 for bad DSL, got: {other:?}"),
    }
}

// -- query lifecycle telemetry -----------------------------------------------

mod lifecycle_capture {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// One captured tracing event: level plus stringified fields.
    #[derive(Debug, Clone)]
    pub struct Captured {
        pub level: String,
        pub fields: BTreeMap<String, String>,
    }

    /// Capture layer recording every event's fields as strings.
    #[derive(Clone, Default)]
    pub struct Capture {
        events: Arc<Mutex<Vec<Captured>>>,
    }

    impl Capture {
        pub fn events(&self) -> Vec<Captured> {
            self.events.lock().unwrap().clone()
        }
    }

    struct Visitor<'a>(&'a mut BTreeMap<String, String>);

    impl tracing::field::Visit for Visitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for Capture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = BTreeMap::new();
            event.record(&mut Visitor(&mut fields));
            self.events.lock().unwrap().push(Captured {
                level: event.metadata().level().as_str().to_owned(),
                fields,
            });
        }
    }
}

/// Default-filter lifecycle events for `/query`, `/export` and `/stream`
/// share a `query_id` and contain no user-supplied content — not the raw
/// DSL, and not an error message either (parser/emitter text quotes the
/// user's own tokens). The details survive only as DEBUG-level
/// `query_text` / `query_error_text` events `trawl_server=info` never stores.
///
/// One test, four sentinels: the capture layer is a global subscriber, and
/// only the first installer in a process wins.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // the sqlx macro used to hide the body in an inner fn
async fn query_export_and_stream_telemetry_carry_no_user_content() {
    use lifecycle_capture::Capture;
    use tracing_subscriber::prelude::*;

    let info_capture = Capture::default();
    let debug_capture = Capture::default();
    let subscriber = tracing_subscriber::registry()
        .with(
            info_capture
                .clone()
                .with_filter(tracing_subscriber::EnvFilter::new(
                    trawl_server::telemetry::DEFAULT_LOG_FILTER,
                )),
        )
        .with(
            debug_capture
                .clone()
                .with_filter(tracing_subscriber::EnvFilter::new("trawl_server=debug")),
        );
    // Global (not thread-local) — the server runs on other tokio workers.
    tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");

    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // The sentinel is a bare-text search term: valid DSL, matches nothing.
    let dsl = "service=nginx zz_sentinel_needle";
    client.query_paginated(dsl, None, None).await.unwrap();

    let info_events = info_capture.events();
    let start = info_events
        .iter()
        .find(|e| {
            e.fields
                .get("event_type")
                .is_some_and(|t| t.contains("query_start"))
        })
        .expect("query_start captured at info");
    let complete = info_events
        .iter()
        .find(|e| {
            e.fields
                .get("event_type")
                .is_some_and(|t| t.contains("query_complete"))
        })
        .expect("query_complete captured at info");

    // Correlatable without query text: same allocated query_id...
    let start_id = start
        .fields
        .get("query_id")
        .expect("query_start has query_id");
    let complete_id = complete
        .fields
        .get("query_id")
        .expect("query_complete has query_id");
    assert_eq!(start_id, complete_id, "lifecycle events share the query_id");

    // ...plus query_len for pathological-request diagnosis.
    assert_eq!(
        start.fields.get("query_len").map(String::as_str),
        Some(dsl.len().to_string().as_str()),
        "query_start carries query_len"
    );

    // The raw text is available, as a separate DEBUG-only event.
    let query_text = debug_capture
        .events()
        .into_iter()
        .find(|e| {
            e.fields
                .get("query")
                .is_some_and(|q| q.contains("zz_sentinel_needle"))
        })
        .expect("query_text event captured at debug");
    assert_eq!(query_text.level, "DEBUG");
    assert!(
        query_text
            .fields
            .get("event_type")
            .is_some_and(|t| t.contains("query_text")),
        "the DEBUG event carrying raw DSL is query_text: {query_text:?}"
    );
    assert!(
        query_text.fields.contains_key("query_id"),
        "query_text is keyed on query_id"
    );

    // -- a failing query --------------------------------------------------
    //
    // The emitter rejects an unknown function by quoting its name, so an
    // error message in default telemetry republishes whatever was typed.
    let bad_dsl = "* | stats zz_failure_needle()";
    let err = client
        .query_paginated(bad_dsl, None, None)
        .await
        .expect_err("unknown function is rejected");
    match err {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 400),
        other => panic!("expected 400 for the bad function name, got: {other:?}"),
    }

    // -- an export ---------------------------------------------------------
    let raw = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let export_dsl = "service=nginx zz_export_needle";
    let export_resp = raw
        .post(format!("{}/api/v1/export?format=csv", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": export_dsl }))
        .send()
        .await
        .expect("export request");
    assert!(export_resp.status().is_success(), "{export_resp:?}");

    // -- an SSE stream -----------------------------------------------------
    let stream_dsl = "service=nginx zz_stream_needle";
    let stream_resp = raw
        .get(format!("{}/api/v1/stream", server.url))
        .query(&[("query", stream_dsl)])
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .expect("stream request");
    // Headers are enough: stream_start is logged before the SSE body opens.
    assert!(stream_resp.status().is_success(), "{stream_resp:?}");
    drop(stream_resp);

    let info_events = info_capture.events();
    let find_info = |event_type: &str| {
        info_events
            .iter()
            .find(|e| {
                e.fields
                    .get("event_type")
                    .is_some_and(|t| t.contains(event_type))
            })
            .unwrap_or_else(|| panic!("{event_type} captured at info"))
    };

    // The failure event classifies, and carries no message of any kind:
    // safe_message() deliberately preserves parse/emit text.
    let failed = find_info("query_failed");
    assert!(
        failed
            .fields
            .get("error_class")
            .is_some_and(|c| c.contains("emit")),
        "query_failed carries a stable error_class: {failed:?}"
    );
    assert!(
        !failed.fields.contains_key("error") && !failed.fields.contains_key("safe_error"),
        "query_failed carries no error text: {failed:?}"
    );

    // Export and stream speak the same id vocabulary as /query.
    let export_start = find_info("export_start");
    let export_complete = find_info("export_complete");
    assert_eq!(
        export_start.fields.get("query_id"),
        export_complete.fields.get("query_id"),
        "export lifecycle events share the query_id"
    );
    assert_eq!(
        export_start.fields.get("query_len").map(String::as_str),
        Some(export_dsl.len().to_string().as_str()),
        "export_start carries query_len"
    );
    let stream_start = find_info("stream_start");
    assert!(
        stream_start.fields.contains_key("query_id"),
        "stream_start is keyed on query_id: {stream_start:?}"
    );
    assert_eq!(
        stream_start.fields.get("query_len").map(String::as_str),
        Some(stream_dsl.len().to_string().as_str()),
        "stream_start carries query_len"
    );

    // No user-supplied content anywhere in the default-filter stream.
    for sentinel in [
        "zz_sentinel_needle",
        "zz_failure_needle",
        "zz_export_needle",
        "zz_stream_needle",
    ] {
        for event in &info_events {
            for (name, value) in &event.fields {
                assert!(
                    !value.contains(sentinel),
                    "{sentinel} leaked into default-filter telemetry: field {name} of {event:?}"
                );
            }
        }
    }

    // Every one of them is recoverable at DEBUG, keyed on query_id.
    let debug_events = debug_capture.events();
    for (sentinel, event_type) in [
        ("zz_failure_needle", "query_error_text"),
        ("zz_export_needle", "query_text"),
        ("zz_stream_needle", "query_text"),
    ] {
        let found = debug_events
            .iter()
            .find(|e| {
                e.fields
                    .get("event_type")
                    .is_some_and(|t| t.contains(event_type))
                    && e.fields.values().any(|v| v.contains(sentinel))
            })
            .unwrap_or_else(|| panic!("{sentinel} recoverable at debug via {event_type}"));
        assert_eq!(found.level, "DEBUG");
        assert!(
            found.fields.contains_key("query_id"),
            "{event_type} is keyed on query_id: {found:?}"
        );
    }
}

// -- schema endpoint tests ---------------------------------------------------

/// The fixture corpus is dated 2024-01-15 — years outside the default
/// 90-day retention window — and the boot conformance pass backfills
/// `field_services` from the partition directories it adopted, so its fields
/// are legitimately aged out of the default listing. `?all=true` is the
/// window-lifted view this test wants (the windowing itself is covered by
/// `catalog_surface::aged_out_field_windowed_away_unless_all`).
#[tokio::test(flavor = "multi_thread")]
async fn schema_returns_columns() {
    let server = setup().await;

    let schema: trawl_api::SchemaResponse = raw_client()
        .get(format!("{}/api/v1/schema?all=true", server.url))
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!schema.columns.is_empty());

    // The fixture corpus carries these columns.
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"_time"), "missing _time column");
    assert!(names.contains(&"_ingested"), "missing _ingested column");
    assert!(names.contains(&"_raw"), "missing _raw column");
    assert!(names.contains(&"env"), "missing env column");
    assert!(names.contains(&"host"), "missing host column");
    assert!(names.contains(&"service"), "missing service column");
    assert!(names.contains(&"severity"), "missing severity column");
    assert!(names.contains(&"message"), "missing message column");
    assert_eq!(schema.file_count, 2);

    // Columns follow query-result display order: envelope first (in the
    // declared order), trailing metadata demoted to the very end — never a
    // raw alphabetical listing with `_ingested` first.
    assert_eq!(names[0], "_time", "envelope leads: {names:?}");
    assert_eq!(
        &names[names.len() - 4..],
        &["_raw", "_ingested", "_repairs", "_producer"],
        "metadata trails: {names:?}"
    );

    // Types are the catalog pins, not a DESCRIBE.
    let severity = schema
        .columns
        .iter()
        .find(|c| c.name == "severity")
        .unwrap();
    assert_eq!(severity.data_type, "BIGINT");

    // And the default listing really does window: the backfilled
    // observations put the fixture's own fields outside 90 days, leaving
    // only pins no file carries (which have nothing to age out).
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let windowed = client.schema().await.unwrap();
    assert!(
        windowed.columns.len() < schema.columns.len(),
        "a 2024 corpus must not all be inside a 90-day window"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_columns_come_from_the_catalog_not_describe() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Pin a field that exists in no parquet file anywhere: a DESCRIBE sweep
    // could never see it, so its presence in the response proves the columns
    // come from a catalog SELECT.
    server
        .state
        .storage
        .catalog
        .pin_missing(&[trawl_server::store::PinProposal {
            field: "zz_catalog_only".to_owned(),
            ty: trawl_core::schema::CanonicalType::BigInt,
            pinned_from: "test".to_owned(),
        }])
        .await
        .unwrap();

    let schema = client.schema().await.unwrap();
    let col = schema
        .columns
        .iter()
        .find(|c| c.name == "zz_catalog_only")
        .expect("a pinned-but-never-written field must appear (catalog-served)");
    assert_eq!(col.data_type, "BIGINT");
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_caching_works() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let first = client.schema().await.unwrap();
    assert!(!first.cached, "first call should not be cached");

    let second = client.schema().await.unwrap();
    assert!(second.cached, "second call should be cached");
}

// -- queries endpoint tests --------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn queries_own_flag_uses_verified_reader_and_preserves_permission_gate() {
    let server = setup().await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    let first = store
        .create_key(
            "twin",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let second = store
        .create_key(
            "twin",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let first_verified = store
        .verify_key(first.plaintext_token.as_str())
        .await
        .unwrap();
    let second_verified = store
        .verify_key(second.plaintext_token.as_str())
        .await
        .unwrap();
    let first_id = server.state.query.pool.allocate_query_id();
    let second_id = server.state.query.pool.allocate_query_id();
    server
        .state
        .query
        .tracker
        .start(first_id, &first_verified, "*");
    server
        .state
        .query
        .tracker
        .start(second_id, &second_verified, "*");
    let a = HttpClient::new_insecure(&server.url, first.plaintext_token.as_str()).unwrap();
    let b = HttpClient::new_insecure(&server.url, second.plaintext_token.as_str()).unwrap();
    for (client, own_id) in [(&a, first_id), (&b, second_id)] {
        let response = client.queries().await.unwrap();
        assert_eq!(response.active.len(), 2);
        for entry in response.active {
            assert_eq!(entry.own, entry.snapshot.id == own_id);
        }
    }
    server.state.query.tracker.complete(first_id, 3);
    server.state.query.tracker.timeout(second_id);
    for (client, own_id) in [(&a, first_id), (&b, second_id)] {
        let response = client.queries().await.unwrap();
        assert!(response.active.is_empty());
        assert_eq!(response.recent.len(), 2);
        for entry in response.recent {
            assert_eq!(entry.own, entry.snapshot.id == own_id);
        }
    }
    let denied = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    assert!(matches!(
        denied.queries().await,
        Err(trawl_client::ClientError::Server { status: 403, .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_shows_history() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    // Run a query so there's something in history.
    analyst.query_paginated("*", None, None).await.unwrap();

    let queries = admin.queries().await.unwrap();
    assert!(
        !queries.recent.is_empty(),
        "recent history should contain the query we just ran"
    );
    assert_eq!(queries.recent[0].snapshot.rows, Some(3));
    assert!(!queries.recent[0].snapshot.timed_out);
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_accessible_by_analyst_and_reader() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    // Both analyst and reader can list running queries.
    analyst.queries().await.unwrap();
    reader.queries().await.unwrap();
}

/// A query whose request timed out keeps its permit until the work
/// actually stops, and `GET /queries` says so (ADR-0024).
///
/// Three readers, one retained entry. An interactive query shows its user
/// and its DSL to every reader of the route, exactly as the active and
/// recent lists have always shown the same query. A reader who can watch
/// it run and read it in history learns nothing from the retained line.
/// The entry is not also listed as active: one permit, one line. What the
/// key id still governs is CANCELLATION, asserted below: an unrelated key
/// sharing the display name is refused.
///
/// The retained window is held open, not timed: this server's own pool
/// parks its worker just past the work-start transition and stays there
/// until the test lets it go, so every assertion below runs against a
/// state that cannot move. A wall-clock window (a long worker delay minus
/// a short request timeout) used to stand in for that, which made the
/// verdict depend on how busy the runner was and put a correct 503 where
/// the test demanded a 504.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one retained window, asserted from three readers
async fn retained_work_is_listed_once_and_carries_its_display_metadata() {
    use trawl_server::pool::seam::Seam;

    let permissive = RateLimitConfig {
        default_rpm: 1_000_000,
        ..RateLimitConfig::default()
    };
    let server = setup_with_query_timeout(permissive, 1).await;
    // This server's pool only: a hold installed here parks nothing in any
    // other test's pool, so the plain parallel harness is safe.
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);

    let store = KeyStore::from_pool(server.fleet_pool.clone());
    let key_a = store
        .create_key(
            "twin",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let key_b = store
        .create_key(
            "twin",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    assert_ne!(key_a.info.id, key_b.info.id);

    let a = HttpClient::new_insecure(&server.url, key_a.plaintext_token.as_str()).unwrap();
    let b = HttpClient::new_insecure(&server.url, key_b.plaintext_token.as_str()).unwrap();
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let dsl = "service=nginx | stats count()";
    let slow = {
        let a = a.clone();
        tokio::spawn(async move { a.query_paginated(dsl, None, None).await })
    };

    // The request answers first: a 504, because the worker is parked PAST
    // the work-start transition and expiry there is a timeout, never the
    // capacity refusal of work that never started.
    let answered = slow.await.expect("the request task joins");
    match answered {
        Err(trawl_client::ClientError::Server { status, .. }) => assert_eq!(status, 504),
        other => panic!("expected the query to time out, got: {other:?}"),
    }

    let seen = admin.queries().await.unwrap();
    let entry = seen
        .retained
        .iter()
        .find(|w| w.query.as_deref() == Some(dsl))
        .expect("the timed-out query still holds its permit");
    let id = entry.id;
    assert_eq!(entry.kind, "query");
    assert!(
        entry.started,
        "the delay sits past the work-start transition"
    );
    assert_eq!(entry.user.as_deref(), Some("twin"));
    assert!(
        !seen.active.iter().any(|q| q.snapshot.id == id),
        "retained work is not counted again as an active request"
    );
    assert!(
        seen.recent
            .iter()
            .any(|q| q.snapshot.id == id && q.snapshot.timed_out),
        "the same request is in recent history, where it recorded its outcome"
    );

    let by_owner = a.queries().await.unwrap();
    let mine = by_owner
        .retained
        .iter()
        .find(|w| w.id == id)
        .expect("the owner sees its own retained work");
    assert_eq!(mine.user.as_deref(), Some("twin"));
    assert_eq!(mine.query.as_deref(), Some(dsl));

    let by_twin = b.queries().await.unwrap();
    let theirs = by_twin
        .retained
        .iter()
        .find(|w| w.id == id)
        .expect("every query reader sees the capacity fact");
    assert_eq!(theirs.kind, "query");
    assert!(theirs.started);
    assert_eq!(
        theirs.user.as_deref(),
        Some("twin"),
        "an interactive entry carries the display metadata this route always carried"
    );
    assert_eq!(theirs.query.as_deref(), Some(dsl));
    assert!(
        by_twin
            .recent
            .iter()
            .any(|q| q.snapshot.id == id && q.snapshot.query == dsl),
        "the same reader reads the same query text in history, so hiding it \
         from the retained line would protect nothing"
    );

    // Cancellation authority follows the same record, not the tracker:
    // the request already answered, and the key that submitted it can
    // still ask for the work to stop while a twin cannot.
    match b.cancel_query(id).await {
        Err(trawl_client::ClientError::Server { status, error }) => {
            assert_eq!(status, 403);
            assert_eq!(error.message, "cannot cancel this query");
        }
        other => panic!("expected 403 for the twin key, got: {other:?}"),
    }
    assert!(
        a.cancel_query(id).await.unwrap().cancelled,
        "the submitting key can still stop work it has been told timed out"
    );
    assert!(
        a.cancel_query(id).await.unwrap().cancelled,
        "cancellation is a request, so it repeats while the work exists"
    );

    // The same one permit, in the counts: retained is a subset of held.
    let stats = admin.stats().await.unwrap();
    assert_eq!(stats.pool_retained, 1);
    assert!(stats.pool_retained <= stats.pool_capacity - stats.pool_available);

    // ...and in the dashboard snapshot the collector publishes.
    let mut dashboard_saw_it = false;
    for _ in 0..40 {
        if admin.dashboard().await.is_ok_and(|d| d.pool_retained == 1) {
            dashboard_saw_it = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        dashboard_saw_it,
        "the dashboard snapshot must carry the retained permit"
    );

    // Everything above held while the worker was parked. Let it finish:
    // when the work stops, every count returns to baseline.
    assert_eq!(started.arrivals(), 1, "one worker, held once");
    started.release();
    let mut reclaimed = false;
    for _ in 0..200 {
        let stats = admin.stats().await.unwrap();
        if stats.pool_retained == 0 && stats.pool_available == stats.pool_capacity {
            reclaimed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(reclaimed, "the retained permit must come back");
    assert!(admin.queries().await.unwrap().retained.is_empty());
}

/// Resolving `from saved` happens after the tracker has already opened an
/// entry for the request, so a resolution failure has to finish that entry
/// like any other failure. When it escaped the handler on its own the id
/// stayed "active" for the life of the process, and `/queries` reported a
/// query nobody was running.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_from_saved_resolution_finishes_its_tracking() {
    const DSL: &str = "| from saved no_such_report | head 1";

    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    match client.query_paginated(DSL, None, None).await {
        Err(trawl_client::ClientError::Server { status, .. }) => assert_eq!(status, 404),
        other => panic!("expected 404 for an unknown saved query, got: {other:?}"),
    }

    let seen = admin.queries().await.unwrap();
    assert!(
        !seen.active.iter().any(|q| q.snapshot.query == DSL),
        "a refused resolution leaves nothing running"
    );
    let recorded = seen
        .recent
        .iter()
        .find(|q| q.snapshot.query == DSL)
        .expect("the failure is recorded once, in history");
    assert!(
        recorded.snapshot.error.is_some(),
        "the entry carries the refusal, not a success"
    );
    assert!(!recorded.snapshot.timed_out, "a 404 is not a timeout");
    assert!(seen.retained.is_empty(), "no permit was ever taken");
}

/// An unparseable timezone is request validation, refused with a 400 before
/// the tracker opens an entry. It used to be resolved after `tracker.start`
/// and returned by `?`, which left the id active for the life of the
/// process because nothing sweeps abandoned entries.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_timezone_leaves_no_active_entry() {
    const DSL: &str = "service=tz-refusal last=1h | head 1";

    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let resp = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .header("authorization", format!("Bearer {}", server.analyst_token))
        .json(&serde_json::json!({ "query": DSL, "timezone": "Mars/Olympus_Mons" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "an unresolvable timezone is a 400");

    let seen = admin.queries().await.unwrap();
    assert!(
        !seen.active.iter().any(|q| q.snapshot.query == DSL),
        "a refused timezone leaves nothing running"
    );
    assert!(seen.retained.is_empty(), "no permit was ever taken");
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_rejects_ingest_role() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let result = client.queries().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 403);
        }
        other => panic!("expected 403 for ingest role, got: {other:?}"),
    }
}

// -- ingest endpoint tests ---------------------------------------------------

/// Build a raw reqwest client that accepts self-signed certs.
fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_accepts_ndjson() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let records = vec![
        serde_json::json!({"service": "test-svc", "host": "web01", "message": "hello"}),
        serde_json::json!({"service": "test-svc", "host": "web02", "message": "world"}),
    ];

    let resp = client.ingest(&records).await.unwrap();
    assert_eq!(resp.accepted, 2);
}

/// The ADR-0013 worked examples, end to end.
///
/// A vector-shaped payload (`timestamp` + `level`) lands as the declared
/// ten-field envelope: `_time` derived and `timestamp` stored verbatim,
/// `_severity` derived and `level` stored verbatim, `_raw` populated, with
/// the severity vocabulary riding `_severity`, which nothing can shadow.
/// The game server's `level:"gold"` keeps its column and gets no
/// `_severity` at all.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // the sqlx macro used to hide the body in an inner fn
async fn vector_shaped_ingest_is_queryable() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let query = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let now = chrono::Utc::now().to_rfc3339();
    let records = vec![
        serde_json::json!({"service": "vec-svc", "host": "web01", "level": "error",
            "timestamp": now, "message": "boom"}),
        serde_json::json!({"service": "vec-svc", "host": "web01", "level": "info",
            "timestamp": now, "message": "fine"}),
        // `level` means loot tier here, not severity.
        serde_json::json!({"service": "vec-svc", "host": "web01", "level": "gold",
            "timestamp": now, "message": "dropped"}),
        // The elastic spelling: an ordinary column whose DuckDB
        // identifier needs quoting everywhere it appears.
        serde_json::json!({"service": "vec-svc", "host": "web01",
            "@timestamp": now, "message": "elastic"}),
    ];
    let resp = ingest.ingest(&records).await.unwrap();
    assert_eq!(resp.accepted, 4);

    let result = query
        .query_paginated("service=vec-svc _severity=error last=1h", None, None)
        .await
        .unwrap();
    assert_eq!(result.result.row_count(), 1, "only the ERROR-band row");
    let col = |name: &str| {
        result
            .result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("column {name} present"))
    };
    let row = &result.result.rows[0];
    assert_eq!(
        row[col(trawl_core::schema::SEVERITY)],
        trawl_api::value::Value::Integer(17),
        "level:error derives the ERROR-band severity 17"
    );
    assert_eq!(
        row[col("level")],
        trawl_api::value::Value::String("error".into()),
        "the source is stored verbatim beside the derived slot"
    );
    assert!(
        matches!(&row[col("_time")], trawl_api::value::Value::String(_)),
        "_time populated"
    );
    assert!(
        matches!(&row[col("timestamp")], trawl_api::value::Value::String(_)),
        "the time SOURCE is stored verbatim too"
    );
    match &row[col("_raw")] {
        trawl_api::value::Value::String(raw) => {
            assert!(raw.contains("boom"), "_raw carries the original: {raw}");
        }
        other => panic!("_raw must be a string, got {other:?}"),
    }

    // The game server's row is fully queryable under its own vocabulary
    // and carries no derived severity at all.
    let gold = query
        .query_paginated("service=vec-svc level=gold last=1h", None, None)
        .await
        .unwrap();
    assert_eq!(gold.result.row_count(), 1, "the sender's own field filters");
    let gold_col = gold
        .result
        .columns
        .iter()
        .position(|c| c.name == trawl_core::schema::SEVERITY);
    if let Some(idx) = gold_col {
        assert_eq!(
            gold.result.rows[0][idx],
            trawl_api::value::Value::Null,
            "an unmappable level derives no severity"
        );
    }

    // `@timestamp` is a stored column with a quoting-hostile name: it
    // filters, projects and sorts as itself, and derives `_time` too.
    let elastic = query
        .query_paginated(
            r"service=vec-svc message=elastic last=1h | table @timestamp, _time | sort @timestamp",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(elastic.result.row_count(), 1);
    assert_eq!(
        elastic
            .result
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["@timestamp", "_time"]
    );

    // The exact OTel short name and the band token agree with each other.
    for (dsl, expected) in [
        ("service=vec-svc _severity=error2 last=1h", 0),
        ("service=vec-svc _severity>=warn last=1h", 1),
        ("service=vec-svc _severity=warn,error last=1h", 1),
        ("service=vec-svc _severity=info last=1h", 1),
    ] {
        let r = query.query_paginated(dsl, None, None).await.unwrap();
        assert_eq!(r.result.row_count(), expected, "{dsl}");
    }

    // An unknown severity value is a 4xx naming the vocabulary, never a
    // filter that quietly matches nothing.
    let resp = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .header("authorization", format!("Bearer {}", server.analyst_token))
        .json(&serde_json::json!({ "query": "_severity=spicy" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("unknown severity value") && body.contains("1-24"),
        "the error must name the vocabulary: {body}"
    );

    // …and `level` is ordinary sender vocabulary (ADR-0013 §6): grouping
    // on it and naming it in a `where` IN-list are plain field usage, in
    // both lanes, batch and live.
    for query_text in [
        "service=vec-svc last=1h | stats count() by level",
        r#"service=vec-svc last=1h | where level in ("error", "fatal")"#,
    ] {
        let resp = raw_client()
            .post(format!("{}/api/v1/query", server.url))
            .header("authorization", format!("Bearer {}", server.analyst_token))
            .json(&serde_json::json!({ "query": query_text }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "{query_text} must be ordinary field usage"
        );

        let sse = raw_client()
            .get(format!("{}/api/v1/stream", server.url))
            .query(&[("query", query_text)])
            .header("authorization", format!("Bearer {}", server.analyst_token))
            .send()
            .await
            .unwrap();
        assert_eq!(sse.status(), 200, "{query_text} must stream too");
    }
}

/// Provenance is data (ADR-0013 ruling 6), end to end.
///
/// `_producer` has to survive the whole journey to be worth anything: the
/// door stamps it, the WAL and hot buffer carry it, the catalog types it,
/// and the DSL filters on it. Two of the three doors run against one
/// server here — the HTTP handler through the real endpoint, and the
/// syslog door through `SyslogDoor::admit` into the server's own pipeline
/// (no in-process harness speaks UDP/TCP frames to a live listener, and
/// the frame parser is not what this test is about). The trawld door's
/// stamp is covered where its events are built, in `telemetry`'s own
/// tests.
///
/// The forgery half matters as much as the stamp: a sender that puts
/// `_producer` on the wire must find it under the bare `producer`
/// remainder, with the real column still naming the door.
#[tokio::test(flavor = "multi_thread")]
async fn producer_is_stamped_stored_and_queryable_per_door() {
    use std::collections::HashMap;

    use indexmap::IndexMap;
    use trawl_server::ingest::pipeline::ServiceBatch;
    use trawl_server::syslog::convert::SyslogDoor;

    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let query = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let now = chrono::Utc::now().to_rfc3339();
    let records = vec![
        serde_json::json!({"service": "prov-svc", "host": "web01",
            "_time": now, "message": "one"}),
        serde_json::json!({"service": "prov-svc", "host": "web01",
            "_time": now, "message": "two"}),
        // A sender claiming to be the syslog door. It is not.
        serde_json::json!({"service": "prov-svc", "host": "web01",
            "_time": now, "message": "forged", "_producer": "syslog"}),
    ];
    assert_eq!(ingest.ingest(&records).await.unwrap().accepted, 3);

    // The syslog door, with the server's own boot-resolved policy, into
    // the server's own pipeline — the same route `spawn_syslog` takes.
    let door = SyslogDoor {
        envs: Arc::clone(&server.state.ingest.envs),
        default_env: Arc::clone(&server.state.ingest.default_env),
        trusted_relays: Arc::clone(&server.state.ingest.trusted_relays),
        derivation: Arc::clone(&server.state.ingest.derivation),
    };
    let frame = format!(
        "<165>1 {} appliance-01 prov-syslog 1234 ID47 - reboot",
        chrono::Utc::now().to_rfc3339()
    );
    let event = door
        .admit(
            &frame,
            "10.0.0.9".parse().unwrap(),
            &HashMap::new(),
            "syslog",
            "udp",
        )
        .expect("the syslog profile must admit a well-formed frame");
    assert_eq!(event.map["_producer"], "syslog", "stamped at the door");
    let mut batch = ServiceBatch::default();
    batch.push(event.map);
    let mut batches = IndexMap::new();
    batches.insert((event.env, event.service), batch);
    let pipeline = Arc::clone(
        server
            .state
            .ingest
            .pipeline
            .as_ref()
            .expect("ingest is enabled in the test config"),
    );
    assert_eq!(
        tokio::task::spawn_blocking(move || pipeline.write(batches))
            .await
            .unwrap(),
        1
    );

    // The column is queryable, and it partitions the two doors.
    let count = async |dsl: &str| -> i64 {
        let result = query.query_paginated(dsl, None, None).await.unwrap();
        assert_eq!(result.result.row_count(), 1, "{dsl}");
        match &result.result.rows[0][0] {
            trawl_api::value::Value::Integer(n) => *n,
            other => panic!("{dsl}: count must be an integer, got {other:?}"),
        }
    };
    assert_eq!(count("_producer=http last=1h | stats count()").await, 3);
    assert_eq!(count("_producer=syslog last=1h | stats count()").await, 1);

    // The forgery landed bare, the real stamp is untouched, and the wire
    // spelling stays findable in `_raw`.
    let forged = query
        .query_paginated(
            "service=prov-svc producer=syslog last=1h | table _producer, producer, _raw",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(forged.result.row_count(), 1, "the stripped column filters");
    let row = &forged.result.rows[0];
    assert_eq!(
        row[0],
        trawl_api::value::Value::String("http".into()),
        "_producer names the door, never the payload"
    );
    assert_eq!(
        row[1],
        trawl_api::value::Value::String("syslog".into()),
        "the forged claim survives under the bare remainder"
    );
    match &row[2] {
        trawl_api::value::Value::String(raw) => assert!(
            raw.contains("_producer"),
            "the wire spelling stays in _raw: {raw}"
        ),
        other => panic!("_raw must be a string, got {other:?}"),
    }
}

/// An event with an unlisted env is rejected per-event with a typed
/// message; the sibling with no env lands under `default_env`.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_unlisted_env_rejected_per_event() {
    let server = setup().await;
    let client = raw_client();

    let body = "{\"service\":\"env-svc\",\"env\":\"nope\",\"message\":\"bad\"}\n\
                {\"service\":\"env-svc\",\"message\":\"good\"}";
    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.ingest_token))
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: trawl_api::IngestResponse = resp.json().await.unwrap();
    assert_eq!(body.accepted, 1);
    assert_eq!(body.rejected, 1);
    assert!(
        body.errors[0].message.contains("nope"),
        "the typed reason names the env: {}",
        body.errors[0].message
    );
}

/// Repairs surface as `trawl_ingest_repairs_total{code, service}` on the
/// unauthenticated /metrics endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_repairs_exposed_on_metrics() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let records = vec![serde_json::json!({
        "service": "repair-svc", "host": "web01", "env": "prod",
        "timestamp": "not-a-date", "message": "clock trouble"
    })];
    let resp = ingest.ingest(&records).await.unwrap();
    assert_eq!(resp.accepted, 1, "a repair is not a rejection");

    let metrics_body = raw_client()
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let line = metrics_body
        .lines()
        .find(|l| {
            l.starts_with("trawl_ingest_repairs_total")
                && l.contains("code=\"time.from_ingest\"")
                && l.contains("service=\"repair-svc\"")
        })
        .unwrap_or_else(|| {
            panic!("expected a labelled trawl_ingest_repairs_total line in:\n{metrics_body}")
        });
    assert!(line.trim_end().ends_with('1'), "counter at 1: {line}");
}

/// An unmappable severity source surfaces as
/// `trawl_severity_unmapped_total{service}` on /metrics — one increment per
/// event whose source mapped to nothing, and no series at all for a sender
/// whose source mapped fine (ADR-0013 §2).
#[tokio::test(flavor = "multi_thread")]
async fn unmapped_severity_exposed_on_metrics() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let records = vec![
        serde_json::json!({"service": "unmapped-svc", "env": "prod", "level": "gold"}),
        serde_json::json!({"service": "unmapped-svc", "env": "prod", "level": "silver"}),
        serde_json::json!({"service": "mapped-svc", "env": "prod", "level": "error"}),
    ];
    let resp = ingest.ingest(&records).await.unwrap();
    assert_eq!(resp.accepted, 3, "an unmapped severity is not a rejection");

    let metrics_body = raw_client()
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let line = metrics_body
        .lines()
        .find(|l| {
            l.starts_with("trawl_severity_unmapped_total") && l.contains("service=\"unmapped-svc\"")
        })
        .unwrap_or_else(|| {
            panic!("expected a labelled trawl_severity_unmapped_total line in:\n{metrics_body}")
        });
    assert!(line.trim_end().ends_with('2'), "counter at 2: {line}");
    assert!(
        !metrics_body
            .lines()
            .any(|l| l.starts_with("trawl_severity_unmapped_total")
                && l.contains("service=\"mapped-svc\"")),
        "a mappable source publishes no series:\n{metrics_body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_rejects_missing_auth() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"x","message":"y"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_rejects_analyst_role() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.analyst_token))
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"x","message":"y"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "forbidden", "{body}");
    assert_eq!(body["error"]["message"], "insufficient permissions");
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_partial_success() {
    let server = setup().await;
    let client = raw_client();

    // 3 ndjson events: good, bad json, good
    let body = "{\"service\":\"test-svc\",\"message\":\"one\"}\nnot json\n{\"service\":\"test-svc\",\"message\":\"three\"}";

    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.ingest_token))
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: trawl_api::IngestResponse = resp.json().await.unwrap();
    assert_eq!(body.accepted, 2);
    assert_eq!(body.rejected, 1);
    assert_eq!(body.errors.len(), 1);
    assert_eq!(body.errors[0].index, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_all_rejected_per_event() {
    let server = setup().await;
    let client = raw_client();

    // All 3 events are bad (no service field)
    let body = "{\"message\":\"no svc\"}\n{\"message\":\"also no svc\"}\n{\"message\":\"nope\"}";

    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.ingest_token))
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap();

    // Returns 200 even when all events rejected (per-event, not batch-level).
    assert_eq!(resp.status(), 200);
    let body: trawl_api::IngestResponse = resp.json().await.unwrap();
    assert_eq!(body.accepted, 0);
    assert_eq!(body.rejected, 3);
    assert_eq!(body.errors.len(), 3);
}

// -- rate limit tests --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_returns_429() {
    let server = setup_with_rate_limit(RateLimitConfig {
        default_rpm: 2, // burst of 2
        ..RateLimitConfig::default()
    })
    .await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // First 2 should succeed (burst capacity).
    client.query_paginated("*", None, None).await.unwrap();
    client.query_paginated("*", None, None).await.unwrap();

    // 3rd should be rate limited.
    let result = client.query_paginated("*", None, None).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 429, "expected 429 Too Many Requests");
        }
        other => panic!("expected 429 rate limit error, got: {other:?}"),
    }
}

/// The interactive ceiling (`default_rpm`) must never apply to `/ingest`, and
/// the shipper-sized `ingest_rpm` must never apply to the query routes — the
/// two route classes hold separate bucket maps. Without the split, one number
/// has to serve both, and sizing it for vector hands every interactive key the
/// same budget (the loosening this test exists to catch).
#[tokio::test(flavor = "multi_thread")]
async fn ingest_rpm_is_independent_of_the_interactive_ceiling() {
    let server = setup_with_rate_limit(RateLimitConfig {
        default_rpm: 1,
        ingest_rpm: 10,
        ..RateLimitConfig::default()
    })
    .await;
    let raw = raw_client();

    // Well past the interactive burst of 1 — the ingest key rides its own
    // bucket, so every one of these is a 200.
    for i in 0..5 {
        let resp = raw
            .post(format!("{}/api/v1/ingest", server.url))
            .header("authorization", format!("Bearer {}", server.ingest_token))
            .header("content-type", "application/x-ndjson")
            .body(format!(r#"{{"service":"test-svc","message":"batch {i}"}}"#))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "ingest request {i} must not be limited");
    }

    // The query routes still enforce default_rpm = 1: second call is 429.
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    client.query_paginated("*", None, None).await.unwrap();
    let result = client.query_paginated("*", None, None).await;
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 429, "interactive routes keep the default_rpm burst");
        }
        other => panic!("expected 429 rate limit error, got: {other:?}"),
    }
}

/// AC1 (ADR-0006 slice 0): two keys holding the SAME role get independent
/// buckets — the limiter keys on the keystore id, not on any shared role
/// bucket. Exhausting key A's quota 429s A while key B still gets 200.
/// (Role here is test-side policy vocabulary only; the limiter never sees it.)
#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_isolates_keys_with_same_role() {
    let server = setup_with_rate_limit(RateLimitConfig {
        default_rpm: 2, // burst of 2 per key
        ..RateLimitConfig::default()
    })
    .await;

    // Two analyst keys with distinct keystore ids.
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    let key_a = store
        .create_key(
            "noisy",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let key_b = store
        .create_key(
            "quiet",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    assert_ne!(
        key_a.info.id, key_b.info.id,
        "keys must have distinct keystore ids"
    );

    let a = HttpClient::new_insecure(&server.url, key_a.plaintext_token.as_str()).unwrap();
    let b = HttpClient::new_insecure(&server.url, key_b.plaintext_token.as_str()).unwrap();

    // Exhaust A's burst, then confirm A is limited.
    a.query_paginated("*", None, None).await.unwrap();
    a.query_paginated("*", None, None).await.unwrap();
    let err = a.query_paginated("*", None, None).await.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 429, "key A must be rate limited");
        }
        other => panic!("expected 429 for exhausted key A, got: {other:?}"),
    }

    // B holds the same role but its own bucket — still 200.
    b.query_paginated("*", None, None)
        .await
        .expect("key B must not be limited by key A's exhaustion");
}

/// The shipper-sized `ingest_rpm` is earned by `Permission::Ingest`, not by
/// reaching `/api/v1/ingest`. The handler's permission check runs downstream of
/// the limiter (axum resolves `body: Bytes` first), so an ungated ingest bucket
/// would widen every reader/analyst key's throughput on the heaviest endpoint
/// to the shipper ceiling. A reader must stay on `default_rpm`.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_ceiling_does_not_apply_to_keys_without_ingest_permission() {
    let server = setup_with_rate_limit(RateLimitConfig {
        default_rpm: 1,
        ingest_rpm: 10,
        ..RateLimitConfig::default()
    })
    .await;
    let raw = raw_client();

    let post_ingest = async |token: &str| {
        raw.post(format!("{}/api/v1/ingest", server.url))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/x-ndjson")
            .body(r#"{"service":"test-svc","message":"probe"}"#)
            .send()
            .await
            .unwrap()
            .status()
    };

    // Reader burst is default_rpm (1): rejected on permission first, then on
    // rate — never the ingest_rpm allowance of 10.
    assert_eq!(post_ingest(&server.reader_token).await, 403);
    assert_eq!(
        post_ingest(&server.reader_token).await,
        429,
        "a reader key must ride the interactive bucket on /ingest, not ingest_rpm"
    );

    // The gate is on the permission, not on the route: a real shipper key
    // still gets its full ingest_rpm burst.
    for i in 0..10 {
        assert_eq!(
            post_ingest(&server.ingest_token).await,
            200,
            "ingest request {i} must not be limited"
        );
    }
}

// ── endpoint tests (cancellation, validation, pagination, stats, field values) ──

#[tokio::test(flavor = "multi_thread")]
async fn cancel_query_by_admin() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let analyst_clone = analyst.clone();
    let slow_query = tokio::spawn(async move {
        let _ = analyst_clone
            .query_paginated("* | timechart span=1s count()", None, None)
            .await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Cancelling id 1 is a timing guess: the fixture is too small to keep a
    // query in flight, so this asserts the endpoint answers, not that it
    // caught anything.
    let _cancel_resp = admin.cancel_query(1).await.unwrap();

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), slow_query).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_query_nonexistent_returns_false() {
    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let resp = admin.cancel_query(9999).await.unwrap();
    assert!(!resp.cancelled);
    assert_eq!(resp.query_id, 9999);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_query_by_analyst_for_nonexistent() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Analyst gets "cannot cancel" for non-existent queries — no information
    // disclosure about whether the query ID exists (only admin sees the difference).
    let result = analyst.cancel_query(9999).await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_query_rejects_ingest_role() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let result = ingest.cancel_query(1).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 403);
        }
        other => panic!("expected 403 for ingest role, got: {other:?}"),
    }
}

/// Non-admin cancellation is authorized by exact keystore id, not display
/// name. Two `QueryCancel` keys sharing a name ("twin") must stay isolated:
/// key B cannot cancel key A's in-flight query, while owner A passes the gate
/// and actually interrupts the query.
///
/// A revert to name-matching would let B (same name) through, so this test
/// guards that regression. It needs a genuinely *active* query — a completed
/// one has no tracked owner under either scheme, so only a live entry can tell
/// id-matching from name-matching apart.
///
/// The pool's `TEST_QUERY_DELAY_MS` hook (via the crate's `test-support`
/// feature) holds the query in-flight deterministically — no dataset-size
/// timing bets against fast CI runners.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_query_isolated_by_key_id_not_name() {
    // Permissive rate limits: the observe/cancel polls below run in a tight
    // window and must not trip the per-minute buckets.
    let permissive = RateLimitConfig {
        default_rpm: 1_000_000,
        ..RateLimitConfig::default()
    };
    let server = setup_with_rate_limit(permissive).await;

    // Hold every pool query open long enough to observe and cancel it.
    // nextest runs each test in its own process, so the global is private
    // to this test; reset at the end regardless.
    trawl_server::pool::TEST_QUERY_DELAY_MS.store(3_000, Ordering::Relaxed);

    // Two analyst keys with the same display name but distinct keystore ids.
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    let key_a = store
        .create_key(
            "twin",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let key_b = store
        .create_key(
            "twin",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    assert_ne!(
        key_a.info.id, key_b.info.id,
        "twin keys must have distinct keystore ids"
    );

    let a = HttpClient::new_insecure(&server.url, key_a.plaintext_token.as_str()).unwrap();
    let b = HttpClient::new_insecure(&server.url, key_b.plaintext_token.as_str()).unwrap();

    // Fire a query as key A in the background; the injected delay keeps it
    // tracked as active until we are done asserting.
    let slow = {
        let a = a.clone();
        tokio::spawn(async move { a.query_paginated("* | stats count()", None, None).await })
    };

    // Wait until A's query shows up as active, capturing its id.
    let mut target = None;
    for _ in 0..500 {
        if let Ok(resp) = a.queries().await
            && let Some(id) = resp
                .active
                .iter()
                .filter(|q| q.snapshot.user == "twin")
                .map(|q| q.snapshot.id)
                .max()
        {
            target = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let target = target.expect("key A should have an active query");

    // Key B (same name, QueryCancel, different id) is rejected by the
    // ownership gate — the query is guaranteed still in-flight here.
    let denied = b.cancel_query(target).await;
    match denied {
        Err(trawl_client::ClientError::Server { status, error }) => {
            assert_eq!(status, 403, "twin key B must not cancel key A's query");
            assert_eq!(error.code, trawl_api::ErrorCode::Forbidden);
            assert_eq!(error.message, "cannot cancel this query");
        }
        other => panic!("expected 403 for non-owner cancel, got: {other:?}"),
    }

    // Owner A passes the ownership gate and interrupts the tracked query:
    // `cancelled == true` proves the tracker id and the pool's interrupt-map
    // id are the same id space. The interrupt handle is registered by the
    // pool task shortly after the tracker entry appears, so retry briefly.
    let mut cancelled = false;
    for _ in 0..500 {
        let resp = a
            .cancel_query(target)
            .await
            .expect("owner key A must pass the ownership gate");
        if resp.cancelled {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        cancelled,
        "owner cancel must interrupt the in-flight query (shared id space)"
    );

    trawl_server::pool::TEST_QUERY_DELAY_MS.store(0, Ordering::Relaxed);

    // The interrupted query surfaces an error to its submitter.
    let _ = slow.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn validate_query_valid() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .validate("service=nginx | stats count()")
        .await
        .unwrap();
    assert!(resp.valid);
    assert!(resp.errors.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn validate_query_syntax_error() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .validate("service=nginx | bad_command")
        .await
        .unwrap();
    assert!(!resp.valid);
    assert!(!resp.errors.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn validate_query_unknown_function() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.validate("* | stats unknown_func()").await.unwrap();
    assert!(!resp.valid);
    assert!(resp.errors.iter().any(|e| e.message.contains("unknown")));
}

#[tokio::test(flavor = "multi_thread")]
async fn query_pagination_limit_offset() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // The fixture holds 3 rows.
    let resp = client.query_paginated("*", Some(2), Some(1)).await.unwrap();
    assert_eq!(resp.pagination.limit, 2);
    assert_eq!(resp.pagination.offset, 1);
    assert_eq!(resp.pagination.returned, 2);
    assert_eq!(resp.result.row_count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn query_pagination_offset_beyond_results() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .query_paginated("*", Some(10), Some(100))
        .await
        .unwrap();
    assert_eq!(resp.pagination.offset, 100);
    assert_eq!(resp.pagination.returned, 0);
    assert_eq!(resp.result.row_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn query_pagination_defaults() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.query_paginated("*", None, None).await.unwrap();
    assert_eq!(resp.pagination.offset, 0);
    assert_eq!(resp.pagination.returned, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_endpoint_admin_only() {
    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let stats = admin.stats().await.unwrap();
    assert!(stats.pool_capacity > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_endpoint_analyst_forbidden() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = analyst.stats().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 403),
        other => panic!("expected 403, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_rejects_analyst() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = analyst.dashboard().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 403),
        other => panic!("expected 403, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_becomes_available_once_collector_ticks() {
    // The harness spawns the snapshot collector (same as trawld's main());
    // before its first tick the endpoint is 503 (snapshot is None), after
    // it the snapshot serves. Poll briefly.
    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match admin.dashboard().await {
            Ok(snapshot) => {
                assert!(snapshot.pool_capacity > 0);
                return;
            }
            Err(trawl_client::ClientError::Server { status: 503, .. }) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "dashboard never became available"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_stream_rejects_analyst() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .get(format!("{}/api/v1/dashboard/stream", server.url))
        .header("authorization", format!("Bearer {}", server.analyst_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "forbidden", "{body}");
    assert_eq!(body["error"]["message"], "insufficient permissions");
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_stream_emits_stats_event() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .get(format!("{}/api/v1/dashboard/stream", server.url))
        .header("authorization", format!("Bearer {}", server.admin_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "unexpected content-type: {content_type}"
    );

    // The handler skips ticks until the collector's first snapshot lands,
    // so read chunks until a full `stats` frame arrives (bounded overall).
    let mut resp = resp;
    let frame = tokio::time::timeout(Duration::from_secs(10), async {
        let mut buf = String::new();
        loop {
            let chunk = resp.chunk().await.unwrap().expect("stream ended early");
            buf.push_str(&String::from_utf8_lossy(&chunk));
            if let Some(start) = buf.find("event: stats")
                && let Some(end) = buf[start..].find("\n\n")
            {
                return buf[start..start + end].to_owned();
            }
        }
    })
    .await
    .expect("no stats event within 10s");

    let data_line = frame
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .expect("stats frame missing data line");
    let snapshot: trawl_api::DashboardSnapshot = serde_json::from_str(data_line).unwrap();
    assert!(snapshot.pool_capacity > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn whoami_admin_has_server_manage() {
    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let resp = admin.whoami().await.unwrap();
    assert_eq!(resp.roles, ["trawl-admin"]);
    assert!(resp.permissions.contains(&"server_manage".to_owned()));
    assert!(resp.permissions.contains(&"query".to_owned()));
    // prefix is the stable 8-char fingerprint of the key; downstream
    // consumers (e.g. coastwatch) key audit records off it, so the
    // endpoint must surface it verbatim.
    assert_eq!(resp.prefix.len(), 8, "prefix must be exactly 8 chars");
    assert!(
        resp.prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "prefix must be base64url charset: got {}",
        resp.prefix
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn whoami_reader_lacks_server_manage() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    let resp = reader.whoami().await.unwrap();
    assert_eq!(resp.roles, ["trawl-reader"]);
    assert!(!resp.permissions.contains(&"server_manage".to_owned()));
    assert!(resp.permissions.contains(&"query".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn whoami_rejects_missing_auth() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "invalid-token").unwrap();

    let result = client.whoami().await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn whoami_no_trawl_grant_is_403() {
    // Policy change with the fleet-auth cutover (ADR-0004): a foreign-app-only
    // key is rejected by the mandatory trawl policy layer on EVERY
    // authenticated route — including /whoami, which previously leaked
    // cross-app assignments to grantless keys.
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.coastwatch_only_token).unwrap();

    let result = client.whoami().await;
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 403),
        other => panic!("expected 403, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn field_values_endpoint() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.field_values("service", Some(5), None).await.unwrap();
    assert_eq!(resp.field, "service");
    assert!(!resp.values.is_empty());
    assert!(resp.values.iter().any(|v| v == "nginx"));
}

#[tokio::test(flavor = "multi_thread")]
async fn field_values_cached() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp1 = client.field_values("service", None, None).await.unwrap();
    assert!(!resp1.cached);

    let resp2 = client.field_values("service", None, None).await.unwrap();
    assert!(resp2.cached);
}

#[tokio::test(flavor = "multi_thread")]
async fn field_values_invalid_field_name() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = client.field_values("bad;name", None, None).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 400),
        other => panic!("expected 400, got: {other:?}"),
    }
}

// -- request ID tests --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn response_includes_ulid_request_id() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();

    let header = resp
        .headers()
        .get("x-request-id")
        .expect("missing x-request-id header");
    let value = header.to_str().unwrap();
    assert_eq!(value.len(), 26, "request ID should be a 26-char ULID");
    assert!(
        value.chars().all(|c| c.is_ascii_alphanumeric()),
        "request ID should be alphanumeric crockford base32"
    );
}

// -- reader role restriction tests -------------------------------------------

fn assert_403<T: std::fmt::Debug>(result: Result<T, trawl_client::ClientError>) {
    let err = result.expect_err("expected 403 but got success");
    match err {
        trawl_client::ClientError::Server { status, error } => {
            assert_eq!(status, 403, "expected 403, got {status}");
            assert_eq!(error.code, trawl_api::ErrorCode::Forbidden);
            assert_eq!(error.message, "insufficient permissions");
        }
        other => panic!("expected Server error with 403, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn validate_rejects_reader() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_403(reader.validate("* | head 1").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_queries_reject_reader() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_403(reader.list_saved().await);
    assert_403(reader.create_saved("test", "* | head 1").await);
    assert_403(reader.update_saved(1, "* | head 2").await);
    assert_403(reader.delete_saved(1).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn export_rejects_reader() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_403(
        reader
            .export("* | head 1", trawl_api::ExportFormat::Csv, None)
            .await,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reader_can_query_and_view_history() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    let resp = reader
        .query_paginated("* | head 1", None, None)
        .await
        .unwrap();
    assert!(!resp.result.columns.is_empty());

    reader.schema().await.unwrap();
    reader.history(Some(10), None).await.unwrap();
}

// ---------------------------------------------------------------------------
// Runs stats endpoint
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn runs_stats_empty() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let stats = client.runs_stats().await.unwrap();
    assert_eq!(stats.total_runs, 0);
    assert_eq!(stats.success_count, 0);
    assert_eq!(stats.error_count, 0);
    assert_eq!(stats.timeout_count, 0);
    assert_eq!(stats.avg_duration_ms, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_stats_rejects_reader() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_403(reader.runs_stats().await);
}

// ---------------------------------------------------------------------------
// Schedule create/update
// ---------------------------------------------------------------------------

/// `enabled` has to mean the same thing on both halves of the PUT. The
/// create path used to hardcode TRUE, so a client that asked for a schedule
/// it would enable later got one that fired on the next tick instead.
#[tokio::test(flavor = "multi_thread")]
async fn put_schedule_honours_enabled_on_create() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("disabled-on-create", "* | head 3")
        .await
        .unwrap();

    // The saved query has no schedule yet, so this PUT takes the CREATE arm.
    let created = client
        .set_schedule(saved.id, "1h", None, false, None, None)
        .await
        .unwrap();
    assert!(
        !created.enabled,
        "a create-path PUT must return the schedule it was asked for"
    );
    assert!(
        !client.get_schedule(saved.id).await.unwrap().enabled,
        "and the row it wrote must be disabled too"
    );

    // The row says disabled; the proof is that a tick does not run it. A
    // schedule is created due at its own creation instant, so an enabled one
    // would be claimed by the very next tick.
    let key_store = KeyStore::from_pool(server.fleet_pool.clone());
    for handle in trawl_server::scheduler::poll_and_execute(
        &server.state.storage.schedule,
        &key_store,
        &server.state.query.pool,
        &trawl_server::config::SchedulerConfig::default(),
        30,
        chrono::Utc::now(),
    )
    .await
    {
        handle.await.expect("a scheduled execution must not panic");
    }

    let runs = client
        .list_report_runs(saved.id, Some(10), None)
        .await
        .unwrap();
    assert!(
        runs.runs.is_empty(),
        "a disabled schedule must not be claimed by a tick, got {:?}",
        runs.runs
    );
}

// ---------------------------------------------------------------------------
// Trigger run endpoint
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn trigger_run_requires_schedule() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("trigger-test", "* | head 5")
        .await
        .unwrap();

    let err = client
        .trigger_run(saved.id)
        .await
        .expect_err("expected error");
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 400);
        }
        other => panic!("expected 400, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trigger_run_starts_execution() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("trigger-exec", "* | head 3")
        .await
        .unwrap();
    client
        .set_schedule(saved.id, "1h", None, true, None, None)
        .await
        .unwrap();

    let summary = client.trigger_run(saved.id).await.unwrap();
    assert_eq!(summary.status, "running");
    assert_eq!(summary.query, "* | head 3");

    // Wait briefly for execution to complete (test fixtures are tiny).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let runs = client
        .list_report_runs(saved.id, Some(10), None)
        .await
        .unwrap();
    assert_eq!(runs.runs.len(), 1);
    assert_eq!(runs.runs[0].status, "success");

    let stats = client.runs_stats().await.unwrap();
    assert_eq!(stats.total_runs, 1);
    assert_eq!(stats.success_count, 1);
    assert!(stats.avg_duration_ms.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn trigger_run_rejects_reader() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_403(reader.trigger_run(1).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn trigger_run_rejects_when_max_runs_reached() {
    use trawl_server::store::schedule::{RunClaim, ScheduleStore};

    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("trigger-cap", "* | head 3")
        .await
        .unwrap();
    let schedule = client
        .set_schedule(saved.id, "1h", Some(1), true, None, None)
        .await
        .unwrap();

    // Seed a run directly so the schedule is already at its max_runs=1 cap;
    // count(*) >= max_runs short-circuits the claim before the insert.
    let store = ScheduleStore::new(common::app_pool(&server.app_db_url).await);
    let seeded = store
        .claim_run(schedule.id, saved.id, "* | head 3", None, None)
        .await
        .unwrap();
    assert!(
        matches!(seeded, RunClaim::Started(_)),
        "seeding the cap must start a run, got {seeded:?}"
    );

    let err = client
        .trigger_run(saved.id)
        .await
        .expect_err("expected max-runs error");
    match err {
        trawl_client::ClientError::Server { status, error } => {
            assert_eq!(status, 400);
            assert!(
                error.message.contains("max runs reached"),
                "unexpected message: {}",
                error.message
            );
        }
        other => panic!("expected 400, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trigger_run_rejects_when_already_running() {
    use trawl_server::store::schedule::{RunClaim, ScheduleStore};

    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("trigger-busy", "* | head 3")
        .await
        .unwrap();
    // No max_runs cap, so the in-progress guard is what rejects the trigger.
    let schedule = client
        .set_schedule(saved.id, "1h", None, true, None, None)
        .await
        .unwrap();

    // Seed an in-progress run directly, avoiding a race with the background
    // task the happy-path trigger spawns.
    let store = ScheduleStore::new(common::app_pool(&server.app_db_url).await);
    let seeded = store
        .claim_run(schedule.id, saved.id, "* | head 3", None, None)
        .await
        .unwrap();
    assert!(
        matches!(seeded, RunClaim::Started(_)),
        "seeding an in-progress run must start it, got {seeded:?}"
    );

    let err = client
        .trigger_run(saved.id)
        .await
        .expect_err("expected already-running error");
    match err {
        trawl_client::ClientError::Server { status, error } => {
            assert_eq!(status, 400);
            assert!(
                error.message.contains("already in progress"),
                "unexpected message: {}",
                error.message
            );
        }
        other => panic!("expected 400, got: {other:?}"),
    }
}

/// Poll until `saved_id` has `n` finished runs, returning them newest first.
///
/// `trigger_run` returns as soon as the run row exists; the execution is a
/// spawned task. Polling beats a fixed sleep: a slow machine gets more time,
/// a fast one does not pay for it.
async fn wait_for_finished_runs(
    client: &HttpClient,
    saved_id: i64,
    n: usize,
) -> Vec<trawl_api::ReportRunSummary> {
    for _ in 0..100 {
        let runs = client
            .list_report_runs(saved_id, Some(10), None)
            .await
            .unwrap()
            .runs;
        if runs.len() >= n && runs.iter().all(|r| r.status != "running") {
            return runs;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{n} finished runs never appeared for saved query {saved_id}");
}

/// ADR-0018 ruling 13, end to end: a report run that finds nothing is
/// recorded with its result, is served by the run endpoint, and is the run
/// `run=latest` answers from.
///
/// The old empty-result path wrote neither a parquet file nor a blob, and
/// `run=latest` skipped runs with no file, so a report that had just gone
/// quiet kept answering with the PREVIOUS run's rows. Nothing said the data
/// was stale.
#[tokio::test(flavor = "multi_thread")]
async fn from_saved_latest_answers_a_zero_row_run_not_an_older_one() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Run one has rows: the fixture corpus carries nginx events.
    let saved = client
        .create_saved("zero_row_report", "service=nginx | table service")
        .await
        .unwrap();
    client
        .set_schedule(saved.id, "1h", None, true, None, None)
        .await
        .unwrap();
    client.trigger_run(saved.id).await.unwrap();
    let first = wait_for_finished_runs(&client, saved.id, 1).await;
    assert_eq!(first[0].status, "success");
    assert!(
        first[0].row_count.unwrap_or(0) > 0,
        "the first run must have rows to be worth mistaking for the latest: {:?}",
        first[0]
    );

    // Run two is the same report over a query that now matches nothing. The
    // filter is on `host`, not `service`: a service filter prunes the path
    // glob, and a run that reaches no file at all has no columns either (the
    // `(SELECT 1 WHERE FALSE)` case). This is the ordinary one: the files
    // are read, and no row survives the filter.
    client
        .update_saved(
            saved.id,
            "service=nginx host=no_such_host | table service, host",
        )
        .await
        .unwrap();
    client.trigger_run(saved.id).await.unwrap();
    let runs = wait_for_finished_runs(&client, saved.id, 2).await;
    let newest = &runs[0];
    assert_eq!(newest.status, "success", "{newest:?}");
    assert_eq!(newest.row_count, Some(0), "{newest:?}");
    assert!(
        newest.result_path.is_none(),
        "a zero-row run has no parquet to point at: {newest:?}"
    );

    // The zero-row run is served with its columns, not as a null result.
    let fetched = client
        .get_report_run(saved.id, newest.id)
        .await
        .unwrap()
        .result
        .expect("a zero-row run still carries a result");
    assert_eq!(fetched.rows.len(), 0, "{fetched:?}");
    assert!(
        !fetched.columns.is_empty(),
        "the blob keeps the column names: {fetched:?}"
    );

    // And it is what `run=latest` resolves. Reading the older run would
    // count its rows here.
    let counted = client
        .query_paginated(
            "| from saved zero_row_report run=latest | stats count()",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(counted.result.rows.len(), 1, "{:?}", counted.result);
    assert_eq!(
        counted.result.rows[0][0].to_string(),
        "0",
        "the newest run found nothing: {:?}",
        counted.result
    );

    let listed = client
        .query_paginated(
            "| from saved zero_row_report run=latest | table service",
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        listed.result.rows.is_empty(),
        "no rows to list: {:?}",
        listed.result
    );
}

// ---------------------------------------------------------------------------
// Rename net (update with name)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn rename_saved_query() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client.create_saved("old-name", "* | head 1").await.unwrap();

    let updated = client
        .update_saved_with_name(saved.id, "* | head 1", Some("new-name"))
        .await
        .unwrap();
    assert_eq!(updated.name, "new-name");
    assert_eq!(updated.query, "* | head 1");

    let list = client.list_saved().await.unwrap();
    assert!(list.queries.iter().any(|q| q.name == "new-name"));
    assert!(!list.queries.iter().any(|q| q.name == "old-name"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_to_duplicate_fails() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    client
        .create_saved("taken-name", "* | head 1")
        .await
        .unwrap();
    let other = client
        .create_saved("other-name", "* | head 2")
        .await
        .unwrap();

    // Try to rename `other` to the taken name. A name conflict is a 409
    // (ADR-0004's StoreError -> HTTP table).
    let err = client
        .update_saved_with_name(other.id, "* | head 2", Some("taken-name"))
        .await
        .expect_err("expected conflict");
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 409);
        }
        other => panic!("expected 409, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// List all runs endpoint
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn list_all_runs_paginated() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.list_all_runs(Some(10), None).await.unwrap();
    assert_eq!(resp.total, 0);
    assert!(resp.runs.is_empty());
}

// ---------------------------------------------------------------------------
// repin surface
// ---------------------------------------------------------------------------

/// `SchemaWrite` gates the trigger: even the admin role (frozen conversion
/// bundle, no `schema_write`) is refused, while the schema-admin role —
/// which deliberately lacks `server_manage` — may trigger. The status
/// surface is `SchemaRead` (read-only surfaces show state without
/// offering the trigger).
#[tokio::test(flavor = "multi_thread")]
async fn repin_permission_matrix() {
    let server = setup().await;

    // Admin: the broadest standing role, server_manage included, and still
    // no schema_write.
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();
    let err = admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            true,
            false,
            trawl_client::RepinCeilings::default(),
        )
        .await
        .expect_err("admin lacks schema_write");
    match err {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 403),
        other => panic!("expected 403, got {other:?}"),
    }
    // No standing role gained the permission silently.
    let resp = admin.whoami().await.unwrap();
    assert!(
        !resp.permissions.contains(&"schema_write".to_owned()),
        "existing roles must not gain schema_write: {:?}",
        resp.permissions
    );

    // Schema-admin: may trigger (an unpinned field is a 400 — authorized,
    // then refused on the merits, with no side effect).
    let schema_admin = HttpClient::new_insecure(&server.url, &server.schema_admin_token).unwrap();
    let err = schema_admin
        .schema_repin(
            "never_pinned_field",
            "VARCHAR",
            None,
            true,
            false,
            trawl_client::RepinCeilings::default(),
        )
        .await
        .expect_err("unpinned field refuses on the merits");
    match err {
        trawl_client::ClientError::Server { status, error } => {
            assert_eq!(status, 400);
            assert!(error.message.contains("not a pinned field"), "{error:?}");
        }
        other => panic!("expected a 400, got {other:?}"),
    }
    let resp = schema_admin.whoami().await.unwrap();
    assert_eq!(resp.roles, ["trawl-schema-admin"]);
    assert!(resp.permissions.contains(&"schema_write".to_owned()));
    assert!(!resp.permissions.contains(&"server_manage".to_owned()));

    // Status: SchemaRead suffices; the reader can see, not trigger.
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();
    let status = reader.schema_repin_status().await.unwrap();
    assert!(status.job.is_none(), "no repin has run on this server");
    let err = reader
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            true,
            false,
            trawl_client::RepinCeilings::default(),
        )
        .await
        .expect_err("reader lacks schema_write");
    match err {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 403),
        other => panic!("expected 403, got {other:?}"),
    }

    // The ingest-only key holds a trawl grant but neither schema permission.
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    assert!(ingest.schema_repin_status().await.is_err());
}

/// Cancel is gated like the trigger, not like the status route: a
/// `SchemaRead` key may watch a repin and may not stop one. With nothing
/// running, an authorized ask is a 404 carrying the `no_job_running`
/// verdict rather than an error envelope — the client decodes the status
/// and the body's own outcome together, so both halves are asserted here.
#[tokio::test(flavor = "multi_thread")]
async fn repin_cancel_is_schema_write_gated_and_answers_when_idle() {
    let server = setup().await;

    for token in [&server.reader_token, &server.admin_token] {
        let client = HttpClient::new_insecure(&server.url, token).unwrap();
        let err = client
            .schema_repin_cancel()
            .await
            .expect_err("cancel needs schema_write");
        match err {
            trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 403),
            other => panic!("expected 403, got {other:?}"),
        }
    }

    let schema_admin = HttpClient::new_insecure(&server.url, &server.schema_admin_token).unwrap();
    match schema_admin.schema_repin_cancel().await.unwrap() {
        trawl_client::RepinCancel::NoJobRunning(receipt) => {
            assert_eq!(
                receipt.outcome,
                trawl_client::RepinCancelOutcome::NoJobRunning
            );
            assert!(receipt.job.is_none(), "no job to attach: {receipt:?}");
        }
        other => panic!("expected no_job_running, got {other:?}"),
    }
    // Asking changed nothing: no job row was claimed by the refusal.
    let status = schema_admin.schema_repin_status().await.unwrap();
    assert!(status.job.is_none(), "a cancel claims no job");
}

/// Contract-typed fields, unknown target types and unpinned fields refuse
/// with 400 before any job row exists — validation is side-effect-free.
#[tokio::test(flavor = "multi_thread")]
async fn repin_validation_refusals_are_side_effect_free() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.schema_admin_token).unwrap();

    for (field, to) in [
        ("_severity", "VARCHAR"), // the derived slot: contract-typed
        ("_time", "VARCHAR"),     // envelope metadata
        ("service", "BIGINT"),    // sender-asserted, still contract-typed
        ("status", "UUID"),       // not a catalog type
        // SEVERITY is an admissible target, so this row is refused for the
        // other reason a repin can be: nothing has pinned `status` on this
        // server, so there is nothing to repin.
        ("status", "SEVERITY"),
    ] {
        let err = client
            .schema_repin(
                field,
                to,
                None,
                true,
                false,
                trawl_client::RepinCeilings::default(),
            )
            .await
            .expect_err("must refuse");
        match err {
            trawl_client::ClientError::Server { status, .. } => {
                assert_eq!(status, 400, "{field} -> {to}");
            }
            other => panic!("expected 400 for {field} -> {to}, got {other:?}"),
        }
    }
    // No job row was ever claimed.
    let status = client.schema_repin_status().await.unwrap();
    assert!(status.job.is_none(), "validation refusals claim no job");
}

// ---------------------------------------------------------------------------
// pin gc surface
// ---------------------------------------------------------------------------

/// `SchemaWrite` gates pin gc exactly as it gates the repin trigger: it
/// deletes catalog rows, so a reader may not reach it and neither may the
/// admin role, which never gained the permission. The schema-admin key
/// runs it and gets a report.
#[tokio::test(flavor = "multi_thread")]
async fn gc_pins_permission_matrix() {
    let server = setup().await;

    for (who, token) in [
        ("admin", &server.admin_token),
        ("reader", &server.reader_token),
        ("analyst", &server.analyst_token),
        ("ingest", &server.ingest_token),
    ] {
        let client = HttpClient::new_insecure(&server.url, token).unwrap();
        let err = client
            .schema_gc_pins(true, Some(0))
            .await
            .expect_err("only schema_write may reclaim pins");
        match err {
            trawl_client::ClientError::Server { status, .. } => {
                assert_eq!(status, 403, "{who} must be refused");
            }
            other => panic!("expected a 403 for {who}, got {other:?}"),
        }
    }

    let ops = HttpClient::new_insecure(&server.url, &server.schema_admin_token).unwrap();
    let report = ops
        .schema_gc_pins(true, Some(0))
        .await
        .expect("schema_write runs a dry gc");
    assert!(report.dry_run);
    assert_eq!(report.deleted, 0, "a dry run deletes nothing: {report:?}");
    // The window numbers are the server's own: a zero request is accepted
    // literally and then floored at the packaged retention window.
    assert_eq!(report.requested_older_than_secs, 0);
    assert_eq!(
        report.effective_older_than_secs,
        report.retention_floor_secs.unwrap_or(0)
    );
}

/// Read from an open SSE response until `needle` shows up, or fail loud.
async fn read_sse_until(resp: &mut reqwest::Response, needle: &str) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let chunk = resp.chunk().await.unwrap().expect("stream ended early");
            bytes.extend_from_slice(&chunk);
            // The whole buffer is re-read each time rather than appended
            // as text: a chunk can split a UTF-8 codepoint, and starting
            // from the front heals it on the next chunk.
            let text = String::from_utf8_lossy(&bytes);
            // Seeing the needle is not enough to stop. A chunk boundary
            // can land inside the `data:` line carrying it, and half a
            // JSON payload does not parse — so read on until that
            // frame's terminator (`\n\n`) has arrived too.
            if let Some(at) = text.find(needle)
                && text[at..].contains("\n\n")
            {
                return text.into_owned();
            }
        }
    })
    .await;
    read.unwrap_or_else(|_| {
        panic!(
            "needle {needle:?} never arrived; got: {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

/// Every `data:` payload in the complete frames of an SSE buffer.
///
/// The read stops at a frame terminator, but the bytes after it can be
/// the first half of the next frame, so the buffer is cut at its last
/// terminator and the remainder is dropped rather than parsed.
fn sse_payloads(buf: &str) -> Vec<serde_json::Value> {
    let complete = buf.rfind("\n\n").map_or("", |end| &buf[..end + 2]);
    complete
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|json| serde_json::from_str(json).expect("an SSE payload is JSON"))
        .collect()
}

/// A read stops at the frame it was waiting for, and the bytes after it
/// can be the first half of the next one — which is not JSON yet.
#[test]
fn sse_payloads_ignores_a_half_arrived_frame() {
    let buf = "event: data\ndata: {\"a\":1}\n\nevent: data\ndata: {\"b\":";
    let payloads = sse_payloads(buf);
    assert_eq!(payloads.len(), 1, "only the complete frame is parsed");
    assert_eq!(payloads[0]["a"], 1);
}

/// ADR-0017 §3 through the real SSE loop, on a live server.
///
/// The fake-clock cases in `trawl-core`'s `live_sampling` pin the rule;
/// this pins the wiring, that `stream_query` samples one instant per
/// event on the pass-through lane and one per emitted snapshot on the
/// aggregate lane, which no in-process test of the door can observe.
/// The clock here is the real one, so the assertions are the ones a real
/// clock can carry: reads that must be EQUAL. Under any per-read or
/// per-row sampling they would differ by the microseconds it takes to
/// evaluate the next stage, which is why equality is the discriminating
/// direction.
#[tokio::test(flavor = "multi_thread")]
async fn sse_freezes_now_per_event_and_per_snapshot() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let raw = raw_client();

    // ── pass-through: one instant per event, across stages ──────────
    let mut stream = raw
        .get(format!("{}/api/v1/stream", server.url))
        .query(&[(
            "query",
            "service=now-svc | let a = now(), b = now() | let c = now()",
        )])
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .expect("open pass-through stream");
    assert!(stream.status().is_success(), "{stream:?}");

    let records = vec![
        serde_json::json!({"service": "now-svc", "host": "web-1", "message": "now-needle-1"}),
        serde_json::json!({"service": "now-svc", "host": "web-2", "message": "now-needle-2"}),
    ];
    assert_eq!(ingest.ingest(&records).await.unwrap().accepted, 2);

    let buf = read_sse_until(&mut stream, "now-needle-2").await;
    let rows = sse_payloads(&buf);
    assert_eq!(rows.len(), 2, "both events must arrive: {buf}");
    for row in &rows {
        let a = row["a"].as_str().expect("a is a timestamp text");
        assert_eq!(row["b"].as_str(), Some(a), "sibling `let` reads: {row}");
        assert_eq!(row["c"].as_str(), Some(a), "a later stage's read: {row}");
    }
    drop(stream);

    // ── aggregate: one instant per emitted snapshot ─────────────────
    let mut stream = raw
        .get(format!("{}/api/v1/stream", server.url))
        .query(&[(
            "query",
            "service=agg-svc | stats count() by host | let seen = now(), seen2 = now()",
        )])
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .expect("open aggregate stream");
    assert!(stream.status().is_success(), "{stream:?}");

    let records = vec![
        serde_json::json!({"service": "agg-svc", "host": "agg-1", "message": "one"}),
        serde_json::json!({"service": "agg-svc", "host": "agg-2", "message": "two"}),
    ];
    assert_eq!(ingest.ingest(&records).await.unwrap().accepted, 2);

    // Both groups in one snapshot is what makes "one instant per
    // snapshot" discriminable from "one instant per row".
    let buf = read_sse_until(&mut stream, "agg-2").await;
    let snapshot = sse_payloads(&buf)
        .into_iter()
        .rfind(|payload| {
            payload["rows"]
                .as_array()
                .is_some_and(|rows| rows.len() == 2)
        })
        .unwrap_or_else(|| panic!("a snapshot carrying both groups: {buf}"));
    let rows = snapshot["rows"].as_array().expect("rows is an array");
    let first = rows[0]["seen"].as_str().expect("seen is a timestamp text");
    for row in rows {
        assert_eq!(
            row["seen"].as_str(),
            Some(first),
            "every group row of one snapshot carries ONE instant: {snapshot}"
        );
        assert_eq!(
            row["seen2"].as_str(),
            Some(first),
            "…and one per row, not one per read: {snapshot}"
        );
    }
}

/// A shutdown that fires BEFORE the accept loop polls must still stop it.
///
/// The flag has to be state, not an edge. `Notify::notify_waiters()` wakes
/// whoever is registered at that instant and stores nothing, so a signal
/// landing before the loop's first `select!` (or while the accept arm's
/// body runs, between two registrations) was lost and the server kept
/// accepting after SIGTERM. Under the watch channel the late subscriber
/// reads the value that is already there.
///
/// This test hangs on the old primitive and returns in milliseconds on the
/// new one; the timeout is generous only so a loaded CI runner cannot turn
/// a pass into a flake.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_set_before_boot_stops_the_accept_loop() {
    let server = setup().await;

    let (shutdown_tx, shutdown_rx) = trawl_server::shutdown::shutdown_channel();
    shutdown_tx
        .send(true)
        .expect("this test holds the receiver");

    // Only now does the loop exist, so the send above cannot have been
    // observed by a registered waiter.
    let task = server.spawn_server_with_shutdown(shutdown_rx);

    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the accept loop must observe a shutdown that predates it")
        .expect("the serve task must not panic")
        .expect("serve returns cleanly after the drain");

    // The sender outlives the loop, so what ended it was the flag and not
    // a dropped channel.
    drop(shutdown_tx);
}

// ---------------------------------------------------------------------------
// Report windows on the write surface (ADR-0018 rulings 6, 7, 12)
// ---------------------------------------------------------------------------

/// The status and message of a refused request, or a panic naming what came
/// back instead. Both halves matter here: the status is what a client
/// branches on, the message is the whole point of a refusal that names two
/// sides.
fn refusal<T: std::fmt::Debug>(result: Result<T, trawl_client::ClientError>) -> (u16, String) {
    match result.expect_err("expected a refusal") {
        trawl_client::ClientError::Server { status, error } => (status, error.message),
        other => panic!("expected a server error, got: {other:?}"),
    }
}

/// Ruling 7: a schedule window and a query that spells its own interval are
/// two answers to one question, so attaching them is a 400 that names both.
/// The operator drops one; the server has no basis for choosing which.
#[tokio::test(flavor = "multi_thread")]
async fn schedule_window_on_a_query_with_a_time_clause_is_refused_naming_both() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    for (name, dsl, spelling) in [
        ("win-last", "service=x last=1h", "last="),
        (
            "win-earliest",
            "service=x earliest=\"2026-01-01T00:00:00Z\"",
            "earliest=",
        ),
        (
            "win-latest",
            "service=x latest=\"2026-01-01T00:00:00Z\"",
            "latest=",
        ),
    ] {
        let saved = client.create_saved(name, dsl).await.unwrap();
        let (status, message) = refusal(
            client
                .set_schedule(saved.id, "1h", None, true, Some("since_last"), None)
                .await,
        );
        assert_eq!(status, 400, "for {dsl}: {message}");
        assert!(message.contains("since_last"), "for {dsl}: {message}");
        assert!(message.contains(spelling), "for {dsl}: {message}");
        assert!(
            client.get_schedule(saved.id).await.is_err(),
            "a refused PUT leaves no schedule behind, for {dsl}"
        );
    }
}

/// The same rule from the other side: the query text is what moves, and the
/// standing window is what refuses it. A refused edit stores nothing.
#[tokio::test(flavor = "multi_thread")]
async fn adding_a_time_clause_to_a_windowed_saved_query_is_refused() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("windowed-edit", "service=x")
        .await
        .unwrap();
    client
        .set_schedule(saved.id, "1h", None, true, Some("since_last"), None)
        .await
        .expect("a query with no time clause takes a window");

    let (status, message) = refusal(
        client
            .update_saved(saved.id, "service=x earliest=\"2026-01-01T00:00:00Z\"")
            .await,
    );
    assert_eq!(status, 400, "{message}");
    assert!(message.contains("since_last"), "{message}");
    assert!(message.contains("earliest="), "{message}");

    let list = client.list_saved().await.unwrap();
    let stored = list
        .queries
        .iter()
        .find(|q| q.id == saved.id)
        .expect("the saved query survives its refused edit");
    assert_eq!(stored.query, "service=x", "a refused edit stores nothing");
}

/// Ruling 12: `from saved` reads stored report rows, not ingest events, so
/// no `_time` window applies to it — in either write direction.
#[tokio::test(flavor = "multi_thread")]
async fn from_saved_source_is_refused_a_window() {
    const FROM_SAVED: &str = "| from saved daily_rollup";

    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let reader = client
        .create_saved("reads-reports", FROM_SAVED)
        .await
        .unwrap();
    let (status, message) = refusal(
        client
            .set_schedule(reader.id, "1h", None, true, Some("2h"), None)
            .await,
    );
    assert_eq!(status, 400, "{message}");
    assert!(message.contains("from saved"), "{message}");
    assert!(message.contains("2h"), "{message}");

    let plain = client
        .create_saved("plain-then-report", "service=x")
        .await
        .unwrap();
    client
        .set_schedule(plain.id, "1h", None, true, Some("2h"), None)
        .await
        .expect("an ingest query takes a fixed window");
    let (status, message) = refusal(client.update_saved(plain.id, FROM_SAVED).await);
    assert_eq!(status, 400, "{message}");
    assert!(message.contains("from saved"), "{message}");
}

/// Ruling 6: a windowed schedule owns what its reports cover, so a manual
/// run has no bounds anyone chose. The 409 names the mode it found and the
/// route that answers "where has coverage reached".
#[tokio::test(flavor = "multi_thread")]
async fn manual_run_of_a_coverage_mode_schedule_is_409() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    for (name, window) in [("cov-since", "since_last"), ("cov-fixed", "2h")] {
        let saved = client.create_saved(name, "* | head 3").await.unwrap();
        client
            .set_schedule(saved.id, "1h", None, true, Some(window), None)
            .await
            .unwrap();

        let (status, message) = refusal(client.trigger_run(saved.id).await);
        assert_eq!(status, 409, "for {window}: {message}");
        assert!(
            message.contains(&format!("\"{window}\"")),
            "for {window}: {message}"
        );
        assert!(
            message.contains(&format!("/api/v1/saved/{}/schedule", saved.id)),
            "for {window}: {message}"
        );
        assert!(
            message.contains("covered_through") && message.contains("next_fire_at"),
            "for {window}: {message}"
        );

        let runs = client
            .list_report_runs(saved.id, Some(10), None)
            .await
            .unwrap();
        assert!(
            runs.runs.is_empty(),
            "for {window}: a refused trigger claims nothing"
        );
    }
}

/// Query mode keeps today's manual run exactly: the saved DSL verbatim, and
/// a run row that claims no coverage.
#[tokio::test(flavor = "multi_thread")]
async fn manual_run_of_a_query_mode_schedule_still_starts() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("manual-query-mode", "* | head 3")
        .await
        .unwrap();
    client
        .set_schedule(saved.id, "1h", None, true, None, None)
        .await
        .unwrap();

    let summary = client.trigger_run(saved.id).await.unwrap();
    assert_eq!(summary.status, "running");
    assert_eq!(summary.query, "* | head 3");
    assert_eq!(summary.window_kind, None);

    let runs = wait_for_finished_runs(&client, saved.id, 1).await;
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.status, "success");
    assert_eq!(run.query, "* | head 3", "the saved DSL runs verbatim");
    assert_eq!(run.window_start, None);
    assert_eq!(run.window_end, None);
    assert_eq!(run.window_truncated, None);
    assert_eq!(run.window_kind, None);
}

/// What a schedule answers about its own window, and what it refuses to be
/// given. The `lag` pair rides the window: a windowed schedule with no lag
/// reports the `"0s"` in force, query mode reports nothing at all.
#[tokio::test(flavor = "multi_thread")]
async fn schedule_response_carries_window_fields() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let tiling = client
        .create_saved("resp-tiling", "service=x")
        .await
        .unwrap();
    let resp = client
        .set_schedule(tiling.id, "1h", None, true, Some("since_last"), Some("5m"))
        .await
        .unwrap();
    assert_eq!(resp.window.as_deref(), Some("since_last"));
    assert_eq!(resp.lag.as_deref(), Some("5m"));
    assert_eq!(resp.lag_secs, Some(300));
    assert!(
        resp.covered_through.is_some(),
        "a since_last schedule is seeded owing coverage from its first window's start"
    );
    assert!(!resp.next_fire_at.is_empty());

    let tiling_covered_through = resp.covered_through;

    let fixed = client
        .create_saved("resp-fixed", "service=x")
        .await
        .unwrap();
    let resp = client
        .set_schedule(fixed.id, "1h", None, true, Some("2h"), None)
        .await
        .unwrap();
    assert_eq!(resp.window.as_deref(), Some("2h"));
    assert_eq!(resp.lag.as_deref(), Some("0s"));
    assert_eq!(resp.lag_secs, Some(0));
    assert_eq!(
        resp.covered_through, None,
        "a fixed window is re-measured from every fire and keeps no watermark"
    );

    // Query mode: the saved DSL owns its own interval, and nothing about a
    // window is reported.
    let query_mode = client
        .create_saved("resp-query-mode", "service=x last=1h")
        .await
        .unwrap();
    let resp = client
        .set_schedule(query_mode.id, "1h", None, true, None, None)
        .await
        .unwrap();
    assert_eq!(resp.window, None);
    assert_eq!(resp.lag, None);
    assert_eq!(resp.lag_secs, None);
    assert_eq!(resp.covered_through, None);
    assert!(!resp.next_fire_at.is_empty());

    let fetched = client.get_schedule(tiling.id).await.unwrap();
    assert_eq!(fetched.window.as_deref(), Some("since_last"));
    assert_eq!(fetched.lag.as_deref(), Some("5m"));
    assert_eq!(fetched.lag_secs, Some(300));
    assert_eq!(fetched.covered_through, tiling_covered_through);

    // A lag with no window shifts nothing, so it is refused rather than
    // stored as a number that changes no answer.
    let refused = client
        .create_saved("resp-refusals", "service=x")
        .await
        .unwrap();
    let (status, message) = refusal(
        client
            .set_schedule(refused.id, "1h", None, true, None, Some("5m"))
            .await,
    );
    assert_eq!(status, 400, "{message}");
    assert!(message.contains("lag"), "{message}");
    assert!(message.contains("window"), "{message}");

    // Below the 60s floor, an unknown unit, and past the ten-year cap.
    for bad in ["5s", "11y", "600w"] {
        let (status, message) = refusal(
            client
                .set_schedule(refused.id, "1h", None, true, Some(bad), None)
                .await,
        );
        assert_eq!(status, 400, "for {bad}: {message}");
        assert!(message.contains("window"), "for {bad}: {message}");
    }
}

/// Ruling 11: a run records the window it covered, and a run that had none
/// omits all four fields rather than claiming a complete window of nothing.
#[tokio::test(flavor = "multi_thread")]
async fn run_listing_carries_window_and_truncated_flag() {
    use trawl_server::report_window::{ReportWindow, WindowKind};
    use trawl_server::store::{RunClaim, RunStatus, ScheduleStore};

    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("run-windows", "service=x")
        .await
        .unwrap();
    let schedule = client
        .set_schedule(saved.id, "1h", None, true, Some("since_last"), None)
        .await
        .unwrap();

    // Seed the runs through the store: the point here is the read surface,
    // and driving a tick would put a clock between the test and its
    // assertions.
    let store = ScheduleStore::new(common::app_pool(&server.app_db_url).await);
    let instant = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    let seed = |window: Option<ReportWindow>| {
        let store = store.clone();
        async move {
            let claimed = store
                .claim_run(schedule.id, saved.id, "service=x", None, window.as_ref())
                .await
                .unwrap();
            let RunClaim::Started(id) = claimed else {
                panic!("seeding a run must start it, got {claimed:?}")
            };
            // One running row per schedule, so each seed finishes before the
            // next one is claimed.
            store
                .finish_run(id, RunStatus::Success, 5, Some(0), None, None, None)
                .await
                .unwrap();
            id
        }
    };

    let complete = seed(Some(ReportWindow {
        start: instant("2026-03-14T02:00:00Z"),
        end: instant("2026-03-14T03:00:00Z"),
        truncated: false,
        kind: WindowKind::SinceLast,
    }))
    .await;
    let truncated = seed(Some(ReportWindow {
        start: instant("2026-03-14T03:00:00Z"),
        end: instant("2026-03-15T03:00:00Z"),
        truncated: true,
        kind: WindowKind::SinceLast,
    }))
    .await;
    let legacy = seed(None).await;

    let runs = client
        .list_report_runs(saved.id, Some(10), None)
        .await
        .unwrap();
    let row = |id: i64| {
        runs.runs
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("run {id} must be listed"))
    };

    let complete = row(complete);
    assert_eq!(
        complete.window_start.as_deref(),
        Some("2026-03-14T02:00:00.000000Z")
    );
    assert_eq!(
        complete.window_end.as_deref(),
        Some("2026-03-14T03:00:00.000000Z")
    );
    assert_eq!(
        complete.window_truncated,
        Some(false),
        "false is the positive claim that the run covers everything it owed"
    );
    assert_eq!(complete.window_kind.as_deref(), Some("since_last"));

    assert_eq!(row(truncated).window_truncated, Some(true));

    let legacy = row(legacy);
    assert_eq!(legacy.window_start, None);
    assert_eq!(legacy.window_end, None);
    assert_eq!(
        legacy.window_truncated, None,
        "a run with no window claims nothing, not a complete window"
    );
    assert_eq!(legacy.window_kind, None);
}

/// The watermark outlives the mode that meant it: ADR-0018 ruling 14 keeps
/// `covered_through` across an edit so a schedule switched back to tiling
/// resumes where it stopped. Reporting the stored value regardless would
/// tell an operator that a fixed-window or query-mode schedule has coverage
/// up to some instant, which is a claim neither mode makes.
#[tokio::test(flavor = "multi_thread")]
async fn covered_through_is_reported_only_while_the_schedule_tiles() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("watermark-modes", "service=x")
        .await
        .unwrap();
    let tiling = client
        .set_schedule(saved.id, "1h", None, true, Some("since_last"), None)
        .await
        .unwrap();
    let seeded = tiling
        .covered_through
        .clone()
        .expect("since_last is seeded at the origin of owed coverage");

    let fixed = client
        .set_schedule(saved.id, "1h", None, true, Some("2h"), None)
        .await
        .unwrap();
    assert_eq!(fixed.window.as_deref(), Some("2h"));
    assert_eq!(
        fixed.covered_through, None,
        "a fixed window is re-measured from every fire and claims no watermark"
    );
    assert_eq!(
        client.get_schedule(saved.id).await.unwrap().covered_through,
        None,
        "GET agrees with the PUT that set the mode"
    );

    let query_mode = client
        .set_schedule(saved.id, "1h", None, true, None, None)
        .await
        .unwrap();
    assert_eq!(query_mode.window, None);
    assert_eq!(query_mode.covered_through, None);

    // Back to tiling. The store kept the value, so coverage resumes where
    // it stopped instead of restarting from the new anchor.
    let again = client
        .set_schedule(saved.id, "1h", None, true, Some("since_last"), None)
        .await
        .unwrap();
    assert_eq!(
        again.covered_through.as_deref(),
        Some(seeded.as_str()),
        "the watermark survived a round trip through two other modes"
    );
    assert_eq!(
        client
            .get_schedule(saved.id)
            .await
            .unwrap()
            .covered_through
            .as_deref(),
        Some(seeded.as_str())
    );
}
