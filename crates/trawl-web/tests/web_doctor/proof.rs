// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The command line, malformed input, and the proof that the doctor binds
//! nothing and writes nothing (#271 AC1 and the leak AC, D3, D13, D15).

use std::ffi::OsString;
use std::path::Path;

use trawl_api::doctor::Outcome;

use crate::support::{
    CHECK_IDS, Userns, WebDoctorConfig, assert_unchanged, doctor, doctor_args, fs_snapshot,
    home_env, report, row, run_web_doctor, run_web_doctor_in_userns, verdict, write_ca,
    write_config, write_key,
};

/// A doctor run's environment.
type Env = Vec<(&'static str, OsString)>;

/// Every row after `proxy.identity` is `not_sampled`, `blocked`: the
/// components by `proxy.config`, health by the trust it waits on.
fn assert_blocked_by_config(report: &trawl_api::doctor::Report, label: &str) {
    let ids: Vec<&str> = report.checks().iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, CHECK_IDS, "{label}");
    for check in &report.checks()[2..] {
        let by = if check.id == "proxy.upstream.health" {
            "proxy.upstream.trust"
        } else {
            "proxy.config"
        };
        assert_eq!(
            (
                check.outcome,
                check.reason.as_deref(),
                check.blocked_by.as_deref()
            ),
            (Outcome::NotSampled, Some("blocked"), Some(by)),
            "{label}: {}",
            check.id
        );
    }
}

/// The doctor checks only a file named by `--config` on its command line
/// (ADR-0047, D13): the path through `TRAWL_CONFIG` alone, or the default
/// path even when a file is there, is a usage error, exit 2, that quotes
/// neither path and writes nothing. The same file named by `--config` is
/// checked, so it is the flag, not the file, that the doctor refused.
#[test]
fn web_doctor_requires_config_flag() {
    let root = tempfile::tempdir().unwrap();
    // A directory name a refused run must not echo back from
    // TRAWL_CONFIG. A checked run may show it: it is the operator's
    // selection.
    let private = root.path().join("planted-config-dir");
    std::fs::create_dir(&private).unwrap();
    let fixture = WebDoctorConfig::in_dir(&private);
    let config = write_config(&private, &fixture);
    // The default path, `~/.trawl/trawld.toml`, holds a configuration too.
    let default_dir = root.path().join(".trawl");
    std::fs::create_dir(&default_dir).unwrap();
    write_config(&default_dir, &WebDoctorConfig::in_dir(&default_dir));
    let planted_owned = fixture.planted();
    let mut planted: Vec<&str> = planted_owned.iter().map(String::as_str).collect();
    planted.push("planted-config-dir");

    let with_env = |extra: &[(&'static str, OsString)]| {
        let mut env = home_env(root.path());
        env.extend(extra.iter().cloned());
        env
    };
    let trawl_config = [("TRAWL_CONFIG", config.clone().into_os_string())];
    let cases: Vec<(Vec<OsString>, Env)> = vec![
        // The path only through TRAWL_CONFIG, which the doctor never reads.
        (vec!["--doctor".into()], with_env(&trawl_config)),
        (
            vec!["--doctor".into(), "--format".into(), "json".into()],
            with_env(&trawl_config),
        ),
        // No path: the default, which exists here, is not read either.
        (vec!["--doctor".into()], with_env(&[])),
        // A spelling of the flag clap refuses.
        (
            vec![
                "--doctor=private-secret".into(),
                "--config".into(),
                config.clone().into_os_string(),
            ],
            with_env(&[]),
        ),
    ];
    let before = fs_snapshot(root.path());
    for (args, env) in cases {
        let run = run_web_doctor(&args, &env, &planted);
        assert_eq!(run.code, 2, "{args:?}: {}", run.stderr);
        assert!(run.stdout.is_empty(), "{args:?}: {}", run.stdout);
        assert!(run.stderr.contains("--config"), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains("--doctor"), "{args:?}: {}", run.stderr);
        assert!(!run.stderr.contains(".trawl"), "{args:?}: {}", run.stderr);
    }
    assert_unchanged(&before, &fs_snapshot(root.path()), "a refused doctor run");

    // Named on the command line, the same file is checked.
    let (_, checked) = doctor(&config, &with_env(&trawl_config), &planted_owned);
    assert_eq!(
        verdict(&checked, "proxy.config"),
        (Outcome::Complete, None),
        "{checked:?}"
    );
    assert_eq!(
        checked.target().source,
        format!("--config {}", config.display())
    );
}

/// The listen port held by a live listener does not stop the doctor: it
/// never binds (D15), so it still produces a report, connects nothing to
/// that listener, and leaves the configuration, key and CA directories and
/// HOME exactly as they were.
#[test]
fn web_doctor_never_binds() {
    let home = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    listener.set_nonblocking(true).unwrap();
    let held = listener.local_addr().unwrap();

    let config_dir = home.path().join("etc");
    std::fs::create_dir(&config_dir).unwrap();
    let mut fixture = WebDoctorConfig::in_dir(home.path());
    fixture.bind_addr = Some(held.to_string());
    fixture.cookie_secret_path = Some(write_key(&home.path().join("lib/web.cookie"), 32));
    fixture.upstream_ca_path = Some(write_ca(&home.path().join("tls/cert.pem")));
    let config = write_config(&config_dir, &fixture);

    let before = fs_snapshot(home.path());
    let (code, report) = doctor(&config, &home_env(home.path()), &fixture.planted());
    assert_unchanged(&before, &fs_snapshot(home.path()), "the doctor");

    assert!(matches!(code, 0 | 1 | 3), "exit {code}");
    let config_row = row(&report, "proxy.config");
    assert_eq!(
        (config_row.outcome, config_row.reason.as_deref()),
        (Outcome::Complete, None),
        "{report:?}"
    );
    assert_eq!(
        config_row.detail.as_deref(),
        Some("parses; the listen address comes from the config file")
    );
    let ids: Vec<&str> = report.checks().iter().map(|c| c.id.as_str()).collect();
    assert_eq!(&ids[..CHECK_IDS.len()], CHECK_IDS);
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("something connected to the held listen port: {other:?}"),
    }
}

/// `proxy.config` owns "the listen address resolves" (D3): startup hands
/// the selected address to its listener, which resolves it as
/// `ToSocketAddrs` does, so an address that does not resolve fails the
/// check, exit 1, from the file or from `TRAWL_WEB_BIND_ADDR`, naming its
/// source and echoing neither the value nor a resolver's message. A
/// literal address, bracketed IPv6 and a name such as `localhost` resolve
/// and stay complete. The doctor still binds none of them.
#[test]
fn web_doctor_listen_address_must_resolve() {
    let home = tempfile::tempdir().unwrap();
    let not_resolving = "the listen address is not a host:port that resolves";
    let from_env = |value: &str| {
        let mut env = home_env(home.path());
        env.push(("TRAWL_WEB_BIND_ADDR", value.into()));
        env
    };

    // (label, [web] bind_addr, TRAWL_WEB_BIND_ADDR, the source named)
    let failing: [(&str, Option<&str>, Option<&str>, &str); 4] = [
        (
            "not an address, in the file",
            Some("not-an-address"),
            None,
            "the config file",
        ),
        (
            "not an address, from the environment over a valid file",
            Some("127.0.0.1:0"),
            Some("not-an-address"),
            "TRAWL_WEB_BIND_ADDR",
        ),
        (
            "a host without a port",
            None,
            Some("private-listen-host"),
            "TRAWL_WEB_BIND_ADDR",
        ),
        (
            "a port out of range",
            Some("127.0.0.1:65536"),
            None,
            "the config file",
        ),
    ];
    for (label, file, env_value, named) in failing {
        let mut fixture = WebDoctorConfig::in_dir(home.path());
        fixture.bind_addr = file.map(str::to_owned);
        let config = write_config(home.path(), &fixture);
        let mut planted = fixture.planted();
        planted.extend(env_value.map(str::to_owned));
        let env = env_value.map_or_else(|| home_env(home.path()), from_env);

        let before = fs_snapshot(home.path());
        let (code, report) = doctor(&config, &env, &planted);
        assert_unchanged(&before, &fs_snapshot(home.path()), label);
        assert_eq!(
            verdict(&report, "proxy.config"),
            (Outcome::Failed, Some(not_resolving)),
            "{label}: {report:?}"
        );
        assert_eq!(code, 1, "{label}");
        let config_row = row(&report, "proxy.config");
        assert_eq!(
            config_row.detail.as_deref(),
            Some(format!("the listen address comes from {named}").as_str()),
            "{label}"
        );
        assert_blocked_by_config(&report, label);
    }

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|held| held.local_addr())
        .expect("find a free port")
        .port();
    let localhost = format!("localhost:{port}");
    let ipv6 = format!("[::1]:{port}");
    let resolving: [(&str, Option<&str>, Option<&str>, &str); 3] = [
        (
            "a literal address with port 0",
            Some("127.0.0.1:0"),
            None,
            "the config file",
        ),
        (
            "localhost from the environment",
            None,
            Some(&localhost),
            "TRAWL_WEB_BIND_ADDR",
        ),
        ("bracketed IPv6", Some(&ipv6), None, "the config file"),
    ];
    for (label, file, env_value, named) in resolving {
        let mut fixture = WebDoctorConfig::in_dir(home.path());
        fixture.bind_addr = file.map(str::to_owned);
        let config = write_config(home.path(), &fixture);
        let mut planted = fixture.planted();
        planted.extend(env_value.map(str::to_owned));
        let env = env_value.map_or_else(|| home_env(home.path()), from_env);

        let (_, report) = doctor(&config, &env, &planted);
        assert_eq!(
            verdict(&report, "proxy.config"),
            (Outcome::Complete, None),
            "{label}: {report:?}"
        );
        assert_eq!(
            row(&report, "proxy.config").detail.as_deref(),
            Some(format!("parses; the listen address comes from {named}").as_str()),
            "{label}"
        );
    }
}

/// Write `bytes` as `dir/trawld.toml` and return its path.
fn write_document(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join("trawld.toml");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Malformed input costs `proxy.config` its row, exit 1, never a leak, and
/// blocks every component (D3): configurations that do not parse, each
/// carrying `private-secret` where a parser's message would quote it, bytes that
/// are not UTF-8, and a path that is a FIFO, a directory, or nothing. A
/// FIFO is refused at once, not waited on.
#[test]
fn web_doctor_malformed_config_leaks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = WebDoctorConfig::in_dir(dir.path());
    let base = std::fs::read_to_string(write_config(dir.path(), &fixture)).unwrap();
    let planted = fixture.planted();
    let not_parsing = "the configuration does not parse";

    let documents: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "an unterminated string",
            format!("{base}private_secret = \"private-secret\n").into_bytes(),
            not_parsing,
        ),
        (
            "an unknown setting named like the secret",
            base.replacen("[web]\n", "[web]\nprivate-secret = \"private-secret\"\n", 1)
                .into_bytes(),
            not_parsing,
        ),
        (
            "a setting of the wrong type",
            format!("{base}allow_insecure_cookies = \"private-secret\"\n").into_bytes(),
            not_parsing,
        ),
        (
            "bytes that are not UTF-8",
            {
                let mut bytes = base
                    .replacen(
                        "[web]\n",
                        "[web]\nupstream_url = \"https://u:private-secret@h/\"\n",
                        1,
                    )
                    .into_bytes();
                bytes.extend_from_slice(b"# \xff\xfe private-secret\n");
                bytes
            },
            "the configuration is not UTF-8 text",
        ),
    ];
    for (label, document, why) in documents {
        let config = write_document(dir.path(), &document);
        let (code, report) = doctor(&config, &home_env(dir.path()), &planted);
        assert_eq!(
            verdict(&report, "proxy.config"),
            (Outcome::Failed, Some(why)),
            "{label}"
        );
        assert_eq!(code, 1, "{label}");
        assert_blocked_by_config(&report, label);
    }

    // Paths that are not a regular file, or nothing at all.
    let fifo = dir.path().join("fifo.toml");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
    let not_regular = "the configuration is not a regular file";
    for (label, path, why) in [
        ("a FIFO", fifo, not_regular),
        ("a directory", dir.path().join("data"), not_regular),
        (
            "nothing",
            dir.path().join("absent.toml"),
            "the configuration file does not exist",
        ),
    ] {
        if label == "a directory" {
            std::fs::create_dir_all(&path).unwrap();
        }
        let started = std::time::Instant::now();
        let (code, report) = doctor(&path, &home_env(dir.path()), &planted);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{label} took {:?}",
            started.elapsed()
        );
        assert_eq!(
            verdict(&report, "proxy.config"),
            (Outcome::Failed, Some(why)),
            "{label}"
        );
        assert_eq!(code, 1, "{label}");
        assert_blocked_by_config(&report, label);
    }
}

/// Usage errors print a fixed message and exit 2, quoting no argument (D13):
/// an unknown flag, a stray value, flags missing their values, a format
/// clap refuses, and a repeated `--config`, each spelled with `private-secret`.
#[test]
fn web_doctor_usage_errors_quote_no_argument() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &WebDoctorConfig::in_dir(dir.path())).into_os_string();
    let doctor_config =
        || -> Vec<OsString> { vec!["--doctor".into(), "--config".into(), config.clone()] };
    let with = |extra: &[&str]| -> Vec<OsString> {
        let mut args = doctor_config();
        args.extend(extra.iter().map(OsString::from));
        args
    };
    let cases: [Vec<OsString>; 7] = [
        with(&["--private-secret"]),
        with(&["private-secret"]),
        with(&["--format"]),
        with(&["--format=private-secret"]),
        with(&["--config", "/private-secret/trawld.toml"]),
        vec!["--doctor".into(), "--config".into()],
        vec![
            "--doctor".into(),
            "--format".into(),
            "json".into(),
            "--config".into(),
        ],
    ];
    let before = fs_snapshot(dir.path());
    for args in cases {
        let run = run_web_doctor(&args, &home_env(dir.path()), &["/private-secret/"]);
        assert_eq!(run.code, 2, "{args:?}: {}", run.stderr);
        assert!(run.stdout.is_empty(), "{args:?}: {}", run.stdout);
        assert!(
            run.stderr
                .contains("Usage: trawl-web --doctor --config <PATH>"),
            "{args:?}: {}",
            run.stderr
        );
    }
    assert_unchanged(&before, &fs_snapshot(dir.path()), "a refused doctor run");
}

/// clap's help shows an env-bound argument's current value, as
/// `[env: NAME=value]`. The doctor's help names `TRAWL_CONFIG` and shows no
/// value.
#[test]
fn web_doctor_help_shows_no_environment_value() {
    let dir = tempfile::tempdir().unwrap();
    let planted = dir.path().join("planted-config-value").join("trawld.toml");
    let env = [("TRAWL_CONFIG", planted.into_os_string())];
    for flag in ["--help", "-h"] {
        let run = run_web_doctor(&["--doctor", flag], &env, &["planted-config-value"]);
        assert_eq!(run.code, 0, "{flag}: {}", run.stderr);
        assert!(
            run.stdout.contains("TRAWL_CONFIG"),
            "{flag}: {}",
            run.stdout
        );
        assert!(run.stdout.contains("--doctor"), "{flag}: {}", run.stdout);
    }
}

/// Run as root, `proxy.identity` is `not_sampled`, `ran_as_root`, so a
/// root run never exits 0, even when every other check could pass (D12).
/// The report shows uid 0.
#[test]
fn web_doctor_root_identity_is_not_sampled() {
    let Some(userns) = Userns::for_test("web_doctor_root_identity_is_not_sampled") else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let fixture = WebDoctorConfig::in_dir(home.path());
    let config = write_config(home.path(), &fixture);
    let planted_owned = fixture.planted();
    let planted: Vec<&str> = planted_owned.iter().map(String::as_str).collect();
    let before = fs_snapshot(home.path());
    let run = run_web_doctor_in_userns(
        &userns,
        None,
        &doctor_args(&config, "json"),
        &home_env(home.path()),
        &planted,
    );
    assert_unchanged(&before, &fs_snapshot(home.path()), "a root doctor run");
    let report = report(&run.stdout);
    let identity = row(&report, "proxy.identity");
    assert_eq!(
        (identity.outcome, identity.reason.as_deref()),
        (Outcome::NotSampled, Some("ran_as_root")),
        "{report:?}"
    );
    assert!(
        identity
            .detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with("uid 0")),
        "{identity:?}"
    );
    assert_ne!(run.code, 0, "{}", run.stdout);
    assert_eq!(i32::from(report.verdict().exit_code()), run.code);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

/// With HOME unset, the home directory `~` in the `--config` path expands
/// to comes from the password database, which can be a network service
/// that stalls. The doctor expands the path on its blocking pool under a
/// 5 s deadline, and the identity row's lookup has 2 s, so the run returns
/// within 15 s whatever the lookup does, with `proxy.config` reporting on
/// the path it found: here, one that does not exist.
#[test]
fn web_doctor_tilde_config_without_home_returns() {
    let missing = format!(
        "~/.trawl-web-doctor-absent-{}/trawld.toml",
        std::process::id()
    );
    let started = std::time::Instant::now();
    let run = run_web_doctor(
        &["--doctor", "--config", &missing, "--format", "json"],
        &[] as &[(&str, &str)],
        &[],
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "took {elapsed:?}"
    );
    let report = report(&run.stdout);
    assert_eq!(
        verdict(&report, "proxy.config"),
        (
            Outcome::Failed,
            Some("the configuration file does not exist")
        ),
        "{}",
        run.stdout
    );
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

/// `~` in the key and CA paths expands where each file is read, as at
/// startup: the key file and the pinned CA under HOME are found through
/// `~`, and each row shows the path as the configuration writes it.
#[test]
fn web_doctor_expands_tilde_where_each_file_is_read() {
    let home = tempfile::tempdir().unwrap();
    write_key(&home.path().join("lib").join("web.cookie"), 32);
    write_ca(&home.path().join("tls").join("ca.pem"));
    let mut fixture = WebDoctorConfig::in_dir(home.path());
    fixture.cookie_secret_path = Some("~/lib/web.cookie".into());
    fixture.upstream_ca_path = Some("~/tls/ca.pem".into());
    let config = write_config(home.path(), &fixture);

    let (_, report) = doctor(&config, &home_env(home.path()), &fixture.planted());
    assert_eq!(
        verdict(&report, "proxy.cookie_key"),
        (Outcome::Complete, None)
    );
    let key = row(&report, "proxy.cookie_key");
    assert!(
        key.source
            .as_deref()
            .is_some_and(|source| source.contains("~/lib/web.cookie")),
        "{key:?}"
    );
    assert_eq!(
        verdict(&report, "proxy.upstream.trust"),
        (Outcome::Complete, None)
    );
    let trust = row(&report, "proxy.upstream.trust");
    assert!(
        trust
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("~/tls/ca.pem")),
        "{trust:?}"
    );
}

/// With HOME unset, expanding `~` in the key and CA paths asks the password
/// database, inside each file's bounded read. The run returns whatever the
/// lookup does: each row reports what its read found at the expanded path,
/// or at the path as written when no home was found, or that the read ran
/// out of time.
#[test]
fn web_doctor_tilde_paths_without_home_return() {
    let dir = tempfile::tempdir().unwrap();
    let absent = format!("~/.trawl-web-doctor-absent-{}", std::process::id());
    let mut fixture = WebDoctorConfig::in_dir(dir.path());
    fixture.cookie_secret_path = Some(format!("{absent}/web.cookie").into());
    fixture.upstream_ca_path = Some(format!("{absent}/ca.pem").into());
    let config = write_config(dir.path(), &fixture);
    let planted = fixture.planted();
    let planted: Vec<&str> = planted.iter().map(String::as_str).collect();

    let started = std::time::Instant::now();
    let run = run_web_doctor(
        &doctor_args(&config, "json"),
        &[] as &[(&str, &str)],
        &planted,
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "took {elapsed:?}"
    );
    let report = report(&run.stdout);
    let timed_out = (Outcome::NotSampled, Some("timed_out"));
    let key = verdict(&report, "proxy.cookie_key");
    assert!(
        key == (Outcome::Failed, Some("the key file does not exist")) || key == timed_out,
        "{}",
        run.stdout
    );
    let trust = verdict(&report, "proxy.upstream.trust");
    assert!(
        trust == (Outcome::NotSampled, Some("ca_not_present")) || trust == timed_out,
        "{}",
        run.stdout
    );
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}
