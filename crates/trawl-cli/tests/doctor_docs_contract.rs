// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CLI reference documents `trawl doctor` from the names the code uses:
//! every check id, the four outcomes, the reason codes a client doctor
//! reports, each verdict with its exit status, and every request the doctor
//! may send. The names are imported, not copied, so a renamed check or a
//! new outcome fails here until the reference names it.

use trawl_api::doctor::{Outcome, Verdict, reason};
use trawl_cli::doctor::{ClientCheck, REQUESTS, api};

const CLI_REFERENCE: &str = include_str!("../../../docs/src/content/docs/reference/cli.md");

/// The `## Doctor mode` section of the reference, up to the next `## `.
fn doctor_section() -> &'static str {
    let start = CLI_REFERENCE
        .find("\n## Doctor mode\n")
        .expect("the CLI reference has a `## Doctor mode` section");
    let rest = &CLI_REFERENCE[start + 1..];
    let end = rest[3..].find("\n## ").map_or(rest.len(), |at| at + 3);
    &rest[..end]
}

fn assert_documented(text: &str, what: &str) {
    assert!(
        doctor_section().contains(text),
        "the `trawl doctor` reference does not name {what} {text:?}"
    );
}

/// Every outcome, listed through an exhaustive match so a new variant fails
/// to compile here until it is added.
fn all_outcomes() -> [Outcome; 4] {
    let all = [
        Outcome::Complete,
        Outcome::Failed,
        Outcome::NotConfigured,
        Outcome::NotSampled,
    ];
    for outcome in all {
        match outcome {
            Outcome::Complete | Outcome::Failed | Outcome::NotConfigured | Outcome::NotSampled => {}
        }
    }
    all
}

/// Every verdict, listed the same way.
fn all_verdicts() -> [Verdict; 3] {
    let all = [Verdict::Pass, Verdict::Fail, Verdict::Incomplete];
    for verdict in all {
        match verdict {
            Verdict::Pass | Verdict::Fail | Verdict::Incomplete => {}
        }
    }
    all
}

/// The name serde gives a value, without its JSON quotes.
fn serde_name(value: impl serde::Serialize) -> String {
    match serde_json::to_value(value).expect("serializes") {
        serde_json::Value::String(name) => name,
        other => panic!("not a unit variant: {other}"),
    }
}

#[test]
fn every_check_id_is_documented() {
    for check in ClientCheck::ALL {
        assert_documented(&format!("| `{}` |", check.id()), "the check");
    }
    let per_key = format!("{}<key>", api::API_HEALTH_KEY_PREFIX);
    assert_documented(&format!("| `{per_key}` |"), "the per-key health row");
    assert_documented(
        &format!("| `{}` |", api::API_HEALTH_INVALID_KEY),
        "the invalid-name health row",
    );
}

#[test]
fn every_outcome_and_reason_code_is_documented() {
    for outcome in all_outcomes() {
        assert_documented(&format!("| `{}` |", serde_name(outcome)), "the outcome");
    }
    for code in [
        reason::BLOCKED,
        reason::RATE_LIMITED,
        reason::RECOVERING,
        reason::REQUEST_LIMIT_REACHED,
        reason::TIMED_OUT,
    ] {
        assert_documented(&format!("`{code}`"), "the reason code");
    }
}

#[test]
fn every_verdict_is_documented_with_its_exit_status() {
    for verdict in all_verdicts() {
        let row = format!("| `{}` |", serde_name(verdict));
        let line = doctor_section()
            .lines()
            .find(|line| line.starts_with(&row))
            .unwrap_or_else(|| panic!("no verdict row starting {row:?}"));
        let status = format!("| `{}` |", verdict.exit_code());
        assert!(
            line.ends_with(&status),
            "the {row:?} row does not end with its exit status {status:?}: {line:?}"
        );
    }
    assert_documented("A refused command line exits `2`.", "the usage status");

    // The binary-wide exit code table lists the doctor's statuses as well.
    let codes_start = CLI_REFERENCE
        .find("## Exit codes\n")
        .expect("the CLI reference has an exit code table");
    let codes = &CLI_REFERENCE[codes_start..];
    let codes = &codes[..codes[3..].find("\n## ").map_or(codes.len(), |at| at + 3)];
    for code in all_verdicts()
        .map(Verdict::exit_code)
        .into_iter()
        .chain([2])
    {
        assert!(
            codes.contains(&format!("| `{code}` |")),
            "the exit code table has no row for {code}"
        );
    }
}

#[test]
fn every_request_is_documented() {
    for (method, path) in REQUESTS {
        assert_documented(&format!("`{method} {path}`"), "the request");
    }
}
