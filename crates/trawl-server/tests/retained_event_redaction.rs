// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A retained event never carries error text trawl did not write (ADR-0040,
//! amended 2026-11-09): a fault trawld swallows keeps its level and carries
//! `error_class` and `cause_kind`, and the store, Postgres or auth text goes
//! to DEBUG, which the default filter drops.
//!
//! Its own test binary because `.init()` installs the global subscriber,
//! exactly as `trawld` does: the production subscriber from
//! `telemetry::build_subscriber` under [`telemetry::DEFAULT_LOG_FILTER`],
//! with a stdout capture and a telemetry WAL layer.
//!
//! Each case makes a real Postgres failure in a database of its own, and
//! the failure's message is a sentinel no other case uses. Every case first
//! proves the raw error really carries its sentinel, then requires the
//! retained event to exist with the class and the cause, then searches the
//! whole of every stdout line and every WAL record for the sentinel. Cases
//! share one subscriber and may run in one process, so each selects its
//! events by a field only it logs, and the sentinel search is its own.

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{TimeZone as _, Utc};
use fleet_auth::{AuthError, KeyStore, PrincipalKind};
use serde_json::Value;
use sqlx::{Executor as _, PgPool};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_server::config::SchedulerConfig;
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::pool::ExecutorPool;
use trawl_server::scheduler::poll_and_execute;
use trawl_server::store::{RunStatus, StorageState, StoreError};
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};

const ENV: &str = "default";

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

/// The production subscriber under the default directives, installed once
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
            telemetry::DEFAULT_LOG_FILTER,
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
    // Tests flush concurrently. Without this lock one test could find the
    // queue already drained by another whose WAL write has not landed yet.
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

/// A sentinel no other case, and no other run, can produce. Alphanumerics
/// and underscores only, so JSON and the formatter carry it unescaped and a
/// substring search of a whole line or record finds it.
fn sentinel(case: &str) -> String {
    format!(
        "SENTINEL_{case}_{}_{:016x}",
        std::process::id(),
        rand::random::<u64>()
    )
}

/// The sentinel is on no stdout line and in no WAL record, whatever the
/// line's level, target or fields.
fn assert_sentinel_retained_nowhere(sentinel: &str) {
    let leaked: Vec<String> = stdout_lines()
        .into_iter()
        .filter(|line| line.contains(sentinel))
        .collect();
    assert!(
        leaked.is_empty(),
        "the sentinel reached stdout: {leaked:#?}"
    );
    let persisted: Vec<Value> = wal_records()
        .into_iter()
        .filter(|record| record.to_string().contains(sentinel))
        .collect();
    assert!(
        persisted.is_empty(),
        "the sentinel reached the WAL: {persisted:#?}"
    );
}

/// The one stdout line and the one WAL record `select` keeps, each at
/// `level_name` and on `target`, with the class and the cause and no
/// `error` field.
fn assert_one_retained_event(
    what: &str,
    stdout_select: impl Fn(&str) -> bool,
    wal_select: impl Fn(&Value) -> bool,
    expect: Retained<'_>,
) {
    let lines: Vec<String> = stdout_lines()
        .into_iter()
        .filter(|line| stdout_select(line))
        .collect();
    assert_eq!(lines.len(), 1, "{what} on stdout: {lines:#?}");
    let line = &lines[0];
    assert_eq!(level(line), expect.level, "{line}");
    assert!(on_target(line, expect.target), "{line}");
    assert_eq!(
        field(line, "error_class").as_deref(),
        Some(expect.error_class),
        "{line}"
    );
    assert_eq!(
        field(line, "cause_kind").as_deref(),
        Some(expect.cause_kind),
        "{line}"
    );
    assert_eq!(field(line, "error"), None, "no error field: {line}");

    let records: Vec<Value> = wal_records()
        .into_iter()
        .filter(|record| wal_select(record))
        .collect();
    assert_eq!(records.len(), 1, "{what} persisted: {records:#?}");
    let record = &records[0];
    assert_eq!(
        record["level"],
        expect.level.to_ascii_lowercase(),
        "{record}"
    );
    assert_eq!(record["target"], expect.target, "{record}");
    assert_eq!(record["error_class"], expect.error_class, "{record}");
    assert_eq!(record["cause_kind"], expect.cause_kind, "{record}");
    assert!(record.get("error").is_none(), "no error field: {record}");
}

/// What the one retained event must say.
#[derive(Clone, Copy)]
struct Retained<'a> {
    level: &'a str,
    target: &'a str,
    error_class: &'a str,
    cause_kind: &'a str,
}

/// The Postgres error inside a store error, which must be the server's
/// answer to a `RAISE EXCEPTION` of `sentinel`.
fn assert_raised(error: &sqlx::Error, sentinel: &str) {
    let sqlx::Error::Database(db) = error else {
        panic!("not a database error: {error:?}");
    };
    assert_eq!(db.code().as_deref(), Some("P0001"), "{db:?}");
    assert_eq!(db.message(), sentinel, "{db:?}");
    assert!(error.to_string().contains(sentinel), "{error}");
}

/// A plpgsql function named `name` that raises `sentinel`, returning
/// `returns`.
async fn create_raising_function(pool: &PgPool, name: &str, returns: &str, sentinel: &str) {
    pool.execute(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION {name}() RETURNS {returns} LANGUAGE plpgsql AS $fn$
         BEGIN RAISE EXCEPTION '{sentinel}'; END
         $fn$"
    )))
    .await
    .unwrap_or_else(|e| panic!("create {name}: {e}"));
}

// -- history_error: a write fails ---------------------------------------------

/// A `query_history` insert that fails through a `BEFORE` trigger leaves
/// one WARN `history_error` with the class and the cause, and the trigger's
/// text nowhere a key holder can read it.
#[tokio::test(flavor = "multi_thread")]
async fn history_error_drops_the_store_text() {
    sinks();
    let sentinel = sentinel("history");
    let server = common::setup().await;
    let key = KeyStore::from_pool(server.fleet_pool.clone())
        .create_key(
            "history-redaction",
            PrincipalKind::Service,
            &common::roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let key_id = key.info.id;

    let app = common::app_pool(&server.app_db_url).await;
    create_raising_function(&app, "raise_history_sentinel", "trigger", &sentinel).await;
    app.execute(
        "CREATE TRIGGER history_sentinel BEFORE INSERT ON query_history
         FOR EACH ROW EXECUTE FUNCTION raise_history_sentinel()",
    )
    .await
    .unwrap();

    // The injection works: the store call the handler makes fails, and its
    // error carries the sentinel as Postgres raised it.
    let injected = server
        .state
        .storage
        .history
        .record_query(key_id, "service=nginx", 1, 0, RunStatus::Success)
        .await
        .expect_err("the trigger refuses the insert");
    let StoreError::Unavailable(raw) = &injected else {
        panic!("not the unclassified store error: {injected:?}");
    };
    assert_raised(raw, &sentinel);

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
    assert_one_retained_event(
        "history_error",
        |line| {
            field(line, "event_type").as_deref() == Some("history_error")
                && field(line, "key_id").as_deref() == Some(&key)
        },
        |record| record["event_type"] == "history_error" && record["key_id"] == key_id,
        Retained {
            level: "WARN",
            target: "trawl_server::handlers",
            error_class: "store",
            cause_kind: "pg_server",
        },
    );
    assert_sentinel_retained_nowhere(&sentinel);
}

// -- catalog_pin_purge_commit_failed: a commit fails ------------------------

/// A pin purge whose `COMMIT` fails, through a deferred constraint trigger,
/// leaves `catalog_pin_purge_commit_failed` with the class and the cause,
/// and the sqlx text nowhere a key holder can read it.
///
/// The event carries no id of its own, only the field count; no other case
/// here purges pins, so its `event_type` alone selects it.
#[tokio::test(flavor = "multi_thread")]
async fn pin_purge_commit_failure_drops_the_sqlx_text() {
    sinks();
    let sentinel = sentinel("purge");
    let app_db_url = common::create_app_database().await;
    let pool = common::app_pool(&app_db_url).await;
    let storage = StorageState::from_pool(pool.clone())
        .await
        .expect("boot the app-state database");

    let pinned = "redaction_purged_field";
    sqlx::query(
        "INSERT INTO field_types (field, duckdb_type, pinned_from) VALUES ($1, 'BIGINT', 'api')",
    )
    .bind(pinned)
    .execute(&pool)
    .await
    .unwrap();
    create_raising_function(&pool, "raise_purge_sentinel", "trigger", &sentinel).await;
    pool.execute(
        "CREATE CONSTRAINT TRIGGER purge_sentinel AFTER DELETE ON field_types
         DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW EXECUTE FUNCTION raise_purge_sentinel()",
    )
    .await
    .unwrap();

    // The injection works, and at the commit: the DELETE succeeds inside
    // the transaction, the COMMIT raises the sentinel, and the row stays.
    let mut tx = pool.begin().await.unwrap();
    let deleted = sqlx::query("DELETE FROM field_types WHERE field = $1")
        .bind(pinned)
        .execute(&mut *tx)
        .await
        .expect("the statement before the commit succeeds");
    assert_eq!(deleted.rows_affected(), 1);
    let raw = tx
        .commit()
        .await
        .expect_err("the deferred trigger fails the commit");
    assert_raised(&raw, &sentinel);
    let still_pinned = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM field_types WHERE field = $1")
            .bind(pinned)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(still_pinned().await, 1, "the failed commit kept the pin");

    let purged = storage.catalog.delete_pins(&[pinned.to_owned()]).await;
    assert!(
        matches!(purged, Err(StoreError::PurgeCommitUnknown)),
        "{purged:?}"
    );
    assert_eq!(still_pinned().await, 1, "the purge's commit failed too");

    assert_one_retained_event(
        "catalog_pin_purge_commit_failed",
        |line| {
            field(line, "event_type").as_deref() == Some("catalog_pin_purge_commit_failed")
                && field(line, "fields").as_deref() == Some("1")
        },
        |record| record["event_type"] == "catalog_pin_purge_commit_failed" && record["fields"] == 1,
        Retained {
            level: "WARN",
            target: "storage.backend",
            error_class: "store",
            cause_kind: "pg_server",
        },
    );
    // The commit failed inside its bound, so the late path never ran.
    assert!(
        !stdout_lines()
            .iter()
            .any(|line| line.contains("catalog_pin_purge_commit_late")),
        "the late path ran"
    );
    assert_sentinel_retained_nowhere(&sentinel);
}

/// Turn the key table into a view of the same name over a function that
/// raises `sentinel`, and prove the scheduler's lookup of `key_id` is what
/// reaches it: the lookup fails with the sentinel as Postgres raised it.
async fn break_key_lookup(fleet_pool: &PgPool, key_store: &KeyStore, key_id: i64, sentinel: &str) {
    fleet_pool
        .execute("ALTER TABLE api_keys RENAME TO api_keys_base")
        .await
        .unwrap();
    create_raising_function(
        fleet_pool,
        "raise_api_keys",
        "SETOF api_keys_base",
        sentinel,
    )
    .await;
    fleet_pool
        .execute("CREATE VIEW api_keys AS SELECT * FROM raise_api_keys()")
        .await
        .unwrap();

    let injected = key_store
        .get_live_key_by_id(key_id)
        .await
        .expect_err("the view raises");
    let AuthError::Database(raw) = &injected else {
        panic!("not the backend auth error: {injected:?}");
    };
    assert_raised(raw, sentinel);
}

// -- scheduler_error: an auth read fails --------------------------------------

/// A scheduler poll whose owning-key lookup fails leaves one
/// `scheduler_error` with the class and the cause, writes no `auth.backend`
/// event for a failure it swallows, and keeps the auth text out of every
/// retained record.
///
/// No `TestServer`: its scheduler loop would poll on its own and could add
/// a second `scheduler_error` for the same schedule.
#[tokio::test(flavor = "multi_thread")]
async fn owning_key_lookup_failure_drops_the_auth_text() {
    sinks();
    let sentinel = sentinel("owning_key");
    let fleet_db_url = common::create_fleet_database().await;
    let app_db_url = common::create_app_database().await;
    let fleet_pool = common::fleet_pool(&fleet_db_url).await;
    let key_store = common::fleet_keystore(&fleet_pool).await;
    let key = key_store
        .create_key(
            "scheduler-redaction",
            PrincipalKind::Service,
            &common::roles(&["trawl-analyst"]),
            None,
        )
        .await
        .expect("mint the schedule-owning key");
    let key_id = key.info.id;

    let storage = StorageState::from_pool(common::app_pool(&app_db_url).await)
        .await
        .expect("boot the app-state database");
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let pool =
        ExecutorPool::for_test(data_dir.to_str().unwrap().to_owned(), 2, 100_000, None).unwrap();

    let t0 = Utc.with_ymd_and_hms(2026, 3, 14, 3, 0, 0).unwrap();
    let saved = storage
        .saved
        .create(key_id, "redaction", "service=nginx")
        .await
        .expect("store the saved query");
    let schedule = storage
        .schedule
        .create_schedule(saved.id, key_id, 3600, None, None, 0, t0)
        .await
        .expect("create the schedule");

    break_key_lookup(&fleet_pool, &key_store, key_id, &sentinel).await;

    let before = stdout_lines().len();
    let spawned = poll_and_execute(
        &storage.schedule,
        &key_store,
        &pool,
        &SchedulerConfig::default(),
        30,
        t0 + chrono::Duration::seconds(1),
    )
    .await;
    assert!(spawned.is_empty(), "an unusable key claims nothing");
    let during: Vec<String> = stdout_lines().split_off(before);

    let (schedule_field, key_field) = (schedule.id.to_string(), key_id.to_string());
    assert_one_retained_event(
        "scheduler_error",
        |line| {
            field(line, "event_type").as_deref() == Some("scheduler_error")
                && field(line, "schedule_id").as_deref() == Some(&schedule_field)
                && field(line, "key_id").as_deref() == Some(&key_field)
        },
        |record| {
            record["event_type"] == "scheduler_error"
                && record["schedule_id"] == schedule.id
                && record["key_id"] == key_id
        },
        Retained {
            level: "WARN",
            target: "trawl_server::scheduler",
            error_class: "service_unavailable",
            cause_kind: "pg_server",
        },
    );
    let auth_backend: Vec<&String> = during
        .iter()
        .filter(|line| on_target(line, "auth.backend"))
        .collect();
    assert!(
        auth_backend.is_empty(),
        "the swallowed failure wrote an auth.backend event: {auth_backend:#?}"
    );
    assert_sentinel_retained_nowhere(&sentinel);

    // The fire cursor did not move: the skip is the only thing that ran.
    let after = storage
        .schedule
        .get_schedule_for_saved_query(saved.id, key_id)
        .await
        .unwrap()
        .expect("the schedule still exists");
    assert_eq!(after.next_fire_at, schedule.next_fire_at);
}
