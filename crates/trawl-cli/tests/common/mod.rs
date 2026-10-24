// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A real trawld, booted in-process, for the CLI suites that drive the
//! `trawl` binary against a server.
//!
//! The server is trawld's own stack, booted the way trawl-server's
//! integration fixture boots it: its own fleet and app databases minted
//! from `DATABASE_URL`, the real boot path (advisory lock, migrations,
//! corpus preparation) and the real HTTPS listener on an ephemeral port
//! with a self-signed certificate the CLI pins. No compaction loop runs;
//! a suite that needs ingested events in Parquet and in the catalog calls
//! [`Server::compact`].
//!
//! Each server drops the databases it minted in [`Server::stop`]. A
//! panicking test leaves them behind under trawl-server's fixture naming
//! scheme (`{prefix}_{run}_{pid}_{rand}`), which that suite's sweeper
//! collects once the owning process is gone.
//!
//! Without `DATABASE_URL` a server does not boot and the test fails rather
//! than skips: a real-server suite that silently skips proves nothing.
//! Every binary that declares this module is in the ADR-0021 `postgres`
//! nextest group.

#![allow(dead_code)] // each test binary uses a subset of these items

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use fleet_auth::{KeyStore, PrincipalKind, RolePermission};
use sqlx::{Connection as _, Executor as _, PgPool, postgres::PgConnection};
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

/// The fixture's default `[server] max_request_body_bytes`.
pub const DEFAULT_BODY_LIMIT: usize = 128 * 1024;

/// The permissions of the agent key: the recipe `trawl mcp` documents.
pub const AGENT_PERMISSIONS: [&str; 3] = ["query", "schema_read", "validate"];

/// What a test may vary about the server it boots.
#[derive(Debug, Clone)]
pub struct Options {
    /// `[ingest] enabled`.
    pub ingest_enabled: bool,
    /// `[server] max_request_body_bytes`.
    pub body_limit: usize,
    /// `[server] link_origin`.
    pub link_origin: Option<String>,
    /// `[ingest] envs`, the env allowlist; empty keeps the default.
    pub envs: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ingest_enabled: true,
            body_limit: DEFAULT_BODY_LIMIT,
            link_origin: None,
            envs: Vec::new(),
        }
    }
}

/// A running trawld and what a test needs to reach it.
pub struct Server {
    pub url: String,
    /// A key with `server_manage` and `query`.
    pub admin_token: String,
    /// A key with `ingest` only: the trial's ingest-key shape.
    pub ingest_token: String,
    /// A key with [`AGENT_PERMISSIONS`].
    pub agent_token: String,
    /// The server's self-signed certificate, pinned by the CLI.
    pub cert_path: PathBuf,
    /// The server's state, shared with its listener.
    pub state: AppState,
    /// The WAL directory ingest writes.
    pub wal: PathBuf,
    /// The data root compaction writes.
    pub data: PathBuf,
    databases: Vec<String>,
    pools: Vec<PgPool>,
    serve_task: tokio::task::JoinHandle<()>,
    /// Background jobs a test started, stopped with the server.
    background: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Server {
    /// Boot a trawld with ingest on or off.
    pub async fn start(ingest_enabled: bool) -> Self {
        Self::start_with(Options {
            ingest_enabled,
            ..Options::default()
        })
        .await
    }

    /// Boot a trawld with ingest on or off and `max_request_body_bytes`
    /// set to `body_limit`.
    pub async fn start_with_body_limit(ingest_enabled: bool, body_limit: usize) -> Self {
        Self::start_with(Options {
            ingest_enabled,
            body_limit,
            ..Options::default()
        })
        .await
    }

    /// Boot a trawld under `options`.
    #[allow(clippy::too_many_lines)] // linear assembly: two databases, one config, one boot
    pub async fn start_with(options: Options) -> Self {
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
        let agent_token = mint_key(&store, "trawl-agent", &AGENT_PERMISSIONS).await;

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
                max_request_body_bytes: options.body_limit,
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
                query_scratch_dir: None,
                results_dir: None,
                link_origin: options.link_origin,
                query_spill_max_bytes: trawl_server::config::DEFAULT_QUERY_SPILL_MAX_BYTES,
                query_memory_limit: trawl_server::config::DEFAULT_QUERY_MEMORY_LIMIT,
                query_threads: trawl_server::config::DEFAULT_QUERY_THREADS,
            },
            data: DataConfig {
                path: data.to_str().expect("utf-8 data path").to_owned(),
            },
            auth: AuthConfig {
                database_url: Some(fleet_db_url.clone()),
                audit_interval_secs: 0,
            },
            ingest: IngestConfig {
                enabled: options.ingest_enabled,
                wal_dir: Some(wal.clone()),
                envs: options.envs,
                ..IngestConfig::default()
            },
            retention: RetentionConfig::default(),
            scheduler: SchedulerConfig::default(),
            syslog: SyslogConfig::default(),
            web: WebConfig::default(),
            storage: StorageConfig {
                database_url: Some(app_db_url.clone()),
            },
            capture: trawl_config::CaptureConfig::default(),
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
        let scratch = trawl_server::state::open_query_scratch(&config).expect("lock the scratch");
        let results_root =
            trawl_server::state::open_results_root(&config).expect("hold the results root");
        let (state, http_config) = AppState::from_parts(
            &config,
            metrics_handle(),
            derivation,
            auth,
            storage,
            scratch,
            results_root,
            trawl_server::results::ResultsClock::System,
        )
        .await
        .expect("build the app state");
        trawl_server::boot::prepare_corpus(&state, &config)
            .await
            .expect("prepare the corpus");

        let server_config = config.server.clone();
        let state_dir = config.state_dir();
        let serve_task = tokio::spawn({
            let state = state.clone();
            async move {
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
            }
        });

        let server = Self {
            url: format!("https://{addr}"),
            admin_token,
            ingest_token,
            agent_token,
            cert_path,
            state,
            wal,
            data,
            databases: vec![database_name(&fleet_db_url), database_name(&app_db_url)],
            pools: vec![fleet, app],
            serve_task,
            background: Vec::new(),
            _dir: dir,
        };
        server.wait_for_ready().await;
        server
    }

    /// A `trawl-client` for this server as `token`, pinning its
    /// certificate, with verification on.
    pub fn client(&self, token: &str) -> trawl_client::HttpClient {
        let pem = std::fs::read(&self.cert_path).expect("read cert");
        trawl_client::HttpClient::with_trust(
            &self.url,
            token,
            &trawl_client::TlsTrust::PinnedCa(pem),
        )
        .expect("build the client")
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

    /// Accept `events` through `/api/v1/ingest` as the ingest key: into the
    /// WAL and the hot buffer.
    pub async fn ingest(&self, events: &[serde_json::Value]) {
        let answer = self
            .client(&self.ingest_token)
            .ingest(events)
            .await
            .expect("the ingest succeeds");
        assert_eq!(answer.accepted, events.len(), "{answer:?}");
    }

    /// One production compaction pass over the WAL into the data root,
    /// draining the hot buffer and recording what it wrote in the field
    /// catalog, as trawld's loop runs it.
    pub async fn compact(&self) {
        let state = &self.state;
        let ingest = trawl_config::IngestConfig::default();
        let catalog = trawl_server::catalog::CatalogContext {
            store: state.storage.catalog.clone(),
            cache: state.query.field_catalog.clone(),
        };
        let failures = trawl_server::ingest::compaction::compact_once(
            &self.wal,
            &self.data,
            Duration::ZERO,
            false,
            state.query.hot_buffer.as_ref(),
            ingest.compaction_chunk_size,
            &ingest.compaction_memory_limit,
            Some(&catalog),
        )
        .await
        .expect("the compaction pass runs");
        assert_eq!(failures, 0, "the compaction pass lost nothing");
    }

    /// Start trawld's schema refresh job, as the daemon's `main` does, and
    /// wait for a pass that fills the per-service schema cache with at
    /// least `services` services. Without it `/api/v1/schema/services`
    /// answers 503.
    pub async fn refresh_schema(&mut self, services: usize) {
        self.background
            .push(trawl_server::schema_refresh::spawn_schema_refresh(
                self.state.clone(),
            ));
        let started = Instant::now();
        loop {
            let filled = self
                .state
                .query
                .service_schema_cache
                .lock()
                .as_ref()
                .is_some_and(|cache| cache.services.len() >= services);
            if filled {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the schema cache did not hold {services} services within 30s"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Stop serving and drop this server's databases.
    pub async fn stop(self) {
        self.serve_task.abort();
        for job in &self.background {
            job.abort();
        }
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
    pub fn config(&self, dir: &Path) -> PathBuf {
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

/// The one pool constructor of every binary that declares this module
/// (ADR-0021 ruling 3).
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
