// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `insecure` warning, observed on the real `trawl` binary.
//!
//! `validate` with no token parses locally and prints `valid`, so it needs
//! no server and its stdout is deterministic. Every run strips the
//! inherited `TRAWL_*` environment and reads its own config file.

use std::path::Path;
use std::process::{Command, Output};

const WARNING: &str = "trawl: warning: insecure is on, so the server's TLS certificate is not verified; pin it with ca_cert instead\n";

fn trawl(config: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAWL_") {
            cmd.env_remove(key);
        }
    }
    cmd.arg("-c")
        .arg(config)
        .args(args)
        .envs(env.iter().copied());
    let output = cmd.output().expect("spawn trawl");
    assert!(
        output.status.success(),
        "trawl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn write_config(dir: &Path, name: &str, toml: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, toml).expect("write config");
    path
}

#[test]
fn insecure_warns_once_on_stderr_and_leaves_stdout_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = write_config(
        dir.path(),
        "plain.toml",
        "[server]\nurl = \"https://127.0.0.1:1\"\n",
    );
    let server_insecure = write_config(
        dir.path(),
        "server.toml",
        "[server]\nurl = \"https://127.0.0.1:1\"\ninsecure = true\n",
    );
    let profile_insecure = write_config(
        dir.path(),
        "profile.toml",
        "[server]\nurl = \"https://127.0.0.1:1\"\n\n[profiles.dev]\ninsecure = true\n",
    );
    let query = "* | head 1";

    let baseline = trawl(&plain, &["validate", query], &[]);
    assert_eq!(baseline.stdout, b"valid\n", "baseline stdout");
    assert!(
        baseline.stderr.is_empty(),
        "baseline stderr: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );

    let runs = [
        (
            "--insecure",
            trawl(&plain, &["--insecure", "validate", query], &[]),
        ),
        (
            "TRAWL_INSECURE",
            trawl(&plain, &["validate", query], &[("TRAWL_INSECURE", "true")]),
        ),
        (
            "[server]",
            trawl(&server_insecure, &["validate", query], &[]),
        ),
        (
            "profile",
            trawl(&profile_insecure, &["-p", "dev", "validate", query], &[]),
        ),
    ];
    for (source, output) in runs {
        assert_eq!(
            output.stdout, baseline.stdout,
            "{source}: stdout must be byte-identical to the plain run"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            WARNING,
            "{source}: stderr must be exactly one warning line"
        );
    }

    // A profile that turns insecure back off silences the warning.
    let profile_secure = write_config(
        dir.path(),
        "profile-secure.toml",
        "[server]\nurl = \"https://127.0.0.1:1\"\ninsecure = true\n\n[profiles.prod]\ninsecure = false\n",
    );
    let output = trawl(&profile_secure, &["-p", "prod", "validate", query], &[]);
    assert_eq!(output.stdout, baseline.stdout);
    assert!(output.stderr.is_empty(), "a secure profile must not warn");
}
