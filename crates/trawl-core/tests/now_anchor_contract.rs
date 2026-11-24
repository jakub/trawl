// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `now()` anchor contract (ADR-0017 §3).
//!
//! The clock has exactly one door in this crate,
//! [`trawl_core::context::EvalContext::capture`], and this test walks the
//! source to prove it. `#[cfg(test)]` code is deliberately in scope.

use std::path::{Path, PathBuf};

/// - `::now` is the clock itself. Naming only `Utc::now` would leave
///   three doors open beside it: `chrono::Local::now()`,
///   `std::time::SystemTime::now()`/`Instant::now()`, and an alias import
///   (`use chrono::Utc as U; U::now()`).
/// - `::capture` is the door, from the inside. Nothing under `src/` may
///   mint its own [`trawl_core::context::EvalContext`].
///
/// A needle matches at a word boundary. Deliberately not "followed by
/// `(`", which misses a callback, a fn pointer and a gap before the paren.
/// A string literal `"Utc::now"` is flagged, because nothing here parses
/// Rust.
const CLOCK_CALLS: &[&str] = &["::now", "::capture"];

const CLOCK_READ: &str = "::now";

/// The file allowed to make them, as a path relative to `src/`.
const CLOCK_OWNER: &str = "context.rs";

const REMEDY: &str =
    "an evaluation path may not read the clock — take an EvalContext (ADR-0017 §3, #106)";

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

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

/// The 1-based lines of `source` that call one of `needles`.
///
/// Comment lines (`//`, `///`, `//!` alike) are dropped, because the
/// crate documents this rule in prose. A trailing comment mentioning a call
/// still counts.
///
/// Deliberate evasion stays out of scope, because nothing here parses
/// Rust: a macro that assembles the path, an `include!`, a raw identifier
/// (`r#now`), a `/*` nested inside another block comment, a re-export under
/// a different name.
fn call_lines(source: &str, needles: &[&str]) -> Vec<usize> {
    let mut text = String::with_capacity(source.len());
    let mut line_of: Vec<usize> = Vec::with_capacity(source.len());
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        // One entry per byte pushed, so `line_of[offset]` is exact for a
        // byte offset into `text` whatever the line holds.
        line_of.extend(std::iter::repeat_n(index + 1, line.len() + 1));
        text.push_str(line);
        text.push('\n');
    }

    // Close the path's own gaps. Only the gap after a `::` can hide a
    // needle: the needle starts at `::`, so whatever precedes it is not part
    // of the match.
    let mut text_line = Vec::with_capacity(line_of.len());
    let mut collapsed = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        if text[cursor..].starts_with("::") {
            collapsed.push_str("::");
            text_line.push(line_of[cursor]);
            text_line.push(line_of[cursor]);
            cursor += 2 + path_gap(&text[cursor + 2..]);
            continue;
        }
        let ch = text[cursor..].chars().next().expect("cursor is in bounds");
        for _ in 0..ch.len_utf8() {
            text_line.push(line_of[cursor]);
        }
        collapsed.push(ch);
        cursor += ch.len_utf8();
    }
    let (text, line_of) = (collapsed, text_line);

    let mut found: Vec<usize> = Vec::new();
    for needle in needles {
        let mut from = 0;
        while let Some(offset) = text[from..].find(needle) {
            let at = from + offset;
            let after = at + needle.len();
            let continues = text[after..]
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_alphanumeric() || next == '_');
            if !continues {
                found.push(line_of[at]);
            }
            from = after;
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// The line-comment arm is not redundant with the comment-line strip in
/// [`call_lines`]: that one drops lines which begin with `//`, and the
/// gap this closes is a trailing comment on a line that begins with code.
///
/// Scoped to the post-`::` position on purpose: this crate writes glob
/// patterns in string literals (`"path=/api/*"`), each of which contains a
/// literal `/*`, and a blanket strip reads them as comment openers. An
/// unterminated `/*` is left alone for the same reason.
fn path_gap(rest: &str) -> usize {
    let mut taken = 0;
    loop {
        let tail = &rest[taken..];
        let space = tail.len() - tail.trim_start().len();
        if space > 0 {
            taken += space;
            continue;
        }
        if tail.starts_with("/*")
            && let Some(close) = tail.find("*/")
        {
            taken += close + 2;
            continue;
        }
        if tail.starts_with("//") {
            taken += tail.find('\n').map_or(tail.len(), |end| end + 1);
            continue;
        }
        return taken;
    }
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

#[test]
fn the_scan_counts_calls_not_mentions() {
    let fixture = "//! [`chrono::Utc::now`] carries nanoseconds\n\
                   /// Capture the current instant: `Utc::now()`\n\
                   // a bare comment mentioning Utc::now() too\n\
                   let mention = \"Utc::now\";\n\
                   Self::at(Utc::now())\n";
    assert_eq!(
        call_lines(fixture, CLOCK_CALLS),
        vec![4, 5],
        "the three COMMENT lines are prose and drop out; the string literal on \
         line 4 does NOT, because the word-boundary rule cannot tell a literal \
         from a path and erring loud is the documented side to err on"
    );

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
        ("a space before the paren", "Utc::now ()"),
        ("a tab before the paren", "Utc::now\t()"),
        ("a comment before the paren", "Utc::now /* why */ ()"),
        ("a space inside the path", "Utc :: now()"),
        ("a comment inside the path", "Utc::/*x*/now()"),
        ("a comment and a space inside the path", "Utc:: /*x*/ now()"),
        (
            "a callback",
            "supplied.unwrap_or_else(EvalContext::capture)",
        ),
        ("a clock callback", "supplied.unwrap_or_else(Utc::now)"),
        ("a fn-pointer binding", "let f = EvalContext::capture;"),
        // Not legal Rust for an inherent associated function anyway, so
        // flagging it costs nothing and errs loud.
        ("a use declaration", "use chrono::Utc::now;"),
    ] {
        assert_eq!(
            call_lines(&format!("let x = {bypass};\n"), CLOCK_CALLS),
            vec![1],
            "{case} must be caught: {bypass}"
        );
    }

    let split = "let x = EvalContext::capture\n\
                 ();\n";
    assert_eq!(
        call_lines(split, CLOCK_CALLS),
        vec![1],
        "a call whose paren is on the next line must be caught, at the line the \
         path starts on"
    );

    let split_path = "let x = Utc::\n\
                      now();\n";
    assert_eq!(
        call_lines(split_path, CLOCK_CALLS),
        vec![1],
        "a path broken after its `::` must be caught, at the line the `::` is on"
    );

    let commented_path = "let _ = chrono::Utc:: // use UTC\n\
                          \x20   now();\n";
    assert_eq!(
        call_lines(commented_path, CLOCK_CALLS),
        vec![1],
        "a line comment in the path's own gap must be caught, at the line the \
         `::` is on"
    );

    let tight_comment = "let _ = chrono::Utc::// c\n\
                         now();\n";
    assert_eq!(
        call_lines(tight_comment, CLOCK_CALLS),
        vec![1],
        "a line comment flush against the `::` must be caught too"
    );

    for (case, innocent) in [
        ("a name that merely starts with the needle", "foo::nowhere;"),
        ("a different function", "let c = Regex::captures(&re, s);"),
        ("a longer capture name", "let c = Thing::capture_all();"),
        ("a longer now name", "let n = Thing::now_ish();"),
        (
            "a glob literal that opens no comment",
            "let g = \"path=/api/*\";",
        ),
        ("a comment that ends the text", "let x = foo:: // c"),
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
