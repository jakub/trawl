// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld execs nothing after the crash-dump monitor (ADR-0023 ruling 4).
//!
//! Normal startup calls `trawl_crashdump::init()` before config reads. It re-execs
//! this binary as the minidump monitor, then seals the daemon: it clears
//! `CAP_SYS_PTRACE` from its own effective and permitted sets and sets
//! `no_new_privs`. Both halves matter here. The cleared permitted set is what
//! a later `execve` would inherit, and `no_new_privs` is what makes the kernel
//! ignore the binary's own `cap_sys_ptrace+p` file capability on that exec.
//!
//! So an exec after the seal is not just pointless, it is a different process
//! than the one the seal describes: it runs with `no_new_privs` inherited,
//! every setuid and file-capability transition silently dropped, and no way to
//! notice. The ruling turns that into a rule on the daemon rather than a
//! description of it, and this test is the enforcement. `trawld` is a log
//! daemon and had no child processes when the ruling landed, so the cost of
//! the rule is zero today and the whole point is to keep it that way.
//!
//! If a future feature genuinely needs to run a program, it does not silence
//! this test. It changes the ruling first: either the daemon stops sealing (and
//! keeps a capability it does not need), or the work moves to a process spawned
//! before `init()`, or the ADR gains a documented exception with the exec's own
//! capability story written down.

use std::path::{Path, PathBuf};

/// The calls that start a program, as they are spelled in Rust.
///
/// Two needles:
///
/// - `Command::new(` covers `std::process::Command` and `tokio::process::Command`
///   alike, since both are constructed that way and an import decides which one
///   a bare `Command` is. It carries its `(` so `Command::new` mentioned as a
///   path (a doc link, a `use`) is not a match, and so `Commander::new(` does
///   not match either.
/// - `.exec(` is the `CommandExt::exec` that replaces this image in place, the
///   one exec that leaves no child to notice. The leading dot keeps
///   `execute(`, `exec_query(` and every sqlx method that starts with those
///   four letters out, because the needle ends at its own paren.
///
/// Not covered, and deliberately: a raw `libc::execve`, a `fork`, a crate that
/// spawns on the daemon's behalf. Nothing here parses Rust and no needle list
/// is a sandbox. This catches the ordinary way the rule would be broken, which
/// is someone reaching for `Command` in a handler without knowing the seal
/// exists.
const EXEC_CALLS: &[&str] = &["Command::new(", ".exec("];

/// Printed by every failure: the rule, and where it comes from.
const REMEDY: &str = "trawld may exec nothing after the crash-dump monitor: \
                      init() has already cleared CAP_SYS_PTRACE from the permitted set \
                      and set no_new_privs, so the exec'd program runs under a security \
                      state nothing described (ADR-0023 ruling 4, #21)";

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, recursively.
///
/// A symlink of any kind is refused rather than followed, the same rule
/// `trawl-core`'s clock-anchor walk uses: a symlinked directory can point the
/// scan out of the tree it is auditing or back into it, and either way the set
/// of files scanned stops being "this crate's sources".
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
             must audit THESE sources",
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
/// Comment lines (`//`, `///`, `//!` alike) are dropped first, because the
/// daemon has to be able to explain this rule in prose beside the code that
/// obeys it. A needle inside a trailing comment on a code line still counts,
/// which errs toward failing loud, and so does a needle inside a string
/// literal: nothing here tells a literal from a path, and a loud failure on
/// `"Command::new("` is the right side to be wrong on.
fn call_lines(source: &str, needles: &[&str]) -> Vec<usize> {
    let mut hits = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        if needles.iter().any(|needle| line.contains(needle)) {
            hits.push(index + 1);
        }
    }
    hits
}

#[test]
fn the_daemon_execs_nothing() {
    let root = src_root();
    let sources = rust_sources(&root);
    assert!(
        sources.len() > 10,
        "source walk infrastructure failure: only {} files under {} — the scan \
         found nothing to audit",
        sources.len(),
        root.display()
    );

    let mut violations = Vec::new();
    for path in &sources {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let relative = path.strip_prefix(&root).unwrap_or(path);
        for line in call_lines(&source, EXEC_CALLS) {
            violations.push(format!("  src/{}:{line}", relative.display()));
        }
    }

    assert!(violations.is_empty(), "{REMEDY}\n{}", violations.join("\n"));
}

#[test]
fn the_scan_sees_the_calls_it_is_looking_for() {
    // The scan is the whole defence, so prove it is not vacuously green.
    let source = "\
let mut cmd = Command::new(\"sh\");
// Command::new( in a comment is prose, not a call
let e = cmd.exec();
let rows = sqlx::query(sql).execute(&pool).await?;
let path = exec_dir(); // execute( and exec_dir( are not the needle
";
    assert_eq!(call_lines(source, EXEC_CALLS), vec![1, 3]);
}
