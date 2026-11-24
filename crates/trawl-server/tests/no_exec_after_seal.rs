// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld execs nothing after the crash-dump monitor (ADR-0023 ruling 4).

use std::path::{Path, PathBuf};

/// `Command::new(` covers `std` and `tokio` `Command` alike. It carries its `(`
/// so `Command::new` mentioned as a path (a doc link, a `use`) is not a match.
/// `.exec(` is `CommandExt::exec`; because the needle ends at its own paren,
/// `execute(` and `exec_query(` are out. Not covered: a raw `libc::execve`, a
/// `fork`, a crate that spawns on the daemon's behalf.
const EXEC_CALLS: &[&str] = &["Command::new(", ".exec("];

const REMEDY: &str = "trawld may exec nothing after the crash-dump monitor: \
                      init() has already cleared CAP_SYS_PTRACE from the permitted set \
                      and set no_new_privs, so the exec'd program runs under a security \
                      state nothing described (ADR-0023 ruling 4, #21)";

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

/// Comment lines are dropped so the daemon can explain this rule in prose. A
/// needle inside a trailing comment or a string literal still counts, which
/// errs toward failing loud.
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

    // `trawld --doctor` runs in the sealed process too, and its code lives
    // in its own directory: the walk must reach every file of it.
    let doctor = root.join("doctor");
    let doctor_files: Vec<&PathBuf> = sources.iter().filter(|p| p.starts_with(&doctor)).collect();
    for name in [
        "mod.rs",
        "output.rs",
        "fsread.rs",
        "db.rs",
        "storage.rs",
        "listener.rs",
    ] {
        assert!(
            doctor_files.contains(&&doctor.join(name)),
            "source walk infrastructure failure: src/doctor/{name} is not scanned"
        );
    }
    let on_disk = std::fs::read_dir(&doctor)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", doctor.display()))
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.path().extension().is_some_and(|ext| ext == "rs"))
        })
        .count();
    assert_eq!(
        doctor_files.len(),
        on_disk,
        "source walk infrastructure failure: not every src/doctor/*.rs is scanned"
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
    let source = "\
let mut cmd = Command::new(\"sh\");
// Command::new( in a comment is prose, not a call
let e = cmd.exec();
let rows = sqlx::query(sql).execute(&pool).await?;
let path = exec_dir(); // execute( and exec_dir( are not the needle
";
    assert_eq!(call_lines(source, EXEC_CALLS), vec![1, 3]);
}
