// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `now()` anchor contract (ADR-0017 §3, issue #106).
//!
//! `now()` is ONE instant per unit of output: the batch lane captures it
//! once per logical query and binds it as a TIMESTAMP parameter, the live
//! lane samples it once per event. Both properties rest on the same
//! structural fact — **no evaluation path reads the clock**. An
//! evaluator, a filter or a stream stage that calls `Utc::now()` reads a
//! DIFFERENT instant from the one its caller anchored, and nothing else
//! in the suite can see that: a value test written against a freshly
//! sampled clock agrees with a freshly sampled clock.
//!
//! So the clock has exactly one door in this crate,
//! [`trawl_core::context::EvalContext::capture`], and this test walks the
//! source to prove it. `#[cfg(test)]` code is deliberately IN scope: a
//! test that samples its own clock cannot prove per-unit freezing, so the
//! test fixtures were converted to fixed anchors too and must stay that
//! way.

use std::path::{Path, PathBuf};

/// The clock call this crate may not make outside its one door.
///
/// The open paren is load-bearing: the crate MENTIONS `Utc::now` in prose
/// (the module note in `context.rs`, the sampling note in `filter.rs`),
/// and a mention is documentation, not a clock read.
const CLOCK_CALL: &str = "Utc::now(";

/// The file allowed to make it, relative to `src/`.
const CLOCK_OWNER: &str = "context.rs";

/// Named once, printed by every failure: a violation is not "delete the
/// line", it is "take the instant your caller anchored".
const REMEDY: &str =
    "an evaluation path may not read the clock — take an EvalContext (ADR-0017 §3, #106)";

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, recursively.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|error| {
        panic!(
            "source walk infrastructure failure at {}: {error}",
            dir.display()
        )
    });
    for entry in entries {
        let path = entry
            .unwrap_or_else(|error| panic!("source walk infrastructure failure: {error}"))
            .path();
        if path.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// The 1-based lines of `source` that CALL the clock.
///
/// Comment lines — `//`, `///`, `//!` alike — are dropped before the
/// match, because the crate documents the rule in prose beside the one
/// call that implements it and a `contains` over the raw text would be
/// green today for the wrong reason and red tomorrow for no reason. The
/// crate uses no block comments (checked by eye and by this file's own
/// fixture test), so `/* … */` is deliberately not handled; a TRAILING
/// comment mentioning the call still counts, which errs toward failing
/// loud.
fn clock_call_lines(source: &str) -> Vec<usize> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| line.contains(CLOCK_CALL))
        .map(|(index, _)| index + 1)
        .collect()
}

#[test]
fn no_evaluation_path_in_trawl_core_reads_the_clock() {
    let root = src_root();
    let sources = rust_sources(&root);
    assert!(
        sources.len() > 10,
        "source walk infrastructure failure: {} yielded {} files",
        root.display(),
        sources.len()
    );

    let mut violations = Vec::new();
    for path in &sources {
        if path.file_name().is_some_and(|name| name == CLOCK_OWNER) {
            continue;
        }
        let source = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!(
                "source read infrastructure failure {}: {error}",
                path.display()
            )
        });
        let relative = path.strip_prefix(&root).unwrap_or(path);
        for line in clock_call_lines(&source) {
            violations.push(format!("{}:{line}", relative.display()));
        }
    }

    assert!(
        violations.is_empty(),
        "{REMEDY}\n\
         {CLOCK_CALL} appears outside src/{CLOCK_OWNER}, at: {violations:?}\n\
         The instant belongs to the unit of output — a query, an event, an \
         emitted snapshot — and reaches an evaluator as a parameter \
         (`EvalContext`), never as a fresh sample. Test code counts: a test \
         that samples its own clock cannot prove per-unit freezing."
    );
}

#[test]
fn the_clock_has_exactly_one_door() {
    let owner = src_root().join(CLOCK_OWNER);
    let source = std::fs::read_to_string(&owner).unwrap_or_else(|error| {
        panic!(
            "source read infrastructure failure {}: {error}",
            owner.display()
        )
    });
    let calls = clock_call_lines(&source);
    assert_eq!(
        calls.len(),
        1,
        "{REMEDY}\n\
         src/{CLOCK_OWNER} must hold EXACTLY ONE {CLOCK_CALL} — the capture \
         `EvalContext::capture()` performs — and it holds {} (lines {calls:?}). \
         A second one is a second clock domain inside the door that exists to \
         remove them.",
        calls.len()
    );
}

/// The instrument's own test: the scan counts CALLS, not mentions.
///
/// Without the comment filter this contract would be red today — the
/// crate's prose names the call it forbids — and the tempting fix (drop
/// the prose) would trade the explanation for the rule. The second half
/// asserts that the filter is still load-bearing on the REAL sources, so
/// nobody can quietly simplify it back to a `contains`.
#[test]
fn the_scan_counts_calls_not_mentions() {
    let fixture = "//! [`chrono::Utc::now`] carries nanoseconds\n\
                   /// Capture the current instant: `Utc::now()`\n\
                   // a bare comment mentioning Utc::now() too\n\
                   let mention = \"Utc::now\";\n\
                   Self::at(Utc::now())\n";
    assert_eq!(
        clock_call_lines(fixture),
        vec![5],
        "only the CALL line counts: a doc, inner-doc or bare comment mentioning \
         {CLOCK_CALL} is prose"
    );

    let mut documented = 0usize;
    for path in rust_sources(&src_root()) {
        let source = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "source read infrastructure failure {}: {error}",
                path.display()
            )
        });
        documented += source
            .lines()
            .filter(|line| line.trim_start().starts_with("//"))
            .filter(|line| line.contains("Utc::now"))
            .count();
    }
    assert!(
        documented > 0,
        "the comment filter is what makes this contract expressible; the crate \
         used to explain the rule in prose beside the one call, and if that \
         prose is gone the filter looks like dead code and gets deleted"
    );
}
