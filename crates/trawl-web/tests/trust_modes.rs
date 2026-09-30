// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How each shipped channel makes `trawl-web` trust trawld (ADR-0048).
//!
//! `trawl-web` always verifies trawld's certificate, against the platform
//! roots or a pinned CA file. A channel that runs trawld with its generated
//! self-signed certificate has to pin that exact file, or its sign-in fails.
//! These tests read the channel's own files rather than restating them.

use std::path::{Path, PathBuf};

use trawl_config::Config;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root must exist relative to the crate")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `[[processes]]` entry of `fleet-dev.toml` with this name.
fn fleet_dev_process<'a>(manifest: &'a toml::Table, name: &str) -> &'a toml::Table {
    manifest["processes"]
        .as_array()
        .expect("fleet-dev.toml declares [[processes]]")
        .iter()
        .filter_map(toml::Value::as_table)
        .find(|process| process.get("name").and_then(toml::Value::as_str) == Some(name))
        .unwrap_or_else(|| panic!("fleet-dev.toml declares no {name} process"))
}

/// The development stack pins the certificate its own trawld generates.
///
/// fleet-dev starts `bin/trawld-dev` and `trawl-web` with no `--config`
/// and no `TRAWL_CONFIG`, so both read the default `~/.trawl/trawld.toml`
/// (config/README.md). That file is made from the starter
/// `config/trawld.toml`, which names no `[server]` certificate, so trawld
/// generates one under its state directory. The pin `trawl-web` gets
/// through `TRAWL_WEB_UPSTREAM_CA_PATH` must be that path, as trawl-config
/// derives it. Neither side expands `~` here: fleet-dev passes the value
/// through literally and `trawl-web` expands it, as trawld expands the
/// data path.
#[test]
fn fleet_dev_pins_the_dev_certificate() {
    let manifest: toml::Table = read("fleet-dev.toml")
        .parse()
        .expect("fleet-dev.toml parses");

    for name in ["trawld", "trawl-web"] {
        let process = fleet_dev_process(&manifest, name);
        let command = process["command"].as_array().expect("command is an array");
        assert_eq!(
            command.len(),
            1,
            "{name} must run with no arguments, so it reads the default config"
        );
        for table in ["env", "resolved_env"] {
            if let Some(env) = process.get(table).and_then(toml::Value::as_table) {
                assert!(
                    !env.contains_key("TRAWL_CONFIG"),
                    "{name} must read the default config, not TRAWL_CONFIG"
                );
            }
        }
    }

    let web = fleet_dev_process(&manifest, "trawl-web");
    let pinned = web
        .get("env")
        .and_then(toml::Value::as_table)
        .and_then(|env| env.get(trawl_web::config::ENV_UPSTREAM_CA_PATH))
        .and_then(toml::Value::as_str)
        .expect("fleet-dev.toml sets TRAWL_WEB_UPSTREAM_CA_PATH on trawl-web");

    let dev_config =
        Config::parse_toml(&read("config/trawld.toml")).expect("config/trawld.toml parses");
    let generated = dev_config
        .generated_cert_path()
        .expect("the starter config names no [server] certificate, so trawld generates one");
    assert_eq!(Path::new(pinned), generated);
}

/// Every file under `path`, depth first. Symlinks are not followed, and
/// build output (`target/`, `node_modules/`) and `skip` are left out.
fn walk(path: &Path, skip: &[PathBuf], files: &mut Vec<PathBuf>) {
    if skip.iter().any(|skipped| skipped == path) {
        return;
    }
    // Tests running beside this one create and delete scratch directories
    // under `crates/`. A path that vanishes mid-walk was never part of the
    // tree, so it is skipped rather than failed.
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => panic!("stat {}: {e}", path.display()),
    };
    if metadata.is_file() {
        files.push(path.to_owned());
        return;
    }
    if !metadata.is_dir()
        || path
            .file_name()
            .is_some_and(|name| name == "target" || name == "node_modules")
    {
        return;
    }
    let listing = match std::fs::read_dir(path) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => panic!("list {}: {e}", path.display()),
    };
    let mut entries: Vec<PathBuf> = listing
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for entry in entries {
        walk(&entry, skip, files);
    }
}

/// The unverified-upstream switch is deleted, not deprecated (ADR-0048).
///
/// No code reads it and nothing shipped may mention it: a config line, a
/// chart value, a dev-stack entry or a doc that names it would teach an
/// operator a setting that does nothing. The needle is spelled in two
/// parts so this file does not match itself; the trial's absence checks
/// and `packaging.sh` rule 11 split it the same way.
///
/// `docs/launch/evidence/` is out of scope: it holds dated records of what
/// earlier builds did, and a record is not rewritten.
#[test]
fn insecure_upstream_switch_is_gone() {
    const NEEDLE: &str = concat!("TRAWL_WEB_", "INSECURE_UPSTREAM");
    let root = repo_root();
    let skip = [root.join("docs/launch/evidence")];
    let mut files = Vec::new();
    for scanned in [
        "crates",
        "chart",
        "fleet-dev.toml",
        "docs/src",
        "docs/releases",
    ] {
        walk(&root.join(scanned), &skip, &mut files);
    }
    assert!(
        files.len() > 100,
        "the scan must cover the tree; it found {} files",
        files.len()
    );

    let mut hits = Vec::new();
    for file in &files {
        let bytes = match std::fs::read(file) {
            Ok(bytes) => bytes,
            // Deleted by a concurrent test after the walk listed it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("read {}: {e}", file.display()),
        };
        // A NUL byte marks a binary file, as git decides it.
        if bytes.contains(&0) {
            continue;
        }
        for (index, line) in bytes.split(|&byte| byte == b'\n').enumerate() {
            if line
                .windows(NEEDLE.len())
                .any(|window| window == NEEDLE.as_bytes())
            {
                let relative = file.strip_prefix(&root).unwrap_or(file);
                hits.push(format!("{}:{}", relative.display(), index + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "{NEEDLE} was removed (ADR-0048) but is still mentioned at:\n{}",
        hits.join("\n")
    );
}
