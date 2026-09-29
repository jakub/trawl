// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor` calls nothing that writes, locks, migrates, or boots
//! (ADR-0047, #269 D20).
//!
//! A doctor that repairs what it inspects has changed the installation it
//! was asked to describe. The doctor's code lives in `src/doctor/`, and this
//! test reads every file there for the calls that would do that: the boot's
//! admission and recovery steps, the pool and keystore owners, the
//! advisory-lock and migration calls, TLS generation, and the filesystem
//! calls that create, write, rename, remove, or change modes.
//!
//! This is a tripwire, not the proof. The proof that nothing changes is
//! `doctor_writes_nothing` in `tests/trawld_doctor/proof.rs`, which runs
//! the doctor against a read-only data tree and a `SELECT`-only role and
//! compares snapshots. This scan catches the ordinary way the rule would be
//! broken, a helper from the boot path reused in a check, at the line that
//! does it.
//!
//! The same limits as `no_exec_after_seal.rs` hold: comment lines are
//! skipped, a needle in a string literal or a trailing comment counts, and a
//! call reached through another module is not seen. Each file's trailing
//! `#[cfg(test)] mod tests` is not scanned: tests plant files with the very
//! calls the doctor may not make.

use std::path::{Path, PathBuf};

/// What the doctor's code may not call, as spelled in Rust.
const FORBIDDEN: &[&str] = &[
    // Locks and migrations (ADR-0047 amendment: observed, never taken).
    "advisory_lock(",
    "advisory_unlock",
    "try_advisory",
    "migrate(",
    // The boot's owners and admission steps, which lock, migrate, create,
    // or repair (the issue's list of code the doctor must never call).
    "StorageState",
    "KeyStore",
    "AppState",
    "PgPool",
    "PoolOptions",
    "ensure_current_epoch",
    "ensure_conformance",
    "prepare_data_root",
    "recover(",
    "recover_",
    "recover::",
    "build_server_config",
    "load_or_generate",
    "publish_marker",
    "publish_cert",
    "write_marker",
    "init_tracing",
    "FileLog",
    "trawl_crashdump::init",
    // Filesystem writes.
    "create_dir",
    "fs::write",
    "remove_file",
    "remove_dir",
    "rename(",
    "OpenOptions",
    "set_permissions",
    "hard_link",
    "fs::symlink(",
    "fsync",
    "sync_all",
];

fn doctor_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/doctor")
}

/// Every `.rs` file under `dir`, recursively, refusing symlinks as
/// `no_exec_after_seal.rs` does.
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
            "source walk infrastructure failure: {} is a symlink",
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

/// `source` without its trailing `#[cfg(test)] mod tests { .. }`.
///
/// The test module must be the file's last item: every line after its
/// opening line is blank, indented, or the one closing `}` that ends the
/// file. Anything else fails, so production code placed after a test module
/// cannot hide from the scan.
fn production_part(source: &str) -> Result<&str, String> {
    let lines: Vec<&str> = source.lines().collect();
    let Some(at) = lines
        .windows(2)
        .position(|pair| pair[0] == "#[cfg(test)]" && pair[1].starts_with("mod tests"))
    else {
        return Ok(source);
    };
    let rest = &lines[at + 2..];
    let last = rest.iter().rposition(|line| !line.trim().is_empty());
    let Some(last) = last else {
        return Err("the test module never closes".to_owned());
    };
    if rest[last] != "}" {
        return Err("the test module is not the file's last item".to_owned());
    }
    if let Some(stray) = rest[..last]
        .iter()
        .find(|line| !line.is_empty() && !line.starts_with(char::is_whitespace))
    {
        return Err(format!(
            "an unindented line follows the test module's start: {stray:?}"
        ));
    }
    let offset: usize = lines[..at].iter().map(|line| line.len() + 1).sum();
    Ok(&source[..offset.min(source.len())])
}

/// The 1-based lines of `source` that name one of `needles`, with the
/// needle. Comment lines are skipped.
fn hits(source: &str, needles: &[&'static str]) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for needle in needles {
            if line.contains(needle) {
                found.push((index + 1, *needle));
            }
        }
    }
    found
}

#[test]
fn the_doctor_touches_nothing() {
    let root = doctor_root();
    let sources = rust_sources(&root);
    for name in [
        "mod.rs",
        "output.rs",
        "fsread.rs",
        "db.rs",
        "storage.rs",
        "listener.rs",
    ] {
        assert!(
            sources.contains(&root.join(name)),
            "source walk infrastructure failure: src/doctor/{name} is not scanned"
        );
    }

    let mut violations = Vec::new();
    for path in &sources {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let relative = path.strip_prefix(&root).unwrap_or(path).display();
        let production =
            production_part(&source).unwrap_or_else(|why| panic!("src/doctor/{relative}: {why}"));
        for (line, needle) in hits(production, FORBIDDEN) {
            violations.push(format!("  src/doctor/{relative}:{line}: {needle}"));
        }
    }
    assert!(
        violations.is_empty(),
        "trawld --doctor may not write, lock, migrate, or run boot steps \
         (ADR-0047); these lines name a call that does:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_scan_sees_the_calls_it_is_looking_for() {
    let source = "\
fn check() {
    // std::fs::write( in a comment is prose
    std::fs::create_dir_all(&root)?;
    let lock = sqlx::query(\"SELECT pg_try_advisory_lock($1)\");
    let path = root.join(\"recovery\"); // recovery is a word, not a call
}

#[cfg(test)]
mod tests {
    fn plant() {
        std::fs::write(\"EPOCH\", \"3\").unwrap();
    }
}
";
    let production = production_part(source).unwrap();
    assert_eq!(
        hits(production, FORBIDDEN),
        [
            (3, "create_dir"),
            (4, "advisory_lock("),
            (4, "try_advisory")
        ]
    );

    // Production code after a test module does not hide from the scan.
    let hidden = "#[cfg(test)]\nmod tests {\n}\nfn later() { std::fs::write(p, b) }\n";
    assert!(production_part(hidden).is_err());
    let hidden = "#[cfg(test)]\nmod tests {\n    fn t() {}\nfn later() {}\n}\n";
    assert!(production_part(hidden).is_err());
}
