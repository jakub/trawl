// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression guard for ADR-0066: the SPA reads every failure through
//! one classifier.
//!
//! Line comments are dropped first, so the transport's prose may still
//! name a status.

const FORBIDDEN: &[&str] = &["ApiError::Status", "401"];

const TRANSPORT: &str = include_str!("../src/api/mod.rs");

#[test]
fn the_transport_reads_no_failure_itself() {
    let code = comment_stripped(TRANSPORT);
    assert!(
        code.contains("response_policy::"),
        "the guard is reading the wrong file"
    );
    let offenders: Vec<&str> = FORBIDDEN
        .iter()
        .copied()
        .filter(|token| code.contains(token))
        .collect();
    assert!(
        offenders.is_empty(),
        "src/api/mod.rs classifies a failure itself ({}); hand the answer to \
         `response_policy` and `api_error::http_failure` instead (ADR-0066)",
        offenders.join(", "),
    );
}

#[test]
fn a_forbidden_token_in_a_comment_is_prose() {
    for src in [
        "// a 401 is the session\n",
        "/// [`ApiError::Status`] is the bare status.\n",
        "    //! no endpoint matches 401.\n",
    ] {
        let stripped = comment_stripped(src);
        assert!(
            !FORBIDDEN.iter().any(|token| stripped.contains(token)),
            "a token in a comment is prose: {src}",
        );
    }
}

#[test]
fn a_forbidden_token_in_code_trips_the_guard() {
    for src in [
        "        401 => Err(ApiError::Unauthorized),\n",
        "        s => Err(ApiError::Status(s)),\n",
        "let _literal = \"/*\";\n        s => Err(ApiError::Status(s)),\n",
    ] {
        let stripped = comment_stripped(src);
        assert!(
            FORBIDDEN.iter().any(|token| stripped.contains(token)),
            "code must survive the strip: {src}",
        );
    }
}

fn comment_stripped(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
