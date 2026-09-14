// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::process::{Command, Output, Stdio};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trawld"));
    command.env_clear();
    // Keep Cargo's loader paths for the shared-DuckDB test build, while
    // excluding ambient daemon configuration and credentials.
    for name in ["LD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn check(document: &str) -> Output {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("trawld.toml");
    std::fs::write(&path, document).unwrap();
    let mut child = command()
        .args(["--check-config", "--config"])
        .arg(&path)
        .env("HOME", root.path())
        .env(
            "FLEET_DATABASE_URL",
            "postgres://user:private-secret@127.0.0.1:1/fleet",
        )
        .env(
            "TRAWL_DATABASE_URL",
            "postgres://user:private-secret@127.0.0.1:1/trawl",
        )
        .env("TRAWL_CRASH_DUMP_DIR", root.path().join("dumps"))
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "config check failed to exit: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let entries: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["trawld.toml"], "config check wrote files");
    assert_eq!(std::fs::read_to_string(path).unwrap(), document);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-secret"));
    output
}

#[test]
fn config_check_has_no_startup_side_effects() {
    // An occupied API port also proves check mode does not try to bind it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let document = format!(
        r#"
[server]
http_addr = "{}"
query_log = "~/query.ndjson"
log_file = "~/daemon.log"
[data]
path = "~/data"
[ingest]
enabled = true
wal_dir = "~/wal"
[web]
cookie_secret_path = "~/session.key"
"#,
        listener.local_addr().unwrap()
    );
    let output = check(&document);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Configuration is valid"));
}

#[test]
fn config_check_rejects_active_log_marker_collisions_without_side_effects() {
    for name in ["EPOCH", "CATALOG", "REPIN"] {
        for telemetry in [false, true] {
            let document = format!(
                "[server]\nlog_file='~/data/child/../{name}'\n[data]\npath='~/data'\n[ingest]\nenabled=true\ninternal_telemetry={telemetry}"
            );
            let output = check(&document);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), telemetry, "{stderr}");
            if !telemetry {
                assert!(stderr.contains("reserved storage marker"), "{stderr}");
            }
        }
    }
    let output = check(
        "[server]\nlog_file='~/data/logs/EPOCH'\n[data]\npath='~/data'\n[ingest]\ninternal_telemetry=false",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_check_accepts_shipped_configuration() {
    for document in [
        include_str!("../../../config/trawld.toml"),
        include_str!("../../../config/trawld.reference.toml"),
        include_str!("../debian/trawld.toml"),
    ] {
        let output = check(document);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn config_check_rejects_errors_without_echoing_config_values() {
    for document in [
        "[server]\nhttp_adrr='private-secret'\n[data]\npath='~/data'",
        "[server]\ntimeout_secs='private-secret'\n[data]\npath='~/data'",
        "[server]\nhttp_addr='private-secret\n[data]\npath='~/data'",
        "[server]\n[data]\npath='~/data'\n[ingest]\nseverity_from=['PRIVATE-SECRET']",
    ] {
        let output = check(document);
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("invalid") || stderr.contains("unknown setting"),
            "{stderr}"
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-SECRET"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Configuration is valid"));
    }
}

#[test]
fn config_check_rejects_daemon_globs_without_side_effects() {
    for path in [
        "~/private-secret/*.parquet",
        "~/private-secret?",
        "~/private-secret[ab]",
        "[",
    ] {
        let output = check(&format!("[server]\n[data]\npath = '{path}'"));
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("data.path must be a directory path")
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Configuration is valid"));
    }
}

#[test]
fn config_check_requires_an_explicit_config() {
    let output = command().arg("--check-config").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--config"));
}
