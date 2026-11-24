// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What every `trawld --doctor` test runs the doctor with.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sqlx::{Connection as _, Executor as _, PgConnection};
use trawl_api::doctor::{Check, Outcome, Report};

use crate::common;
use crate::common::userns::Userns;

#[path = "shapes.rs"]
mod shapes;

pub use shapes::forbidden_shape;

pub const SECRET: &str = "private-secret";

pub const PLANTED_FLEET_URL: &str = "postgres://user:private-secret@127.0.0.1:1/fleet";

pub const PLANTED_APP_URL: &str = "postgres://user:private-secret@127.0.0.1:1/trawl";

pub const ERROR_FRAGMENTS: &[&str] = &[
    "password authentication failed",
    "os error",
    "no such file",
    "connection refused",
    "permission denied (os",
    "error returned from database",
    "error communicating with database",
    "invalid peer certificate",
];

const RUN_DEADLINE: Duration = Duration::from_secs(60);

/// A `trawld` command with an empty environment, except the loader's
/// library path, which the shared-DuckDB test build needs.
pub fn trawld() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trawld"));
    command.env_clear();
    for name in ["LD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

#[derive(Debug)]
pub struct Observed {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Whether some sample of `/proc/<pid>/status` showed `NoNewPrivs: 1`,
    /// which the crash-dump seal sets.
    pub sealed_seen: bool,
}

pub fn run_doctor<A, K, V>(args: &[A], env: &[(K, V)]) -> (i32, String, String)
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let observed = run_doctor_observed(args, env);
    (observed.code, observed.stdout, observed.stderr)
}

pub fn run_doctor_observed<A, K, V>(args: &[A], env: &[(K, V)]) -> Observed
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = trawld();
    command.args(args);
    observe(command, args, env)
}

fn observe<A, K, V>(mut command: Command, args: &[A], env: &[(K, V)]) -> Observed
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    for (name, value) in env {
        command.env(name, value);
        if name.as_ref() == "HOME" {
            command.current_dir(value.as_ref());
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("trawld starts");

    // Drain both pipes on their own threads, so a report larger than a pipe
    // buffer cannot stall the doctor while this thread watches it.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || read_all(&mut stdout));
    let err = std::thread::spawn(move || read_all(&mut stderr));

    let mut watch = ChildWatch::new(child.id());
    let deadline = Instant::now() + RUN_DEADLINE;
    let status = loop {
        watch.sample();
        if let Some(status) = child.try_wait().expect("wait on trawld") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill a hung trawld");
            let _ = child.wait();
            panic!(
                "trawld {:?} did not exit within {RUN_DEADLINE:?}",
                args.iter().map(AsRef::as_ref).collect::<Vec<_>>()
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let stdout = out.join().expect("stdout reader");
    let stderr = err.join().expect("stderr reader");
    watch.assert_no_child(&stderr);

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert!(
            status.signal().is_none(),
            "trawld died by signal {:?}: {stderr}",
            status.signal()
        );
    }
    Observed {
        code: status.code().expect("an exit status"),
        stdout,
        stderr,
        sealed_seen: watch.sealed_seen,
    }
}

/// [`run_doctor`] as root in a user namespace, with `read_only`, when
/// given, bind-mounted read-only over itself for the run.
pub fn run_doctor_in_userns<A, K, V>(
    userns: &Userns,
    read_only: Option<&Path>,
    args: &[A],
    env: &[(K, V)],
) -> (i32, String, String)
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = userns.command(read_only);
    command.arg(env!("CARGO_BIN_EXE_trawld")).args(args);
    let observed = observe(command, args, env);
    assert!(
        matches!(observed.code, 0..=3),
        "the user namespace set-up failed (exit {}): {}",
        observed.code,
        observed.stderr
    );
    (observed.code, observed.stdout, observed.stderr)
}

/// Whether this test process itself runs with `no_new_privs`, which its
/// children inherit, so that a child showing it proves nothing.
#[cfg(target_os = "linux")]
pub fn harness_has_no_new_privs() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .expect("/proc/self/status")
        .lines()
        .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["NoNewPrivs:", "1"])
}

fn read_all(stream: &mut impl std::io::Read) -> String {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read trawld output");
    String::from_utf8_lossy(&bytes).into_owned()
}

struct ChildWatch {
    pid: u32,
    seen: std::collections::BTreeSet<u32>,
    sealed_seen: bool,
}

impl ChildWatch {
    const fn new(pid: u32) -> Self {
        Self {
            pid,
            seen: std::collections::BTreeSet::new(),
            sealed_seen: false,
        }
    }

    /// Once it is trawld: a user-namespace run starts as `unshare` and a shell,
    /// whose `mount` children are the set-up, not the doctor's.
    fn sample(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let comm = std::fs::read_to_string(format!("/proc/{}/comm", self.pid));
            if comm.ok().as_deref().map(str::trim_end) != Some("trawld") {
                return;
            }
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", self.pid)) {
                self.sealed_seen |= status.lines().any(|line| {
                    line.split_whitespace().collect::<Vec<_>>() == ["NoNewPrivs:", "1"]
                });
            }
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", self.pid)) else {
                return;
            };
            for task in tasks.flatten() {
                if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                    self.seen.extend(
                        children
                            .split_whitespace()
                            .filter_map(|child| child.parse::<u32>().ok()),
                    );
                }
            }
        }
    }

    fn assert_no_child(&self, stderr: &str) {
        assert!(
            self.seen.is_empty(),
            "trawld --doctor started child processes {:?}: {stderr}",
            self.seen
        );
        #[cfg(target_os = "linux")]
        {
            let marker = format!("TRAWL_CRASHDUMP_PARENT={}", self.pid);
            let procs = std::fs::read_dir("/proc").expect("/proc lists");
            for entry in procs.flatten() {
                let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                    continue;
                };
                let found = environ
                    .split(|b| *b == 0)
                    .any(|var| var == marker.as_bytes());
                assert!(
                    !found,
                    "a crash-dump monitor for trawld --doctor (pid {}) is running as pid {}",
                    self.pid,
                    entry.file_name().display()
                );
            }
        }
    }
}

pub fn assert_no_values(stdout: &str, stderr: &str, planted: &[&str]) {
    for (name, stream) in [("stdout", stdout), ("stderr", stderr)] {
        for value in std::iter::once(&SECRET).chain(planted) {
            assert!(
                !value.is_empty() && !stream.contains(value),
                "{name} shows the planted value {value:?}:\n{stream}"
            );
        }
        let lower = stream.to_lowercase();
        for fragment in ERROR_FRAGMENTS {
            assert!(
                !lower.contains(fragment),
                "{name} shows driver or OS error text {fragment:?}:\n{stream}"
            );
        }
        if let Some((shape, found)) = forbidden_shape(stream) {
            panic!("{name} shows {shape} {found:?}:\n{stream}");
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoctorConfig {
    pub http_addr: String,
    pub data_path: PathBuf,
    pub ingest: bool,
    pub wal_dir: Option<PathBuf>,
    pub tls: Option<(PathBuf, PathBuf)>,
    pub fleet_url: Option<String>,
    pub app_url: Option<String>,
    pub extra: String,
}

impl DoctorConfig {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            http_addr: "127.0.0.1:1".to_owned(),
            data_path: dir.join("data"),
            ingest: true,
            wal_dir: None,
            tls: None,
            fleet_url: None,
            app_url: None,
            extra: String::new(),
        }
    }
}

pub fn write_doctor_config(dir: &Path, config: &DoctorConfig) -> PathBuf {
    use std::fmt::Write as _;
    let quote = |path: &Path| toml_string(&path.to_string_lossy());
    let mut document = String::new();
    let mut line = |text: String| writeln!(document, "{text}").expect("writing to a String");
    line("[server]".to_owned());
    line(format!("http_addr = {}", toml_string(&config.http_addr)));
    if let Some((cert, key)) = &config.tls {
        line(format!("tls_cert_path = {}", quote(cert)));
        line(format!("tls_key_path = {}", quote(key)));
    }
    line("[data]".to_owned());
    line(format!("path = {}", quote(&config.data_path)));
    line("[ingest]".to_owned());
    line(format!("enabled = {}", config.ingest));
    if let Some(wal) = &config.wal_dir {
        line(format!("wal_dir = {}", quote(wal)));
    }
    line("[auth]".to_owned());
    if let Some(url) = &config.fleet_url {
        line(format!("database_url = {}", toml_string(url)));
    }
    line("[storage]".to_owned());
    if let Some(url) = &config.app_url {
        line(format!("database_url = {}", toml_string(url)));
    }
    document.push_str(&config.extra);
    let path = dir.join("trawld.toml");
    std::fs::write(&path, document).expect("write trawld.toml");
    path
}

fn toml_string(value: &str) -> String {
    let mut quoted = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                write!(quoted, "\\u{:04X}", u32::from(c)).expect("writing to a String");
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

pub fn planted_env(home: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("HOME", home.as_os_str().to_owned()),
        ("FLEET_DATABASE_URL", PLANTED_FLEET_URL.into()),
        ("TRAWL_DATABASE_URL", PLANTED_APP_URL.into()),
    ]
}

pub fn report(stdout: &str) -> trawl_api::doctor::Report {
    serde_json::from_str(stdout)
        .unwrap_or_else(|error| panic!("not a doctor report ({error}):\n{stdout}"))
}

pub const ROLE: &str = "trawl_doctor_planted";

pub const LOCKLESS: &str = "trawl_doctor_lockless";

pub async fn ensure_role() {
    let mut admin = PgConnection::connect(&common::admin_database_url())
        .await
        .expect("connect to the admin database");
    let mut tx = admin.begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('trawl_doctor_planted'))")
        .execute(&mut *tx)
        .await
        .expect("serialize role setup");
    for role in [ROLE, LOCKLESS] {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(role)
                .fetch_one(&mut *tx)
                .await
                .expect("look up the role");
        if !exists {
            tx.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN")))
                .await
                .expect("create the planted role");
        }
        tx.execute(sqlx::AssertSqlSafe(format!(
            "ALTER ROLE {role} LOGIN NOSUPERUSER PASSWORD '{SECRET}'"
        )))
        .await
        .expect("set the planted password");
    }
    let admin_role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *tx)
        .await
        .expect("admin role");
    tx.execute(sqlx::AssertSqlSafe(format!(
        r#"GRANT "{admin_role}" TO {ROLE}"#
    )))
    .await
    .expect("let the planted role read what the admin role owns");
    let memberships: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.member
         WHERE r.rolname = $1",
    )
    .bind(LOCKLESS)
    .fetch_one(&mut *tx)
    .await
    .expect("look up the lockless role's memberships");
    assert_eq!(memberships, 0, "{LOCKLESS} must inherit nothing");
    tx.commit().await.expect("commit role setup");
    admin.close().await.expect("close");
}

pub const ADVISORY_FUNCTIONS: &str =
    "SELECT oid FROM pg_proc WHERE proname ~ '^pg_(try_)?advisory'";

/// Function privileges live in each database's own `pg_proc`, so this
/// changes no other database.
pub async fn forbid_advisory_locks(url: &str) {
    let mut conn = admin(url).await;
    conn.execute(sqlx::AssertSqlSafe(format!(
        "DO $$ DECLARE f regprocedure; BEGIN
            FOR f IN SELECT oid::regprocedure FROM pg_proc WHERE oid IN ({ADVISORY_FUNCTIONS}) LOOP
                EXECUTE format('REVOKE EXECUTE ON FUNCTION %s FROM PUBLIC', f);
                EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO %I', f, current_user);
            END LOOP;
        END $$"
    )))
    .await
    .expect("revoke the advisory-lock functions from PUBLIC");
    conn.execute(sqlx::AssertSqlSafe(format!(
        "GRANT SELECT ON ALL TABLES IN SCHEMA public TO {LOCKLESS}"
    )))
    .await
    .expect("let the lockless role read the ledger and catalog_state");
    let (total, allowed): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT count(*), count(*) FILTER (WHERE has_function_privilege($1, oid, 'EXECUTE'))
         FROM pg_proc WHERE oid IN ({ADVISORY_FUNCTIONS})"
    )))
    .bind(LOCKLESS)
    .fetch_one(&mut conn)
    .await
    .expect("count the advisory-lock functions");
    // Postgres 18 has 21: lock, lock_shared, unlock, unlock_shared,
    // xact_lock and xact_lock_shared in both key forms, their try_
    // forms, and unlock_all.
    assert!(total >= 21, "only {total} advisory-lock functions found");
    assert_eq!(allowed, 0, "{LOCKLESS} may still take an advisory lock");
    conn.close().await.unwrap();
}

pub fn lockless(url: &str) -> String {
    with_login(url, LOCKLESS, Some(SECRET))
}

/// Every database row of `report` is `complete`: what a doctor run as
/// [`LOCKLESS`] shows when it calls no advisory-lock function. A call is
/// refused with `insufficient_privilege`, which turns the row that made
/// it `not_sampled`, `permission_denied`.
pub fn database_rows_complete(report: &Report) -> Result<(), String> {
    for id in [
        "server.fleet.connect",
        "server.fleet.schema",
        "server.app.connect",
        "server.app.schema",
        "server.app.writer",
    ] {
        let check = row(report, id);
        if check.outcome != Outcome::Complete {
            return Err(format!(
                "{id} is {:?} ({:?})",
                check.outcome,
                check.reason.as_deref()
            ));
        }
    }
    Ok(())
}

pub fn with_login(url: &str, user: &str, password: Option<&str>) -> String {
    let (scheme, rest) = url.split_once("://").expect("a URL with a scheme");
    let host_at = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..host_at];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let login = match password {
        Some(password) => format!("{user}:{password}"),
        None => user.to_owned(),
    };
    format!("{scheme}://{login}@{host}{}", &rest[host_at..])
}

pub fn planted(url: &str) -> String {
    with_login(url, ROLE, Some(SECRET))
}

pub fn url_values(url: &str) -> Vec<String> {
    let rest = url.split_once("://").unwrap().1;
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let database = rest[rest.find('/').unwrap() + 1..]
        .split('?')
        .next()
        .unwrap();
    vec![
        url.to_owned(),
        host.to_owned(),
        database.to_owned(),
        ROLE.to_owned(),
        LOCKLESS.to_owned(),
    ]
}

pub async fn admin(url: &str) -> PgConnection {
    PgConnection::connect(url)
        .await
        .expect("connect to a test database")
}

pub async fn migrated_fleet() -> String {
    let url = common::create_fleet_database().await;
    let mut conn = admin(&url).await;
    fleet_auth::MIGRATOR
        .run(&mut conn)
        .await
        .expect("migrate the Fleet database");
    conn.close().await.unwrap();
    url
}

pub async fn migrated_app() -> String {
    let url = common::create_app_database().await;
    let mut conn = admin(&url).await;
    trawl_server::store::migrations::MIGRATOR
        .run(&mut conn)
        .await
        .expect("migrate the app-state database");
    conn.close().await.unwrap();
    url
}

pub fn write_parquet(path: &Path, value: i64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    duckdb::Connection::open_in_memory()
        .unwrap()
        .execute_batch(&format!(
            "COPY (SELECT TIMESTAMP '2026-09-23 07:00:00' AS _time, {value}::BIGINT AS n) \
             TO '{}' (FORMAT PARQUET)",
            path.to_string_lossy().replace('\'', "''")
        ))
        .unwrap();
}

pub fn pending_rollup(day: &Path, service: &str, hours: &[&str]) -> (PathBuf, String) {
    use trawl_server::ingest::publication_marker::identity_of;
    use trawl_server::ingest::rollup_marker::{RollupMarker, marker_file_name};

    let inputs = hours
        .iter()
        .zip(1..)
        .map(|(hour, value)| {
            let name = format!("{hour}/{service}.parquet");
            let path = day.join(&name);
            if !path.exists() {
                write_parquet(&path, value);
            }
            (name, identity_of(&path).unwrap())
        })
        .collect();
    let tmp = day.join(format!("{service}.parquet.tmp"));
    write_parquet(&tmp, -1);
    let canonical = day.join(format!("{service}.parquet"));
    let prior = canonical.exists().then(|| identity_of(&canonical).unwrap());
    let marker = RollupMarker::new(service, identity_of(&tmp).unwrap(), prior, inputs)
        .expect("a valid rollup marker");
    (day.join(marker_file_name(service)), marker.encode())
}

pub fn row<'a>(report: &'a Report, id: &str) -> &'a Check {
    report
        .checks()
        .iter()
        .find(|check| check.id == id)
        .unwrap_or_else(|| panic!("no {id} row: {report:?}"))
}

pub fn verdict<'a>(report: &'a Report, id: &str) -> (Outcome, Option<&'a str>) {
    let check = row(report, id);
    (check.outcome, check.reason.as_deref())
}

pub fn assert_fresh_install_rows(report: &Report) {
    for id in [
        "server.app.schema",
        "server.data.root",
        "server.data.epoch",
        "server.tls.material",
    ] {
        assert_eq!(
            verdict(report, id),
            (Outcome::Complete, Some("will_initialize")),
            "{id}: {report:#?}"
        );
    }
    assert_eq!(
        verdict(report, "server.listener.identity"),
        (Outcome::NotSampled, Some("not_listening")),
        "{report:#?}"
    );
}
