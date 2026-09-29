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
    DoctorConfig, PLANTED_APP_URL, PLANTED_FLEET_URL, SECRET, admin, assert_fresh_install_rows,
    assert_no_values, ensure_role, forbidden_shape, migrated_app, migrated_fleet, planted,
    planted_env, report, row, run_doctor, url_values, verdict, write_doctor_config,
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
