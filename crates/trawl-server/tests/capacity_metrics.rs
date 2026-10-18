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
/// the data, WAL or query scratch path or an environment with stored data.
/// The dashboard then lists the planted environment from the same cached
/// walk, which proves the scrape's sample saw what it must not leak.
///
/// The same scrape carries the ADR-0056 query series, the permits in use,
/// the pool size and the engine sample's age, with no labels at all; and
/// the headroom's spill role samples the query scratch directory, which
/// sits beside the data root and so collapses into the data row. The data
/// root is under Cargo's target temp directory rather than the system one,
/// so where those two are on different devices, a spill role still
/// sampling the system temp directory would stand in a row of its own.
/// One test, because this binary keeps its one scrape the first collection
/// in the process.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scrape per process, so every series is read in it
async fn capacity_metrics_endpoint_exposes_query_scratch_and_role_series_only() {
    const SERIES: [&str; 5] = [
        "trawl_disk_total_bytes",
        "trawl_disk_available_bytes",
        "trawl_retention_min_free_disk_bytes",
        "trawl_retention_deletions_total",
        "trawl_retention_pressure_attempts_total",
    ];
    // Cargo makes this directory at build time, and CI's test jobs run
    // binaries extracted from an archive that does not carry it.
    std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let server = common::setup_with_query_knobs(
        tmp.path(),
        common::seed_data_root(tmp.path()),
        common::DEFAULT_TEST_TIMEOUT_SECS,
        common::QueryKnobs::DEFAULT,
    )
    .await;
    let scratch_dir = server.state.query.pool.scratch().path().to_path_buf();
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
    let capacity = dashboard_capacity(&client, &server, env).await;
    let planted = capacity
        .environments
        .iter()
        .find(|capacity| capacity.env == env)
        .expect("the planted environment");
    assert_eq!(planted.stored_bytes, payload.len() as u64);
    assert_eq!(planted.oldest_date, "2026-09-20");

    for path in [&data_root, &wal_dir, &scratch_dir] {
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
    // The data row always exists; a WAL or spill row joins it only on
    // another device, so only the data role is required here.
    for series in [
        "trawl_disk_total_bytes{role=\"data\"}",
        "trawl_disk_available_bytes{role=\"data\"}",
        "trawl_retention_deletions_total{trigger=\"age\"}",
        "trawl_retention_deletions_total{trigger=\"disk_pressure\"}",
    ] {
        assert!(
            body.contains(series),
            "{series} is missing from /metrics:\n{body}"
        );
    }
    assert_data_gauges_match(&body, &capacity);

    assert_query_series(&body);
    assert_spill_is_query_scratch(&body, &capacity, &data_root, &scratch_dir);
}

/// The ADR-0056 query series: present and unlabelled, so nothing names a
/// principal. Nothing runs, so no permit is in use; the pool has the
/// fixture's size; and the sample is younger than its success window.
fn assert_query_series(body: &str) {
    let unlabelled = |name: &str| -> &str {
        assert!(
            !body
                .lines()
                .any(|line| line.starts_with(&format!("{name}{{"))),
            "{name} carries labels:\n{body}"
        );
        body.lines()
            .find_map(|line| line.strip_prefix(&format!("{name} ")))
            .map_or_else(
                || panic!("{name} is missing from /metrics:\n{body}"),
                str::trim,
            )
    };
    assert_eq!(
        unlabelled("trawl_query_pool_size"),
        common::QueryKnobs::DEFAULT
            .max_concurrent_queries
            .to_string()
    );
    assert_eq!(unlabelled("trawl_query_permits_in_use"), "0");
    let age: f64 = unlabelled("trawl_engine_sample_age_seconds")
        .parse()
        .expect("a number of seconds");
    assert!((0.0..60.0).contains(&age), "sample age {age}");
}

/// The spill role is the query scratch directory, on the data root's
/// device: it shares the data row, and no row holds it alone.
fn assert_spill_is_query_scratch(
    body: &str,
    capacity: &trawl_api::Capacity,
    data_root: &std::path::Path,
    scratch_dir: &std::path::Path,
) {
    assert_eq!(
        scratch_dir.parent(),
        data_root.parent(),
        "the default scratch is the data root's sibling"
    );
    let data_row = capacity
        .headroom
        .filesystems
        .iter()
        .find(|row| row.roles.contains(&trawl_api::FilesystemRole::Data))
        .expect("a data row");
    assert!(
        data_row.roles.contains(&trawl_api::FilesystemRole::Spill),
        "the spill role samples the scratch beside the data root: {:?}",
        capacity.headroom.filesystems
    );
    assert!(!body.contains("role=\"spill\""), "{body}");
}

/// The data-role gauges carry the retained headroom sample's numbers, which
/// the dashboard shows as the row holding the data role.
fn assert_data_gauges_match(body: &str, capacity: &trawl_api::Capacity) {
    // Whole byte counts render without a fraction, so they parse as `u64`.
    let gauge = |series: &str| -> u64 {
        body.lines()
            .find_map(|line| line.strip_prefix(series))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| panic!("{series} has a whole-byte value:\n{body}"))
    };
    let data = capacity
        .headroom
        .filesystems
        .iter()
        .find(|row| row.roles.contains(&trawl_api::FilesystemRole::Data))
        .expect("a data row");
    assert!(data.total_bytes > 0 && data.available_bytes <= data.total_bytes);
    assert_eq!(
        gauge("trawl_disk_total_bytes{role=\"data\"} "),
        data.total_bytes
    );
    assert_eq!(
        gauge("trawl_disk_available_bytes{role=\"data\"} "),
        data.available_bytes
    );
}

/// The dashboard's capacity object, once a snapshot lists `env`.
///
/// The snapshot collector reads the cache a scrape filled, so an environment
/// that walk saw appears once the next snapshot lands. Nothing else in this
/// process collects, so the headroom rows are the same retained sample the
/// scrape published. The collector ticks once a second; polling at 250ms
/// keeps a full timeout of reads inside the admin key's default 100 rpm
/// budget.
async fn dashboard_capacity(
    client: &reqwest::Client,
    server: &common::TestServer,
    env: &str,
) -> trawl_api::Capacity {
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
            if snapshot
                .capacity
                .environments
                .iter()
                .any(|capacity| capacity.env == env)
            {
                break snapshot.capacity;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("the dashboard lists the planted environment from the scrape's walk")
}
