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
