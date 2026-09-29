// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The doctor's entry: sealed like `--check-config`, never crash-dump init,
//! and `--config` only from the command line (ADR-0023, ADR-0047).
//!
//! The pattern is `tests/check_config.rs`'s: crash-dump capture is enabled
//! through `TRAWL_CRASH_DUMP_DIR` in a temporary directory, the database
//! URLs carry a planted password, and afterwards the directory is exactly as
//! it was. `trawl_crashdump::init` creates that directory, prunes old dumps
//! in it, and starts a monitor process before trawld reads anything, so an
//! absent directory that stays absent, a full one that stays full, and no
//! child process (which [`run_doctor`] asserts on every run) together show
//! the doctor never reached it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use trawl_api::doctor::{Outcome, Vantage};

#[cfg(target_os = "linux")]
use crate::support::harness_has_no_new_privs;
use crate::support::{
    DoctorConfig, assert_no_values, planted_env, report, run_doctor, run_doctor_observed,
    write_doctor_config,
};

/// Every entry under `dir`, recursively, with its bytes and modification
/// time. `None` when `dir` does not exist.
fn snapshot(dir: &Path) -> Option<BTreeMap<String, (Vec<u8>, std::time::SystemTime)>> {
    fn walk(
        root: &Path,
        dir: &Path,
        into: &mut BTreeMap<String, (Vec<u8>, std::time::SystemTime)>,
    ) {
        for entry in std::fs::read_dir(dir).expect("list") {
            let path = entry.expect("entry").path();
            let meta = std::fs::symlink_metadata(&path).expect("metadata");
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let modified = meta.modified().expect("mtime");
            if meta.is_dir() {
                into.insert(format!("{name}/"), (Vec::new(), modified));
                walk(root, &path, into);
            } else {
                into.insert(name, (std::fs::read(&path).expect("read"), modified));
            }
        }
    }
    if !dir.exists() {
        return None;
    }
    let mut entries = BTreeMap::new();
    walk(dir, dir, &mut entries);
    Some(entries)
}

/// The names directly under `dir`.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn doctor_seals_before_config() {
    // Once with no dump directory, once with one holding more dumps than
    // capture would retain: init would create the first and prune the
    // second.
    for existing in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let dumps = root.path().join("dumps");
        if existing {
            std::fs::create_dir(&dumps).unwrap();
            for name in ["a.dmp", "b.dmp", "c.dmp"] {
                std::fs::write(dumps.join(name), name).unwrap();
            }
        }
        let before = snapshot(&dumps);
        let config = write_doctor_config(root.path(), &DoctorConfig::in_dir(root.path()));
        let document = std::fs::read(&config).unwrap();

        for format in ["json", "table"] {
            let mut env = planted_env(root.path());
            env.push(("TRAWL_CRASH_DUMP_DIR", dumps.clone().into_os_string()));
            env.push(("TRAWL_CRASH_DUMP_RETAIN", "1".into()));
            let args: [OsString; 5] = [
                "--doctor".into(),
                "--config".into(),
                config.clone().into_os_string(),
                "--format".into(),
                format.into(),
            ];
            let observed = run_doctor_observed(&args, &env);
            let (code, stdout, stderr) = (observed.code, observed.stdout, observed.stderr);
            assert_no_values(&stdout, &stderr, &[crate::support::PLANTED_FLEET_URL]);
            // The seal sets no_new_privs, and the process was seen with it.
            // That proves something only when this harness lacks it.
            #[cfg(target_os = "linux")]
            {
                assert!(
                    !harness_has_no_new_privs(),
                    "the test harness runs with no_new_privs, so the doctor's seal cannot be \
                     observed; run the suite without it"
                );
                assert!(
                    observed.sealed_seen,
                    "the doctor was never seen sealed: {stderr}"
                );
            }

            // A report, not a usage error: nothing the doctor can reach
            // is healthy, so it is fail or incomplete, never pass.
            assert!(matches!(code, 1 | 3), "exit {code}: {stderr}\n{stdout}");
            if format == "json" {
                let report = report(&stdout);
                assert_eq!(report.vantage(), Vantage::Server);
                assert_eq!(report.target().origin, None);
                let config_row = &report.checks()[0];
                assert_eq!(config_row.id, "server.config");
                assert_eq!(config_row.outcome, Outcome::Complete, "{stdout}");
                assert_eq!(report.verdict().exit_code(), u8::try_from(code).unwrap());
            } else {
                assert!(
                    stdout.starts_with("target: this host (--config "),
                    "{stdout}"
                );
                assert!(stdout.contains(&format!("(exit {code})")), "{stdout}");
            }

            assert_eq!(snapshot(&dumps), before, "the crash-dump directory changed");
            let mut expected = vec!["trawld.toml".to_owned()];
            if existing {
                expected.insert(0, "dumps".to_owned());
            }
            assert_eq!(names(root.path()), expected, "the doctor wrote files");
            assert_eq!(std::fs::read(&config).unwrap(), document);
        }
    }
}

/// A doctor run's environment.
type Env = Vec<(&'static str, OsString)>;

#[test]
fn doctor_requires_config_flag() {
    let root = tempfile::tempdir().unwrap();
    // A directory name that must not be echoed back from TRAWL_CONFIG.
    let private = root.path().join("private-secret-dir");
    std::fs::create_dir(&private).unwrap();
    let config = write_doctor_config(&private, &DoctorConfig::in_dir(&private));
    let dumps = root.path().join("dumps");
    let config_arg = config.clone().into_os_string();

    let with_config_env = |extra: &[(&'static str, OsString)]| {
        let mut env = planted_env(root.path());
        env.push(("TRAWL_CRASH_DUMP_DIR", dumps.clone().into_os_string()));
        env.extend(extra.iter().cloned());
        env
    };

    let cases: Vec<(Vec<OsString>, Env)> = vec![
        // The path only through TRAWL_CONFIG, which the doctor never reads.
        (
            vec!["--doctor".into()],
            with_config_env(&[("TRAWL_CONFIG", config_arg.clone())]),
        ),
        // No path at all.
        (vec!["--doctor".into()], with_config_env(&[])),
        // A value clap refuses, which clap's own message would quote.
        (
            vec![
                "--doctor".into(),
                "--config".into(),
                config_arg.clone(),
                "--format".into(),
                "private-secret".into(),
            ],
            with_config_env(&[]),
        ),
        // The two check modes conflict.
        (
            vec![
                "--doctor".into(),
                "--check-config".into(),
                "--config".into(),
                config_arg.clone(),
            ],
            with_config_env(&[]),
        ),
        // A spelling of the flag clap refuses still seals first.
        (
            vec![
                "--doctor=private-secret".into(),
                "--config".into(),
                config_arg.clone(),
            ],
            with_config_env(&[]),
        ),
    ];
    for (args, env) in cases {
        let (code, stdout, stderr) = run_doctor(&args, &env);
        assert_eq!(code, 2, "{args:?}: {stderr}");
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
        assert!(stderr.contains("--config"), "{args:?}: {stderr}");
        assert!(stderr.contains("--doctor"), "{args:?}: {stderr}");
        assert_no_values(&stdout, &stderr, &["private-secret-dir"]);
        assert!(!dumps.exists(), "{args:?} reached crash-dump init");
        assert_eq!(names(root.path()), ["private-secret-dir"]);
    }
}

/// clap's help shows an env-bound argument's current value, as
/// `[env: NAME=value]`. The doctor's help, short and long, names each
/// variable and shows no value.
#[test]
fn doctor_help_shows_no_environment_value() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("planted-config-value").join("trawld.toml");
    let query_log = root.path().join("planted-query-log-value.ndjson");
    for flag in ["-h", "--help"] {
        let mut env = planted_env(root.path());
        env.push(("TRAWL_CONFIG", config.clone().into_os_string()));
        env.push(("TRAWL_QUERY_LOG", query_log.clone().into_os_string()));
        let (code, stdout, stderr) = run_doctor(&["--doctor", flag], &env);
        assert_eq!(code, 0, "{flag}: {stderr}");
        for name in ["--doctor", "TRAWL_CONFIG", "TRAWL_QUERY_LOG"] {
            assert!(
                stdout.contains(name),
                "{flag} no longer names {name}:\n{stdout}"
            );
        }
        assert_no_values(
            &stdout,
            &stderr,
            &["planted-config-value", "planted-query-log-value"],
        );
        assert!(names(root.path()).is_empty(), "{flag} wrote files");
    }
}

/// `config` with `log_file` set in `[server]` and internal telemetry off,
/// so the file logger is the one in use and `server.log_file` is
/// validated.
fn with_file_log(config: &Path, log_file: &Path) {
    let document = std::fs::read_to_string(config).unwrap();
    let document = document
        .replacen(
            "[server]\n",
            &format!("[server]\nlog_file = '{}'\n", log_file.display()),
            1,
        )
        .replacen("[ingest]\n", "[ingest]\ninternal_telemetry = false\n", 1);
    std::fs::write(config, document).unwrap();
}

/// A `server.log_file` refusal fails `server.config` only when it proves
/// the destination invalid: a reserved storage marker, or a path through a
/// regular file. One the running user may not inspect is `not_sampled`,
/// `permission_denied`. Either way every later check is blocked, no path
/// the configuration names is shown, and nothing is written.
#[cfg(unix)]
#[test]
fn doctor_log_file_refusals_fail_only_when_proven() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("planted-data-root");
    let blocked = root.path().join("planted-log-dir");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let privileged = std::fs::read_dir(&blocked).is_ok();

    let mut doctor = DoctorConfig::in_dir(root.path());
    doctor.data_path.clone_from(&data);
    let config = write_doctor_config(root.path(), &doctor);
    let cases = [
        (
            data.join("EPOCH"),
            Outcome::Failed,
            "server.log_file is a reserved storage marker",
            1,
        ),
        (
            config.join("server.log"),
            Outcome::Failed,
            "server.log_file does not validate",
            1,
        ),
        (
            blocked.join("server.log"),
            Outcome::NotSampled,
            trawl_api::doctor::reason::PERMISSION_DENIED,
            3,
        ),
    ];
    let mut results = Vec::new();
    for (log_file, ..) in &cases {
        write_doctor_config(root.path(), &doctor);
        with_file_log(&config, log_file);
        let args: [OsString; 5] = [
            "--doctor".into(),
            "--config".into(),
            config.clone().into_os_string(),
            "--format".into(),
            "json".into(),
        ];
        results.push(run_doctor(&args, &planted_env(root.path())));
    }
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        !privileged,
        "this harness can traverse a mode 000 directory, so the denied inspection cannot be \
         observed; run the suite as an unprivileged user"
    );

    for ((log_file, outcome, why, exit), (code, stdout, stderr)) in cases.iter().zip(results) {
        assert_no_values(
            &stdout,
            &stderr,
            &[
                crate::support::PLANTED_FLEET_URL,
                crate::support::PLANTED_APP_URL,
                "planted-data-root",
                "planted-log-dir",
                "trawld.toml/server.log",
                "EPOCH",
            ],
        );
        let report = report(&stdout);
        let row = &report.checks()[0];
        assert_eq!(row.id, "server.config");
        assert_eq!(
            (row.outcome, row.reason.as_deref()),
            (*outcome, Some(*why)),
            "{log_file:?}: {stdout}"
        );
        for later in &report.checks()[2..] {
            assert_eq!(
                (later.outcome, later.reason.as_deref()),
                (Outcome::NotSampled, Some("blocked")),
                "{}",
                later.id
            );
        }
        assert_eq!(code, *exit, "{log_file:?}: {stderr}");
    }
    assert_eq!(
        names(root.path()),
        ["planted-log-dir", "trawld.toml"],
        "the doctor wrote files"
    );
    assert!(names(&blocked).is_empty());
}
