// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The events reference lists exactly the reject reasons and repair codes
//! the canonicalizer can produce. Both travel on the wire as these strings
//! (`IngestEventError::reason`, the ingest preview, `_repairs`, metric
//! labels), so the documented tables are the contract a sender reads. The
//! codes are imported, not copied: a new code fails here until the
//! reference names it, and a documented code the server cannot produce
//! fails too.

use std::collections::BTreeSet;

use trawl_server::ingest::envelope::{RejectReason, RepairCode};

const EVENTS: &str = include_str!("../../../docs/src/content/docs/reference/events.md");

/// The codes in the first column of the table under `heading`, in
/// document order: every row whose first cell is one backticked code.
fn table_codes(heading: &str) -> Vec<&'static str> {
    let start = EVENTS
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("the events reference has a {heading:?} section"));
    let section = &EVENTS[start + heading.len() + 2..];
    let section = &section[..section.find("\n## ").unwrap_or(section.len())];
    let codes: Vec<&str> = section
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|rest| rest.split_once("` |").map(|(code, _)| code))
        .collect();
    assert!(!codes.is_empty(), "no code rows under {heading:?}");
    codes
}

fn assert_same(heading: &str, documented: &[&str], code: &[&'static str]) {
    let unique: BTreeSet<&str> = documented.iter().copied().collect();
    assert_eq!(
        unique.len(),
        documented.len(),
        "{heading:?} lists a code twice: {documented:?}"
    );
    let code: BTreeSet<&str> = code.iter().copied().collect();
    let undocumented: Vec<_> = code.difference(&unique).collect();
    let unknown: Vec<_> = unique.difference(&code).collect();
    assert!(
        undocumented.is_empty() && unknown.is_empty(),
        "{heading:?} drifted from the code: undocumented {undocumented:?}, \
         documented but never produced {unknown:?}"
    );
}

#[test]
fn rejection_table_matches_every_reject_reason() {
    let code: Vec<&'static str> = RejectReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_same(
        "## Rejection reasons",
        &table_codes("## Rejection reasons"),
        &code,
    );
}

#[test]
fn repair_table_matches_every_repair_code() {
    let code: Vec<&'static str> = RepairCode::ALL.iter().map(|r| r.as_str()).collect();
    assert_same("## Repair codes", &table_codes("## Repair codes"), &code);
}
