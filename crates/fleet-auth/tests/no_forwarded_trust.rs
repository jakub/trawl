// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Structural guard: fleet-auth reads no host or forwarding header
//! (ADR-0016).
//!
//! A unit test under `src` may not plant a `Host` or `X-Forwarded-*` header,
//! because a scanner cannot tell a planted header from a trusted one. Those
//! tests live in `tests/origin_guard_truth_table.rs`.
//!
//! # Threat model
//!
//! The scan catches an honest reintroduction, and that is all it is for.
//! It does not catch a contributor who wants the read to go unseen:
//! `DOCUMENTED_NON_GOALS` below lists evasions it misses by decision. What
//! holds the invariant against a hostile contributor is code review.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
enum Rule {
    Identifier(&'static str),
    /// A substring, ASCII-case-insensitively. The needle is spelled
    /// lowercase.
    Text(&'static str),
}

const FORBIDDEN: &[(&str, Rule)] = &[
    ("HOST", Rule::Identifier("HOST")),
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

const FIXTURE: &[&str] = &[
    // 1: a doc comment may name every refused header.
    "//! The verdict ignores Host, Forwarded and X-Forwarded-Proto.",
    // 2: the alias import. The read below it is invisible on its own, so
    //    the import is what has to be caught.
    "use http::header::{HOST as REQUEST_HOST};",
    // 3: an early cfg(test).
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

const DOCUMENTED_NON_GOALS: &[&str] = &[
    // 1-2: a string literal continued across a line break. rustc joins it
    //      back into "host"; the scanner reads two lines, neither of which
    //      contains the name.
    "    let split = parts.headers.get(\"ho\\",
    "st\");",
    // 3: a raw string whose spaces are trimmed at runtime.
    "    let trimmed = parts.headers.get(r\" host \".trim());",
    // 4: the name assembled by a macro at compile time.
    "    let joined = parts.headers.get(concat!(\"ho\", \"st\"));",
    // 5: the name built from bytes, no text at all.
    "    let built = HeaderName::from_bytes(&[104, 111, 115, 116]).unwrap();",
];

#[test]
fn documented_non_goals_are_not_detected() {
    let findings = scan_source(&DOCUMENTED_NON_GOALS.join("\n"));
    assert!(
        findings.is_empty(),
        "these evasions are out of scope by decision, not by accident. A text scan \
         cannot prove this invariant, and review is the control for a hostile \
         reintroduction. If widening the scan was intended, update the threat model \
         in this file's module doc and move the line out of DOCUMENTED_NON_GOALS. \
         Detected: {findings:?}"
    );
}

#[test]
fn the_scan_reaches_a_nested_module() {
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
