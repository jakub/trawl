// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Workspace task runner. Currently only hosts `build-web`, which runs
//! the two-step SPA-into-binary build in the right order:
//! 1. `trunk build [--release]` inside `crates/trawl-web-ui/`
//! 2. `cargo build [--release] -p trawl-web`
//!
//! Aliased as `cargo xtask` via `.cargo/config.toml`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "trawl workspace task runner")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build the SPA, then compile trawl-web with the fresh dist/ baked in.
    BuildWeb {
        /// Pass --release to both trunk and cargo.
        #[arg(long)]
        release: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::BuildWeb { release } => build_web(release),
    }
}

fn build_web(release: bool) -> ExitCode {
    let root = workspace_root();
    let web_ui = root.join("crates").join("trawl-web-ui");

    let mut trunk = Command::new("trunk");
    trunk.current_dir(&web_ui).arg("build");
    if release {
        trunk.arg("--release");
    }
    eprintln!(
        "xtask: trunk build{} (in {})",
        if release { " --release" } else { "" },
        web_ui.display()
    );
    if !run(trunk) {
        return ExitCode::FAILURE;
    }

    let mut cargo = Command::new(env!("CARGO"));
    cargo.current_dir(&root).args(["build", "-p", "trawl-web"]);
    if release {
        cargo.arg("--release");
    }
    eprintln!(
        "xtask: cargo build -p trawl-web{}",
        if release { " --release" } else { "" }
    );
    if !run(cargo) {
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run(mut cmd: Command) -> bool {
    match cmd.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("xtask: command failed: {status}");
            false
        }
        Err(e) => {
            eprintln!("xtask: failed to spawn: {e}");
            false
        }
    }
}

/// The workspace root — parent of `xtask/`.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map_or_else(|| manifest.to_path_buf(), Path::to_path_buf)
}
