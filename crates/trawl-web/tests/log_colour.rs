// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-web's log lines carry no ANSI colour when stdout is not a
//! terminal.
//!
//! Under systemd and Kubernetes the proxy's stdout is a log stream, and a
//! collector ships it into stored events, escapes and all. The test runs the
//! real binary with its output captured, so stdout is a pipe, and with an
//! empty environment, so `NO_COLOR` plays no part. The test holds the
//! proxy's bind address itself, so the proxy logs `loaded config`, fails to
//! bind, and exits.

use std::net::TcpListener;
use std::process::{Command, Stdio};

#[test]
fn a_piped_stdout_gets_plain_log_lines() {
    let dir = tempfile::tempdir().expect("temp dir");
    let taken = TcpListener::bind("127.0.0.1:0").expect("hold a loopback port");
    let bind_addr = taken.local_addr().expect("held address");
    let config = dir.path().join("trawld.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nhttp_addr = \"127.0.0.1:1\"\n[data]\npath = \"{}\"\n\
             [web]\nbind_addr = \"{bind_addr}\"\n\
             public_origins = [\"https://trawl.example.com\"]\n",
            dir.path().join("data").display()
        ),
    )
    .expect("write trawld.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_trawl-web"))
        .env_clear()
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::null())
        .output()
        .expect("run trawl-web");
    drop(taken);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("loaded config"),
        "no startup line to check; stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "trawl-web coloured a stdout that is not a terminal: {stdout:?}"
    );
}
