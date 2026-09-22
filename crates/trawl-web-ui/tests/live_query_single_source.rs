// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression guard for ADR-0027 as amended 2026-09-21: a live stream
//! carries no range. `search_url::mode_query` is the ONE place either
//! DSL is produced, so the three shell source trees the guard walks,
//! `pages`, `components` and `state`, never name `effective_query` or
//! `live_query` themselves. A component that reached for one of them
//! directly could fold the popover's range into a stream again, and the
//! stream lane would drop every event older than that window while the
//! page still said Live.
//!
//! Line comments are dropped first, so prose about the merge is still
//! allowed to name the functions it describes. Nothing else is: a
//! stripper that tracked `/*` would read one inside a string literal or
//! a line comment as an opener and hide every call after it, so this
//! guard fails closed instead. A name inside a block comment trips it,
//! loudly, and the reader moves the prose to a `//` line.

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
        "    //! `effective_query` folds the range in.\n",
    ] {
        let stripped = comment_stripped(src);
        assert!(
            !FORBIDDEN.iter().any(|name| stripped.contains(name)),
            "a name in a comment is prose, not a call: {src}",
        );
    }
}

#[test]
fn a_call_is_never_hidden_by_what_precedes_it() {
    // Each of these once slipped past a stripper that tracked `/*`: an
    // opener inside a line comment or a string literal swallowed the
    // rest of the file. The call after it must still trip the guard.
    for src in [
        "let q = live_query(b, f);\n",
        "// treat /* as literal log text.\nlet q = crate::query_merge::live_query(b, f);\n",
        "let _literal = \"/*\";\nlet q = crate::query_merge::effective_query(b, f, r);\n",
        "/* prose */ let q = live_query(b, f);\n",
    ] {
        let stripped = comment_stripped(src);
        assert!(
            FORBIDDEN.iter().any(|name| stripped.contains(name)),
            "a call must survive the strip: {src}",
        );
    }
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

/// `src` without the lines whose first non-space characters are `//`.
/// That is the whole rule: it cannot be fooled by a `/*` inside a
/// string or a comment, because it never looks for one.
fn comment_stripped(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
