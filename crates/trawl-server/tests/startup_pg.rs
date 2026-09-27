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
    /// `[ingest] enabled`; false boots a query-only node.
    ingest: bool,
}

/// The file whose existence releases a boot pass held by
/// `TRAWL_TEST_HOLD_BOOT_PASS`.
const BOOT_PASS_RELEASE: &str = "boot-pass.release";

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
            ingest: true,
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
        self.spawn_with(false)
    }

    /// Spawn with the first compaction pass held before it reads the WAL,
    /// until [`release_boot_pass`](Self::release_boot_pass): the corpus the
    /// first query sees is what boot recovery and hydration left, and no
    /// pass has published anything (ADR-0041 slice 2).
    fn spawn_holding_boot_pass(&self) -> Daemon {
        self.spawn_with(true)
    }

    fn release_boot_pass(&self) {
        std::fs::write(self.root.path().join(BOOT_PASS_RELEASE), b"").unwrap();
    }

    fn spawn_with(&self, hold_boot_pass: bool) -> Daemon {
        let quote = |s: &str| toml::Value::String(s.to_owned()).to_string();
        let (cert, key) = common::ensure_test_cert();
        let config = format!(
            "[server]\nhttp_addr = '127.0.0.1:0'\nshutdown_drain_secs = 1\n\
             tls_cert_path = {}\ntls_key_path = {}\n{log_file}\n\
             [data]\npath = {}\n[auth]\ndatabase_url = {}\naudit_interval_secs = 0\n\
             [storage]\ndatabase_url = {}\n\
             [ingest]\nenabled = {ingest}\ninternal_telemetry = {telemetry}\nwal_dir = {}\n\
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
            ingest = self.ingest,
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
        if hold_boot_pass {
            let release = self.root.path().join(BOOT_PASS_RELEASE);
            match std::fs::remove_file(&release) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("clear {}: {e}", release.display()),
            }
            command.env("TRAWL_TEST_HOLD_BOOT_PASS", release);
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
        self.serving("ok").await
    }

    /// Wait until the daemon listens and `/api/v1/health` answers with
    /// `status`; returns its base URL.
    async fn serving(&mut self, status: &str) -> String {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let deadline = Instant::now() + Duration::from_secs(30);
        let client = common::harness_client_builder()
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
                    if body["status"] == status {
                        return url;
                    }
                }
            }
            assert!(Instant::now() < deadline, "readiness timed out: {log}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until the first compaction pass logs that it is held (see
    /// [`Fixture::spawn_holding_boot_pass`]). A daemon built without the
    /// hold never logs it, so the test fails instead of racing the pass.
    async fn held_at_boot_pass(&mut self) {
        self.wait_for_log("the boot pass never held", |line| {
            line.contains("event_type=\"test_hold_point\"")
                && line.contains("point=\"compaction:boot_pass\"")
        })
        .await;
    }

    /// Wait up to 30 s for a log line matching `matches`, while the daemon
    /// runs; returns the log.
    async fn wait_for_log(&mut self, failure: &str, matches: impl Fn(&str) -> bool) -> String {
        self.wait_for_lines(failure, 1, matches).await
    }

    /// Wait up to 30 s for `at_least` log lines matching `matches`, while
    /// the daemon runs; returns the log.
    async fn wait_for_lines(
        &mut self,
        failure: &str,
        at_least: usize,
        matches: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                self.log()
            );
            let log = self.log();
            if log.lines().filter(|line| matches(line)).count() >= at_least {
                return log;
            }
            assert!(Instant::now() < deadline, "{failure}: {log}");
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
/// listener. The first compaction pass runs at boot, after recovery, and
/// publishes the kept WAL once (ADR-0041 slice 2); a long compaction
/// interval keeps every later tick out of the picture.
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
    // The live writer's name and line format: the unpublished file that
    // recovery keeps is hydrated at boot, not left as overhang.
    let plant = |service: &str, seq: u32| {
        let name = format!("{service}_1730000000000_{seq:04x}.ndjson");
        let path = wal.join("prod").join(&name);
        let mut line = serde_json::to_vec(&serde_json::json!({
            "_time": "2026-09-22T07:00:00.000000Z",
            "_ingested": "2026-09-22T07:00:00.000000Z",
            "service": service,
            "message": "m",
            "crash_tag": format!("boot-recovery-{service}"),
        }))
        .unwrap();
        line.push(b'\n');
        std::fs::write(&path, line).unwrap();
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
    // The boot pass publishes the kept WAL under a marker and a tmp of its
    // own, at the paths recovery removed. Read the end state after it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while web_wal.exists() || web_marker.marker_path(&wal).exists() {
        assert!(
            Instant::now() < deadline,
            "the boot pass publishes the WAL recovery kept"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !api_marker.marker_path(&wal).exists(),
        "published marker removed"
    );
    assert!(!api_wal.exists(), "published WAL retired");
    assert!(canonical.is_file(), "published output kept");
    assert!(!web_tmp.exists(), "no staged output left");
    let log = daemon.log();
    let hydration = log
        .lines()
        .find(|line| line.contains("event_type=\"boot_hydration\""))
        .unwrap_or_else(|| panic!("no boot_hydration line: {log}"));
    assert!(
        hydration.contains("hydrated=1") && hydration.contains("overhang=false"),
        "the kept WAL file is resident: {hydration}"
    );
    assert_eq!(
        parquet_count(&data, "boot-recovery-web"),
        1,
        "recovery kept the unpublished WAL, and the boot pass published it once"
    );

    assert_recovery_precedes_boot_steps(&daemon).await;
    daemon.stop().await;
    fixture.assert_lock_free().await;
}

/// Assert the boot order (ADR-0041): both `publication_recovered` outcomes,
/// then rollup-marker recovery, then boot conformance (its skip on an
/// already-conformed root still logs `catalog_conform`), then hydration,
/// each strictly after the one before. Every later step's first line
/// follows hydration: the ingest pipeline (logged after self-telemetry is
/// activated, and before compaction is spawned), the compaction,
/// retention, syslog and scheduler tasks, and the listener. Those tasks log
/// from their own threads, some after the listener, so they are ordered
/// against hydration only; wait for every line first.
async fn assert_recovery_precedes_boot_steps(daemon: &Daemon) {
    let later: [(&str, &str); 9] = [
        ("ingest pipeline", "ingest pipeline enabled"),
        ("compaction task", "action=\"compaction_start\""),
        ("retention task", "action=\"retention_start\""),
        ("syslog", "syslog listener enabled"),
        ("syslog UDP", "event_type=\"syslog_udp_listening\""),
        ("syslog TCP", "event_type=\"syslog_tcp_listening\""),
        ("scheduler", "event_type=\"scheduler_started\""),
        ("listener", "HTTPS server listening"),
        ("hydration", "event_type=\"boot_hydration\""),
    ];
    let deadline = Instant::now() + Duration::from_secs(10);
    let log = loop {
        let log = daemon.log();
        if later.iter().all(|(_, needle)| log.contains(needle)) {
            break log;
        }
        assert!(Instant::now() < deadline, "missing a boot step line: {log}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let line_of = |needle: &dyn Fn(&str) -> bool| log.lines().position(needle);
    let first = |needle: &str| {
        line_of(&|line| line.contains(needle)).unwrap_or_else(|| panic!("no {needle}: {log}"))
    };
    let mut recovered = Vec::new();
    for outcome in ["published", "unpublished"] {
        recovered.push(
            line_of(&|line| {
                line.contains("event_type=\"publication_recovered\"")
                    && line.contains(&format!("outcome=\"{outcome}\""))
            })
            .unwrap_or_else(|| panic!("no {outcome} recovery line: {log}")),
        );
    }
    let rollups = first("event_type=\"rollup_boot_recovery\"");
    let conformance = first("event_type=\"catalog_conform");
    let hydration = first("event_type=\"boot_hydration\"");
    for recovered in recovered {
        assert!(
            recovered < rollups,
            "publication recovery precedes rollup recovery: {log}"
        );
    }
    assert!(
        rollups < conformance,
        "rollup recovery precedes conformance: {log}"
    );
    assert!(
        conformance < hydration,
        "conformance precedes hydration: {log}"
    );
    for (step, needle) in &later[..later.len() - 1] {
        assert!(
            hydration < first(needle),
            "hydration must precede the {step}: {log}"
        );
    }
    assert!(
        first("ingest pipeline enabled") < first("action=\"compaction_start\""),
        "self-telemetry is active before compaction starts: {log}"
    );
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

/// What boot publication recovery did with a crashed publish, and so where
/// the first query after the restart finds the acknowledged rows.
#[derive(Debug, Clone, Copy)]
enum Recovered {
    /// Recovery finished the publish: every row is in parquet, and the
    /// consumed WAL is retired.
    Published,
    /// Recovery rolled the publish back and kept the WAL, which the boot
    /// hydrated: parquet holds none of the rows.
    RolledBack,
}

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
        let token = writer_token(&fixture).await;
        let time = recent_time();
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

    /// Restart without a crash point, with the boot pass held, and assert
    /// the first query at readiness counts every acknowledged event exactly
    /// once from where boot recovery left it ([`Recovered`]). No pass can
    /// have published anything yet, so when recovery rolled the publish
    /// back, parquet holds none of the rows and the query counts them from
    /// the hot buffer the boot hydrated from the kept WAL (ADR-0041 slice
    /// 2). Then release the boot pass, which publishes that WAL under a
    /// marker of its own, and assert the count again over the parquet files
    /// and the query API after two further completed compaction ticks. Ends
    /// with no marker, no tmp and no WAL.
    async fn restart_exactly_once(&self, recovered: Recovered) {
        let mut daemon = self.fixture.spawn_holding_boot_pass();
        let client = client(&daemon.ready().await, &self.token);
        daemon.held_at_boot_pass().await;
        let (in_parquet, kept_wal, hydrated) = match recovered {
            Recovered::Published => (self.acknowledged, Vec::new(), "0"),
            Recovered::RolledBack => (0, vec![self.wal_file.clone()], "1"),
        };
        assert_eq!(
            parquet_count(&self.fixture.data(), &self.tag),
            in_parquet,
            "parquet at readiness, before any pass"
        );
        assert_eq!(wal_files(&self.fixture), kept_wal, "WAL at readiness");
        assert!(
            !marker_path(&self.fixture).exists(),
            "boot recovery resolved the marker"
        );
        let log = daemon.log();
        let hydration = boot_hydration(&log);
        assert_eq!(
            (
                log_field(hydration, "hydrated"),
                log_field(hydration, "overhang")
            ),
            (hydrated, "false"),
            "{hydration}"
        );
        assert_api_exactly_once(
            &client,
            &self.tag,
            self.acknowledged,
            "the first query after the restart",
        )
        .await;

        self.fixture.release_boot_pass();
        wait_for_drain(
            &self.fixture,
            "the boot pass left a marker or WAL after its release",
        )
        .await;
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
        assert_api_exactly_once(
            client,
            &self.tag,
            self.acknowledged,
            &format!("query API {when}"),
        )
        .await;
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

/// A key allowed to ingest and query, minted in the fixture's keystore.
async fn writer_token(fixture: &Fixture) -> String {
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
    key.plaintext_token.as_str().to_owned()
}

/// An `_time` five minutes ago, inside `last=24h`.
fn recent_time() -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(5))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
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

/// Exact count of events tagged `tag` over every published parquet file;
/// 0 when nothing was ever published.
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
    let env = data.join("prod");
    if env.exists() {
        walk(&env, &mut files);
    }
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

/// Assert the query API counts every event tagged `tag` exactly once:
/// `expected` rows, and as many distinct `seq` values, so no event is
/// missing while another is doubled.
async fn assert_api_exactly_once(
    client: &trawl_client::HttpClient,
    tag: &str,
    expected: i64,
    when: &str,
) {
    let dsl = format!("crash_tag=\"{tag}\" last=24h | stats count(), dc(seq)");
    let result = client.query_paginated(&dsl, None, None).await.unwrap();
    assert!(result.result.row_count() <= 1, "{dsl}");
    // A corpus with no source at all answers no row, not a row of zeros.
    let counts: Vec<i64> = result.result.rows.first().map_or_else(
        || vec![0, 0],
        |row| {
            row.iter()
                .map(|value| match value {
                    trawl_api::value::Value::Integer(n) => *n,
                    other => panic!("{dsl}: counts must be integers, got {other:?}"),
                })
                .collect()
        },
    );
    assert_eq!(
        counts,
        vec![expected, expected],
        "{when}: [events, distinct seq] tagged {tag}"
    );
}

/// The value of the field `name` on a formatted log line.
fn log_field<'a>(line: &'a str, name: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(name)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("no {name} field: {line}"))
}

/// The one `boot_hydration` line in a daemon's log.
fn boot_hydration(log: &str) -> &str {
    let mut lines = log
        .lines()
        .filter(|line| line.contains("event_type=\"boot_hydration\""));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("no boot_hydration line: {log}"));
    assert!(lines.next().is_none(), "one boot_hydration line: {log}");
    line
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
        wait_for_drain(fixture, "compaction never retired the probe").await;
    }
    probes
}

/// Wait up to 30 s until compaction has retired every WAL file and left no
/// publication marker.
async fn wait_for_drain(fixture: &Fixture, failure: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !wal_files(fixture).is_empty() || marker_path(fixture).exists() {
        assert!(
            Instant::now() < deadline,
            "{failure}: {:?}",
            wal_files(fixture)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
    crashed.restart_exactly_once(Recovered::Published).await;
}

/// AC3 (#252): a kill after the marker and before the rename leaves the
/// marker, the staged tmp and the WAL. Recovery removes the tmp and the
/// marker and keeps the WAL, the boot hydrates it, so the first query
/// counts it (#265), and the next tick publishes it once.
#[tokio::test]
async fn restart_compaction_crash_after_marker_before_rename_is_exactly_once() {
    let crashed = CrashedPublish::at("publish:after_marker").await;
    assert!(marker_path(&crashed.fixture).exists());
    crashed.assert_staged_unpublished();
    crashed.restart_exactly_once(Recovered::RolledBack).await;
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
    crashed.restart_exactly_once(Recovered::Published).await;
}

/// AC2 (#252), real-process complement: a kill during boot recovery of an
/// unpublished marker, after the marker is removed and before the tmp is,
/// reruns without the marker: the WAL is kept, hydrated at boot (#265) and
/// published once.
#[tokio::test]
async fn restart_publication_recovery_crash_in_unpublished_branch_reruns_exactly_once() {
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
    crashed.restart_exactly_once(Recovered::RolledBack).await;
}

/// AC1 (#265): events acknowledged before the process stops, and still in
/// the WAL, are counted exactly once by the first successful query after
/// the restart, before any compaction pass could have published them. All
/// of them arrive in ONE request, so one WAL file holds them. An hour
/// between compaction ticks keeps any pass from publishing it before the
/// stop. The restart holds its boot pass, the first pass, which would
/// otherwise publish the file at once: the query then finds no parquet row
/// for the tag and counts every event from the hot buffer, which the boot
/// hydrated before the listener bound. Released, the boot pass publishes
/// the file and drains the hydrated batch under one guard, and the count
/// stays exact.
async fn restart_counts_every_acknowledged_event_once(kill: bool) {
    let mut fixture = Fixture::new().await;
    fixture.current_fleet().await;
    fixture.compaction_interval_secs = Some(3600);
    let token = writer_token(&fixture).await;
    let time = recent_time();
    let tag = format!(
        "restart-{}-{}",
        std::process::id(),
        if kill { "kill9" } else { "graceful" }
    );

    let mut daemon = fixture.spawn();
    let acknowledged = ingest(
        &client(&daemon.ready().await, &token),
        &time,
        &tag,
        CRASH_EVENTS,
    )
    .await;
    let [wal_file] = <[PathBuf; 1]>::try_from(wal_files(&fixture))
        .unwrap_or_else(|files| panic!("one request, one WAL file: {files:?}"));
    if kill {
        daemon.child.kill().unwrap();
        daemon.child.wait().unwrap();
    } else {
        daemon.stop().await;
    }
    fixture.assert_lock_free().await;
    assert!(wal_file.is_file(), "the acknowledged WAL survived the stop");
    assert!(
        !fixture.data().join("prod").exists(),
        "nothing was compacted before the stop"
    );

    let mut daemon = fixture.spawn_holding_boot_pass();
    let client = client(&daemon.ready().await, &token);
    daemon.held_at_boot_pass().await;
    assert_eq!(
        parquet_count(&fixture.data(), &tag),
        0,
        "no pass has published the acknowledged WAL"
    );
    assert_eq!(wal_files(&fixture), vec![wal_file.clone()]);
    assert_api_exactly_once(
        &client,
        &tag,
        acknowledged,
        "the first query after the restart, from the hot buffer alone",
    )
    .await;
    let log = daemon.log();
    let hydration = boot_hydration(&log);
    assert_eq!(
        (
            log_field(hydration, "hydrated"),
            log_field(hydration, "events"),
            log_field(hydration, "overhang"),
        ),
        ("1", CRASH_EVENTS.to_string().as_str(), "false"),
        "the acknowledged WAL file became resident: {hydration}"
    );

    fixture.release_boot_pass();
    wait_for_drain(&fixture, "the released boot pass never published the WAL").await;
    assert!(!wal_file.exists(), "the boot pass retired the WAL");
    assert_eq!(
        parquet_count(&fixture.data(), &tag),
        acknowledged,
        "the boot pass published every acknowledged event once"
    );
    assert_api_exactly_once(&client, &tag, acknowledged, "after the boot pass").await;
    daemon.stop().await;
    fixture.assert_lock_free().await;
}

#[tokio::test]
async fn restart_graceful_stop_counts_every_acknowledged_event_once() {
    restart_counts_every_acknowledged_event_once(false).await;
}

#[tokio::test]
async fn restart_kill9_counts_every_acknowledged_event_once() {
    restart_counts_every_acknowledged_event_once(true).await;
}

/// AC14 (#265): a query-only node never reads the WAL. Booted over WAL an
/// ingest node left behind, it hydrates nothing, answers from parquet
/// alone, leaves the WAL untouched and logs exactly one warning carrying
/// the file count and no path.
#[tokio::test]
async fn query_only_boot_with_wal() {
    let mut fixture = Fixture::new().await;
    fixture.current_fleet().await;
    fixture.compaction_interval_secs = Some(1);
    let token = writer_token(&fixture).await;
    let time = recent_time();

    // An ingest node publishes one tagged batch to parquet.
    let mut daemon = fixture.spawn();
    let published = ingest(
        &client(&daemon.ready().await, &token),
        &time,
        "published",
        3,
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !wal_files(&fixture).is_empty() || marker_path(&fixture).exists() {
        assert!(
            Instant::now() < deadline,
            "compaction never published the batch"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon.stop().await;
    fixture.assert_lock_free().await;

    // WAL left behind, in the live writer's format.
    let wal = fixture.storage_root().join("wal");
    for seq in 1..=2_u32 {
        let mut line = serde_json::to_vec(&serde_json::json!({
            "_time": time,
            "_ingested": time,
            "env": "prod",
            "service": CRASH_SERVICE,
            "crash_tag": "unread",
            "message": "never read",
        }))
        .unwrap();
        line.push(b'\n');
        std::fs::write(
            wal.join(format!(
                "prod/{CRASH_SERVICE}_1730000000000_{seq:04x}.ndjson"
            )),
            line,
        )
        .unwrap();
    }
    let before = bytes(&wal);

    fixture.ingest = false;
    let mut daemon = fixture.spawn();
    let client = client(&daemon.ready().await, &token);
    assert_eq!(api_count(&client, "published").await, published);
    assert_eq!(api_count(&client, "unread").await, 0, "the WAL is not read");
    let log = daemon.log();
    let warnings: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("event_type=\"wal_present_on_query_node\""))
        .collect();
    assert_eq!(warnings.len(), 1, "{log}");
    assert!(warnings[0].contains("WARN"), "{log}");
    assert!(warnings[0].contains("files=2"), "{log}");
    assert!(
        !warnings[0].contains(&*fixture.root.path().to_string_lossy()),
        "no path: {log}"
    );
    assert!(
        !log.contains("event_type=\"boot_hydration\""),
        "a query-only node hydrates nothing: {log}"
    );
    daemon.stop().await;
    fixture.assert_lock_free().await;
    assert_eq!(bytes(&wal), before, "the WAL is untouched");
}

/// A query for every event tagged `tag` answers 503 `corpus_recovering`,
/// with no `Retry-After` (ADR-0041 slice 2).
async fn assert_search_refused(url: &str, token: &str, tag: &str) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{url}/api/v1/query"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "query": format!("crash_tag=\"{tag}\" last=24h | stats count()"),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(
        response.headers().get("retry-after").is_none(),
        "no retry hint for a corpus refusal"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "corpus_recovering", "{body}");
}

/// `/api/v1/health` at HTTP 200: its `status` and `checks.corpus`.
async fn corpus_health(url: &str) -> (String, String) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{url}/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    (
        body["status"].as_str().unwrap().to_owned(),
        body["checks"]["corpus"].as_str().unwrap().to_owned(),
    )
}

/// AC13 (#265): a contradictory publication marker at boot keeps its scope
/// out of hydration, and search stays refused as `restart_backlog` pass
/// after pass for as long as the marker stays: a standing fault refuses
/// all search (ADR-0041 slice 2). Another service's WAL hydrates and
/// drains meanwhile.
///
/// The marker's canonical output and its temporary output are both gone
/// (`output_missing`), so the WAL it lists was never published. That is
/// what the `TrawlPublicationRecoveryBlocked` runbook asks an operator to
/// establish before removing a marker, because removing it makes
/// compaction merge that WAL. With the marker gone, the next compaction
/// pass merges the WAL once and its coverage proof settles the corpus:
/// `corpus_settled` is logged, health reads `ok`, and reads answer 200
/// with every planted event counted once.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one scenario, stated in order
async fn blocked_marker_at_boot_refuses_reads_until_resolved() {
    use trawl_server::ingest::publication_marker::{ValidatedMarker, identity_of, write_marker};

    const BLOCKED: &str = "blockedsvc";
    const FREE: &str = "freesvc";
    const BLOCKED_EVENTS: i64 = 7;
    const FREE_EVENTS: i64 = 5;

    let mut fixture = Fixture::new().await;
    fixture.current_fleet().await;
    fixture.compaction_interval_secs = Some(1);
    let token = writer_token(&fixture).await;
    // A first boot initializes the data root and databases.
    let mut daemon = fixture.spawn();
    daemon.ready().await;
    daemon.stop().await;
    fixture.assert_lock_free().await;

    // WAL in the live writer's name and line format, every event tagged
    // with its service.
    let wal = fixture.storage_root().join("wal");
    let data = fixture.data();
    let observed = chrono::Utc::now() - chrono::Duration::minutes(5);
    let time = observed.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let millis = chrono::Utc::now().timestamp_millis();
    std::fs::create_dir_all(wal.join("prod")).unwrap();
    let plant = |service: &str, seq: u32, count: i64| {
        let name = format!("{service}_{millis}_{seq:04x}.ndjson");
        let mut ndjson = Vec::new();
        for n in 0..count {
            serde_json::to_writer(
                &mut ndjson,
                &serde_json::json!({
                    "_time": time,
                    "_ingested": time,
                    "env": "prod",
                    "service": service,
                    "crash_tag": service,
                    "seq": n,
                    "message": format!("{service} {n}"),
                }),
            )
            .unwrap();
            ndjson.push(b'\n');
        }
        let path = wal.join("prod").join(&name);
        std::fs::write(&path, ndjson).unwrap();
        (name, path)
    };
    let (blocked_name, blocked_wal) = plant(BLOCKED, 1, BLOCKED_EVENTS);
    let (_, free_wal) = plant(FREE, 2, FREE_EVENTS);

    // A marker for a publish of the blocked WAL whose staged output was
    // then lost, before any rename: recovery cannot tell whether the rows
    // were published, so it touches nothing.
    let hour = u8::try_from(chrono::Timelike::hour(&observed)).unwrap();
    let day = observed.date_naive();
    let tmp = data.join(format!("prod/{day}/{hour:02}/{BLOCKED}.parquet.tmp"));
    std::fs::create_dir_all(tmp.parent().unwrap()).unwrap();
    std::fs::write(&tmp, b"PAR1 staged output PAR1").unwrap();
    let marker = ValidatedMarker::new(
        "prod",
        BLOCKED,
        day,
        hour,
        vec![blocked_name],
        identity_of(&tmp).unwrap(),
    )
    .unwrap();
    assert_eq!(marker.tmp(&data), tmp);
    write_marker(&wal, &marker).unwrap();
    std::fs::remove_file(&tmp).unwrap();

    // The boot serves, degraded: the blocked scope is not hydrated, so the
    // corpus is overhang and every search is refused.
    let mut daemon = fixture.spawn();
    let url = daemon.serving("degraded").await;
    let log = daemon.log();
    assert!(
        log.lines().any(|line| {
            line.contains("event_type=\"publication_recovery_failed\"")
                && line.contains("reason=\"output_missing\"")
        }),
        "boot recovery left the contradiction alone: {log}"
    );
    let hydration = boot_hydration(&log);
    assert_eq!(
        (
            log_field(hydration, "hydrated"),
            log_field(hydration, "claimed"),
            log_field(hydration, "overhang"),
        ),
        ("1", "1", "true"),
        "only the unblocked scope is hydrated: {hydration}"
    );
    assert_eq!(
        corpus_health(&url).await,
        ("degraded".to_owned(), "restart_backlog".to_owned())
    );
    for tag in [BLOCKED, FREE] {
        assert_search_refused(&url, &token, tag).await;
    }
    daemon
        .wait_for_log("the refusal was not logged as restart_backlog", |line| {
            line.contains("event_type=\"http_failure\"")
                && line.contains("cause_kind=\"restart_backlog\"")
        })
        .await;

    // A standing fault: pass after pass, the proof finds the marker
    // pending, while the unblocked service drains.
    daemon
        .wait_for_lines("no second pass found the marker pending", 2, |line| {
            line.contains("event_type=\"coverage_proof\"")
                && line.contains("outcome=\"marker_pending\"")
        })
        .await;
    assert!(!free_wal.exists(), "the unblocked WAL drained");
    assert_eq!(parquet_count(&data, FREE), FREE_EVENTS);
    assert!(blocked_wal.is_file(), "the blocked WAL is untouched");
    assert!(marker.marker_path(&wal).is_file(), "the marker stays");
    assert_search_refused(&url, &token, FREE).await;
    assert_eq!(
        corpus_health(&url).await,
        ("degraded".to_owned(), "restart_backlog".to_owned())
    );

    // Resolve it as the runbook directs: neither the canonical output nor
    // the temporary one exists, so the rows the marker lists are not in a
    // published file, and the marker can go.
    assert!(!marker.canonical(&data).exists() && !marker.tmp(&data).exists());
    std::fs::remove_file(marker.marker_path(&wal)).unwrap();
    let log = daemon
        .wait_for_log("the next pass never settled the corpus", |line| {
            line.contains("event_type=\"corpus_settled\"")
        })
        .await;
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("event_type=\"corpus_settled\""))
            .count(),
        1,
        "{log}"
    );
    assert!(!blocked_wal.exists(), "the pass merged the blocked WAL");
    assert_eq!(
        corpus_health(&url).await,
        ("ok".to_owned(), "ok".to_owned())
    );
    let client = client(&url, &token);
    for (tag, events) in [(BLOCKED, BLOCKED_EVENTS), (FREE, FREE_EVENTS)] {
        assert_api_exactly_once(&client, tag, events, "after the corpus settled").await;
        assert_eq!(parquet_count(&data, tag), events, "{tag} published once");
    }
    daemon.stop().await;
    fixture.assert_lock_free().await;
}
