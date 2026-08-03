// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const DEV_WRAPPER: &str = include_str!("../../../bin/dev");
const FLEET_WRAPPER: &str = include_str!("../../../bin/fleet-dev");
const TRAWLD_DEV_WRAPPER: &str = include_str!("../../../bin/trawld-dev");

fn trawl_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("fleet-dev lives at crates/fleet-dev")
        .to_owned()
}

fn executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(target_os = "linux")]
fn fake_trawld_target(root: &Path, target: &Path) {
    let profile = target.join("debug");
    let deps = profile.join("deps");
    std::fs::create_dir_all(&deps).unwrap();
    std::fs::write(deps.join("libduckdb.so"), b"").unwrap();
    executable(
        &profile.join("trawld"),
        "#!/bin/sh\nprintf 'loader=%s\\n' \"$LD_LIBRARY_PATH\"\nprintf 'arg=%s\\n' \"$@\"\nif test \"${1:-}\" = exit; then exit \"${2:-0}\"; fi\n",
    );
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    executable(&bin.join("trawld-dev"), TRAWLD_DEV_WRAPPER);
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

#[cfg(target_os = "linux")]
#[test]
fn trawld_dev_supports_absolute_target_and_preserves_loader_path_and_arguments() {
    let root = tempfile::Builder::new()
        .prefix("trawld dev absolute ")
        .tempdir()
        .unwrap();
    let target = root.path().join("absolute target");
    fake_trawld_target(root.path(), &target);
    let elsewhere = tempfile::tempdir().unwrap();

    let output = Command::new(root.path().join("bin/trawld-dev"))
        .current_dir(elsewhere.path())
        .env("CARGO_TARGET_DIR", &target)
        .env("LD_LIBRARY_PATH", "/existing/libs")
        .args(["alpha beta", "gamma"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            format!(
                "loader={}:/existing/libs",
                target.join("debug/deps").display()
            ),
            "arg=alpha beta".to_owned(),
            "arg=gamma".to_owned(),
        ]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn trawld_dev_resolves_relative_target_against_checkout_and_propagates_exit() {
    let root = tempfile::Builder::new()
        .prefix("trawld dev relative ")
        .tempdir()
        .unwrap();
    let target = root.path().join("relative-target");
    fake_trawld_target(root.path(), &target);
    let elsewhere = tempfile::tempdir().unwrap();

    let output = Command::new(root.path().join("bin/trawld-dev"))
        .current_dir(elsewhere.path())
        .env("CARGO_TARGET_DIR", "relative-target")
        .env_remove("LD_LIBRARY_PATH")
        .args(["exit", "37"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.lines().next().is_some_and(|line| {
            line == format!("loader={}", target.join("debug/deps").display())
        })
    );
}

#[cfg(target_os = "linux")]
#[test]
fn trawld_dev_reports_a_missing_downloaded_library_without_running_binary() {
    let root = tempfile::Builder::new()
        .prefix("trawld dev missing ")
        .tempdir()
        .unwrap();
    let target = root.path().join("target");
    fake_trawld_target(root.path(), &target);
    std::fs::remove_file(target.join("debug/deps/libduckdb.so")).unwrap();

    let output = Command::new(root.path().join("bin/trawld-dev"))
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("missing DuckDB development library"));
    assert!(stderr.contains("cargo build --no-default-features -p trawl-server"));
}

#[test]
fn trawld_dev_has_valid_bash_syntax() {
    let output = Command::new("bash")
        .args(["-n", trawl_root().join("bin/trawld-dev").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
