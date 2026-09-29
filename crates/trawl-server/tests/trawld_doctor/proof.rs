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

use trawl_api::doctor::{Outcome, Report};

use crate::support::{
    DoctorConfig, PLANTED_APP_URL, PLANTED_FLEET_URL, SECRET, assert_no_values, forbidden_shape,
    planted_env, report, row, run_doctor, write_doctor_config,
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
