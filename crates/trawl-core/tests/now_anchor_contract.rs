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

/// The calls this crate may not make outside its one door.
///
/// Two needles, both deliberately shaped as `::verb` rather than as one
/// fully-qualified spelling:
///
/// - `::now` — the clock itself. Naming only `Utc::now` left three doors
///   open beside it: `chrono::Local::now()`,
///   `std::time::SystemTime::now()`/`Instant::now()`, and an alias import
///   (`use chrono::Utc as U; U::now()`) that renames the type without
///   changing the call.
/// - `::capture` — the door, from the inside. Nothing under `src/` may
///   MINT its own [`trawl_core::context::EvalContext`]: a unit of output
///   RECEIVES its instant. `EvalContext::capture().now_value()` inside an
///   evaluator is this PR's own public API used to reintroduce exactly
///   the per-call sampling it removed, and no clock-name needle can see
///   it.
///
/// A needle alone is not a match: [`call_lines`] requires an open paren
/// after it, so the crate may keep MENTIONING `Utc::now` in prose (the
/// module note in `context.rs`, the sampling note in `filter.rs`) and
/// `::captures(` — a different function — is not `::capture(`. The paren
/// may be separated by any run of whitespace, NEWLINES INCLUDED, because
/// `Utc::now ()` and a call split across two lines are the same call to
/// the compiler and were the same bypass to a per-line literal scan. A
/// STRING LITERAL shaped like a call would be flagged; that errs loud,
/// which is the right side to err on.
const CLOCK_CALLS: &[&str] = &["::now", "::capture"];

/// The clock read proper — the needle
/// [`the_clock_has_exactly_one_door`] counts inside the owner, which is
/// allowed to mint contexts freely.
const CLOCK_READ: &str = "::now";

/// The file allowed to make them, as a path RELATIVE TO `src/`.
///
/// Compared against the relative path, never against the file NAME: a
/// future `src/anything/context.rs` would otherwise be exempt from the
/// rule by virtue of its basename alone.
const CLOCK_OWNER: &str = "context.rs";

/// Named once, printed by every failure: a violation is not "delete the
/// line", it is "take the instant your caller anchored".
const REMEDY: &str =
    "an evaluation path may not read the clock — take an EvalContext (ADR-0017 §3, #106)";

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, recursively.
///
/// A SYMLINK of any kind is refused rather than followed. A symlinked
/// directory can point the walk outside the tree it is auditing (making
/// the scan pass over sources that are not these), or back into it
/// (making the walk loop), and either way the set of files scanned stops
/// being "this crate's sources". Refusing is loud; following is silent.
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
        let meta = std::fs::symlink_metadata(&path).unwrap_or_else(|error| {
            panic!(
                "source walk infrastructure failure at {}: {error}",
                path.display()
            )
        });
        assert!(
            !meta.file_type().is_symlink(),
            "source walk infrastructure failure: {} is a symlink, and this scan \
             must audit THESE sources — a symlinked directory can point the walk \
             out of the tree or back into it",
            path.display()
        );
        if meta.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// The 1-based lines of `source` that CALL one of `needles`.
///
/// Comment lines — `//`, `///`, `//!` alike — are dropped before the
/// match, because the crate documents the rule in prose beside the one
/// call that implements it and a `contains` over the raw text would be
/// green today for the wrong reason and red tomorrow for no reason. The
/// crate uses no block comments (checked by eye and by this file's own
/// fixture test), so `/* … */` is deliberately not handled; a TRAILING
/// comment mentioning a call still counts, which errs toward failing
/// loud.
///
/// The surviving lines are then scanned JOINED rather than one at a
/// time, and a needle counts only when the next non-whitespace byte
/// after it is `(`. Rust puts no constraint on the whitespace between a
/// path and its call paren, so `Utc::now ()` and
///
/// ```text
/// EvalContext::capture
///     ()
/// ```
///
/// are ordinary calls that a per-line literal `::now(` scan walked
/// straight past. Leaning on rustfmt to normalize the spacing would make
/// this contract depend on a DIFFERENT gate holding, which is exactly the
/// kind of second owner it exists to remove. Requiring the paren is also
/// what keeps the paren-less needles honest: `::nowhere` and `::captures`
/// continue past it into a non-paren byte and do not match.
///
/// Each byte of the joined text remembers the ORIGINAL line it came
/// from, so a violation is still reported at the line the needle starts
/// on — best effort for a split call, which is the line a reader wants
/// anyway.
fn call_lines(source: &str, needles: &[&str]) -> Vec<usize> {
    let mut text = String::with_capacity(source.len());
    let mut line_of: Vec<usize> = Vec::with_capacity(source.len());
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        // One entry per BYTE pushed, so `line_of[offset]` is exact for a
        // byte offset into `text` whatever the line holds.
        line_of.extend(std::iter::repeat_n(index + 1, line.len() + 1));
        text.push_str(line);
        text.push('\n');
    }

    let mut found: Vec<usize> = Vec::new();
    for needle in needles {
        let mut from = 0;
        while let Some(offset) = text[from..].find(needle) {
            let at = from + offset;
            let after = at + needle.len();
            if text[after..].trim_start().starts_with('(') {
                found.push(line_of[at]);
            }
            from = after;
        }
    }
    found.sort_unstable();
    // One report per LINE, as the per-line scan gave: two needles meeting
    // on one line is one violation to fix.
    found.dedup();
    found
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
    let mut saw_owner = false;
    for path in &sources {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or_else(|error| panic!("{} is not under src: {error}", path.display()));
        if relative == Path::new(CLOCK_OWNER) {
            saw_owner = true;
            continue;
        }
        let source = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!(
                "source read infrastructure failure {}: {error}",
                path.display()
            )
        });
        for line in call_lines(&source, CLOCK_CALLS) {
            violations.push(format!("{}:{line}", relative.display()));
        }
    }
    assert!(
        saw_owner,
        "source walk infrastructure failure: src/{CLOCK_OWNER} was not visited, \
         so the exemption is pinned to a path that no longer exists"
    );

    assert!(
        violations.is_empty(),
        "{REMEDY}\n\
         one of {CLOCK_CALLS:?} appears outside src/{CLOCK_OWNER}, at: {violations:?}\n\
         The instant belongs to the unit of output — a query, an event, an \
         emitted snapshot — and reaches an evaluator as a parameter \
         (`EvalContext`), never as a fresh sample or a fresh capture. Test code \
         counts: a test that samples its own clock cannot prove per-unit \
         freezing."
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
    let calls = call_lines(&source, &[CLOCK_READ]);
    assert_eq!(
        calls.len(),
        1,
        "{REMEDY}\n\
         src/{CLOCK_OWNER} must hold EXACTLY ONE {CLOCK_READ} — the capture \
         `EvalContext::capture()` performs — and it holds {} (lines {calls:?}). \
         A second one is a second clock domain inside the door that exists to \
         remove them.",
        calls.len()
    );
}

/// The instrument's own test: the scan counts CALLS, not mentions, and it
/// counts the bypasses a clock-name needle cannot see.
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
        call_lines(fixture, CLOCK_CALLS),
        vec![5],
        "only the CALL line counts: a doc, inner-doc or bare comment mentioning \
         a clock read is prose"
    );

    // The bypasses a `Utc::now(` needle could not see, each of which puts
    // a second clock domain inside an evaluation path.
    for (case, bypass) in [
        ("this PR's own API", "EvalContext::capture().now_value()"),
        (
            "a qualified capture",
            "crate::context::EvalContext::capture()",
        ),
        ("the std clock", "std::time::SystemTime::now()"),
        ("a monotonic read", "std::time::Instant::now()"),
        ("the local zone", "chrono::Local::now()"),
        ("an alias import", "U::now()"),
        // Rust puts no constraint on the whitespace before a call's
        // paren, so neither may this scan.
        ("a space before the paren", "Utc::now ()"),
        ("a tab before the paren", "Utc::now\t()"),
    ] {
        assert_eq!(
            call_lines(&format!("let x = {bypass};\n"), CLOCK_CALLS),
            vec![1],
            "{case} must be caught: {bypass}"
        );
    }

    // A call SPLIT across lines is one call to the compiler, and it is
    // reported at the line the path starts on.
    let split = "let x = EvalContext::capture\n\
                 ();\n";
    assert_eq!(
        call_lines(split, CLOCK_CALLS),
        vec![1],
        "a call whose paren is on the next line must be caught, at the line the \
         path starts on"
    );

    // …and the paren requirement is what lets the needles be paren-less:
    // these continue into a byte that is not `(`, so they are not calls
    // to the things this contract forbids.
    for (case, innocent) in [
        ("a name that merely starts with the needle", "foo::nowhere;"),
        ("a different function", "let c = Regex::captures(&re, s);"),
        ("a mention in a string", "let s = \"Utc::now\";"),
        ("a use declaration", "use chrono::Utc::now;"),
    ] {
        assert_eq!(
            call_lines(&format!("{innocent}\n"), CLOCK_CALLS),
            Vec::<usize>::new(),
            "{case} is not a clock call: {innocent}"
        );
    }

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
