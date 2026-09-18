// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dashboard storage configuration before the first metrics collection.
//! Keep metrics scrapes out of this binary: they populate process-global caches.

mod common;

use std::time::Duration;

/// The fixture boots the real config and background snapshot collector. It does
/// not run a metrics scrape or emitter, so configured sources remain unmeasured.
/// This dedicated integration-test binary isolates the process-global storage
/// caches from metrics scrapes in other suites under both Cargo test and Nextest.
async fn assert_dashboard_storage_configuration(ingest_enabled: bool) {
    use trawl_api::StorageMeasurementStatus as Status;
    let tmp = tempfile::tempdir().unwrap();
    let server = common::setup_in_dir_with_ingest(tmp.path(), ingest_enabled).await;
    assert_eq!(server.state.ingest.wal_writer.is_some(), ingest_enabled);
    let (cert_path, _) = common::ensure_test_cert();
    let certificate = reqwest::Certificate::from_pem(&std::fs::read(cert_path).unwrap()).unwrap();
    let client = reqwest::Client::builder()
        .add_root_certificate(certificate)
        .build()
        .unwrap();
    let get_data = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = client
                .get(format!("{}/api/v1/dashboard", server.url))
                .header("authorization", format!("Bearer {}", server.admin_token))
                .send()
                .await
                .unwrap();
            if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            assert_eq!(response.status(), 200);
            break response.text().await.unwrap();
        }
    })
    .await
    .expect("dashboard snapshot");
    let snapshot: trawl_api::DashboardSnapshot = serde_json::from_str(&get_data).unwrap();
    let expected_wal = if ingest_enabled {
        Status::NotSampled
    } else {
        Status::NotConfigured
    };
    let assert_measurements = |snapshot: &trawl_api::DashboardSnapshot| {
        assert_eq!(snapshot.wal_measurement.status, expected_wal);
        assert_eq!(snapshot.parquet_measurement.status, Status::NotSampled);
        assert_eq!(snapshot.wal_measurement.sample_age_secs, None);
        assert_eq!(snapshot.parquet_measurement.sample_age_secs, None);
        assert_eq!((snapshot.wal_files, snapshot.wal_bytes), (0, 0));
        assert_eq!((snapshot.parquet_files, snapshot.parquet_bytes), (0, 0));
    };
    assert_measurements(&snapshot);
    let mut response = client
        .get(format!("{}/api/v1/dashboard/stream", server.url))
        .header("authorization", format!("Bearer {}", server.admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let frame = tokio::time::timeout(Duration::from_secs(10), async {
        let mut buffer = String::new();
        loop {
            let chunk = response.chunk().await.unwrap().expect("stream ended early");
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            if let Some(start) = buffer.find("event: stats")
                && let Some(end) = buffer[start..].find("\n\n")
            {
                break buffer[start..start + end].to_owned();
            }
        }
    })
    .await
    .expect("stats frame");
    let data = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let streamed: trawl_api::DashboardSnapshot = serde_json::from_str(data).unwrap();
    assert_measurements(&streamed);
    // GET and SSE serialize the same metadata objects and never filesystem paths.
    for payload in [get_data.as_str(), data] {
        let wire: serde_json::Value = serde_json::from_str(payload).unwrap();
        for field in ["wal_measurement", "parquet_measurement"] {
            assert_eq!(wire[field].as_object().unwrap().len(), 2);
            assert!(wire[field]["sample_age_secs"].is_null());
        }
        assert!(!payload.contains(tmp.path().to_str().unwrap()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_storage_with_ingest_enabled_starts_not_sampled() {
    assert_dashboard_storage_configuration(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_storage_with_ingest_disabled_reports_wal_not_configured() {
    assert_dashboard_storage_configuration(false).await;
}
