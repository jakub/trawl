// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dashboard storage configuration before the first metrics collection.
//! Keep metrics scrapes out of this binary: they populate process-global caches.

mod common;

use std::time::Duration;

use trawl_client::HttpClient;

/// The fixture boots the real config and background snapshot collector. It does
/// not run a metrics scrape or emitter, so configured sources remain unmeasured.
/// This dedicated integration-test binary isolates the process-global storage
/// caches from metrics scrapes in other suites under both Cargo test and Nextest.
async fn assert_dashboard_storage_configuration(ingest_enabled: bool) {
    use trawl_api::StorageMeasurementStatus as Status;
    let tmp = tempfile::tempdir().unwrap();
    let server = common::setup_in_dir_with_ingest(tmp.path(), ingest_enabled).await;
    assert_eq!(server.state.ingest.wal_writer.is_some(), ingest_enabled);
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();
    let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match admin.dashboard().await {
                Ok(snapshot) => break snapshot,
                Err(trawl_client::ClientError::Server { status: 503, .. }) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("dashboard failed: {error:?}"),
            }
        }
    })
    .await
    .expect("dashboard snapshot");
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
    let mut response = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
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
    let wire: serde_json::Value = serde_json::from_str(data).unwrap();
    for field in ["wal_measurement", "parquet_measurement"] {
        assert_eq!(wire[field].as_object().unwrap().len(), 2);
        assert!(wire[field]["sample_age_secs"].is_null());
    }
    assert!(!data.contains(tmp.path().to_str().unwrap()));
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_storage_with_ingest_enabled_starts_not_sampled() {
    assert_dashboard_storage_configuration(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_storage_with_ingest_disabled_reports_wal_not_configured() {
    assert_dashboard_storage_configuration(false).await;
}
