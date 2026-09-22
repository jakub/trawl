// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression guard for ADR-0027 as amended 2026-09-21: a live stream
//! carries no range. `search_url::mode_query` is the ONE place either
//! DSL is produced, so the wasm shell — pages, components, state — never
//! names `effective_query` or `live_query` itself. A component that
//! reached for one of them directly could fold the popover's range into
//! a stream again, and the stream lane would drop every event older than
//! that window while the page still said Live.
//!
//! Comments are stripped first, with the same idiom
//! `native_control_contract.rs` uses, so prose about the merge is still
//! allowed to name the functions it describes.

use std::fs;
use std::path::{Path, PathBuf};

/// The DSL producers only `mode_query` may call.
const FORBIDDEN: &[&str] = &["effective_query", "live_query"];

/// Source trees that make up the wasm shell.
const SHELL_DIRS: &[&str] = &["pages", "components", "state"];

#[test]
fn the_shell_reaches_a_dsl_only_through_mode_query() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    let files = shell_files();
    assert!(
        files.len() > 10,
        "only {} files found under {:?} — the walk is looking in the wrong place",
        files.len(),
        SHELL_DIRS,
    );
    for path in files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let src = comment_stripped(
            &fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}")),
        );
        for name in FORBIDDEN {
            if src.contains(name) {
                offenders.push(format!("{rel}: {name}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "the wasm shell produces a DSL itself instead of asking \
         `search_url::mode_query` which one this mode runs (ADR-0027, \
         amended 2026-09-21): {}",
        offenders.join(", "),
    );
}

#[test]
fn a_forbidden_name_in_a_comment_is_prose() {
    for src in [
        "// merged via effective_query\n",
        "/// The `live_query` a stream runs.\n",
        "/* effective_query */\n",
    ] {
        let stripped = comment_stripped(src);
        assert!(
            !FORBIDDEN.iter().any(|name| stripped.contains(name)),
            "a name in a comment is prose, not a call: {src}",
        );
    }
    assert!(comment_stripped("let q = live_query(b, f);\n").contains("live_query"));
}

/// Every `.rs` file under the shell's source trees, sorted so a failure
/// reads the same way twice.
fn shell_files() -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    for dir in SHELL_DIRS {
        collect(&src.join(dir), &mut out);
    }
    out.sort();
    out
}

/// Recursive half of [`shell_files`].
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// `src` with its comments removed. Same idiom as
/// `native_control_contract.rs`: nested block comments go first, then any
/// line whose first non-space characters are `//`.
fn comment_stripped(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut kept: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if depth > 0 && bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
        } else {
            if depth == 0 {
                kept.push(bytes[i]);
            } else if bytes[i] == b'\n' {
                kept.push(b'\n');
            }
            i += 1;
        }
    }
    let out = String::from_utf8(kept).expect("dropping whole comment spans keeps the rest valid");
    out.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
