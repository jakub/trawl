// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ADR-0042 capacity series on `/metrics`, scraped from a data root this
//! binary alone has walked.
//!
//! The Parquet and headroom caches are process-global and a sample lives for
//! the cache TTL. Under Cargo test every test in a binary shares one process,
//! so a scrape in a shared binary can reuse a sample another test took from a
//! different data root. A leak check against that sample would pass without
//! the planted environment ever being walked. This dedicated integration-test
//! binary keeps its one scrape the first collection in the process under both
//! Cargo test and Nextest. Keep other metrics scrapes out of it.

mod common;

use std::time::Duration;

/// The ADR-0042 series on the real, unauthenticated `/metrics` endpoint:
/// headroom per role, the configured floor, and retention's two counters.
/// Their labels are `role` and `trigger` only, and nothing in the body names
/// the data or WAL path or an environment with stored data. The dashboard
/// then lists the planted environment from the same cached walk, which
/// proves the scrape's sample saw what it must not leak.
#[tokio::test(flavor = "multi_thread")]
async fn capacity_metrics_endpoint_exposes_role_series_only() {
    const SERIES: [&str; 5] = [
        "trawl_disk_total_bytes",
        "trawl_disk_available_bytes",
        "trawl_retention_min_free_disk_bytes",
        "trawl_retention_deletions_total",
        "trawl_retention_pressure_attempts_total",
    ];
    let server = common::setup().await;
    let glob = server.state.query.pool.fallback_glob().to_string();
    let data_root = std::path::PathBuf::from(&glob[..glob.find('*').expect("a glob")])
        .components()
        .collect::<std::path::PathBuf>();
    let wal_dir = server
        .state
        .ingest
        .wal_writer
        .as_ref()
        .expect("the harness ingests")
        .dir()
        .to_path_buf();
    // An environment with stored data, so its name is there to leak.
    let env = "plantedcapacityenv";
    let payload = "0123456789";
    let partition = data_root.join(env).join("2026-09-20");
    std::fs::create_dir_all(&partition).unwrap();
    std::fs::write(partition.join("x.parquet"), payload).unwrap();

    let client = common::harness_client_builder().build().unwrap();
    let body = client
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Positive control: the sample this scrape used walked the planted
    // environment.
    let planted = dashboard_environment(&client, &server, env).await;
    assert_eq!(planted.stored_bytes, payload.len() as u64);
    assert_eq!(planted.oldest_date, "2026-09-20");

    for path in [&data_root, &wal_dir] {
        let path = path.to_str().unwrap();
        assert!(!body.contains(path), "{path} leaks into /metrics:\n{body}");
    }
    assert!(!body.contains(env), "{env} leaks into /metrics:\n{body}");
    for name in SERIES {
        let lines: Vec<&str> = body
            .lines()
            .filter(|line| {
                line.strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with([' ', '{']))
            })
            .collect();
        assert!(
            !lines.is_empty(),
            "{name} is missing from /metrics:\n{body}"
        );
        for line in lines {
            let labels = line[name.len()..]
                .strip_prefix('{')
                .and_then(|rest| rest.split_once('}'))
                .map_or("", |(labels, _)| labels);
            for label in labels.split(',').filter(|label| !label.is_empty()) {
                let (key, _) = label.split_once('=').expect("key=value");
                assert!(
                    matches!(key, "role" | "trigger"),
                    "{name} carries label {key}: {line}"
                );
            }
        }
    }
    let floor = body
        .lines()
        .find_map(|line| line.strip_prefix("trawl_retention_min_free_disk_bytes "))
        .expect("an unlabelled floor gauge");
    assert_eq!(
        floor.trim(),
        // The harness keeps the default floor, a whole number of bytes.
        trawl_config::RetentionConfig::default()
            .min_free_disk_bytes
            .to_string()
    );
    assert!(
        body.contains("trawl_disk_total_bytes{role=\"data\"}"),
        "the data row is a series:\n{body}"
    );
}

/// The dashboard's capacity row for `env`, once a snapshot lists it.
///
/// The snapshot collector reads the cache a scrape filled, so an environment
/// that walk saw appears once the next snapshot lands. Nothing else in this
/// process collects. The collector ticks once a second; polling at 250ms
/// keeps a full timeout of reads inside the admin key's default 100 rpm
/// budget.
async fn dashboard_environment(
    client: &reqwest::Client,
    server: &common::TestServer,
    env: &str,
) -> trawl_api::EnvironmentCapacity {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = client
                .get(format!("{}/api/v1/dashboard", server.url))
                .header("authorization", format!("Bearer {}", server.admin_token))
                .send()
                .await
                .unwrap();
            if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            assert_eq!(response.status(), 200);
            let snapshot: trawl_api::DashboardSnapshot = response.json().await.unwrap();
            if let Some(capacity) = snapshot
                .capacity
                .environments
                .into_iter()
                .find(|capacity| capacity.env == env)
            {
                break capacity;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("the dashboard lists the planted environment from the scrape's walk")
}
