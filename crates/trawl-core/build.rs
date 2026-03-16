// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

fn main() {
    let sha = cmd("git", &["rev-parse", "--short", "HEAD"]);
    println!(
        "cargo:rustc-env=TRAWL_GIT_SHA={}",
        sha.as_deref().unwrap_or("unknown")
    );

    let dirty = cmd("git", &["status", "--porcelain"])
        .map_or_else(|| "false".to_string(), |s| (!s.is_empty()).to_string());
    println!("cargo:rustc-env=TRAWL_GIT_DIRTY={dirty}");

    let date = cmd("git", &["log", "-1", "--format=%cd", "--date=short"]);
    println!(
        "cargo:rustc-env=TRAWL_GIT_DATE={}",
        date.as_deref().unwrap_or("unknown")
    );

    let rustc_ver = cmd("rustc", &["-Vv"]).and_then(|s| {
        s.lines()
            .find(|l| l.starts_with("release:"))
            .map(|l| l.trim_start_matches("release:").trim().to_string())
    });
    println!(
        "cargo:rustc-env=TRAWL_RUSTC_VERSION={}",
        rustc_ver.as_deref().unwrap_or("unknown")
    );

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TRAWL_TARGET_TRIPLE={target}");

    // Rerun when git state changes.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let git_dir = std::path::Path::new(&manifest).join("../../.git");
    for entry in &["HEAD", "refs", "packed-refs"] {
        println!("cargo:rerun-if-changed={}", git_dir.join(entry).display());
    }
    println!("cargo:rerun-if-changed=build.rs");
}

fn cmd(program: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}
