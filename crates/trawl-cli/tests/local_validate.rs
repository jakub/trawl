// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Local `trawl validate`, observed on the real `trawl` binary: with no
//! token it parses the query and then runs the semantic checks both
//! query lanes share, so a query either lane would refuse is refused
//! here too, with the same sentence (#457).
//!
//! Every run strips the inherited `TRAWL_*` environment and reads its own
//! empty config file, so no saved key sends the query to a server.

use std::path::Path;
use std::process::{Command, Output};

fn validate(config: &Path, query: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAWL_") {
            cmd.env_remove(key);
        }
    }
    cmd.arg("-c")
        .arg(config)
        .arg("validate")
        .arg(query)
        .output()
        .expect("spawn trawl")
}

fn empty_config(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, "").expect("write config");
    path
}

/// A refusal: a non-zero exit, nothing on stdout, and `sentence` on
/// stderr.
fn assert_refused(output: &Output, query: &str, sentence: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "{query}: exited 0, stderr {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "{query}: stdout {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains(sentence),
        "{query}: stderr should hold {sentence:?}:\n{stderr}"
    );
}

#[test]
fn local_validate_refuses_a_malformed_json_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = empty_config(dir.path());
    let query = r#"* | let k = json(message, "$[")"#;
    let output = validate(&config, query);
    assert_refused(
        &output,
        query,
        r#"json(): invalid JSON path "$[" in argument 2: expected an array index after ["#,
    );
    // The hint travels with it, said once.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("a JSON path is $ for the root").count(),
        1,
        "{stderr}"
    );
}

#[test]
fn local_validate_refuses_a_computed_json_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = empty_config(dir.path());
    let query = "* | let k = json_extract(message, path_field)";
    assert_refused(
        &validate(&config, query),
        query,
        "json_extract() argument 2 must be a string literal JSON path",
    );
}

#[test]
fn local_validate_refuses_an_invalid_format() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = empty_config(dir.path());
    let query = r#"* | eval x = strftime(_time, "%Q")"#;
    assert_refused(
        &validate(&config, query),
        query,
        r#"strftime(): invalid format string "%Q""#,
    );
}

/// A query that passes every check still prints `valid` and exits 0, and
/// a parse failure is still refused as before.
#[test]
fn local_validate_accepts_a_valid_query_and_refuses_a_parse_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = empty_config(dir.path());
    for query in [
        r#"* | let k = json(message, "$.k")"#,
        "_severity>=error | stats count() by host",
    ] {
        let output = validate(&config, query);
        assert!(
            output.status.success(),
            "{query}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"valid\n", "{query}");
    }
    let query = "* | staats count() by host";
    assert_refused(&validate(&config, query), query, "unknown command 'staats'");
}
