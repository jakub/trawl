//! Real-daemon startup admission, filesystem preservation, and recovery.
//!
//! Unlike the in-process API fixtures, these tests must execute `main` and
//! its production connectors. The exclusive postgres-group reservation in
//! nextest accounts for those production pools. Every database and path is
//! fixture-owned; no daemon profile or ambient configuration is inherited.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sqlx::{Connection as _, Executor as _, PgConnection, PgPool};
use trawl_server::repin::marker::{RepinMarker, RepinPhase, write_marker};

const LOCK_KEY: i64 = 0x0074_7261_776c_2131;

struct Fixture {
    root: tempfile::TempDir,
    fleet_url: String,
    app_url: String,
    fleet: PgPool,
    app: PgPool,
    internal_telemetry: bool,
    log_file: Option<PathBuf>,
    compaction_interval_secs: Option<u64>,
    /// `TRAWL_TEST_CRASH_AT` for the next spawn: the named publish or
    /// recovery point parks the daemon there so the test can SIGKILL it.
    crash_at: Option<&'static str>,
    /// Enable the syslog listeners (ephemeral ports) and the scheduler.
    producers: bool,
}

impl Fixture {
    async fn new() -> Self {
        std::env::var("DATABASE_URL").expect("select an owned PostgreSQL cluster explicitly");
        let fleet_url = common::create_fleet_database().await;
        let app_url = common::create_app_database().await;
        let fleet = common::fixture_pool(&fleet_url, 1).await;
        let app = common::fixture_pool(&app_url, 1).await;
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/startup");
        std::fs::create_dir_all(&parent).unwrap();
        Self {
            root: tempfile::tempdir_in(parent).unwrap(),
            fleet_url,
            app_url,
            fleet,
            app,
            internal_telemetry: false,
            log_file: None,
            compaction_interval_secs: None,
            crash_at: None,
            producers: false,
        }
    }

    fn storage_root(&self) -> PathBuf {
        self.root.path().join("storage")
    }

    fn data(&self) -> PathBuf {
        self.storage_root().join("data")
    }

    async fn current_fleet(&self) {
        fleet_auth::migrate(&self.fleet).await.unwrap();
    }

    async fn current_app(&self) {
        trawl_server::store::migrations::migrate(&self.app)
            .await
            .unwrap();
    }

    async fn seed_cutover_job(&self) -> i64 {
        self.app.execute("INSERT INTO field_types(field,duckdb_type,pinned_from) VALUES ('status','BIGINT','api');
            INSERT INTO repin_jobs(field,from_type,to_type,dry_run,force,status) VALUES ('status','BIGINT','VARCHAR',false,false,'running')")
            .await.unwrap();
        sqlx::query_scalar("SELECT id FROM repin_jobs")
            .fetch_one(&self.app)
            .await
            .unwrap()
    }

    async fn current_cutover(&self) {
        let job_id = self.seed_cutover_job().await;
        self.cutover(job_id);
        // The refusal-only WAL sentinel is deliberately not ingestible.
        std::fs::remove_dir_all(self.storage_root().join("wal")).unwrap();
        std::fs::remove_file(trawl_server::repin::shadow_root(&self.data()).join("sentinel"))
            .unwrap();
    }

    async fn old(&self, owner: &str) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/schema-baseline/fixtures")
            .join(owner);
        let pool = if owner == "fleet" {
            &self.fleet
        } else {
            &self.app
        };
        sqlx::migrate::Migrator::new(path)
            .await
            .unwrap()
            .run(pool)
            .await
            .unwrap();
        let seed = if owner == "fleet" {
            "INSERT INTO api_keys(prefix,name,hash,kind) VALUES ('oldkey01','retained key','$argon2id$fixture','service')"
        } else {
            "INSERT INTO saved_queries(key_id,name,query,created_at,updated_at) VALUES (1,'retained query','service=test',now(),now())"
        };
        pool.execute(seed).await.unwrap();
    }

    fn cutover(&self, job_id: i64) {
        let data = self.data();
        let shadow = trawl_server::repin::shadow_root(&data);
        for (root, value) in [
            (&data, "200::BIGINT"),
            (&shadow, "'new-generation'::VARCHAR"),
        ] {
            let path = root.join("prod/2024-01-15/10/api.parquet");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let conn = duckdb::Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "COPY (SELECT TIMESTAMP '2024-01-15 10:00:00' AS _time,
                 TIMESTAMP '2024-01-15 10:00:01' AS _ingested, 'raw' AS _raw,
                 NULL::VARCHAR AS _repairs, 'prod' AS env, 'api' AS service,
                 'host' AS host, 9::BIGINT AS severity, 'info' AS severity_text,
                 {value} AS status) TO '{}' (FORMAT PARQUET)",
                path.to_string_lossy().replace('\'', "''")
            ))
            .unwrap();
        }
        std::fs::write(data.join("EPOCH"), "3\n").unwrap();
        // A valid partitioned WAL path and non-Parquet sentinels must be
        // compared too, even though this refusal should never open them.
        let wal = self.storage_root().join("wal/prod/2024-01-15/10");
        std::fs::create_dir_all(&wal).unwrap();
        std::fs::write(wal.join("retained.ndjson"), b"retained WAL bytes\n").unwrap();
        std::fs::write(shadow.join("sentinel"), b"staging sentinel\0\xff").unwrap();
        write_marker(
            &data,
            &RepinMarker {
                job_id,
                field: "status".into(),
                from_type: "BIGINT".into(),
                to_type: "VARCHAR".into(),
                phase: RepinPhase::Cutover,
            },
        )
        .unwrap();
    }

    fn spawn(&self) -> Daemon {
        let quote = |s: &str| toml::Value::String(s.to_owned()).to_string();
        let (cert, key) = common::ensure_test_cert();
        let config = format!(
            "[server]\nhttp_addr = '127.0.0.1:0'\nshutdown_drain_secs = 1\n\
             tls_cert_path = {}\ntls_key_path = {}\n{log_file}\n\
             [data]\npath = {}\n[auth]\ndatabase_url = {}\naudit_interval_secs = 0\n\
             [storage]\ndatabase_url = {}\n\
             [ingest]\nenabled = true\ninternal_telemetry = {telemetry}\nwal_dir = {}\n\
             envs = ['prod']\ndefault_env = 'prod'\n{compaction}\
             [retention]\nmax_age_days = 0\nmin_free_disk_bytes = 0\n\
             [scheduler]\nenabled = {producers}\n{syslog}",
            quote(&cert.to_string_lossy()),
            quote(&key.to_string_lossy()),
            quote(&self.data().to_string_lossy()),
            quote(&self.fleet_url),
            quote(&self.app_url),
            quote(&self.storage_root().join("wal").to_string_lossy()),
            telemetry = self.internal_telemetry,
            producers = self.producers,
            syslog = if self.producers {
                "[syslog]\nenabled = true\nudp_addr = '127.0.0.1:0'\ntcp_addr = '127.0.0.1:0'\n"
            } else {
                ""
            },
            compaction = self
                .compaction_interval_secs
                .map_or_else(String::new, |secs| {
                    format!("compaction_interval_secs = {secs}\n")
                }),
            log_file = self.log_file.as_ref().map_or_else(String::new, |path| {
                format!("log_file = {}", quote(&path.to_string_lossy()))
            }),
        );
        let path = self.root.path().join("trawld.toml");
        std::fs::write(&path, config).unwrap();
        let log = self.root.path().join("daemon.log");
        let output = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_trawld"));
        command.env_clear();
        for name in ["LD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(point) = self.crash_at {
            command.env("TRAWL_TEST_CRASH_AT", point);
        }
        let child = command
            .args(["--no-monitor", "--config"])
            .arg(path)
            .env("HOME", self.root.path())
            .env("RUST_LOG", "info")
            .env("NO_COLOR", "1")
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        Daemon { child, log }
    }

    async fn assert_lock_free(&self) {
        let mut conn = PgConnection::connect(&self.app_url).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(LOCK_KEY)
                .fetch_one(&mut conn)
                .await
                .unwrap();
            if locked {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "failed startup retained the sole-writer lock"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        conn.close().await.unwrap();
    }
}

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap()
    }

    async fn refused(&mut self, diagnostic: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let log = self.log();
                assert!(!status.success(), "incompatible startup succeeded: {log}");
                assert!(log.contains(diagnostic), "wrong refusal: {log}");
                assert!(
                    !log.contains("HTTPS server listening"),
                    "served before refusal: {log}"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "refusal timed out: {}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn ready(&mut self) -> String {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let deadline = Instant::now() + Duration::from_secs(30);
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        loop {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                self.log()
            );
            let log = self.log();
            if let Some(line) = log
                .lines()
                .find(|line| line.contains("HTTPS server listening"))
            {
                let addr = line
                    .split("addr=")
                    .nth(1)
                    .expect("listening address")
                    .split_whitespace()
                    .next()
                    .unwrap();
                let url = format!("https://{addr}");
                if let Ok(response) = client.get(format!("{url}/api/v1/health")).send().await {
                    let body: serde_json::Value = response.json().await.unwrap();
                    if body["status"] == "ok" {
                        return url;
                    }
                }
            }
            assert!(Instant::now() < deadline, "readiness timed out: {log}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until the daemon parks at the test crash point `point`, then
    /// SIGKILL it and reap it. Returns the log of the killed process.
    async fn kill_at(&mut self, point: &str) -> String {
        let needle = format!("point=\"{point}\"");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "daemon exited before {point}: {}",
                self.log()
            );
            let log = self.log();
            if log.lines().any(|line| {
                line.contains("event_type=\"test_crash_point\"") && line.contains(&needle)
            }) {
                self.child.kill().unwrap();
                self.child.wait().unwrap();
                return log;
            }
            assert!(Instant::now() < deadline, "never parked at {point}: {log}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn stop(&mut self) {
        assert!(
            Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "shutdown failed: {}", self.log());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "shutdown timed out: {}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn bytes(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        if !path.exists() {
            return;
        }
        let directory = path.is_dir();
        entries.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            (!directory).then(|| std::fs::read(path).unwrap()),
        );
        if directory {
            for entry in std::fs::read_dir(path).unwrap() {
                walk(root, &entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

async fn rows(pool: &PgPool) -> BTreeMap<String, Vec<String>> {
    let relations: Vec<(String, String)> = sqlx::query_as(
        "SELECT quote_ident(n.nspname)||'.'||quote_ident(c.relname), c.relkind::text
         FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
           AND c.relkind IN ('r','S') ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut rows = BTreeMap::new();
    for (name, kind) in relations {
        let query = if kind == "S" {
            format!("SELECT jsonb_build_array(last_value,is_called)::text FROM {name}")
        } else {
            format!("SELECT to_jsonb(t)::text FROM {name} t ORDER BY to_jsonb(t)::text")
        };
        let values = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .fetch_all(pool)
            .await
            .unwrap();
        rows.insert(name, values);
    }
    rows
}

async fn refusal(owner: &str, legacy: bool) {
    let mut fixture = Fixture::new().await;
    fixture.log_file = Some(fixture.data().join("logs/server.json"));
    if owner == "fleet" {
        fixture.current_app().await;
    } else {
        fixture.current_fleet().await;
    }
    if legacy {
        fixture.old(owner).await;
    } else {
        let pool = if owner == "fleet" {
            &fixture.fleet
        } else {
            &fixture.app
        };
        pool.execute("CREATE TABLE retained(value text); INSERT INTO retained VALUES ('keep me')")
            .await
            .unwrap();
    }
    let job_id = if owner == "fleet" || legacy {
        fixture.seed_cutover_job().await
    } else {
        // An untracked app DB has no recognized job schema, but its
        // filesystem must still remain untouched when admission refuses it.
        1
    };
    fixture.cutover(job_id);
    let before_files = bytes(&fixture.storage_root());
    let before_fleet = rows(&fixture.fleet).await;
    let before_app = rows(&fixture.app).await;
    assert!(
        before_files
            .values()
            .filter(|bytes| bytes.is_some())
            .count()
            >= 5
    );
    let diagnostic = if legacy {
        "unsupported pre-1.0 migration"
    } else {
        "nonempty without a supported baseline"
    };
    for _ in 0..2 {
        fixture.spawn().refused(diagnostic).await;
        assert!(
            bytes(&fixture.storage_root()) == before_files,
            "refused startup changed data/WAL/staging bytes"
        );
        assert_eq!(rows(&fixture.fleet).await, before_fleet);
        assert_eq!(rows(&fixture.app).await, before_app);
        fixture.assert_lock_free().await;
    }
}

#[tokio::test]
async fn old_fleet_refuses_before_cutover() {
    refusal("fleet", true).await;
}

#[tokio::test]
async fn old_trawl_refuses_before_cutover() {
    refusal("trawl", true).await;
}

#[tokio::test]
async fn untracked_fleet_refuses_before_cutover() {
    refusal("fleet", false).await;
}

#[tokio::test]
async fn untracked_trawl_refuses_before_cutover() {
    refusal("trawl", false).await;
}

#[tokio::test]
async fn database_refusal_does_not_initialize_fresh_root() {
    let mut fixture = Fixture::new().await;
    fixture.log_file = Some(fixture.data().join("logs/server.json"));
    fixture.current_fleet().await;
    fixture.old("trawl").await;
    fixture
        .spawn()
        .refused("unsupported pre-1.0 migration")
        .await;
    assert!(
        !fixture.storage_root().exists(),
        "refused startup initialized storage"
    );
    fixture.assert_lock_free().await;
}

#[tokio::test]
async fn epoch_refusal_preserves_storage_and_releases_admitted_lock() {
    let mut fixture = Fixture::new().await;
    fixture.log_file = Some(fixture.data().join("logs/server.json"));
    fixture.current_fleet().await;
    fixture.current_app().await;
    fixture.cutover(1);
    std::fs::write(fixture.data().join("EPOCH"), "2\n").unwrap();
    let before = bytes(&fixture.storage_root());
    fixture.spawn().refused("epoch").await;
    assert_eq!(bytes(&fixture.storage_root()), before);
    fixture.assert_lock_free().await;
}

#[tokio::test]
async fn competing_writer_refuses_before_filesystem_recovery() {
    let mut fixture = Fixture::new().await;
    fixture.log_file = Some(fixture.data().join("logs/server.json"));
    fixture.current_fleet().await;
    fixture.current_app().await;
    fixture.current_cutover().await;
    let before = bytes(&fixture.storage_root());
    let mut owner = PgConnection::connect(&fixture.app_url).await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut owner)
        .await
        .unwrap();
    fixture.spawn().refused("advisory lock").await;
    assert!(
        bytes(&fixture.storage_root()) == before,
        "competing writer changed the corpus"
    );
    owner.close().await.unwrap();
    fixture.assert_lock_free().await;
}

#[tokio::test]
async fn admitted_lock_and_fatal_watcher_cover_store_reconciliation() {
    let mut fixture = Fixture::new().await;
    fixture.internal_telemetry = true;
    fixture.log_file = Some(fixture.data().join("logs/server.json"));
    fixture.current_fleet().await;
    fixture.current_app().await;
    fixture.current_cutover().await;
    let mut blocker = fixture.app.begin().await.unwrap();
    sqlx::query("SELECT id FROM repin_jobs FOR UPDATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let mut daemon = fixture.spawn();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            daemon.log()
        );
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE NOT granted
             AND pg_backend_pid()=ANY(pg_blocking_pids(pid)))",
        )
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
        if waiting && !trawl_server::repin::shadow_root(&fixture.data()).exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reconciliation did not wait: {}",
            daemon.log()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(LOCK_KEY)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    assert!(
        !locked,
        "reconciliation lost the admitted sole-writer owner"
    );
    assert!(
        !daemon.log().contains("HTTPS server listening"),
        "served a swapped corpus before its pin committed"
    );
    assert!(
        !fixture.storage_root().join("wal").exists(),
        "telemetry wrote to WAL before reconciliation completed"
    );
    // Kill the actual lock-holding PostgreSQL session while reconciliation
    // is blocked. The startup watcher must terminate without serving.
    let killed: bool = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype='advisory'
         AND database=(SELECT oid FROM pg_database WHERE datname=current_database())
         AND classid=$1::bigint::oid AND objid=$2::bigint::oid AND objsubid=1 AND granted",
    )
    .bind(LOCK_KEY >> 32)
    .bind(LOCK_KEY & 0xffff_ffff)
    .fetch_one(&mut *blocker)
    .await
    .unwrap();
    assert!(killed);
    daemon
        .refused("app-state sole-writer lock lost; terminating trawld")
        .await;
    blocker.rollback().await.unwrap();
    fixture.assert_lock_free().await;
    // The interrupted recovery remains restartable after the lock failure.
    let mut replacement = fixture.spawn();
    replacement.ready().await;
    replacement.stop().await;
    fixture.assert_lock_free().await;
    assert!(!fixture.log_file.as_ref().unwrap().exists());
}

#[tokio::test]
async fn log_marker_collision_refuses_before_database_or_storage_mutation() {
    for marker in ["EPOCH", "CATALOG", "REPIN"] {
        for current in [false, true] {
            let mut fixture = Fixture::new().await;
            fixture.current_fleet().await;
            fixture.log_file = Some(fixture.data().join(marker));
            if current {
                std::fs::create_dir_all(fixture.data()).unwrap();
                std::fs::write(fixture.data().join("EPOCH"), b"3\n").unwrap();
            }
            let before_files = bytes(&fixture.storage_root());
            let before_app = rows(&fixture.app).await;
            let before_fleet = rows(&fixture.fleet).await;
            fixture.spawn().refused("reserved storage marker").await;
            assert_eq!(bytes(&fixture.storage_root()), before_files);
            assert_eq!(rows(&fixture.app).await, before_app);
            assert_eq!(rows(&fixture.fleet).await, before_fleet);
        }
    }
}

#[tokio::test]
async fn log_inside_fresh_data_root_starts_and_restarts() {
    let mut fixture = Fixture::new().await;
    fixture.log_file = Some(fixture.data().join("logs/server.json"));
    fixture.current_fleet().await;
    for _ in 0..2 {
        let mut daemon = fixture.spawn();
        daemon.ready().await;
        daemon.stop().await;
        fixture.assert_lock_free().await;
        assert_eq!(std::fs::read(fixture.data().join("EPOCH")).unwrap(), b"3\n");
        let log = std::fs::read_to_string(fixture.log_file.as_ref().unwrap()).unwrap();
        assert!(log.contains("HTTPS server listening"), "{log}");
        for line in log.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }
}

#[tokio::test]
async fn interrupted_epoch_publication_starts_and_restarts() {
    let fixture = Fixture::new().await;
    fixture.current_fleet().await;
    std::fs::create_dir_all(fixture.data()).unwrap();
    std::fs::write(fixture.data().join("EPOCH.next.123"), b"3").unwrap();
    for _ in 0..2 {
        let mut daemon = fixture.spawn();
        daemon.ready().await;
        daemon.stop().await;
        fixture.assert_lock_free().await;
        assert_eq!(std::fs::read(fixture.data().join("EPOCH")).unwrap(), b"3\n");
        assert!(!fixture.data().join("EPOCH.next.123").exists());
    }
}

#[tokio::test]
async fn fresh_boot_restart_and_interrupted_current_cutover() {
    let fixture = Fixture::new().await;
    fixture.current_fleet().await;
    // The daemon owns fresh Trawl initialization.
    let mut daemon = fixture.spawn();
    daemon.ready().await;
    daemon.stop().await;
    fixture.assert_lock_free().await;
    assert_eq!(
        std::fs::read_to_string(fixture.data().join("EPOCH"))
            .unwrap()
            .trim(),
        "3"
    );
    let mut daemon = fixture.spawn();
    daemon.ready().await;
    daemon.stop().await;
    fixture.assert_lock_free().await;

    fixture.current_cutover().await;
    // Crash after the first rename, before installing the shadow env.
    let aside = trawl_server::repin::aside_root(&fixture.data());
    std::fs::create_dir_all(&aside).unwrap();
    std::fs::rename(fixture.data().join("prod"), aside.join("prod")).unwrap();

    let store = fleet_auth::KeyStore::from_pool(fixture.fleet.clone());
    store
        .create_role("reader", None, &common::trawl_perms(&["query"]))
        .await
        .unwrap();
    let key = store
        .create_key(
            "startup-test",
            fleet_auth::PrincipalKind::Service,
            &common::roles(&["reader"]),
            None,
        )
        .await
        .unwrap();
    let mut daemon = fixture.spawn();
    let url = daemon.ready().await;
    let client =
        trawl_client::HttpClient::new_insecure(&url, key.plaintext_token.as_str()).unwrap();
    let result = client
        .query_paginated("service=api | table status", None, None)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&result.result.rows).unwrap(),
        serde_json::json!([["new-generation"]])
    );
    let pin: String =
        sqlx::query_scalar("SELECT duckdb_type FROM field_types WHERE field='status'")
            .fetch_one(&fixture.app)
            .await
            .unwrap();
    assert_eq!(pin, "VARCHAR");
    let status: String = sqlx::query_scalar("SELECT status FROM repin_jobs")
        .fetch_one(&fixture.app)
        .await
        .unwrap();
    assert_eq!(status, "succeeded");
    assert!(!aside.exists());
    assert!(!trawl_server::repin::shadow_root(&fixture.data()).exists());
    assert!(!trawl_server::repin::marker_path(&fixture.data()).exists());
    daemon.stop().await;
    fixture.assert_lock_free().await;
}

/// AC8 (#252): publication recovery runs at boot, before the listener and
/// every producer. One marker records a publish whose output is in place
/// (canonical identity matches); the other records one that never renamed
/// (canonical absent, tmp present). After the boot, both markers are gone,
/// the published WAL is retired, the unpublished WAL is kept and its tmp is
/// removed, and both outcomes are logged before the first line of boot
/// conformance, of every producer and background task, and of the
/// listener. A long compaction interval keeps the first tick, which would
/// also recover, out of the picture.
#[tokio::test]
async fn boot_recovers_publication_markers_before_serving() {
    use trawl_server::ingest::publication_marker::{ValidatedMarker, identity_of, write_marker};

    let mut fixture = Fixture::new().await;
    fixture.current_fleet().await;
    fixture.compaction_interval_secs = Some(3600);
    fixture.producers = true;
    // A first boot initializes the data root and databases, as a daemon
    // that later crashed mid-publish would have.
    let mut daemon = fixture.spawn();
    daemon.ready().await;
    daemon.stop().await;
    fixture.assert_lock_free().await;

    let wal = fixture.storage_root().join("wal");
    let data = fixture.data();
    let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
    std::fs::create_dir_all(wal.join("prod")).unwrap();
    let plant = |service: &str, seq: u32| {
        let name = format!("{service}_1730000000000_{seq:04x}.ndjson");
        let path = wal.join("prod").join(&name);
        std::fs::write(
            &path,
            format!(
                r#"{{"_time":"2026-09-22T07:00:00Z","_ingested":"2026-09-22T07:00:00Z","service":"{service}","message":"m"}}"#
            ),
        )
        .unwrap();
        (name, path)
    };

    // Published: the canonical output carries the recorded identity.
    let (api_name, api_wal) = plant("api", 1);
    let canonical = data.join("prod/2026-09-22/07/api.parquet");
    std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT TIMESTAMP '2026-09-22 07:00:00' AS _time,
         TIMESTAMP '2026-09-22 07:00:01' AS _ingested, 'raw' AS _raw,
         NULL::VARCHAR AS _repairs, 'prod' AS env, 'api' AS service,
         'host' AS host, 9::BIGINT AS severity, 'info' AS severity_text)
         TO '{}' (FORMAT PARQUET)",
        canonical.to_string_lossy().replace('\'', "''")
    ))
    .unwrap();
    let api_marker = ValidatedMarker::new(
        "prod",
        "api",
        day,
        7,
        vec![api_name],
        identity_of(&canonical).unwrap(),
    )
    .unwrap();
    assert_eq!(api_marker.canonical(&data), canonical);
    write_marker(&wal, &api_marker).unwrap();

    // Unpublished: the tmp is in place and the canonical output is absent.
    let (web_name, web_wal) = plant("web", 2);
    let web_tmp = data.join("prod/2026-09-22/07/web.parquet.tmp");
    std::fs::write(&web_tmp, b"PAR1 staged output PAR1").unwrap();
    let web_marker = ValidatedMarker::new(
        "prod",
        "web",
        day,
        7,
        vec![web_name],
        identity_of(&web_tmp).unwrap(),
    )
    .unwrap();
    assert_eq!(web_marker.tmp(&data), web_tmp);
    write_marker(&wal, &web_marker).unwrap();

    let mut daemon = fixture.spawn();
    daemon.ready().await;
    assert!(
        !api_marker.marker_path(&wal).exists(),
        "published marker removed"
    );
    assert!(
        !web_marker.marker_path(&wal).exists(),
        "unpublished marker removed"
    );
    assert!(!api_wal.exists(), "published WAL retired");
    assert!(canonical.is_file(), "published output kept");
    assert!(web_wal.is_file(), "unpublished WAL kept for compaction");
    assert!(!web_tmp.exists(), "unpublished tmp removed");
    assert!(!web_marker.canonical(&data).exists());

    assert_recovery_precedes_boot_steps(&daemon).await;
    daemon.stop().await;
    fixture.assert_lock_free().await;
}

/// Assert both `publication_recovered` outcomes precede the first line each
/// later boot step emits: boot conformance (its skip on an already-conformed
/// root still logs `catalog_conform`), the ingest pipeline, the compaction,
/// retention, syslog and scheduler tasks, and the listener. Some of these
/// tasks log after the listener, so wait for every line first.
async fn assert_recovery_precedes_boot_steps(daemon: &Daemon) {
    let anchors: [(&str, &str); 9] = [
        ("boot conformance", "event_type=\"catalog_conform"),
        ("ingest pipeline", "ingest pipeline enabled"),
        ("compaction task", "action=\"compaction_start\""),
        ("retention task", "action=\"retention_start\""),
        ("syslog", "syslog listener enabled"),
        ("syslog UDP", "event_type=\"syslog_udp_listening\""),
        ("syslog TCP", "event_type=\"syslog_tcp_listening\""),
        ("scheduler", "event_type=\"scheduler_started\""),
        ("listener", "HTTPS server listening"),
    ];
    let deadline = Instant::now() + Duration::from_secs(10);
    let log = loop {
        let log = daemon.log();
        if anchors.iter().all(|(_, needle)| log.contains(needle)) {
            break log;
        }
        assert!(Instant::now() < deadline, "missing a boot step line: {log}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let line_of = |needle: &dyn Fn(&str) -> bool| log.lines().position(needle);
    for outcome in ["published", "unpublished"] {
        let recovered = line_of(&|line| {
            line.contains("event_type=\"publication_recovered\"")
                && line.contains(&format!("outcome=\"{outcome}\""))
        })
        .unwrap_or_else(|| panic!("no {outcome} recovery line: {log}"));
        for (step, needle) in anchors {
            let first = line_of(&|line| line.contains(needle)).unwrap();
            assert!(
                recovered < first,
                "{outcome} recovery must precede the {step}: {log}"
            );
        }
    }
}

// Real-process crash tests for the publication protocol (#252, ADR-0041).
//
// Each test ingests uniquely tagged events through authenticated HTTP into a
// daemon whose `TRAWL_TEST_CRASH_AT` parks it at one publish or recovery
// point, SIGKILLs it there, checks what the kill left on disk, and restarts
// it. The daemon then has to publish every acknowledged event exactly once:
// counted over the parquet files with DuckDB and through the query API,
// after the restart and after two further completed compaction ticks.

/// The service every crash test ingests into, so every publish of the test
/// lands in one canonical file and a later tick merges into it.
const CRASH_SERVICE: &str = "crashsvc";
/// Events the crash test acknowledges, in one ingest request to one service.
///
/// One request makes one WAL file, renamed into place and acknowledged
/// before its min-age starts counting down. So the first eligible tick sees
/// every acknowledged event, and the marker of the killed publish names that
/// one file. Two requests would let a tick select the first file while the
/// second is still in flight, and the marker would then cover only part of
/// the acknowledged events. Retirement order across several consumed WAL
/// files is covered in-process by the `ac2_*` matrix in
/// `ingest::publication_marker`.
const CRASH_EVENTS: usize = 100;

/// A daemon killed mid-publish, with what it had acknowledged.
struct CrashedPublish {
    fixture: Fixture,
    token: String,
    /// `_time` of every event, fixed so probes merge into the same hour.
    time: String,
    tag: String,
    acknowledged: i64,
    /// The one WAL file the acknowledged request wrote.
    wal_file: PathBuf,
    marker: trawl_server::ingest::publication_marker::ValidatedMarker,
}

impl CrashedPublish {
    /// Ingest the tagged events into a daemon that parks at `point` on its
    /// first publish, and SIGKILL it there.
    async fn at(point: &'static str) -> Self {
        let mut fixture = Fixture::new().await;
        fixture.current_fleet().await;
        // One second between ticks and as the WAL min-age: the WAL file
        // becomes eligible a second after it is written.
        fixture.compaction_interval_secs = Some(1);
        let store = fleet_auth::KeyStore::from_pool(fixture.fleet.clone());
        store
            .create_role("writer", None, &common::trawl_perms(&["ingest", "query"]))
            .await
            .unwrap();
        let key = store
            .create_key(
                "crash-test",
                fleet_auth::PrincipalKind::Service,
                &common::roles(&["writer"]),
                None,
            )
            .await
            .unwrap();
        let token = key.plaintext_token.as_str().to_owned();
        let time = (chrono::Utc::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let tag = format!("acked-{}-{}", std::process::id(), point.replace(':', "-"));

        fixture.crash_at = Some(point);
        let mut daemon = fixture.spawn();
        let client = client(&daemon.ready().await, &token);
        let acknowledged = ingest(&client, &time, &tag, CRASH_EVENTS).await;
        // The parked publish never retires its WAL, so the acknowledged
        // file stays in place until the kill whether or not a tick has
        // already selected it.
        let [wal_file] = <[PathBuf; 1]>::try_from(wal_files(&fixture))
            .unwrap_or_else(|files| panic!("one request, one WAL file: {files:?}"));
        let log = daemon.kill_at(point).await;
        assert!(
            log.contains(&format!("point=\"{point}\"")),
            "killed at {point}: {log}"
        );
        fixture.crash_at = None;
        let marker = trawl_server::ingest::publication_marker::read_marker(&marker_path(&fixture))
            .expect("the killed publish left its marker");
        assert_eq!(
            marker.wal_paths(&fixture.storage_root().join("wal")),
            vec![wal_file.clone()],
            "the marker covers the one acknowledged WAL file"
        );
        Self {
            fixture,
            token,
            time,
            tag,
            acknowledged,
            wal_file,
            marker,
        }
    }

    /// Restart without a crash point and assert every acknowledged event is
    /// counted exactly once, `after_restart` right at readiness (when the
    /// output was published before the kill) and always after two further
    /// completed compaction ticks. Ends with no marker, no tmp and no WAL.
    async fn restart_exactly_once(&self, after_restart: bool) {
        let mut daemon = self.fixture.spawn();
        let client = client(&daemon.ready().await, &self.token);
        assert!(
            !marker_path(&self.fixture).exists(),
            "boot recovered the marker"
        );
        if after_restart {
            self.assert_counted_once(&client, "after the restart").await;
        }
        let probes = complete_ticks(&self.fixture, &client, &self.time, 2).await;
        self.assert_counted_once(&client, "after two further ticks")
            .await;
        assert_eq!(
            parquet_count(&self.fixture.data(), "probe"),
            probes,
            "every probe published once"
        );
        assert_eq!(wal_files(&self.fixture), Vec::<PathBuf>::new());
        assert!(!marker_path(&self.fixture).exists());
        assert!(!self.marker.tmp(&self.fixture.data()).exists());
        daemon.stop().await;
        self.fixture.assert_lock_free().await;
    }

    async fn assert_counted_once(&self, client: &trawl_client::HttpClient, when: &str) {
        assert_eq!(
            parquet_count(&self.fixture.data(), &self.tag),
            self.acknowledged,
            "parquet count {when}"
        );
        assert_eq!(
            api_count(client, &self.tag).await,
            self.acknowledged,
            "query API count {when}"
        );
    }

    /// The kill after the rename: the canonical output carries the marker's
    /// identity and every WAL file it consumed is still in place.
    fn assert_published_unretired(&self) {
        let data = self.fixture.data();
        let canonical = self.marker.canonical(&data);
        assert_eq!(
            trawl_server::ingest::publication_marker::identity_of(&canonical).unwrap(),
            self.marker.identity(),
            "the canonical output is the published one"
        );
        assert!(!self.marker.tmp(&data).exists(), "the tmp was renamed");
        self.assert_wal_kept();
    }

    /// The kill before the rename: the tmp carries the marker's identity, no
    /// canonical output exists, and every consumed WAL file is in place.
    fn assert_staged_unpublished(&self) {
        let data = self.fixture.data();
        assert_eq!(
            trawl_server::ingest::publication_marker::identity_of(&self.marker.tmp(&data)).unwrap(),
            self.marker.identity(),
            "the staged tmp is the marker's output"
        );
        assert!(
            !self.marker.canonical(&data).exists(),
            "nothing was published"
        );
        self.assert_wal_kept();
    }

    fn assert_wal_kept(&self) {
        assert!(self.wal_file.is_file(), "the consumed WAL is kept");
    }

    /// Restart with a recovery crash point; boot recovery parks there and is
    /// killed before the daemon serves.
    async fn kill_boot_recovery_at(&mut self, point: &'static str) {
        self.fixture.crash_at = Some(point);
        let mut daemon = self.fixture.spawn();
        let log = daemon.kill_at(point).await;
        self.fixture.crash_at = None;
        assert!(
            !log.contains("HTTPS server listening"),
            "boot recovery runs before serving: {log}"
        );
    }
}

fn client(url: &str, token: &str) -> trawl_client::HttpClient {
    trawl_client::HttpClient::new_insecure(url, token).unwrap()
}

/// Ingest `size` events tagged `tag` in one request; returns the count the
/// daemon acknowledged, asserting it acknowledged all of them.
async fn ingest(client: &trawl_client::HttpClient, time: &str, tag: &str, size: usize) -> i64 {
    let records: Vec<serde_json::Value> = (0..size)
        .map(|seq| {
            serde_json::json!({
                "_time": time,
                "service": CRASH_SERVICE,
                "crash_tag": tag,
                "seq": seq,
                "message": format!("{tag} {seq}"),
            })
        })
        .collect();
    let response = client.ingest(&records).await.unwrap();
    assert_eq!(response.accepted, size, "{response:?}");
    assert_eq!(response.rejected, 0, "{response:?}");
    i64::try_from(size).unwrap()
}

fn marker_path(fixture: &Fixture) -> PathBuf {
    fixture
        .storage_root()
        .join(format!("wal/prod/.publish-{CRASH_SERVICE}.json"))
}

/// Unconsumed WAL files: every `*.ndjson` in the env's WAL directory.
fn wal_files(fixture: &Fixture) -> Vec<PathBuf> {
    let dir = fixture.storage_root().join("wal/prod");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ndjson"))
        .collect();
    files.sort();
    files
}

/// Exact count of events tagged `tag` over every published parquet file.
fn parquet_count(data: &Path, tag: &str) -> i64 {
    fn walk(path: &Path, files: &mut Vec<String>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "parquet") {
                files.push(format!("'{}'", path.to_string_lossy().replace('\'', "''")));
            }
        }
    }
    let mut files = Vec::new();
    walk(&data.join("prod"), &mut files);
    if files.is_empty() {
        return 0;
    }
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.query_row(
        &format!(
            "SELECT count(*) FROM read_parquet([{}], union_by_name=true) WHERE crash_tag = ?",
            files.join(",")
        ),
        [tag],
        |row| row.get(0),
    )
    .unwrap()
}

/// Exact count of events tagged `tag` through the query API.
async fn api_count(client: &trawl_client::HttpClient, tag: &str) -> i64 {
    let dsl = format!("crash_tag=\"{tag}\" last=24h | stats count()");
    let result = client.query_paginated(&dsl, None, None).await.unwrap();
    assert_eq!(result.result.row_count(), 1, "{dsl}");
    match &result.result.rows[0][0] {
        trawl_api::value::Value::Integer(n) => *n,
        other => panic!("{dsl}: count must be an integer, got {other:?}"),
    }
}

/// Wait for `ticks` further compaction ticks to complete; returns the number
/// of probe events published on the way.
///
/// Compaction logs no per-tick line, so ticks are observed through WAL
/// state: each probe is one acknowledged event, ingested only after the
/// previous probe's WAL was retired and no marker remained. Ticks run one at
/// a time, and a tick scans its WAL before it publishes, so probe `i + 1`
/// publishes in a later tick than probe `i`, and its retirement proves probe
/// `i`'s tick ran to completion. Each probe tick also recovers markers first
/// and merges into the canonical file the crashed publish wrote, which is
/// where a surviving consumed WAL file would be merged a second time.
async fn complete_ticks(
    fixture: &Fixture,
    client: &trawl_client::HttpClient,
    time: &str,
    ticks: usize,
) -> i64 {
    let mut probes = 0;
    for _ in 0..=ticks {
        probes += ingest(client, time, "probe", 1).await;
        let deadline = Instant::now() + Duration::from_secs(30);
        while !wal_files(fixture).is_empty() || marker_path(fixture).exists() {
            assert!(
                Instant::now() < deadline,
                "compaction never retired the probe: {:?}",
                wal_files(fixture)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    probes
}

/// AC1 (#252): a kill after the canonical rename and before WAL retirement
/// leaves the marker, the published output and its consumed WAL. Boot
/// recovery completes the publish, retiring that WAL instead of merging it
/// again.
#[tokio::test]
async fn compaction_crash_after_publish_before_retire_is_exactly_once() {
    let crashed = CrashedPublish::at("publish:after_rename").await;
    assert!(marker_path(&crashed.fixture).exists());
    crashed.assert_published_unretired();
    crashed.restart_exactly_once(true).await;
}

/// AC3 (#252): a kill after the marker and before the rename leaves the
/// marker, the staged tmp and the WAL. Recovery removes the tmp and the
/// marker and keeps the WAL, and the next tick publishes it once.
#[tokio::test]
async fn compaction_crash_after_marker_before_rename_is_exactly_once() {
    let crashed = CrashedPublish::at("publish:after_marker").await;
    assert!(marker_path(&crashed.fixture).exists());
    crashed.assert_staged_unpublished();
    crashed.restart_exactly_once(false).await;
}

/// AC2 (#252), real-process complement of the in-process `ac2_*` matrix: a
/// kill during boot recovery of a published marker, after it retired the
/// consumed WAL file and before the WAL directory fsync and the marker
/// removal, reruns to the same end state. The marker names one file here;
/// the `ac2_*` matrix covers retirement across several.
#[tokio::test]
async fn publication_recovery_crash_in_published_branch_reruns_exactly_once() {
    let mut crashed = CrashedPublish::at("publish:after_rename").await;
    crashed.assert_published_unretired();
    crashed
        .kill_boot_recovery_at("recover:published:after_retire:0")
        .await;
    assert!(marker_path(&crashed.fixture).exists(), "the marker stays");
    assert!(!crashed.wal_file.exists(), "the consumed WAL was retired");
    assert_eq!(wal_files(&crashed.fixture), Vec::<PathBuf>::new());
    crashed.restart_exactly_once(true).await;
}

/// AC2 (#252), real-process complement: a kill during boot recovery of an
/// unpublished marker, after the marker is removed and before the tmp is,
/// reruns without the marker: the WAL is kept and published once.
#[tokio::test]
async fn publication_recovery_crash_in_unpublished_branch_reruns_exactly_once() {
    let mut crashed = CrashedPublish::at("publish:after_marker").await;
    crashed.assert_staged_unpublished();
    crashed
        .kill_boot_recovery_at("recover:unpublished:after_marker_remove")
        .await;
    assert!(
        !marker_path(&crashed.fixture).exists(),
        "the marker is gone"
    );
    let data = crashed.fixture.data();
    assert!(
        crashed.marker.tmp(&data).is_file(),
        "the tmp is not yet removed"
    );
    assert!(!crashed.marker.canonical(&data).exists());
    crashed.assert_wal_kept();
    crashed.restart_exactly_once(false).await;
}
