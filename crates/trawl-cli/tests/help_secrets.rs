// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Help output never prints a secret taken from the environment.
//!
//! clap shows an env-bound argument's current value in help, as
//! `[env: NAME=value]`. For `--token` that value is the API token, and help
//! output is the kind of text users paste into bug reports. The check runs
//! the real `trawl` binary with a sentinel token in its environment.

use std::process::Command;

const SENTINEL: &str = "flt_sentinel";

fn help(args: &[&str]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAWL_") {
            cmd.env_remove(key);
        }
    }
    let output = cmd
        .args(args)
        .env("TRAWL_TOKEN", SENTINEL)
        .output()
        .expect("spawn trawl");
    assert!(
        output.status.success(),
        "trawl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.is_empty(), "trawl {args:?} printed nothing");
    text
}

#[test]
fn help_names_the_token_variable_without_its_value() {
    // Long and short help at the top level, and a subcommand's help, which
    // repeats the global `--token`.
    for args in [&["--help"][..], &["-h"], &["validate", "--help"]] {
        let text = help(args);
        assert!(
            !text.contains(SENTINEL),
            "trawl {args:?} printed the token:\n{text}"
        );
        assert!(
            text.contains("TRAWL_TOKEN"),
            "trawl {args:?} no longer names TRAWL_TOKEN:\n{text}"
        );
    }
}
