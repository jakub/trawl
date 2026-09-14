// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub fn main() {
    let sha = cmd("git", &["rev-parse", "--short", "HEAD"]);
    println!(
        "cargo:rustc-env=TRAWL_GIT_SHA={}",
        sha.as_deref().unwrap_or("unknown")
    );

    let dirty = cmd("git", &["status", "--porcelain", "--untracked-files=no"])
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

    // Git resolves linked-worktree metadata into its real administrative
    // directory. A worktree's .git is a file, not a directory of refs.
    for entry in ["HEAD", "index", "refs", "packed-refs"] {
        if let Some(path) = cmd("git", &["rev-parse", "--git-path", entry]) {
            let path = std::path::Path::new(&path);
            // Missing packed-refs is normal. Watching a missing path makes
            // Cargo rerun forever; loose ref changes are covered by refs.
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    // Index/ref watches cover staging and commits. Unstaged changes anywhere
    // in the product also affect provenance, even outside trawl-core.
    if let Some(root) = cmd("git", &["rev-parse", "--show-toplevel"])
        && let Some(files) = cmd_raw("git", &["ls-files", "--full-name", "-z", "--", ":/"])
    {
        for file in files.split('\0').filter(|file| !file.is_empty()) {
            let path = std::path::Path::new(&root).join(file);
            // A tracked deletion remains dirty; watch its containing
            // directory so restoring the file updates the metadata too.
            let mut watched = path.as_path();
            while !watched.exists() {
                let Some(parent) = watched.parent() else {
                    break;
                };
                watched = parent;
            }
            println!("cargo:rerun-if-changed={}", watched.display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}

fn cmd(program: &str, args: &[&str]) -> Option<String> {
    cmd_raw(program, args).map(|s| s.trim().to_string())
}

fn cmd_raw(program: &str, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new(program);
    // Status is a read here. Its optional index refresh would otherwise
    // change our own rerun input and force an extra build on every run.
    if program == "git" {
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    command
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}
