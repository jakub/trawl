// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl preview-ingest` against a real trawld (ADR-0049).
//!
//! The server is trawld's own stack, booted in-process the way
//! trawl-server's integration fixture boots it: its own fleet and app
//! databases minted from `DATABASE_URL`, the real boot path (advisory
//! lock, migrations, corpus preparation) and the real HTTPS listener on an
//! ephemeral port with a self-signed certificate. The CLI is the real
//! `trawl` binary in a subprocess, so the exit status, stdin and stdout are
//! the ones an operator sees. Every run strips the inherited `TRAWL_*`
//! environment, gets a temp `HOME`, and pins the server's certificate
//! through its own config file.
//!
//! Each test drops the databases it minted when it finishes. A panicking
//! test leaves them behind under trawl-server's fixture naming scheme
//! (`{prefix}_{run}_{pid}_{rand}`), which that suite's sweeper collects
//! once the owning process is gone.
//!
//! Without `DATABASE_URL` these tests fail rather than skip: a real-server
//! suite that silently skips proves nothing.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use fleet_auth::{KeyStore, PrincipalKind, RolePermission};
use sqlx::{Connection as _, Executor as _, PgPool, postgres::PgConnection};
use tokio::io::AsyncWriteExt as _;
use trawl_client::{PreviewEvent, PreviewResponse};
use trawl_server::config::{
    AuthConfig, Config, DataConfig, IngestConfig, RateLimitConfig, RetentionConfig,
    SchedulerConfig, ServerConfig, StorageConfig, SyslogConfig, WebConfig,
};
use trawl_server::state::AppState;

/// Prefixes trawl-server's fixture sweeper collects (`tests/common`).
const APP_DB_PREFIX: &str = "trawl_app_test";
const FLEET_DB_PREFIX: &str = "trawl_fleet_test";

/// Pool sizes, the same as trawl-server's full-server fixture: seven
/// connections a server, inside the postgres group's per-test budget.
const FLEET_POOL_MAX: u32 = 3;
const APP_POOL_MAX: u32 = 3;

/// A sample with every row shape the table renders: an event that keeps
/// its own slots, one repaired from the peer and the arrival time, a blank
/// line (not a position, but it counts toward the next index), a line that
/// is not JSON, an event refused for having no service, and one whose
/// severity maps to nothing.
const MIXED: &str = concat!(
    r#"{"_time":"2026-01-01T10:00:00Z","service":"api","env":"prod","host":"web-1","level":"warn","msg":"disk 91%"}"#,
    "\n",
    r#"{"service":"worker","Level":"error","timestamp":"yesterday","ctx":{"job":7}}"#,
    "\n",
    "\n",
    "not json\n",
    r#"{"msg":"who sent this"}"#,
    "\n",
    r#"{"service":"api","host":"web-2","severity":"loud","_time":"2026-01-01T10:00:01Z"}"#,
    "\n",
);

/// The two events of [`MIXED`] that are accepted, as a JSON array.
const ACCEPTED: &str = r#"[
  {"_time":"2026-01-01T10:00:00Z","service":"api","env":"prod","host":"web-1","level":"warn","msg":"disk 91%"},
  {"service":"worker","Level":"error","timestamp":"yesterday","ctx":{"job":7}}
]"#;

/// A running trawld and what a test needs to reach it.
struct Server {
    url: String,
    /// A key with `server_manage`.
    admin_token: String,
    /// A key with `ingest` only: the trial's ingest-key shape.
    ingest_token: String,
    /// The server's self-signed certificate, pinned by the CLI.
    cert_path: PathBuf,
    databases: Vec<String>,
    pools: Vec<PgPool>,
    serve_task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Server {
    /// Boot a trawld with ingest on or off.
    #[allow(clippy::too_many_lines)] // linear assembly: two databases, one config, one boot
    async fn start(ingest_enabled: bool) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().expect("tempdir");

        let fleet_db_url = create_database(FLEET_DB_PREFIX).await;
        let app_db_url = create_database(APP_DB_PREFIX).await;
        let fleet = fixture_pool(&fleet_db_url, FLEET_POOL_MAX).await;
        fleet_auth::MIGRATOR
            .run(&fleet)
            .await
            .expect("migrate the fleet database");
        let store = KeyStore::from_pool(fleet.clone());
        let admin_token = mint_key(&store, "trawl-admin", &["server_manage", "query"]).await;
        let ingest_token = mint_key(&store, "trawl-ingest", &["ingest"]).await;

        let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
        ])
        .expect("self-signed pair");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.pem()).expect("write cert");
        std::fs::write(&key_path, signing_key.serialize_pem()).expect("write key");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr").to_string();
        let data = dir.path().join("data");
        let wal = dir.path().join("wal");
        std::fs::create_dir_all(&data).expect("data dir");
        std::fs::create_dir_all(&wal).expect("wal dir");

        let config = Config {
            server: ServerConfig {
                http_addr: addr.clone(),
                timeout_secs: 10,
                max_concurrent_queries: 2,
                max_result_rows: 10_000,
                max_export_rows: 10_000,
                max_request_body_bytes: 128 * 1024,
                max_concurrent_requests: 256,
                shutdown_drain_secs: 5,
                log_file: None,
                tls_cert_path: Some(cert_path.clone()),
                tls_key_path: Some(key_path),
                tls_reload_interval_secs: 0,
                cors_allowed_origins: vec![],
                schema_cache_ttl_secs: 60,
                max_query_history: 1000,
                max_sse_connections: 32,
                query_log: None,
                query_log_max_bytes: trawl_server::config::DEFAULT_QUERY_LOG_MAX_BYTES,
                rate_limit: RateLimitConfig::default(),
                monitor_refresh_ms: 1000,
            },
            data: DataConfig {
                path: data.to_str().expect("utf-8 data path").to_owned(),
            },
            auth: AuthConfig {
                database_url: Some(fleet_db_url.clone()),
                audit_interval_secs: 0,
            },
            ingest: IngestConfig {
                enabled: ingest_enabled,
                wal_dir: Some(wal),
                ..IngestConfig::default()
            },
            retention: RetentionConfig::default(),
            scheduler: SchedulerConfig::default(),
            syslog: SyslogConfig::default(),
            web: WebConfig::default(),
            storage: StorageConfig {
                database_url: Some(app_db_url.clone()),
            },
        };

        let app = fixture_pool(&app_db_url, APP_POOL_MAX).await;
        let auth = trawl_server::state::AuthState::from_key_store(store);
        let storage = trawl_server::store::StorageState::from_pool(app.clone())
            .await
            .expect("boot the app-state database");
        let derivation = Arc::new(
            trawl_server::ingest::producer::Derivation::resolve(&config.ingest)
                .expect("the default derivation lists resolve"),
        );
        let (state, http_config) =
            AppState::from_parts(&config, metrics_handle(), derivation, auth, storage)
                .await
                .expect("build the app state");
        trawl_server::boot::prepare_corpus(&state, &config)
            .await
            .expect("prepare the corpus");

        let server_config = config.server.clone();
        let state_dir = config.state_dir();
        let serve_task = tokio::spawn(async move {
            trawl_server::transport::http::serve_with_listener(
                listener,
                state,
                &http_config,
                &server_config,
                &state_dir,
                None,
            )
            .await
            .expect("serve");
        });

        let server = Self {
            url: format!("https://{addr}"),
            admin_token,
            ingest_token,
            cert_path,
            databases: vec![database_name(&fleet_db_url), database_name(&app_db_url)],
            pools: vec![fleet, app],
            serve_task,
            _dir: dir,
        };
        server.wait_for_ready().await;
        server
    }

    /// Poll health until the listener answers, under one deadline.
    async fn wait_for_ready(&self) {
        let pem = std::fs::read(&self.cert_path).expect("read cert");
        let client = trawl_client::HttpClient::with_trust_timeout(
            &self.url,
            "",
            &trawl_client::TlsTrust::PinnedCa(pem),
            Duration::from_millis(500),
        )
        .expect("health client");
        let started = Instant::now();
        while client.health().await.is_err() {
            assert!(
                !self.serve_task.is_finished(),
                "the serve task exited before the server answered"
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "trawld was not ready within 10s"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Stop serving and drop this server's databases.
    async fn stop(self) {
        self.serve_task.abort();
        for pool in &self.pools {
            pool.close().await;
        }
        let mut admin = PgConnection::connect(&admin_database_url())
            .await
            .expect("connect to the admin database");
        for name in &self.databases {
            admin
                .execute(sqlx::AssertSqlSafe(format!(
                    r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#
                )))
                .await
                .expect("drop a test database");
        }
    }

    /// A config file that points the CLI at this server and pins its
    /// certificate.
    fn config(&self, dir: &Path) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[server]\nurl = {:?}\nca_cert = {:?}\n",
                self.url,
                self.cert_path.to_str().expect("utf-8 cert path")
            ),
        )
        .expect("write the CLI config");
        path
    }
}

/// The base admin DSN every database is minted from.
fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for this real-server suite \
         (e.g. postgres://fleet:fleet@localhost:5433/fleet_test); it fails rather than skips",
    )
}

/// Create an empty database named under trawl-server's fixture scheme and
/// return its DSN.
async fn create_database(prefix: &str) -> String {
    use rand::Rng as _;
    let marker: String = std::env::var("NEXTEST_RUN_ID")
        .unwrap_or_else(|_| std::process::id().to_string())
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect();
    let suffix: String = (0..12)
        .map(|_| char::from(b'a' + rand::thread_rng().gen_range(0..26u8)))
        .collect();
    let name = format!("{prefix}_{marker}_{}_{suffix}", std::process::id());
    let admin_url = admin_database_url();
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect to the admin database");
    admin
        .execute(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
        .await
        .expect("CREATE DATABASE — does the role have CREATEDB?");
    swap_database(&admin_url, &name)
}

/// Replace the database of a postgres URL, keeping any query string.
fn swap_database(url: &str, db: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let after_scheme = base.find("://").map_or(0, |i| i + 3);
    let authority = base[after_scheme..]
        .find('/')
        .map_or(base, |i| &base[..after_scheme + i]);
    match query {
        Some(q) => format!("{authority}/{db}?{q}"),
        None => format!("{authority}/{db}"),
    }
}

/// The database a DSN names.
fn database_name(url: &str) -> String {
    url.rsplit('/')
        .next()
        .and_then(|last| last.split('?').next())
        .expect("a database in the url")
        .to_owned()
}

/// This binary's one pool constructor (ADR-0021 ruling 3).
async fn fixture_pool(dsn: &str, max_connections: u32) -> PgPool {
    #[allow(clippy::disallowed_methods)]
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(dsn)
        .await
        .expect("fixture pool connect");
    pool
}

/// The process's one metrics recorder, shared by every server it boots.
fn metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .expect("install the metrics recorder")
        })
        .clone()
}

/// Seed a role holding `permissions` and mint a key in it.
async fn mint_key(store: &KeyStore, role: &str, permissions: &[&str]) -> String {
    let permissions: Vec<RolePermission> = permissions
        .iter()
        .map(|permission| RolePermission {
            app: "trawl".into(),
            permission: (*permission).to_owned(),
        })
        .collect();
    store
        .create_role(role, None, &permissions)
        .await
        .expect("seed a role");
    store
        .create_key(
            &format!("{role}-key"),
            PrincipalKind::Service,
            &[role.to_owned()],
            None,
        )
        .await
        .expect("mint a key")
        .plaintext_token
        .to_string()
}

/// Run `trawl -c <config> preview-ingest <args>`, with `stdin` on its
/// standard input and `token` as `TRAWL_TOKEN`.
async fn trawl(config: &Path, token: Option<&str>, args: &[&str], stdin: &[u8]) -> Output {
    let home = config.parent().expect("config dir");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_trawl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAWL_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("HOME", home)
        .arg("-c")
        .arg(config)
        .arg("preview-ingest")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(token) = token {
        cmd.env("TRAWL_TOKEN", token);
    }
    let mut child = cmd.spawn().expect("spawn trawl");
    let mut input = child.stdin.take().expect("stdin");
    input.write_all(stdin).await.expect("write stdin");
    drop(input);
    tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("trawl finished within 30s")
        .expect("trawl ran")
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("utf-8 output")
}

/// The table with the header's arrival instant, the one value that
/// changes between runs, masked. The instant must have the report's fixed
/// shape (RFC 3339 UTC, microseconds) to be masked at all.
fn mask_arrival(table: &str) -> String {
    let (header, rest) = table.split_once('\n').expect("a header line");
    let (head, arrival) = header.rsplit_once("; arrival ").expect("an arrival");
    let shape = arrival.len() == "2026-01-01T00:00:00.000000Z".len()
        && arrival.ends_with('Z')
        && chrono::DateTime::parse_from_rfc3339(arrival).is_ok();
    assert!(
        shape,
        "arrival {arrival:?} is not RFC 3339 UTC microseconds"
    );
    format!("{head}; arrival [ARRIVAL]\n{rest}")
}

fn write_sample(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write the sample");
    path
}

/// The table, read from stdin with no peer: a rejected event exits 1, the
/// header names the placeholder, and host-less events are marked as
/// depending on the sender. The snapshot pins the whole table; only the
/// arrival instant changes between runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_event_exits_1_and_the_table_shows_every_outcome() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());

    let output = trawl(&config, Some(&server.admin_token), &["-"], MIXED.as_bytes()).await;
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), "");
    insta::assert_snapshot!(
        "table_mixed_sample_no_peer",
        mask_arrival(text(&output.stdout))
    );

    // No FILE argument reads stdin the same way `-` does.
    let bare = trawl(&config, Some(&server.admin_token), &[], MIXED.as_bytes()).await;
    assert_eq!(bare.status.code(), Some(1), "{}", text(&bare.stderr));
    assert_eq!(
        text(&bare.stdout).lines().skip(1).collect::<Vec<_>>(),
        text(&output.stdout).lines().skip(1).collect::<Vec<_>>(),
        "only the header's arrival may differ"
    );
    server.stop().await;
}

/// Every event accepted, read from a file with a peer: exit 0, the peer
/// fills `host`, and nothing depends on the sender.
#[tokio::test(flavor = "multi_thread")]
async fn all_accepted_from_a_file_with_a_peer_exits_0() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());
    let sample = write_sample(dir.path(), "accepted.json", ACCEPTED);

    let output = trawl(
        &config,
        Some(&server.admin_token),
        &[sample.to_str().unwrap(), "--peer-ip", "10.0.0.7"],
        b"",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with("peer 10.0.0.7 (given), not a trusted relay; arrival "),
        "{stdout}"
    );
    assert!(
        stdout.contains("┆ 10.0.0.7 "),
        "the peer fills host: {stdout}"
    );
    assert!(!stdout.contains("(sender)"), "{stdout}");
    assert!(
        stdout.ends_with("2 event(s): 2 accepted, 0 rejected\n"),
        "{stdout}"
    );
    server.stop().await;
}

/// `--json` prints the server's report, whole, and keeps the exit status.
#[tokio::test(flavor = "multi_thread")]
async fn json_prints_the_report_and_keeps_the_exit_status() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());
    let sample = write_sample(dir.path(), "mixed.ndjson", MIXED);

    let output = trawl(
        &config,
        Some(&server.admin_token),
        &["--json", sample.to_str().unwrap()],
        b"",
    )
    .await;
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    let report: PreviewResponse =
        serde_json::from_slice(&output.stdout).expect("stdout is the report");
    assert_eq!(report.producer, "http");
    assert_eq!(report.peer.ip, "192.0.2.1");
    assert!(!report.peer.given);
    assert_eq!((report.accepted, report.rejected), (3, 2));
    let outcomes: Vec<(usize, &str, bool)> = report
        .events
        .iter()
        .map(|event| match event {
            PreviewEvent::Accepted {
                index,
                host_depends_on_sender,
                ..
            } => (*index, "accepted", *host_depends_on_sender),
            PreviewEvent::Rejected {
                index,
                host_depends_on_sender,
                ..
            } => (*index, "rejected", *host_depends_on_sender),
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (0, "accepted", false),
            (1, "accepted", true),
            (3, "rejected", false),
            (4, "rejected", true),
            (5, "accepted", false),
        ]
    );
    server.stop().await;
}

/// A key without `server_manage` gets the server's 403, which is no
/// report: exit 2, and nothing on stdout.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_without_server_manage_exits_2() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());

    let output = trawl(
        &config,
        Some(&server.ingest_token),
        &["-"],
        MIXED.as_bytes(),
    )
    .await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "");
    assert!(
        text(&output.stderr).starts_with("trawl: server error (HTTP 403)"),
        "{}",
        text(&output.stderr)
    );
    server.stop().await;
}

/// A server with ingest off has no preview route. Its 404 says the server
/// does not ingest, and exits 2.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_ingest_exits_2_and_says_it_does_not_ingest() {
    let server = Server::start(false).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());

    let output = trawl(&config, Some(&server.admin_token), &["-"], MIXED.as_bytes()).await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        format!("trawl: {}\n", trawl_cli::preview_ingest::DOES_NOT_INGEST)
    );
    server.stop().await;
}

/// A `--peer-ip` that is not an address is clap's usage error, exit 2,
/// before anything is read or sent.
#[tokio::test]
async fn a_bad_peer_ip_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_sample(
        dir.path(),
        "config.toml",
        "[server]\nurl = \"https://127.0.0.1:1\"\n",
    );
    let output = trawl(
        &config,
        Some("unused"),
        &["--peer-ip", "not-an-ip", "-"],
        b"",
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(text(&output.stdout), "");
    assert!(
        text(&output.stderr).contains("--peer-ip <IP>"),
        "{}",
        text(&output.stderr)
    );
}

/// A sample that cannot be read is exit 2, naming the path, and no
/// request is attempted (the configured port has nothing behind it).
#[tokio::test]
async fn an_unreadable_sample_exits_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_sample(
        dir.path(),
        "config.toml",
        "[server]\nurl = \"https://127.0.0.1:1\"\n",
    );
    let missing = dir.path().join("missing.ndjson");
    let output = trawl(&config, Some("unused"), &[missing.to_str().unwrap()], b"").await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!("trawl: cannot read {}: ", missing.display())),
        "{stderr}"
    );
}
