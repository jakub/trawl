// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl driver` under the reserved `trial` profile, on the real binary.
//!
//! The driver sends to whatever TUI holds the socket, whichever server
//! that TUI is connected to, so `-p trial` cannot make it reach the trial.
//! It is refused before the socket is touched. A finished trial exists, so
//! nothing else stops the command first. Each case points `--socket` at a
//! listening Unix socket in a temp directory that counts the connections
//! it receives.

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run `trawl` with a temp `HOME` and `XDG_STATE_HOME`, the inherited
/// `TRAWL_*` environment stripped, plus `env`, while `listener` accepts.
/// Returns the output and how many connections reached the listener. An
/// accepted connection is closed at once, so a driver that did connect
/// fails instead of waiting for a TUI that is not there.
fn trawl(
    home: &Path,
    listener: &UnixListener,
    args: &[&str],
    env: &[(&str, &str)],
) -> (Output, usize) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAWL_") {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .args(args)
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trawl");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut connections = 0;
    loop {
        if let Ok((stream, _)) = listener.accept() {
            connections += 1;
            drop(stream);
        }
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("trawl {args:?} did not exit");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // A connection made just before exit is still queued.
    while listener.accept().is_ok() {
        connections += 1;
    }
    (child.wait_with_output().expect("output"), connections)
}

/// A finished trial under `home/state`, as `-p trial` reads it.
fn finished_trial(home: &Path) {
    let dir = home.join("state/trawl/trial");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    let state = serde_json::json!({
        "schema": 1,
        "trial_id": "0123456789abcdef0123456789abcdef",
        "project": "trawl-trial",
        "cli_version": "0.9.0",
        "created_at": "2026-09-25T12:00:00Z",
        "engine_id": "ENGINE:ID",
        "ports": {"api": 15514, "web": 18090},
        "images": {
            "trawl": {"reference": "ghcr.io/jakub/trawl:0.9.0", "id": "sha256:aa", "repo_digest": null},
            "postgres": {"reference": "postgres:18", "id": "sha256:bb", "repo_digest": null},
            "trawl_overridden": false
        },
        "phases": {"database": true, "fleet_migrated": true, "tls": true, "services_verified": true},
        "tls": {"sha256_fingerprint": "AB:CD"},
        "keys": {
            "operator": {"name": "trial-operator", "prefix": "pfx00001"},
            "ingest": {"name": "trial-ingest", "prefix": "pfx00002"}
        },
        "samples": {"state": "skipped"}
    });
    for (name, bytes, mode) in [
        ("state.json", state.to_string().into_bytes(), 0o600),
        ("operator.token", b"flt_trialdrivertoken\n".to_vec(), 0o600),
        (
            "ca.pem",
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".to_vec(),
            0o644,
        ),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

/// Case name, flags before `driver`, extra environment.
type Case<'a> = (&'a str, &'a [&'a str], &'a [(&'a str, &'a str)]);

#[test]
fn driver_is_refused_under_the_trial_profile_before_the_socket() {
    let tmp = tempfile::tempdir().expect("tempdir");
    finished_trial(tmp.path());
    let socket = tmp.path().join("driver.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let socket = socket.to_str().unwrap();

    // Control: without the profile, the driver does reach the socket.
    let (control, connections) = trawl(
        tmp.path(),
        &listener,
        &["driver", "--socket", socket, "status"],
        &[],
    );
    assert_eq!(connections, 1, "control: the driver connected");
    assert!(!control.status.success(), "control: no TUI answered");

    let cases: [Case<'_>; 3] = [
        ("-p trial", &["-p", "trial"], &[]),
        ("--profile trial", &["--profile", "trial"], &[]),
        ("TRAWL_PROFILE=trial", &[], &[("TRAWL_PROFILE", "trial")]),
    ];
    for (case, flags, env) in cases {
        for sub in [&["status"][..], &["query", "* | head 1"], &["quit"]] {
            let mut args = flags.to_vec();
            args.extend(["driver", "--socket", socket]);
            args.extend(sub);
            let (output, connections) = trawl(tmp.path(), &listener, &args, env);
            assert_eq!(connections, 0, "{case} {sub:?}: no connection");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{case} {sub:?}: must fail");
            assert!(output.stdout.is_empty(), "{case} {sub:?}: stdout");
            assert!(
                stderr.contains("already running") && stderr.contains("trawl -p trial --driver"),
                "{case} {sub:?}: the refusal explains the driver: {stderr}"
            );
        }
    }
}
