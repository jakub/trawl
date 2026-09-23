// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A request's span context reaches both of trawld's sinks, whatever the
//! same worker thread asked the subscriber before.
//!
//! sqlx's statement logger asks `log::log_enabled!(target: "sqlx::query",
//! Warn)` after every statement slower than its threshold, and the `log`
//! bridge answers by asking the tracing dispatcher. trawld's filters refuse
//! that target, so the question is asked and nothing is ever dispatched.
//! The request span or event that the same thread opens next must still
//! reach stdout and the WAL, with the span's `request_id`.
//!
//! This is its own test binary because `.init()` installs the global
//! subscriber and the global `log` bridge, exactly as `trawld` does. Every
//! test here shares that one subscriber and tells its own events apart by
//! a unique marker.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::Instrument as _;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};

const ENV: &str = "default";
const REQUEST_TARGET: &str = "trawl_server::transport::http";
const HANDLER_TARGET: &str = "trawl_server::handlers";

/// Everything the global subscriber writes to stdout.
#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Stdout {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct Sinks {
    stdout: Stdout,
    wal: WalLayer,
    wal_dir: tempfile::TempDir,
}

/// The production subscriber, built by `telemetry::build_subscriber` under
/// the default directives and installed once for the whole binary.
fn sinks() -> &'static Sinks {
    static SINKS: OnceLock<Sinks> = OnceLock::new();
    SINKS.get_or_init(|| {
        let wal_dir = tempfile::tempdir().unwrap();
        let handle = WalHandle::new();
        handle.set(Arc::new(WalWriter::new(wal_dir.path().to_path_buf())), ENV);
        let wal = WalLayer::new_with_buffer_cap(
            handle,
            &[ENV.to_owned()],
            ENV,
            Arc::new(Derivation::defaults()),
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
        );
        let stdout = Stdout::default();
        let (subscriber, _) = telemetry::build_subscriber(
            telemetry::DEFAULT_LOG_FILTER,
            LogSinks {
                stdout: Some(stdout.clone()),
                wal: Some(wal.clone()),
                file_log: false,
            },
        );
        subscriber.init();
        Sinks {
            stdout,
            wal,
            wal_dir,
        }
    })
}

/// The stdout line that carries `marker`, if one was written.
fn stdout_line(marker: &str) -> Option<String> {
    let bytes = sinks().stdout.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .find(|line| line.contains(marker))
        .map(str::to_owned)
}

/// The persisted telemetry record whose `marker` field is `marker`, if one
/// reached the WAL.
fn wal_record(marker: &str) -> Option<serde_json::Value> {
    // Tests flush concurrently. Without this lock one test could find the
    // queue already drained by another whose WAL write has not landed yet.
    static FLUSH: Mutex<()> = Mutex::new(());
    let _flush = FLUSH.lock().unwrap();
    sinks().wal.flush();
    wal_records(&sinks().wal_dir.path().join(ENV))
        .into_iter()
        .find(|record| record["marker"] == marker)
}

fn wal_records(dir: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ndjson"))
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// What sqlx's `QueryLogger` does after a slow statement (sqlx-core
/// `logger.rs`): ask the `log` bridge, then tracing, whether `sqlx::query`
/// WARN is wanted, and emit through a `tracing::event!` callsite if either
/// says yes. trawld's filters refuse the target, so that callsite's interest
/// is `never` and nothing is dispatched after the question.
fn sqlx_slow_statement_probe() {
    if log::log_enabled!(target: "sqlx::query", log::Level::Warn)
        || tracing::enabled!(target: "sqlx::query", tracing::Level::WARN)
    {
        tracing::event!(
            target: "sqlx::query",
            tracing::Level::WARN,
            elapsed_secs = 0.0,
            "slow statement: execution time exceeded alert threshold"
        );
    }
}

/// Assert that the event marked `marker` reached stdout and the WAL, and
/// that both carry the request span's `request_id`.
fn assert_in_both_sinks_with_request_id(marker: &str, request_id: &str) {
    let mut failures = Vec::new();
    match stdout_line(marker) {
        None => failures.push(format!("stdout never received the event marked {marker}")),
        Some(line) if !line.contains(request_id) => failures.push(format!(
            "the stdout line lost the http_request span (request_id {request_id}): {line}"
        )),
        Some(_) => {}
    }
    match wal_record(marker) {
        None => failures.push(format!("the WAL never received the event marked {marker}")),
        Some(record) if record["request_id"] != request_id => failures.push(format!(
            "the telemetry record lost the http_request span (request_id {request_id}): {record}"
        )),
        Some(_) => {}
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_log_enabled_probe_does_not_strip_the_next_request_span() {
    sinks();
    let request_id = "01K5SPANPROBE0000000000001";
    let marker = "span-after-probe";

    sqlx_slow_statement_probe();
    let span = tracing::info_span!(
        target: REQUEST_TARGET,
        "http_request",
        request_id,
        path = "/api/v1/query",
    );
    span.in_scope(|| {
        tracing::warn!(
            target: HANDLER_TARGET,
            event_type = "query_failed",
            marker,
            "query failed: bad request"
        );
    });

    assert_in_both_sinks_with_request_id(marker, request_id);
}

#[test]
fn a_log_enabled_probe_does_not_drop_the_next_event() {
    sinks();
    let request_id = "01K5SPANPROBE0000000000002";
    let marker = "event-after-probe";

    let span = tracing::info_span!(
        target: REQUEST_TARGET,
        "http_request",
        request_id,
        path = "/api/v1/query",
    );
    span.in_scope(|| {
        sqlx_slow_statement_probe();
        tracing::error!(
            target: HANDLER_TARGET,
            event_type = "internal_error",
            marker,
            "internal error"
        );
    });

    assert_in_both_sinks_with_request_id(marker, request_id);
}

/// Guard for the other suspect: a request future that migrates between
/// runtime workers across awaits must still enter and exit its span on
/// one thread per poll, so every event it emits carries its own span and
/// no other request's.
#[test]
fn a_request_future_keeps_its_span_across_worker_threads() {
    sinks();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let requests: Vec<(String, String)> = (0..64)
        .map(|i| (format!("01K5SPANMIGRATE{i:011}"), format!("migrate-{i:03}")))
        .collect();

    runtime.block_on(async {
        let tasks: Vec<_> = requests
            .iter()
            .cloned()
            .map(|(request_id, marker)| {
                let span = tracing::info_span!(
                    target: REQUEST_TARGET,
                    "http_request",
                    request_id = %request_id,
                );
                tokio::spawn(
                    async move {
                        for _ in 0..4 {
                            tokio::task::yield_now().await;
                        }
                        tracing::warn!(
                            target: HANDLER_TARGET,
                            event_type = "query_failed",
                            marker = %marker,
                            "query failed: bad request"
                        );
                    }
                    .instrument(span),
                )
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
    });

    for (request_id, marker) in &requests {
        let line = stdout_line(marker)
            .unwrap_or_else(|| panic!("stdout never received the event marked {marker}"));
        assert!(
            line.contains(request_id.as_str()),
            "the stdout line carries the wrong span (want {request_id}): {line}"
        );
        let record = wal_record(marker)
            .unwrap_or_else(|| panic!("the WAL never received the event marked {marker}"));
        assert_eq!(record["request_id"], request_id.as_str(), "{record}");
    }
}

/// The same failure end to end: real sqlx connections whose every
/// statement counts as slow, so sqlx's logger runs the `log_enabled!` probe
/// after each one, on whichever runtime worker polled it. The request that
/// worker opens next must keep its span in both sinks. Plain connections,
/// not a pool: a pool has one owner per process (ADR-0021 ruling 3), and
/// the probe lives in the connection's statement logger either way.
#[test]
fn a_slow_sqlx_statement_does_not_strip_the_next_request_span() {
    use std::str::FromStr as _;

    use sqlx::{ConnectOptions as _, Connection as _};

    const CONNECTIONS: usize = 4;
    const REQUESTS_PER_CONNECTION: usize = 8;

    sinks();
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for the pg-backed suites; \
         these tests fail rather than skip",
    );
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)
        .unwrap()
        .log_slow_statements(log::LevelFilter::Warn, std::time::Duration::ZERO);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let requests: Vec<Vec<(String, String)>> = (0..CONNECTIONS)
        .map(|c| {
            (0..REQUESTS_PER_CONNECTION)
                .map(|r| {
                    (
                        format!("01K5SPANSQLX{c:07}{r:07}"),
                        format!("sqlx-slow-{c}-{r}"),
                    )
                })
                .collect()
        })
        .collect();

    runtime.block_on(async {
        let tasks: Vec<_> = requests
            .iter()
            .cloned()
            .map(|connection_requests| {
                let options = options.clone();
                tokio::spawn(async move {
                    let mut conn = sqlx::postgres::PgConnection::connect_with(&options)
                        .await
                        .unwrap();
                    for (request_id, marker) in connection_requests {
                        sqlx::query("SELECT 1").execute(&mut conn).await.unwrap();
                        let span = tracing::info_span!(
                            target: REQUEST_TARGET,
                            "http_request",
                            request_id = %request_id,
                        );
                        async {
                            tracing::warn!(
                                target: HANDLER_TARGET,
                                event_type = "query_failed",
                                marker = %marker,
                                "query failed: engine error"
                            );
                        }
                        .instrument(span)
                        .await;
                    }
                    conn.close().await.unwrap();
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
    });

    for (request_id, marker) in requests.iter().flatten() {
        assert_in_both_sinks_with_request_id(marker, request_id);
    }
}
