// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `now()` anchor contract (ADR-0017 §3).
//!
//! `now()` is one instant per unit of output: the batch lane captures it
//! once per logical query and binds it as a TIMESTAMP parameter, the live
//! lane samples it once per event. Both properties rest on the same
//! structural fact, that **no evaluation path reads the clock**. An
//! evaluator, a filter or a stream stage that calls `Utc::now()` reads a
//! different instant from the one its caller anchored, and nothing else
//! in the suite can see that: a value test written against a freshly
//! sampled clock agrees with a freshly sampled clock.
//!
//! So the clock has exactly one door in this crate,
//! [`trawl_core::context::EvalContext::capture`], and this test walks the
//! source to prove it. `#[cfg(test)]` code is deliberately in scope: a
//! test that samples its own clock cannot prove per-unit freezing, so the
//! test fixtures use fixed anchors too and must stay that way.

use std::path::{Path, PathBuf};

/// The calls this crate may not make outside its one door.
///
/// Two needles, both deliberately shaped as `::verb` rather than as one
/// fully-qualified spelling:
///
/// - `::now` is the clock itself. Naming only `Utc::now` would leave
///   three doors open beside it: `chrono::Local::now()`,
///   `std::time::SystemTime::now()`/`Instant::now()`, and an alias import
///   (`use chrono::Utc as U; U::now()`) that renames the type without
///   changing the call.
/// - `::capture` is the door, from the inside. Nothing under `src/` may
///   mint its own [`trawl_core::context::EvalContext`]: a unit of output
///   receives its instant. `EvalContext::capture().now_value()` inside an
///   evaluator is per-call sampling wearing the door's own public API,
///   and no clock-name needle can see it.
///
/// A needle matches at a word boundary: [`call_lines`] counts it when
/// the byte that follows is not an identifier character. Deliberately not
/// "followed by `(`", because that spelling reads only the ordinary call
/// and misses every way Rust has of naming a function without calling it
/// on the spot:
///
/// ```text
/// supplied.unwrap_or_else(EvalContext::capture)   // callback
/// let f = EvalContext::capture;   f()             // fn pointer
/// Utc::now /* why */ ()                           // comment between
/// Utc::now ()                                     // whitespace between
/// ```
///
/// each of which reads the clock exactly as `Utc::now()` does. The
/// boundary is what keeps the paren-less needles honest: `::nowhere` and
/// `::captures` continue into an identifier byte and do not match.
///
/// What the crate loses is the ability to mention a needle in running
/// code: a string literal `"Utc::now"` is flagged, because nothing here
/// parses Rust and a literal is indistinguishable from a path. Prose is
/// still free: comment lines are stripped before the scan, which is
/// where the crate explains this rule (the module note in `context.rs`,
/// the sampling note in `filter.rs`). Erring loud on a literal is the
/// right side to err on.
const CLOCK_CALLS: &[&str] = &["::now", "::capture"];

/// The clock read proper — the needle
/// [`the_clock_has_exactly_one_door`] counts inside the owner, which is
/// allowed to mint contexts freely.
const CLOCK_READ: &str = "::now";

/// The file allowed to make them, as a path relative to `src/`.
///
/// Compared against the relative path, never against the file name: a
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
/// A symlink of any kind is refused rather than followed. A symlinked
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

/// The 1-based lines of `source` that call one of `needles`.
///
/// Three steps:
///
/// 1. **Comment lines** (`//`, `///`, `//!` alike) are dropped, because
///    the crate documents this rule in prose beside the one call that
///    implements it, and a `contains` over the raw text would be green
///    today for the wrong reason and red tomorrow for no reason. A
///    trailing comment mentioning a call still counts, which errs toward
///    failing loud.
/// 2. **The surviving lines are joined**, and the gap a path may carry
///    after its own `::` is closed ([`path_gap`]): whitespace, a line
///    break, a block comment, a trailing `//` comment. So `Utc:: now()`,
///    a path broken across two lines, `Utc::/*x*/now()` and
///    `Utc:: // why` + newline + `now()` are all seen as the single-line
///    spelling is.
/// 3. **A needle then counts at a word boundary**, the rule
///    [`CLOCK_CALLS`] documents.
///
/// Each byte remembers the original line it came from, so a violation is
/// reported at the line its `::` sits on. Best effort for a split path,
/// which is the line a reader wants anyway.
///
/// # What this defends against, and what it does not
///
/// The threat is accidental reintroduction: someone adds a clock read to
/// an evaluation path, or reaches for `EvalContext::capture()` because a
/// context is inconvenient to thread, and no value test can see it,
/// because a test sampling its own clock agrees with an evaluator
/// sampling its own. The scan is the whole defence, and it deliberately
/// does not lean on rustfmt to normalize spacing first: a contract that
/// only holds while a different gate holds has a second owner.
///
/// Consuming whitespace, block comments and line comments inside the
/// path's own gap draws the line at every spelling a reviewer could read
/// past as ordinary source. Deliberate evasion stays out of scope,
/// because nothing here parses Rust: a macro that assembles the path, an
/// `include!`, a raw identifier (`r#now`), a `/*` nested inside another
/// block comment, a re-export under a different name. Each of those is
/// contrivance, visible as contrivance on the diff, and review's job
/// rather than this file's.
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

    // Close the path's own gaps. A needle is a fragment of a path, and
    // Rust lets a path carry whitespace, a line break or a block comment
    // between its `::` and the name that follows (`Utc:: now()`,
    // `Utc::\n    now()`, `Utc::/*x*/now()`), each of which reads the
    // clock while a literal `::now` scan sees nothing. Only the gap after
    // a `::` can hide a needle: the needle starts at `::`, so whatever
    // precedes it is not part of the match.
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
            // A word boundary, not a paren: `::now` ends here unless the
            // source continues the identifier (`::nowhere`, `::captures`).
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
    // One report per line: two needles meeting on one line is one
    // violation to fix.
    found.dedup();
    found
}

/// The byte length of the gap a path may carry after its `::`:
/// whitespace, terminated block comments, and `//` line comments, in any
/// order.
///
/// The line-comment arm is not redundant with the comment-line strip in
/// [`call_lines`]: that one drops lines which begin with `//`, and the
/// gap this closes is a trailing comment on a line that begins with code:
///
/// ```text
/// let _ = chrono::Utc:: // use UTC
///     now();
/// ```
///
/// Ordinary-looking, valid, and rustfmt leaves it exactly as written.
///
/// Scoped to the post-`::` position on purpose. Stripping every
/// `/* … */` from the whole source instead would be a worse instrument
/// than the hole it closes: this crate writes glob patterns in string
/// literals (`"path=/api/*"`, `"/data/**/*.parquet"`), each of which
/// contains a literal `/*`, and a blanket strip reads them as comment
/// openers. Measured on `src/filter.rs`, a blanket strip deletes about a
/// fifth of the file, and a `Utc::now()` planted on the line after
/// `"path=/api/*"` disappears from the scan. A contract that fails to see
/// is worse than one that can be dodged on purpose.
///
/// An unterminated `/*` is left alone for the same reason: at this
/// position it is far likelier to be a glob than a comment, and consuming
/// to end-of-file would blind everything after it.
///
/// The anchoring is also what makes the comment arms safe rather than
/// merely narrow. They run only inside a gap that a real `::` already
/// opened, and each consumes a bounded span, to the `*/` or to the end of
/// one line. The worst a string literal shaped like `":: // x"` can do is
/// join its `::` to whatever the next joined line begins with, which is a
/// false positive. This contract may cry wolf, it may not go blind.
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
            // Through the newline, so the path continues on the next
            // line. A comment ending the text ends the gap with it.
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

/// The instrument's own test: the scan counts calls, not mentions, and it
/// counts the bypasses a clock-name needle cannot see.
///
/// Without the comment filter this contract would be red today, since the
/// crate's prose names the call it forbids, and the tempting fix (drop
/// the prose) would trade the explanation for the rule. The second half
/// asserts that the filter is still load-bearing on the real sources, so
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
        vec![4, 5],
        "the three COMMENT lines are prose and drop out; the string literal on \
         line 4 does NOT, because the word-boundary rule cannot tell a literal \
         from a path and erring loud is the documented side to err on"
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
        ("a comment before the paren", "Utc::now /* why */ ()"),
        // …and the same gaps inside the path, which a literal `::now`
        // scan walks past. rustfmt normalizes the whitespace-only form
        // away; both comment forms it leaves exactly as written.
        ("a space inside the path", "Utc :: now()"),
        ("a comment inside the path", "Utc::/*x*/now()"),
        ("a comment and a space inside the path", "Utc:: /*x*/ now()"),
        // …and no paren at all is still a clock read: these name the
        // function and something else calls it.
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

    // A call split across lines is one call to the compiler, and it is
    // reported at the line the path starts on.
    let split = "let x = EvalContext::capture\n\
                 ();\n";
    assert_eq!(
        call_lines(split, CLOCK_CALLS),
        vec![1],
        "a call whose paren is on the next line must be caught, at the line the \
         path starts on"
    );

    // The break can also fall inside the path, between its `::` and the
    // name: still one path to the compiler, still reported at its `::`.
    let split_path = "let x = Utc::\n\
                      now();\n";
    assert_eq!(
        call_lines(split_path, CLOCK_CALLS),
        vec![1],
        "a path broken after its `::` must be caught, at the line the `::` is on"
    );

    // …and a trailing line comment may sit in that break. The
    // comment-line strip cannot help here: this line begins with code.
    // Ordinary-looking, valid, and rustfmt leaves it as written.
    let commented_path = "let _ = chrono::Utc:: // use UTC\n\
                          \x20   now();\n";
    assert_eq!(
        call_lines(commented_path, CLOCK_CALLS),
        vec![1],
        "a line comment in the path's own gap must be caught, at the line the \
         `::` is on"
    );

    // The same with no space before the comment, so the gap opens on the
    // `//` itself.
    let tight_comment = "let _ = chrono::Utc::// c\n\
                         now();\n";
    assert_eq!(
        call_lines(tight_comment, CLOCK_CALLS),
        vec![1],
        "a line comment flush against the `::` must be caught too"
    );

    // …and the word boundary is what lets the needles be paren-less:
    // these continue into an identifier byte, so the needle does not end
    // where it appears to and they are not the things this contract
    // forbids.
    for (case, innocent) in [
        ("a name that merely starts with the needle", "foo::nowhere;"),
        ("a different function", "let c = Regex::captures(&re, s);"),
        ("a longer capture name", "let c = Thing::capture_all();"),
        ("a longer now name", "let n = Thing::now_ish();"),
        // An unterminated `/*` is a glob in a string literal far more
        // often than a comment in this crate, and `path_gap` leaves it
        // alone precisely so it cannot blind the rest of the scan.
        (
            "a glob literal that opens no comment",
            "let g = \"path=/api/*\";",
        ),
        // A gap that runs off the end of the text ends with it — the
        // consumer is bounded, never looping.
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
