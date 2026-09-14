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
             tls_cert_path = {}\ntls_key_path = {}\n\
             [data]\npath = {}\n[auth]\ndatabase_url = {}\naudit_interval_secs = 0\n\
             [storage]\ndatabase_url = {}\n\
             [ingest]\nenabled = true\ninternal_telemetry = {telemetry}\nwal_dir = {}\n\
             envs = ['prod']\ndefault_env = 'prod'\n\
             [retention]\nmax_age_days = 0\nmin_free_disk_bytes = 0\n\
             [scheduler]\nenabled = false\n",
            quote(&cert.to_string_lossy()),
            quote(&key.to_string_lossy()),
            quote(&self.data().to_string_lossy()),
            quote(&self.fleet_url),
            quote(&self.app_url),
            quote(&self.storage_root().join("wal").to_string_lossy()),
            telemetry = self.internal_telemetry,
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
    let fixture = Fixture::new().await;
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
    let fixture = Fixture::new().await;
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
    let fixture = Fixture::new().await;
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
    let fixture = Fixture::new().await;
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
