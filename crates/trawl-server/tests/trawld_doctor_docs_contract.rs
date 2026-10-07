// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The configuration reference documents `trawld --doctor` from the names
//! the code uses: every check id, the four outcomes, and each verdict with its
//! exit status. The names are imported, not copied, so a renamed check or a
//! new outcome fails here until the reference names it.

use trawl_api::doctor::{Outcome, Verdict};
use trawl_server::doctor::ServerCheck;

const CONFIGURATION: &str =
    include_str!("../../../docs/src/content/docs/reference/configuration.md");

/// The section headed "Check the installation with `trawld --doctor`", up
/// to the next heading of the same or a higher level.
fn doctor_section() -> &'static str {
    let start = CONFIGURATION
        .find("\n### Check the installation with `trawld --doctor`\n")
        .expect("the configuration reference has a `trawld --doctor` section");
    let rest = &CONFIGURATION[start + 1..];
    let end = rest[4..]
        .find("\n### ")
        .into_iter()
        .chain(rest[4..].find("\n## "))
        .min()
        .map_or(rest.len(), |at| at + 4);
    &rest[..end]
}

fn assert_documented(text: &str, what: &str) {
    assert!(
        doctor_section().contains(text),
        "the `trawld --doctor` reference does not name {what} {text:?}"
    );
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
    for check in ServerCheck::ALL {
        assert_documented(&format!("| `{}` |", check.id()), "the check");
    }
    assert_documented(
        &format!("`{}.<key>`", ServerCheck::ListenerHealth.id()),
        "the per-key health rows",
    );
}

#[test]
fn every_outcome_is_documented() {
    let all = [
        Outcome::Complete,
        Outcome::Failed,
        Outcome::NotConfigured,
        Outcome::NotSampled,
    ];
    for outcome in all {
        // An exhaustive match: a new variant fails to compile until listed.
        match outcome {
            Outcome::Complete | Outcome::Failed | Outcome::NotConfigured | Outcome::NotSampled => {}
        }
        assert_documented(&format!("| `{}` |", serde_name(outcome)), "the outcome");
    }
    assert_documented("`will_initialize`", "the reason code");
    assert_documented("`ran_as_root`", "the reason code");
    assert_documented(
        &format!("`{}`", trawl_api::doctor::reason::WILL_TIGHTEN),
        "the reason code",
    );
    assert_documented(
        &format!("| `{}` |", trawl_api::doctor::reason::NOT_POSTGRES),
        "the reason code",
    );
}

#[test]
fn exit_codes_zero_to_three_are_documented() {
    let all = [Verdict::Pass, Verdict::Fail, Verdict::Incomplete];
    for verdict in all {
        // An exhaustive match: a new verdict fails to compile until listed.
        match verdict {
            Verdict::Pass | Verdict::Fail | Verdict::Incomplete => {}
        }
        let row = format!("| `{}` |", verdict.exit_code());
        let line = doctor_section()
            .lines()
            .find(|line| line.starts_with(&row))
            .unwrap_or_else(|| panic!("no exit code row starting {row:?}"));
        assert!(
            line.contains(&format!("`{}`", serde_name(verdict))),
            "the {row:?} row does not name its verdict: {line:?}"
        );
    }
    assert_documented("| `2` | Usage error", "the usage status");
}
