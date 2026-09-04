// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Structural guard: fleet-auth reads no host or forwarding header
//! (ADR-0016).
//!
//! Every other test in this crate asserts a verdict. This one asserts an
//! absence, because that is the only way to state the rule that matters:
//! the CSRF decision must not depend on anything a client can type or a
//! proxy must remember to strip. A behavioural test cannot prove it.
//! Reading `X-Forwarded-Proto` as a tiebreak, or `Host` as a fallback
//! "when `public_origins` looks like a loopback bind", would pass every
//! truth-table row in `tests/origin_guard_truth_table.rs` and quietly hand
//! the verdict back to the network. So this test reads the source instead.
//!
//! Scope: every `.rs` file under `crates/fleet-auth/src`, recursively, in
//! full. Two earlier narrowings are gone because each was a way for a real
//! read to hide. The walk used to be one non-recursive `read_dir`, so the
//! first `src/session/mod.rs` split would have taken the whole module out
//! of scope. And the scan used to stop at a file's first `#[cfg(test)]`
//! line, which means an `#[cfg(test)] mod tests;` declaration near the top
//! would have hidden every production item below it.
//!
//! Whole-file scanning costs one thing: a unit test may no longer plant a
//! `Host` or `X-Forwarded-*` header, because a scanner cannot tell a
//! planted header from a trusted one. Those tests moved to
//! `tests/origin_guard_truth_table.rs`, where they still prove the same
//! headers move no verdict, through the same public API.
//!
//! Comment lines are skipped, deliberately: the doc comments that explain
//! why these headers are ignored have to be able to name them, and a rule
//! that forbids naming the refused input makes the refusal undocumentable.
//! A comment cannot read a header, so nothing is lost.
//!
//! This file lives in `tests/`, which the walk never enters, so it neither
//! scans itself nor the sibling test that does plant those headers.

use std::fs;
use std::path::{Path, PathBuf};

/// How a token is recognized in a line of source.
#[derive(Debug, Clone, Copy)]
enum Rule {
    /// The exact identifier, bounded by non-identifier characters on both
    /// sides. `HOST` hits in `header::HOST`, `{HOST as H}` and `(HOST,`,
    /// and does not hit inside `MAX_HOST_LEN` or `OriginHost`.
    Identifier(&'static str),
    /// A substring, ASCII-case-insensitively. The needle is spelled
    /// lowercase.
    Text(&'static str),
}

/// Source that would put the CSRF verdict back in the network's hands.
///
/// These are tokens, not fragments. The distinction is the whole reason
/// this list was rewritten: the old scan looked for the literal string
/// `header::HOST`, so `use http::header::{HOST as REQUEST_HOST};` followed
/// by `headers.get(REQUEST_HOST)` passed it cleanly. A header name reaches
/// code in exactly two shapes, and both are covered here: the typed
/// constant (any import path, any alias, because the identifier itself has
/// to appear at least once) and the quoted name handed to
/// `HeaderName::from_static` or a raw map lookup.
const FORBIDDEN: &[(&str, Rule)] = &[
    ("HOST", Rule::Identifier("HOST")),
    // Catches `Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`,
    // `X-Forwarded-Proto`, `X-Forwarded-Port`, `x_forwarded_proto` and
    // every case variant of each, in a constant or a string.
    ("forwarded", Rule::Text("forwarded")),
    ("\"host\"", Rule::Text("\"host\"")),
    ("\"x-forwarded", Rule::Text("\"x-forwarded")),
    // HTTP/2 spells the host as a pseudo-header, so a `Host` fallback can
    // be reintroduced without the word `host` appearing anywhere.
    (":authority", Rule::Text(":authority")),
];

/// One forbidden token, and the 1-based line it was found on.
#[derive(Debug, PartialEq, Eq)]
struct Finding {
    line: usize,
    token: &'static str,
}

/// Whether `line` matches `rule`.
fn matches(line: &str, lowercased: &str, rule: Rule) -> bool {
    match rule {
        Rule::Identifier(word) => line.match_indices(word).any(|(at, _)| {
            let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
            let before = line[..at].chars().next_back();
            let after = line[at + word.len()..].chars().next();
            !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
        }),
        Rule::Text(needle) => lowercased.contains(needle),
    }
}

/// Scan one file's text. Pure, so the self-tests below can feed it source
/// that is not on disk.
///
/// Reports at most one finding per line per token, in line order, so the
/// expected set in a self-test is a plain list.
fn scan_source(source: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let lowercased = line.to_ascii_lowercase();
        for (token, rule) in FORBIDDEN {
            if matches(line, &lowercased, *rule) {
                findings.push(Finding {
                    line: index + 1,
                    token,
                });
            }
        }
    }
    findings
}

/// Every `.rs` file under `root`, at any depth, in a stable order.
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Scan a source tree, returning the files scanned and one message per
/// finding.
fn scan_tree(root: &Path) -> (usize, Vec<String>) {
    let files = rust_sources(root);
    let mut findings = Vec::new();
    for path in &files {
        let source =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let lines: Vec<&str> = source.lines().collect();
        for finding in scan_source(&source) {
            findings.push(format!(
                "{}:{}: reads {}: {}",
                path.display(),
                finding.line,
                finding.token,
                lines[finding.line - 1].trim()
            ));
        }
    }
    (files.len(), findings)
}

#[test]
fn no_source_file_reads_a_host_or_forwarding_header() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let (scanned, findings) = scan_tree(&src);

    assert!(
        scanned >= 5,
        "expected to scan the crate's sources, found {scanned} files, did the layout move?"
    );
    assert!(
        findings.is_empty(),
        "ADR-0016: the origin verdict is the configured allowlist and the browser's \
         Origin, nothing else. These lines read a header a client or proxy controls:\n{}",
        findings.join("\n")
    );
}

/// Source that must trip the scan, and source that must not.
///
/// A guard whose failure mode is "finds nothing, ever" is worse than no
/// guard, because it reads as evidence. Every line here is either a shape
/// the old fragment-matching scan missed or a shape that must stay legal.
const FIXTURE: &[&str] = &[
    // 1: a doc comment may name every refused header.
    "//! The verdict ignores Host, Forwarded and X-Forwarded-Proto.",
    // 2: the alias import. The read below it is invisible on its own, so
    //    the import is what has to be caught.
    "use http::header::{HOST as REQUEST_HOST};",
    // 3: an early cfg(test) that used to end the scan.
    "#[cfg(test)]",
    // 4
    "mod tests;",
    // 5
    "fn verdict(parts: &Parts, allowed: &PublicOrigins) -> bool {",
    // 6: reads the aliased constant. No token of its own: `REQUEST_HOST`
    //    is one identifier, and line 2 already failed the file.
    "    let aliased = parts.headers.get(REQUEST_HOST);",
    // 7: a nested module path to the same constant.
    "    let typed = parts.headers.get(crate::net::inbound::header::HOST);",
    // 8: the quoted spelling, lowercase.
    "    let raw = parts.headers.get(\"host\");",
    // 9: a forwarding header by its quoted name, mixed case.
    "    let proxied = parts.headers.get(\"X-Forwarded-Proto\");",
    // 10: the typed forwarding constant.
    "    let listed = parts.headers.get(header::FORWARDED);",
    // 11: the HTTP/2 pseudo-header.
    "    let pseudo = parts.headers.get(\":authority\");",
    // 12: not a header read. `authority()` is the parsed URI, and the
    //     origin parser's own `OriginHost` type must stay spellable.
    "    let uri_authority: Option<OriginHost> = parts.uri.authority().map(parse);",
    // 13: an identifier that merely contains the word.
    "    let bound = MAX_HOST_LEN.min(HOSTS.len());",
    // 14
    "    aliased.is_some() || typed.is_some() || raw.is_some() || allowed.is_empty()",
    // 15
    "}",
    // 16: an indented line comment, still a comment.
    "    // Host and X-Forwarded-Host are read nowhere above.",
];

#[test]
fn the_scan_catches_every_shape_a_reintroduction_takes() {
    let findings = scan_source(&FIXTURE.join("\n"));
    let expected = [
        Finding {
            line: 2,
            token: "HOST",
        },
        Finding {
            line: 7,
            token: "HOST",
        },
        Finding {
            line: 8,
            token: "\"host\"",
        },
        Finding {
            line: 9,
            token: "forwarded",
        },
        Finding {
            line: 9,
            token: "\"x-forwarded",
        },
        Finding {
            line: 10,
            token: "forwarded",
        },
        Finding {
            line: 11,
            token: ":authority",
        },
    ];

    assert_eq!(
        findings,
        expected,
        "scanned fixture:\n{}",
        FIXTURE.join("\n")
    );
}

#[test]
fn the_scan_reaches_a_nested_module() {
    // The reason the walk is recursive: `session.rs` becoming
    // `session/mod.rs` plus `session/origin_guard.rs` is an ordinary
    // refactor, and it must not quietly empty the guard.
    let tree = tempfile::tempdir().expect("tempdir");
    let src = tree.path();
    fs::write(src.join("lib.rs"), "pub mod session;\n").expect("write lib.rs");
    let nested = src.join("session");
    fs::create_dir(&nested).expect("create session/");
    fs::write(nested.join("mod.rs"), "mod guard;\n").expect("write mod.rs");
    fs::write(
        nested.join("guard.rs"),
        "fn host(parts: &Parts) -> Option<&HeaderValue> {\n    parts.headers.get(\"host\")\n}\n",
    )
    .expect("write guard.rs");

    let (scanned, findings) = scan_tree(src);
    assert_eq!(scanned, 3, "every .rs file at any depth is read");
    assert_eq!(findings.len(), 1, "got: {findings:?}");
    assert!(
        findings[0].contains("guard.rs:2: reads \"host\""),
        "got: {}",
        findings[0]
    );
}
