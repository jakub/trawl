// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a swallowed fault's error text goes (ADR-0040, amended
//! 2026-11-09): to a DEBUG event with the retained event's `event_type`
//! and target, and nowhere else.
//!
//! Its own test binary because `.init()` installs one global subscriber per
//! process, and this one runs under a filter `retained_event_redaction`
//! cannot share: the production subscriber from
//! `telemetry::build_subscriber` under [`telemetry::DEFAULT_LOG_FILTER`]
//! with `trawl_server` raised to `debug`, with a stdout capture and a
//! telemetry WAL layer. Raising the one directive, not replacing the
//! filter, keeps the `trawld`, `storage.backend` and `auth.backend`
//! targets at the levels they ship at.

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use fleet_auth::{KeyStore, PrincipalKind};
use serde_json::Value;
use sqlx::Executor as _;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};

const ENV: &str = "default";

/// [`telemetry::DEFAULT_LOG_FILTER`] with only the `trawl_server` level
/// raised to `debug`.
fn debug_filter() -> String {
    let filter = telemetry::DEFAULT_LOG_FILTER.replace("trawl_server=info", "trawl_server=debug");
    assert_ne!(
        filter,
        telemetry::DEFAULT_LOG_FILTER,
        "the default filter no longer names trawl_server=info"
    );
    filter
}

// -- the production subscriber, captured ------------------------------------

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

/// The production subscriber under the raised directives, installed once
/// for the whole binary.
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
            &debug_filter(),
            LogSinks {
                stdout_ansi: false,
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

/// Every stdout line so far.
fn stdout_lines() -> Vec<String> {
    let bytes = sinks().stdout.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A field's value on a formatted stdout line: `name="quoted"` or
/// `name=bare`. The first span field follows `{`, every other field a
/// space.
fn field(line: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let at = line
        .match_indices(&key)
        .find(|(at, _)| matches!(line[..*at].chars().last(), Some(' ' | '{')))?
        .0
        + key.len();
    let rest = &line[at..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"').map(|end| quoted[..end].to_owned())
    } else {
        Some(
            rest.split(|c: char| c.is_whitespace() || c == '}')
                .next()
                .unwrap_or("")
                .to_owned(),
        )
    }
}

/// Whether a formatted stdout line was logged on `target`. The formatter
/// writes the target after the level and any spans, followed by `: `.
fn on_target(line: &str, target: &str) -> bool {
    line.contains(&format!(" {target}: "))
}

/// Every record the telemetry WAL holds so far, flushed first.
fn wal_records() -> Vec<Value> {
    static FLUSH: Mutex<()> = Mutex::new(());
    let _flush = FLUSH.lock().unwrap();
    sinks().wal.flush();
    read_wal(&sinks().wal_dir.path().join(ENV))
}

fn read_wal(dir: &Path) -> Vec<Value> {
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

/// The level a formatted stdout line was logged at.
fn level(line: &str) -> &'static str {
    ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"]
        .into_iter()
        .find(|level| line.split_whitespace().any(|word| word == *level))
        .unwrap_or("none")
}

/// The `history_error` fault of `retained_event_redaction`, under the
/// raised filter: its trigger's text is on a DEBUG `history_error` line, at
/// least once, and on no line of any other level or `event_type`. A WAL
/// record that holds it is that DEBUG event.
#[tokio::test(flavor = "multi_thread")]
async fn retained_event_text_is_debug_only() {
    sinks();
    let sentinel = format!(
        "SENTINEL_debug_history_{}_{:016x}",
        std::process::id(),
        rand::random::<u64>()
    );
    let server = common::setup().await;
    let key = KeyStore::from_pool(server.fleet_pool.clone())
        .create_key(
            "history-debug",
            PrincipalKind::Service,
            &common::roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let key_id = key.info.id;

    let app = common::app_pool(&server.app_db_url).await;
    app.execute(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION raise_history_sentinel() RETURNS trigger LANGUAGE plpgsql AS $fn$
         BEGIN RAISE EXCEPTION '{sentinel}'; END
         $fn$"
    )))
    .await
    .unwrap();
    app.execute(
        "CREATE TRIGGER history_sentinel BEFORE INSERT ON query_history
         FOR EACH ROW EXECUTE FUNCTION raise_history_sentinel()",
    )
    .await
    .unwrap();

    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(key.plaintext_token.to_string())
        .json(&trawl_api::QueryRequest {
            query: "service=nginx".to_owned(),
            limit: Some(10),
            offset: None,
            histogram: false,
        })
        .send()
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(
        status,
        200,
        "history is best effort: {:?}",
        response.text().await
    );

    let key = key_id.to_string();
    let holding: Vec<String> = stdout_lines()
        .into_iter()
        .filter(|line| line.contains(&sentinel))
        .collect();
    assert!(
        !holding.is_empty(),
        "the DEBUG event carries the store text: nothing on stdout holds the sentinel"
    );
    for line in &holding {
        assert_eq!(level(line), "DEBUG", "{line}");
        assert_eq!(
            field(line, "event_type").as_deref(),
            Some("history_error"),
            "{line}"
        );
        assert!(on_target(line, "trawl_server::handlers"), "{line}");
        assert_eq!(
            field(line, "key_id").as_deref(),
            Some(key.as_str()),
            "{line}"
        );
    }

    let persisted: Vec<Value> = wal_records()
        .into_iter()
        .filter(|record| record.to_string().contains(&sentinel))
        .collect();
    for record in &persisted {
        assert_eq!(record["level"], "debug", "{record}");
        assert_eq!(record["event_type"], "history_error", "{record}");
        assert_eq!(record["target"], "trawl_server::handlers", "{record}");
        assert_eq!(record["key_id"], key_id, "{record}");
    }

    // The retained WARN is still there beside it, without the text.
    let warn: Vec<String> = stdout_lines()
        .into_iter()
        .filter(|line| {
            field(line, "event_type").as_deref() == Some("history_error")
                && field(line, "key_id").as_deref() == Some(key.as_str())
                && level(line) == "WARN"
        })
        .collect();
    assert_eq!(warn.len(), 1, "{warn:#?}");
    assert_eq!(field(&warn[0], "error_class").as_deref(), Some("store"));
    assert_eq!(field(&warn[0], "cause_kind").as_deref(), Some("pg_server"));
}
