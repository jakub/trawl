// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const DEV_WRAPPER: &str = include_str!("../../../bin/dev");
const FLEET_WRAPPER: &str = include_str!("../../../bin/fleet-dev");

fn executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn legacy_wrapper_translates_flags_from_any_cwd() {
    let root = tempfile::Builder::new()
        .prefix("fleet dev wrapper ")
        .tempdir()
        .unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    executable(&bin.join("dev"), DEV_WRAPPER);
    executable(&bin.join("fleet-dev"), "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
    let elsewhere = tempfile::tempdir().unwrap();

    let output = Command::new(bin.join("dev"))
        .current_dir(elsewhere.path())
        .args([
            "--release-spa",
            "--tailscale",
            "--with-coastwatch",
            "--tailscale",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let args = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["--exposure", "tailscale", "all", "--release-spa"]
    );

    let output = Command::new(bin.join("dev")).output().unwrap();
    assert!(output.status.success());
    let args = String::from_utf8(output.stdout).unwrap();
    let expected_root = root.path().display().to_string();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["dev", "trawl", "--app-root", expected_root.as_str()]
    );
}

#[test]
fn legacy_wrapper_rejects_unknown_flags() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    executable(&bin.join("dev"), DEV_WRAPPER);
    executable(&bin.join("fleet-dev"), "#!/bin/sh\nexit 99\n");

    let output = Command::new(bin.join("dev"))
        .arg("--old-secret-mode")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unknown flag")
    );
}

#[test]
fn cargo_binstub_preserves_argument_boundaries_and_root() {
    let root = tempfile::Builder::new()
        .prefix("fleet dev cargo ")
        .tempdir()
        .unwrap();
    let bin = root.path().join("bin");
    let fake_bin = root.path().join("fake tools");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&fake_bin).unwrap();
    std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    executable(&bin.join("fleet-dev"), FLEET_WRAPPER);
    executable(
        &fake_bin.join("cargo"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\n",
    );

    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(bin.join("fleet-dev"))
        .env("PATH", path)
        .args(["plan", "trawl", "--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let args = String::from_utf8(output.stdout).unwrap();
    let args: Vec<_> = args.lines().collect();
    assert_eq!(args[0], "run");
    assert_eq!(args[1], "--manifest-path");
    assert_eq!(args[2], root.path().join("Cargo.toml").to_str().unwrap());
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--trawl-root", root.path().to_str().unwrap()])
    );
    assert!(args.ends_with(&["plan", "trawl", "--format", "json"]));
}
