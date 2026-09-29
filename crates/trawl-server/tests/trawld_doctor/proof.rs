// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor` tests of whole installations, and the proof that the
//! doctor writes nothing.
//!
//! The leak rule holds across the binary: every function in
//! `tests/trawld_doctor/` that runs the doctor also calls
//! [`assert_no_values`] on what it printed
//! (`every_doctor_run_is_checked_for_leaks`), and [`assert_no_values`]
//! refuses the shapes of the values no report may show, whatever a test
//! planted (`the_leak_check_refuses_every_forbidden_shape`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sqlx::Connection as _;
use trawl_api::doctor::{Outcome, Report};

use crate::common;
use crate::support::{
    DoctorConfig, LOCKLESS, PLANTED_APP_URL, PLANTED_FLEET_URL, SECRET, Userns, admin,
    assert_fresh_install_rows, assert_no_values, database_rows_complete, ensure_role,
    forbid_advisory_locks, forbidden_shape, lockless, migrated_app, migrated_fleet, planted,
    planted_env, report, row, run_doctor, run_doctor_in_userns, url_values, verdict,
    write_doctor_config,
};

/// The doctor's arguments for `config` in `format`.
fn doctor_args(config: &Path, format: &str) -> [OsString; 5] {
    [
        "--doctor".into(),
        "--config".into(),
        config.as_os_str().to_owned(),
        "--format".into(),
        format.into(),
    ]
}

/// Run the doctor over `config` with `env`, in JSON and then in table
/// form, check both runs for every value in `planted`, and return the exit
/// status and the JSON report. Both forms must exit alike.
fn doctor(config: &Path, env: &[(&str, OsString)], planted: &[String]) -> (i32, Report) {
    let planted: Vec<&str> = planted.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_doctor(&doctor_args(config, "json"), env);
    assert_no_values(&stdout, &stderr, &planted);
    let (table_code, table_out, table_err) = run_doctor(&doctor_args(config, "table"), env);
    assert_no_values(&table_out, &table_err, &planted);
    assert_eq!(code, table_code, "{stdout}\n{table_out}\n{table_err}");
    let parsed = report(&stdout);
    assert_eq!(
        i32::from(parsed.verdict().exit_code()),
        code,
        "the exit status is the verdict's: {stdout}"
    );
    (code, parsed)
}

/// The top-level functions in `source`, as `(name, body)`, each body from
/// its `fn` line to the closing brace rustfmt puts at the same indentation.
/// Nested functions are listed too, inside their parent's body and on
/// their own.
fn functions(source: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let signature = ["pub(crate) ", "pub(super) ", "pub ", ""]
            .iter()
            .filter_map(|vis| trimmed.strip_prefix(vis))
            .find_map(|rest| {
                ["async fn ", "const fn ", "fn "]
                    .iter()
                    .find_map(|kw| rest.strip_prefix(kw))
            });
        let Some(signature) = signature else {
            continue;
        };
        let name: String = signature
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let close = format!("{}}}", " ".repeat(indent));
        let end = if trimmed.ends_with('}') {
            at
        } else {
            (at + 1..lines.len())
                .find(|&end| lines[end] == close)
                .unwrap_or_else(|| panic!("fn {name} at line {} has no closing brace", at + 1))
        };
        found.push((name, lines[at..=end].join("\n")));
    }
    found
}

/// The functions in `source` that run the doctor (call any `run_doctor*`
/// helper) without calling [`assert_no_values`], by name. The helpers
/// themselves are not callers. Comment lines are ignored.
fn unchecked_runs(source: &str) -> (usize, Vec<String>) {
    let mut callers = 0;
    let mut unchecked = Vec::new();
    for (name, body) in functions(source) {
        if name.starts_with("run_doctor") {
            continue;
        }
        let code: String = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let runs = code.match_indices("run_doctor").any(|(at, _)| {
            let rest = &code[at..];
            let ident = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            rest[ident..].starts_with('(') || rest[ident..].starts_with("::<")
        });
        if runs {
            callers += 1;
            if !code.contains("assert_no_values(") {
                unchecked.push(name);
            }
        }
    }
    (callers, unchecked)
}

/// Every function in `tests/trawld_doctor/` that runs the doctor checks
/// what it printed with [`assert_no_values`] in the same function, so no
/// run's output escapes the leak check (#269 AC12). The scan must see the
/// runs of every group, and it catches a run that is not checked.
#[test]
fn every_doctor_run_is_checked_for_leaks() {
    let negative = [
        "fn quiet() {",
        "    let (code, out, err) = run_doctor(&args, &env);",
        "}",
        "fn checked() {",
        "    let x = run_doctor_in_userns(&u, None, &a, &e);",
        "    assert_no_values(&x.1, &x.2, &[]);",
        "}",
    ]
    .join("\n");
    assert_eq!(unchecked_runs(&negative), (2, vec!["quiet".to_owned()]));

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trawld_doctor");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("list tests/trawld_doctor")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    files.sort();
    let mut seen = Vec::new();
    let mut unchecked = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).expect("read a test source");
        let (callers, missing) = unchecked_runs(&source);
        let name = file.file_stem().unwrap().to_string_lossy().into_owned();
        unchecked.extend(missing.into_iter().map(|f| format!("{name}::{f}")));
        seen.push((name, callers));
    }
    assert!(
        unchecked.is_empty(),
        "these run the doctor without assert_no_values: {unchecked:?}"
    );
    for group in ["seal", "db", "storage", "listener", "proof"] {
        assert!(
            seen.iter()
                .any(|(name, callers)| name == group && *callers > 0),
            "the scan found no doctor run in {group}.rs: {seen:?}"
        );
    }
}

/// [`assert_no_values`] refuses each shape no report may show, and passes
/// the text the doctor does print, such as versions, uids and paths.
#[test]
fn the_leak_check_refuses_every_forbidden_shape() {
    for (text, shape) in [
        (
            "catalog 0b7c9d2e-1f3a-4c5b-9d8e-7a6b5c4d3e2f",
            "a catalog id",
        ),
        (
            "sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "a hex fingerprint",
        ),
        (
            "SHA256 Fingerprint=9F:86:D0:81:88:4C:7D:65:9A:2F",
            "a colon-hex fingerprint",
        ),
        ("dialed 127.0.0.1:5514", "an IPv4 address"),
        ("listening on 0.0.0.0", "an IPv4 address"),
        ("at [::1]:5514", "an IPv6 address"),
        ("at [fe80::1%eth0]", "an IPv6 address"),
    ] {
        assert_eq!(
            forbidden_shape(text).map(|(found, _)| found),
            Some(shape),
            "{text}"
        );
        let refused = std::panic::catch_unwind(|| assert_no_values(text, "", &[]));
        assert!(refused.is_err(), "{text} passed the leak check");
        let refused = std::panic::catch_unwind(|| assert_no_values("", text, &[]));
        assert!(refused.is_err(), "{text} on stderr passed the leak check");
    }
    for text in [
        "trawld 0.9.0; uid 1000 (jakub); HTTP 503",
        "--config /tmp/.tmpAbC123/trawld.toml",
        "2026-09-28 10:00:00",
        "1 pending and 0 malformed rollup marker(s)",
        "[server] http_addr in /etc/trawl/trawld.toml",
        "deadbeef",
    ] {
        assert_eq!(forbidden_shape(text), None, "{text}");
        assert_no_values(text, text, &[]);
    }
}

/// The ways a configuration can be malformed, each carrying [`SECRET`]
/// where a parser's message would quote it, and what the doctor must make
/// of it: the `server.config` outcome and reason.
fn malformed_configs(dir: &Path) -> Vec<(&'static str, Vec<u8>, Outcome, &'static str)> {
    let base = std::fs::read_to_string(write_doctor_config(dir, &DoctorConfig::in_dir(dir)))
        .expect("read the base configuration");
    let with = |extra: &str| format!("{base}{extra}").into_bytes();
    let not_parsing = "the configuration does not parse";
    vec![
        (
            "an unterminated string",
            with("[web]\nprivate_secret = \"private-secret\n"),
            Outcome::Failed,
            not_parsing,
        ),
        (
            "an unknown setting named like the secret",
            base.replacen(
                "[server]\n",
                "[server]\nprivate-secret = \"private-secret\"\n",
                1,
            )
            .into_bytes(),
            Outcome::Failed,
            not_parsing,
        ),
        (
            "a setting of the wrong type",
            base.replacen("enabled = true", "enabled = \"private-secret\"", 1)
                .into_bytes(),
            Outcome::Failed,
            not_parsing,
        ),
        (
            "bytes that are not UTF-8",
            {
                let mut bytes = base
                    .replacen(
                        "[auth]\n",
                        "[auth]\ndatabase_url = \"postgres://u:private-secret@h/db\"\n",
                        1,
                    )
                    .into_bytes();
                bytes.extend_from_slice(b"# \xff\xfe private-secret\n");
                bytes
            },
            Outcome::Failed,
            "the configuration is not UTF-8 text",
        ),
    ]
}

/// Malformed input costs a row, never a leak: a configuration that does not
/// parse, database URLs that do not parse, a listener address that is no
/// address, and a certificate and key that are not PEM, each carrying
/// [`SECRET`] where a parser would quote it. Every run is checked in both
/// forms with [`assert_no_values`].
#[test]
fn doctor_malformed_input_leaks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("trawld.toml");
    let planted = vec![PLANTED_FLEET_URL.to_owned(), PLANTED_APP_URL.to_owned()];

    for (label, document, outcome, why) in malformed_configs(dir.path()) {
        std::fs::write(&config, document).unwrap();
        let (code, report) = doctor(&config, &planted_env(dir.path()), &planted);
        let config_row = row(&report, "server.config");
        assert_eq!(
            (config_row.outcome, config_row.reason.as_deref()),
            (outcome, Some(why)),
            "{label}"
        );
        assert_eq!(code, 1, "{label}");
    }

    // Database URLs that do not parse, from the environment and the file.
    let malformed_urls = [
        "postgres://user:private-secret@[::1/fleet",
        "private-secret is not a URL",
        "postgres://user:private-secret@127.0.0.1:notaport/trawl",
        "mysql://user:private-secret@127.0.0.1:1/trawl",
    ];
    for url in malformed_urls {
        let mut shown = planted.clone();
        shown.push(url.to_owned());
        let from_file = DoctorConfig {
            fleet_url: Some(url.to_owned()),
            app_url: Some(url.to_owned()),
            ..DoctorConfig::in_dir(dir.path())
        };
        let path = write_doctor_config(dir.path(), &from_file);
        let env = vec![("HOME", dir.path().as_os_str().to_owned())];
        let (_, from_file) = doctor(&path, &env, &shown);
        let path = write_doctor_config(dir.path(), &DoctorConfig::in_dir(dir.path()));
        let env = vec![
            ("HOME", dir.path().as_os_str().to_owned()),
            ("FLEET_DATABASE_URL", url.into()),
            ("TRAWL_DATABASE_URL", url.into()),
        ];
        let (_, from_env) = doctor(&path, &env, &shown);
        for report in [&from_file, &from_env] {
            for id in ["server.fleet.connect", "server.app.connect"] {
                let check = row(report, id);
                assert_ne!(check.outcome, Outcome::Complete, "{url}: {id}");
                assert!(check.reason.is_some(), "{url}: {id}: {check:?}");
            }
        }
    }

    // A listener address that is no address, and one naming no host that
    // resolves.
    for addr in ["private-secret", "private-secret.invalid:5514"] {
        let mut shown = planted.clone();
        shown.push(addr.to_owned());
        let mut listener = DoctorConfig::in_dir(dir.path());
        listener.http_addr = addr.to_owned();
        let path = write_doctor_config(dir.path(), &listener);
        let (_, report) = doctor(&path, &planted_env(dir.path()), &shown);
        let config_row = row(&report, "server.config");
        let identity = row(&report, "server.listener.identity");
        assert!(
            config_row.outcome == Outcome::Failed
                || (identity.outcome != Outcome::Complete && identity.reason.is_some()),
            "{addr}: {config_row:?} {identity:?}"
        );
    }

    // A configured certificate and key that are not PEM.
    let cert = dir.path().join("private.crt");
    let key = dir.path().join("private.key");
    std::fs::write(
        &cert,
        format!("-----BEGIN CERTIFICATE-----\n{SECRET}\n-----END CERTIFICATE-----\n"),
    )
    .unwrap();
    std::fs::write(&key, format!("{SECRET} is not a key\n")).unwrap();
    let mut pem = DoctorConfig::in_dir(dir.path());
    pem.tls = Some((cert, key));
    let path = write_doctor_config(dir.path(), &pem);
    let (code, report) = doctor(&path, &planted_env(dir.path()), &planted);
    assert_eq!(
        row(&report, "server.tls.material").outcome,
        Outcome::Failed,
        "{report:?}"
    );
    assert_eq!(code, 1);
}

/// Usage errors print a fixed message and exit 2, quoting no argument:
/// an unknown flag, a stray value, and flags missing their values, each
/// spelled with [`SECRET`].
#[test]
fn doctor_usage_errors_quote_no_argument() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_doctor_config(dir.path(), &DoctorConfig::in_dir(dir.path()));
    let config = config.into_os_string();
    let cases: [Vec<OsString>; 6] = [
        vec![
            "--doctor".into(),
            "--config".into(),
            config.clone(),
            "--private-secret".into(),
        ],
        vec![
            "--doctor".into(),
            "--config".into(),
            config.clone(),
            "private-secret".into(),
        ],
        vec![
            "--doctor".into(),
            "--config".into(),
            config.clone(),
            "--format".into(),
        ],
        vec!["--doctor".into(), "--config".into()],
        vec![
            "--doctor".into(),
            "--config".into(),
            config.clone(),
            "--format=private-secret".into(),
        ],
        vec![
            "--doctor".into(),
            "--config".into(),
            config,
            "--config".into(),
            "/private-secret/trawld.toml".into(),
        ],
    ];
    for args in cases {
        let (code, stdout, stderr) = run_doctor(&args, &planted_env(dir.path()));
        assert_no_values(&stdout, &stderr, &["/private-secret/"]);
        assert_eq!(code, 2, "{args:?}: {stderr}");
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
    }
}

/// The subject of every certificate the shared fixture pair and trawld's
/// own generated pair carry (rcgen's default).
const RCGEN_SUBJECT: &str = "rcgen self signed cert";

/// `catalog_state.catalog_id` of the app-state database `url`.
async fn catalog_id(url: &str) -> String {
    let mut conn = admin(url).await;
    let id: String = sqlx::query_scalar("SELECT catalog_id::text FROM catalog_state")
        .fetch_one(&mut conn)
        .await
        .expect("read the catalog id");
    conn.close().await.unwrap();
    id
}

/// A running fixture server over an installation in `dir` that trawld's
/// own boot steps made: the epoch gate initializes the absent data root, as
/// `main` does before anything else, the seed parquet then stands for what
/// ingest wrote, and the fixture's boot (`boot::prepare_corpus`) runs
/// conformance and publishes the `CATALOG` marker.
async fn running_server(dir: &Path) -> common::TestServer {
    trawl_server::epoch::ensure_current_epoch(&dir.join("data"), &dir.join("wal"), true)
        .expect("the epoch gate initializes the data root");
    let data = common::seed_data_root(dir);
    common::setup_in_dir_with_data(dir, data, trawl_server::config::RateLimitConfig::default())
        .await
}

/// A doctor configuration for a running fixture server in `dir`, which
/// [`running_server`] put its data root and WAL under, and what to run the
/// doctor with: the fixture's two databases through the planted role, and
/// every value of the installation the report must not show.
async fn running_installation(
    server: &common::TestServer,
    dir: &Path,
) -> (PathBuf, Vec<(&'static str, OsString)>, Vec<String>) {
    let (cert, key) = common::ensure_test_cert();
    let addr = server
        .url
        .strip_prefix("https://")
        .expect("an https fixture");
    let config = DoctorConfig {
        http_addr: addr.to_owned(),
        data_path: dir.join("data"),
        ingest: true,
        wal_dir: Some(dir.join("wal")),
        tls: Some((cert, key)),
        ..DoctorConfig::in_dir(dir)
    };
    let path = write_doctor_config(dir, &config);
    let fleet = planted(&server.fleet_db_url);
    let app = planted(&server.app_db_url);
    let mut values = url_values(&fleet);
    values.extend(url_values(&app));
    values.extend([
        catalog_id(&server.app_db_url).await,
        RCGEN_SUBJECT.to_owned(),
        "localhost".to_owned(),
        addr.to_owned(),
        config.data_path.display().to_string(),
    ]);
    let env = vec![
        ("HOME", dir.as_os_str().to_owned()),
        ("FLEET_DATABASE_URL", fleet.into()),
        ("TRAWL_DATABASE_URL", app.into()),
    ];
    (path, env, values)
}

/// `(id, outcome, reason)` of every row that is not `complete` or
/// `not_configured`.
fn open_rows(report: &Report) -> Vec<(String, Outcome, Option<String>)> {
    report
        .checks()
        .iter()
        .filter(|check| !matches!(check.outcome, Outcome::Complete | Outcome::NotConfigured))
        .map(|check| (check.id.clone(), check.outcome, check.reason.clone()))
        .collect()
}

/// Against a real, healthy, running trawld with both databases current, a
/// configured certificate and ingest on, the run passes: exit 0, every row
/// `complete` or `not_configured`, the writer lock seen held, trawld's own
/// certificate proven and its health read, and no note (#269 AC2).
#[tokio::test(flavor = "multi_thread")]
async fn doctor_running_installation_passes() {
    ensure_role().await;
    let dir = tempfile::tempdir().unwrap();
    let server = running_server(dir.path()).await;
    let (config, env, planted) = running_installation(&server, dir.path()).await;
    let (code, report) = tokio::task::spawn_blocking(move || doctor(&config, &env, &planted))
        .await
        .expect("the doctor run");

    assert_eq!(open_rows(&report), [], "{report:#?}");
    assert_eq!(code, 0, "{report:#?}");
    assert_eq!(
        verdict(&report, "server.app.writer"),
        (Outcome::Complete, Some("held"))
    );
    for id in [
        "server.data.root",
        "server.data.epoch",
        "server.data.identity",
        "server.data.conformance",
        "server.tls.material",
        "server.listener.identity",
        "server.listener.health",
    ] {
        assert_eq!(verdict(&report, id), (Outcome::Complete, None), "{id}");
    }
    let health_keys = report
        .checks()
        .iter()
        .filter(|check| check.id.starts_with("server.listener.health."))
        .count();
    assert!(health_keys >= 6, "{report:#?}");
    assert!(report.notes().is_empty(), "{:?}", report.notes());
    assert!(!server.serve_task.is_finished(), "trawld stopped serving");
}

/// A fresh installation before its first start: an empty app-state
/// database, a migrated Fleet database, no data root on an ingest node,
/// auto TLS with no pair yet, and trawld not running. Nothing fails, what
/// boot initializes is `complete`/`will_initialize`, the listener is
/// `not_listening`, and the run is incomplete, exit 3 (#269 AC3). The same
/// state with an empty Fleet database is `db::doctor_unmigrated_fleet_fails`.
/// Nothing was created.
#[tokio::test]
async fn doctor_fresh_install_is_incomplete() {
    ensure_role().await;
    let fleet = planted(&migrated_fleet().await);
    let app = planted(&common::create_app_database().await);
    let dir = tempfile::tempdir().unwrap();
    let config = DoctorConfig::in_dir(dir.path());
    let path = write_doctor_config(dir.path(), &config);
    let mut values = url_values(&fleet);
    values.extend(url_values(&app));
    values.push(config.data_path.display().to_string());
    let env = vec![
        ("HOME", dir.path().as_os_str().to_owned()),
        ("FLEET_DATABASE_URL", fleet.into()),
        ("TRAWL_DATABASE_URL", app.into()),
    ];

    let (code, report) = doctor(&path, &env, &values);
    assert_fresh_install_rows(&report);
    assert_eq!(
        open_rows(&report),
        [
            (
                "server.listener.identity".to_owned(),
                Outcome::NotSampled,
                Some("not_listening".to_owned())
            ),
            (
                "server.listener.health".to_owned(),
                Outcome::NotSampled,
                Some("blocked".to_owned())
            ),
        ],
        "{report:#?}"
    );
    assert_eq!(code, 3);
    let mut left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["trawld.toml"], "the doctor created files");
}

/// The note "another process may own this database" is added when trawld's
/// writer lock is held while nothing answers at the listener's address, and
/// it changes no outcome: the report with the lock held, taken by a real
/// `StorageState` as trawld's boot takes it, differs from the one without
/// only in `server.app.writer`'s reason and the note (#269 D6).
#[tokio::test]
async fn doctor_notes_a_writer_lock_without_a_listener() {
    const NOTE: &str = "another process may own this database";
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    let dir = tempfile::tempdir().unwrap();
    let config = DoctorConfig::in_dir(dir.path());
    let path = write_doctor_config(dir.path(), &config);
    let mut values = url_values(&planted(&fleet));
    values.extend(url_values(&planted(&app)));
    values.push(catalog_id(&app).await);
    let env = vec![
        ("HOME", dir.path().as_os_str().to_owned()),
        ("FLEET_DATABASE_URL", planted(&fleet).into()),
        ("TRAWL_DATABASE_URL", planted(&app).into()),
    ];
    let rows = |report: &Report| {
        report
            .checks()
            .iter()
            .map(|check| {
                (
                    check.id.clone(),
                    check.outcome,
                    check.reason.clone(),
                    check.blocked_by.clone(),
                )
            })
            .collect::<Vec<_>>()
    };

    let (free_code, free) = doctor(&path, &env, &values);
    assert!(free.notes().is_empty(), "{:?}", free.notes());
    assert_eq!(
        verdict(&free, "server.app.writer"),
        (Outcome::Complete, Some("not_observed"))
    );

    let pool = common::app_pool(&app).await;
    let holder = trawl_server::store::StorageState::from_pool(pool.clone())
        .await
        .expect("take trawld's writer lock as boot does");
    let (held_code, held) = doctor(&path, &env, &values);
    drop(holder);
    pool.close().await;

    assert_eq!(
        verdict(&held, "server.app.writer"),
        (Outcome::Complete, Some("held"))
    );
    assert_eq!(
        verdict(&held, "server.listener.identity"),
        (Outcome::NotSampled, Some("not_listening"))
    );
    assert_eq!(held.notes().len(), 1, "{:?}", held.notes());
    assert!(held.notes()[0].contains(NOTE), "{:?}", held.notes());
    assert_eq!(held_code, free_code, "the note changed the verdict");
    let mut expected = rows(&free);
    for row in &mut expected {
        if row.0 == "server.app.writer" {
            row.2 = Some("held".to_owned());
        }
    }
    assert_eq!(rows(&held), expected, "the note changed an outcome");
}

/// One entry of a [`fs_snapshot`]: everything a write changes, and not the
/// access time, which a read changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    kind: &'static str,
    mode: u32,
    size: u64,
    inode: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    /// A file's bytes, a symlink's target; empty for a directory.
    content: Vec<u8>,
}

/// Every entry under `root`, `root` itself included as `.`, by its path
/// relative to `root`.
fn fs_snapshot(root: &Path) -> std::collections::BTreeMap<String, Entry> {
    use std::os::unix::fs::MetadataExt as _;
    fn walk(root: &Path, path: &Path, into: &mut std::collections::BTreeMap<String, Entry>) {
        let meta = std::fs::symlink_metadata(path).expect("metadata");
        let kind = if meta.is_dir() {
            "dir"
        } else if meta.file_type().is_symlink() {
            "symlink"
        } else if meta.is_file() {
            "file"
        } else {
            "other"
        };
        let content = match kind {
            "file" => std::fs::read(path).expect("read a file"),
            "symlink" => std::fs::read_link(path)
                .expect("read a link")
                .into_os_string()
                .into_encoded_bytes(),
            _ => Vec::new(),
        };
        let name = path.strip_prefix(root).unwrap().display().to_string();
        into.insert(
            if name.is_empty() {
                ".".to_owned()
            } else {
                name
            },
            Entry {
                kind,
                mode: meta.mode(),
                size: meta.size(),
                inode: meta.ino(),
                mtime: (meta.mtime(), meta.mtime_nsec()),
                ctime: (meta.ctime(), meta.ctime_nsec()),
                content,
            },
        );
        if kind == "dir" {
            let mut children: Vec<PathBuf> = std::fs::read_dir(path)
                .expect("list a directory")
                .map(|entry| entry.expect("an entry").path())
                .collect();
            children.sort();
            for child in children {
                walk(root, &child, into);
            }
        }
    }
    let mut entries = std::collections::BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

/// Fail, naming only the entries that differ, when `after` is not
/// `before`.
fn assert_unchanged(
    before: &std::collections::BTreeMap<String, Entry>,
    after: &std::collections::BTreeMap<String, Entry>,
    label: &str,
) {
    let changed: Vec<String> = before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|name| before.get(*name) != after.get(*name))
        .map(|name| {
            let show = |entry: Option<&Entry>| {
                entry.map(|e| {
                    format!(
                        "{} mode {:o} size {} inode {} mtime {:?} ctime {:?} content {:?}",
                        e.kind,
                        e.mode,
                        e.size,
                        e.inode,
                        e.mtime,
                        e.ctime,
                        String::from_utf8_lossy(&e.content)
                            .chars()
                            .take(80)
                            .collect::<String>()
                    )
                })
            };
            format!(
                "{name}:\n    before {:?}\n    after  {:?}",
                show(before.get(name)),
                show(after.get(name))
            )
        })
        .collect();
    assert!(
        changed.is_empty(),
        "{label} wrote:\n  {}",
        changed.join("\n  ")
    );
}

/// What a write to the database `url` would change: its schemas, every
/// relation outside the system schemas with its kind, file node, owner and
/// grants, every function there, and every row of every table and
/// sequence there, which covers the ledger and `catalog_state`.
async fn db_snapshot(url: &str) -> std::collections::BTreeMap<String, String> {
    let mut conn = admin(url).await;
    let mut snapshot = std::collections::BTreeMap::new();
    let listed: Vec<(String, String)> = sqlx::query_as(
        r"SELECT 'schema ' || nspname, nspowner::regrole::text || ' ' || coalesce(nspacl::text, '')
            FROM pg_namespace
          UNION ALL
          SELECT 'relation ' || n.nspname || '.' || c.relname,
                 c.relkind::text || ' ' || c.relfilenode::text || ' '
                 || c.relowner::regrole::text || ' ' || coalesce(c.relacl::text, '')
            FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\_%'
          UNION ALL
          SELECT 'function ' || p.oid::regprocedure::text, md5(p.prosrc)
            FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
           WHERE n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\_%'
          UNION ALL
          SELECT 'database', coalesce(datacl::text, '') FROM pg_database
           WHERE datname = current_database()",
    )
    .fetch_all(&mut conn)
    .await
    .expect("list the database's objects");
    let relations: Vec<(String, String)> = sqlx::query_as(
        r"SELECT quote_ident(n.nspname) || '.' || quote_ident(c.relname), c.relkind::text
            FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE c.relkind IN ('r', 'S')
             AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\_%'",
    )
    .fetch_all(&mut conn)
    .await
    .expect("list the tables and sequences");
    for (relation, kind) in relations {
        let read = if kind == "S" {
            format!("SELECT last_value::text || ' ' || is_called::text FROM {relation}")
        } else {
            format!("SELECT string_agg(t::text, E'\\n' ORDER BY t::text) FROM {relation} t")
        };
        let rows: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(read))
            .fetch_one(&mut conn)
            .await
            .expect("read a relation's rows");
        snapshot.insert(format!("rows {relation}"), rows.unwrap_or_default());
    }
    snapshot.extend(listed);
    conn.close().await.unwrap();
    snapshot
}

/// Make [`LOCKLESS`] a role that holds only `CONNECT` and `SELECT` in the
/// database `url` names, and no advisory-lock function
/// ([`forbid_advisory_locks`]), and assert that it does: no `CREATE` or
/// `TEMPORARY` on the database, no `CREATE` on `public`, no privilege but
/// `SELECT` on any table there, none on any sequence, and no role
/// attribute that grants more.
async fn connect_and_select_only(url: &str) {
    forbid_advisory_locks(url).await;
    let mut conn = admin(url).await;
    let database: String = sqlx::query_scalar("SELECT current_database()::text")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    sqlx::Executor::execute(
        &mut conn,
        sqlx::AssertSqlSafe(format!(
            r#"REVOKE ALL ON DATABASE "{database}" FROM PUBLIC;
               GRANT CONNECT ON DATABASE "{database}" TO {LOCKLESS};
               REVOKE CREATE ON SCHEMA public FROM PUBLIC;"#
        )),
    )
    .await
    .expect("leave the lockless role CONNECT and SELECT only");
    let held: (bool, bool, bool, bool, i64, i64, i64) = sqlx::query_as(
        r"SELECT has_database_privilege($1, current_database(), 'CONNECT'),
                 has_database_privilege($1, current_database(), 'CREATE')
                   OR has_database_privilege($1, current_database(), 'TEMPORARY'),
                 has_schema_privilege($1, 'public', 'CREATE'),
                 EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1 AND (rolsuper
                   OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls)),
                 (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                   WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
                     AND (has_table_privilege($1, c.oid, 'INSERT')
                       OR has_table_privilege($1, c.oid, 'UPDATE')
                       OR has_table_privilege($1, c.oid, 'DELETE')
                       OR has_table_privilege($1, c.oid, 'TRUNCATE')
                       OR has_table_privilege($1, c.oid, 'REFERENCES')
                       OR has_table_privilege($1, c.oid, 'TRIGGER'))),
                 (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                   WHERE n.nspname = 'public' AND c.relkind = 'S'
                     AND (has_sequence_privilege($1, c.oid, 'USAGE')
                       OR has_sequence_privilege($1, c.oid, 'UPDATE'))),
                 (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                   WHERE n.nspname = 'public' AND c.relkind = 'r'
                     AND NOT has_table_privilege($1, c.oid, 'SELECT'))",
    )
    .bind(LOCKLESS)
    .fetch_one(&mut conn)
    .await
    .expect("read the lockless role's privileges");
    assert_eq!(
        held,
        (true, false, false, false, 0, 0, 0),
        "{LOCKLESS} holds more than CONNECT and SELECT, or cannot read a table"
    );
    conn.close().await.unwrap();
}

/// An installation for [`doctor_writes_nothing`]: its state directory, the
/// parent of `[data] path`, holds the data root, the WAL, the generated
/// certificate, the crash-dump directory and `trawld.toml`.
struct Installation {
    name: &'static str,
    dir: tempfile::TempDir,
    fleet: String,
    app: String,
    catalog: Option<String>,
}

impl Installation {
    fn dumps(&self) -> PathBuf {
        self.dir.path().join("dumps")
    }

    /// Write the configuration of an ingest or a query-only node.
    fn configure(&self, ingest: bool) -> PathBuf {
        let config = DoctorConfig {
            ingest,
            ..DoctorConfig::in_dir(self.dir.path())
        };
        write_doctor_config(self.dir.path(), &config)
    }

    /// The doctor's environment: both databases as [`LOCKLESS`], and
    /// crash-dump capture enabled on this installation's dump directory.
    fn env(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("HOME", self.dir.path().as_os_str().to_owned()),
            ("FLEET_DATABASE_URL", lockless(&self.fleet).into()),
            ("TRAWL_DATABASE_URL", lockless(&self.app).into()),
            ("TRAWL_CRASH_DUMP_DIR", self.dumps().into_os_string()),
            ("TRAWL_CRASH_DUMP_RETAIN", "1".into()),
        ]
    }

    fn planted(&self) -> Vec<String> {
        let mut values = url_values(&lockless(&self.fleet));
        values.extend(url_values(&lockless(&self.app)));
        values.extend(self.catalog.clone());
        values.push(self.dir.path().join("data").display().to_string());
        values.push(RCGEN_SUBJECT.to_owned());
        values
    }

    async fn db_snapshot(&self) -> [std::collections::BTreeMap<String, String>; 2] {
        [db_snapshot(&self.fleet).await, db_snapshot(&self.app).await]
    }
}

/// An installation before its first start: a migrated Fleet database, an
/// empty app-state database, no data root, no certificate, and no
/// crash-dump directory yet (capture would create one).
async fn fresh_installation() -> Installation {
    let fleet = migrated_fleet().await;
    let app = common::create_app_database().await;
    connect_and_select_only(&fleet).await;
    connect_and_select_only(&app).await;
    Installation {
        name: "fresh",
        dir: tempfile::tempdir().unwrap(),
        fleet,
        app,
        catalog: None,
    }
}

/// An installation that has run: both schemas current and conformance
/// recorded, a data root trawld's epoch gate initialized holding parquet
/// and its `CATALOG` marker, WAL with a pending publication marker, a
/// pending rollup marker, the certificate trawld generates, and a
/// crash-dump directory holding more dumps than capture would retain.
async fn populated_installation() -> Installation {
    use trawl_server::ingest::publication_marker::{OutputIdentity, ValidatedMarker};

    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    let mut conn = admin(&app).await;
    sqlx::query("UPDATE catalog_state SET conformed_at = now()")
        .execute(&mut conn)
        .await
        .expect("record conformance");
    conn.close().await.unwrap();
    let catalog = catalog_id(&app).await;
    connect_and_select_only(&fleet).await;
    connect_and_select_only(&app).await;

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let wal = data.join("wal");
    trawl_server::epoch::ensure_current_epoch(&data, &wal, true)
        .expect("the epoch gate initializes the data root");
    common::seed_data_root(dir.path());
    std::fs::write(data.join("CATALOG"), format!("{catalog}\n")).unwrap();

    let batch = "nginx_1700000000000_ab12.ndjson";
    std::fs::create_dir_all(wal.join("prod")).unwrap();
    std::fs::write(
        wal.join("prod").join(batch),
        b"{\"_time\":\"2026-09-23T07:00:00Z\",\"message\":\"pending\"}\n",
    )
    .unwrap();
    let marker = ValidatedMarker::new(
        "prod",
        "nginx",
        chrono::NaiveDate::from_ymd_opt(2026, 9, 23).unwrap(),
        7,
        vec![batch.to_owned()],
        OutputIdentity {
            size: 26,
            hash: blake3::hash(b"PAR1 new output bytes PAR1"),
        },
    )
    .expect("a valid publication marker");
    std::fs::write(marker.marker_path(&wal), marker.encode()).unwrap();
    let day = data.join("prod/2024-01-15");
    std::fs::write(
        day.join(".rollup-nginx"),
        day.join("10/nginx.parquet").display().to_string(),
    )
    .unwrap();

    let _ = rustls::crypto::ring::default_provider().install_default();
    trawl_server::tls::build_server_config(None, None, dir.path())
        .expect("trawld generates its certificate");

    let dumps = dir.path().join("dumps");
    std::fs::create_dir(&dumps).unwrap();
    for name in ["a.dmp", "b.dmp", "c.dmp"] {
        std::fs::write(dumps.join(name), name).unwrap();
    }
    Installation {
        name: "populated",
        dir,
        fleet,
        app,
        catalog: Some(catalog),
    }
}

/// Every file and directory under a root with its write bits cleared, as
/// long as this lives; the modes come back on drop, so the temporary
/// directory can be removed even after a failed assertion.
struct WriteProtected(Vec<(PathBuf, u32)>);

impl WriteProtected {
    fn apply(root: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        fn walk(path: &Path, into: &mut Vec<(PathBuf, u32)>) {
            let meta = std::fs::symlink_metadata(path).unwrap();
            if meta.file_type().is_symlink() {
                return;
            }
            if meta.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    walk(&entry.unwrap().path(), into);
                }
            }
            let mode = meta.permissions().mode() & 0o7777;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o222)).unwrap();
            into.push((path.to_owned(), mode));
        }
        let mut modes = Vec::new();
        walk(root, &mut modes);
        Self(modes)
    }
}

impl Drop for WriteProtected {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        for (path, mode) in self.0.iter().rev() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode));
        }
    }
}

/// How a run of [`doctor_writes_nothing`] saw the installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leg {
    /// As the test's own user, over the tree as it is.
    AsIs,
    /// As the test's own user, with every write bit cleared.
    WriteProtected,
    /// As root in a user namespace, over a read-only bind mount.
    ReadOnlyMount,
}

/// What a run over `installation` must have looked at, so a run that read
/// nothing cannot pass for one that wrote nothing: every database row
/// complete as [`LOCKLESS`] (an advisory-lock call is refused, and a write
/// is refused by the read-only session and the role), and every storage
/// content row run and complete: the epoch, the archive's identity,
/// conformance (`not_configured` on a query-only node), and both recovery
/// checks.
///
/// Only the write-protected leg of an ingest node may stop at the data
/// root, which the running user then cannot write. A root run over the
/// read-only mount never asks about access, so its data root is
/// `ran_as_root` and every content row still has to run.
fn assert_looked(
    installation: &Installation,
    ingest: bool,
    leg: Leg,
    report: &Report,
    label: &str,
) {
    database_rows_complete(report).unwrap_or_else(|why| panic!("{label}: {why}: {report:#?}"));
    let root = verdict(report, "server.data.root");
    match leg {
        Leg::ReadOnlyMount => assert_eq!(
            root,
            (Outcome::NotSampled, Some("ran_as_root")),
            "{label}: {report:#?}"
        ),
        Leg::WriteProtected if ingest => {
            assert_eq!(root.0, Outcome::Failed, "{label}: {report:#?}");
            return;
        }
        Leg::AsIs | Leg::WriteProtected => {
            assert_eq!(root.0, Outcome::Complete, "{label}: {report:#?}");
        }
    }
    let conformance = if ingest {
        Outcome::Complete
    } else {
        Outcome::NotConfigured
    };
    for (id, outcome) in [
        ("server.data.epoch", Outcome::Complete),
        ("server.data.identity", Outcome::Complete),
        ("server.data.conformance", conformance),
        ("server.recovery.repin", Outcome::Complete),
        ("server.recovery.publication", Outcome::Complete),
    ] {
        let check = row(report, id);
        assert_eq!(
            (check.outcome, check.blocked_by.as_deref()),
            (outcome, None),
            "{label}: {id}: {report:#?}"
        );
    }
    if installation.name == "populated" {
        assert_eq!(
            verdict(report, "server.recovery.publication"),
            (Outcome::Complete, Some("pending_at_next_boot")),
            "{label}: {report:#?}"
        );
    }
}

/// `trawld --doctor` writes nothing (#269 AC4, D19). Over a fresh and a
/// populated installation, as an ingest and as a query-only node, the data
/// root, the WAL, the state directory and the crash-dump directory keep
/// every entry's content, mode, size, inode, modification and change time,
/// and both databases keep their objects, grants and rows.
///
/// Leg 1, always: the doctor logs in as a role with only `CONNECT` and
/// `SELECT` and no advisory-lock function, over the tree as it is (where a
/// write would land and show) and then with every write bit cleared (where
/// the doctor must still find what it reads). Leg 2: as root in a user
/// namespace, over a read-only bind mount of the tree, which root cannot
/// write through; skipped only where user namespaces are unavailable and
/// `TRAWL_TEST_REQUIRE_USERNS` is not `1`.
#[tokio::test]
async fn doctor_writes_nothing() {
    ensure_role().await;
    let userns = Userns::for_test("doctor_writes_nothing leg 2");
    for installation in [fresh_installation().await, populated_installation().await] {
        let root = installation.dir.path();
        let databases = installation.db_snapshot().await;
        for ingest in [true, false] {
            let config = installation.configure(ingest);
            let label =
                |leg: &str| format!("{} installation, ingest {ingest}, {leg}", installation.name);

            let before = fs_snapshot(root);
            let (_, report) = doctor(&config, &installation.env(), &installation.planted());
            assert_unchanged(&before, &fs_snapshot(root), &label("as it is"));
            assert_looked(
                &installation,
                ingest,
                Leg::AsIs,
                &report,
                &label("as it is"),
            );
            if installation.name == "fresh" && ingest {
                assert_fresh_install_rows(&report);
            }

            {
                let _protected = WriteProtected::apply(root);
                let before = fs_snapshot(root);
                let (_, report) = doctor(&config, &installation.env(), &installation.planted());
                assert_unchanged(&before, &fs_snapshot(root), &label("write-protected"));
                assert_looked(
                    &installation,
                    ingest,
                    Leg::WriteProtected,
                    &report,
                    &label("write-protected"),
                );
            }

            if let Some(userns) = &userns {
                let planted = installation.planted();
                let planted: Vec<&str> = planted.iter().map(String::as_str).collect();
                let before = fs_snapshot(root);
                let (code, stdout, stderr) = run_doctor_in_userns(
                    userns,
                    Some(root),
                    &doctor_args(&config, "json"),
                    &installation.env(),
                );
                assert_no_values(&stdout, &stderr, &planted);
                let (table_code, table_out, table_err) = run_doctor_in_userns(
                    userns,
                    Some(root),
                    &doctor_args(&config, "table"),
                    &installation.env(),
                );
                assert_no_values(&table_out, &table_err, &planted);
                assert_unchanged(&before, &fs_snapshot(root), &label("read-only mount"));
                let parsed = crate::support::report(&stdout);
                assert_eq!(i32::from(parsed.verdict().exit_code()), code);
                assert_eq!(table_code, code, "{table_out}");
                assert!(
                    row(&parsed, "server.identity")
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.starts_with("uid 0")),
                    "{parsed:#?}"
                );
                assert_looked(
                    &installation,
                    ingest,
                    Leg::ReadOnlyMount,
                    &parsed,
                    &label("read-only mount"),
                );
            }
        }
        assert_eq!(
            installation.db_snapshot().await,
            databases,
            "the {} installation's databases changed",
            installation.name
        );
    }
}

/// Run as root, the doctor cannot pass (#269 AC11, D16). As uid 0 in a
/// user namespace, against the running installation of
/// `doctor_running_installation_passes`, the one access check,
/// `server.data.root`, is `not_sampled`/`ran_as_root`, every content check
/// still reports and completes (the epoch, the archive's identity and
/// conformance, both recovery checks, the certificate, the listener and its
/// health), and so the run is incomplete, exit 3, where the same run as the
/// test's own user exits 0. Skipped only where user namespaces are
/// unavailable and `TRAWL_TEST_REQUIRE_USERNS` is not `1`.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_root_run_is_incomplete() {
    let Some(userns) = Userns::for_test("doctor_root_run_is_incomplete") else {
        return;
    };
    ensure_role().await;
    let dir = tempfile::tempdir().unwrap();
    let server = running_server(dir.path()).await;
    let (config, env, planted) = running_installation(&server, dir.path()).await;
    let (code, report) = tokio::task::spawn_blocking(move || {
        let planted: Vec<&str> = planted.iter().map(String::as_str).collect();
        let (code, stdout, stderr) =
            run_doctor_in_userns(&userns, None, &doctor_args(&config, "json"), &env);
        assert_no_values(&stdout, &stderr, &planted);
        let (table_code, table_out, table_err) =
            run_doctor_in_userns(&userns, None, &doctor_args(&config, "table"), &env);
        assert_no_values(&table_out, &table_err, &planted);
        assert_eq!(table_code, code, "{table_out}");
        (code, report(&stdout))
    })
    .await
    .expect("the doctor run");

    assert_eq!(
        open_rows(&report),
        [(
            "server.data.root".to_owned(),
            Outcome::NotSampled,
            Some("ran_as_root".to_owned())
        )],
        "{report:#?}"
    );
    let identity = row(&report, "server.identity").detail.clone();
    assert!(
        identity
            .as_deref()
            .is_some_and(|detail| detail.starts_with("uid 0")
                && detail.contains("access checks are not sampled")),
        "{identity:?}"
    );
    for id in [
        "server.data.epoch",
        "server.data.identity",
        "server.data.conformance",
        "server.recovery.repin",
        "server.recovery.publication",
        "server.tls.material",
        "server.listener.identity",
        "server.listener.health",
    ] {
        let check = row(&report, id);
        assert_eq!(
            (check.outcome, check.blocked_by.as_deref()),
            (Outcome::Complete, None),
            "{id}"
        );
    }
    assert_ne!(code, 0, "a root run passed");
    assert_eq!(code, 3, "{report:#?}");
    assert!(!server.serve_task.is_finished(), "trawld stopped serving");
}
