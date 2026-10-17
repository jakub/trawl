// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What trawld's queries may use, against the bundled `DuckDB` (ADR-0056):
//! the query database spills, snapshots and stages exports only inside
//! the query scratch directory.
//!
//! A scenario that needs its own process environment runs in a CHILD
//! PROCESS: this test binary re-executes itself with the ignored
//! [`query_resources_child`] test selected, the scenario named in
//! [`CHILD_SCENARIO`] and a work directory the parent owns in
//! [`CHILD_WORK`]. The child boots a server under that directory, drives
//! it, and panics on a failed assertion; the parent asserts it exited
//! cleanly.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use trawl_server::bus::IngestBatch;
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::seam::Seam;

/// Set only in the child: which scenario to run.
const CHILD_SCENARIO: &str = "TRAWL_QUERY_RESOURCES_SCENARIO";

/// Set only in the child: the directory the parent made for it. The
/// child's server keeps its WAL, data root and query scratch here.
const CHILD_WORK: &str = "TRAWL_QUERY_RESOURCES_WORK";

/// The subprocess entry point. Without [`CHILD_SCENARIO`] it does nothing,
/// so a run with `--run-ignored` passes it untouched.
#[test]
#[ignore = "entry point for the child process; the tests below run it"]
fn query_resources_child() {
    let Some(scenario) = std::env::var_os(CHILD_SCENARIO) else {
        return;
    };
    let work = PathBuf::from(std::env::var_os(CHILD_WORK).expect("the parent names a work dir"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    match scenario.to_str().expect("a UTF-8 scenario") {
        "tmpdir" => runtime.block_on(tmpdir_child(&work)),
        other => panic!("unknown scenario {other}"),
    }
}

/// Run `scenario` in a child process over a fresh work directory, with
/// `env` added to this process's environment, and fail with the child's
/// output if it did not pass.
fn run_child(scenario: &str, work: &Path, env: &[(&str, &Path)]) {
    let mut command = Command::new(std::env::current_exe().expect("locate this test binary"));
    command
        .args([
            "query_resources_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD_SCENARIO, scenario)
        .env(CHILD_WORK, work);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().expect("run the child");
    assert!(
        output.status.success(),
        "the {scenario} child failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Every entry directly in `dir`, by name.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A hot-buffer batch of `n` events for `service`.
fn hot_batch(id: &str, service: &str, n: usize) -> Arc<IngestBatch> {
    let events = (0..n)
        .map(|i| {
            let mut event = serde_json::Map::new();
            event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
            event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
            event.insert("service".into(), service.into());
            event.insert("message".into(), format!("hot event {i}").into());
            event
        })
        .collect();
    Arc::new(IngestBatch {
        batch_id: format!("prod/{service}_{id}").into(),
        service: service.into(),
        byte_size: 64 * n,
        events,
    })
}

// -- AC9: nothing large in TMPDIR ---------------------------------------------

/// Hot-buffer snapshots and Parquet export staging files are made inside
/// the query scratch directory, and nothing trawld writes during a query,
/// an export or a snapshot lands in `TMPDIR` (ADR-0056).
///
/// The child runs with `TMPDIR` pointed at a private empty directory, and
/// its server keeps everything else under its work directory. The parent
/// checks the directory is still empty once the child is gone.
#[test]
fn nothing_large_in_tmpdir() {
    let work = tempfile::tempdir().expect("work dir");
    let tmpdir = tempfile::tempdir().expect("private TMPDIR");
    run_child("tmpdir", work.path(), &[("TMPDIR", tmpdir.path())]);
    assert_eq!(
        entries(tmpdir.path()),
        Vec::<String>::new(),
        "nothing trawld wrote landed in TMPDIR"
    );
}

/// The child half of [`nothing_large_in_tmpdir`].
async fn tmpdir_child(work: &Path) {
    let tmpdir = std::env::temp_dir();
    assert_ne!(tmpdir, work, "the parent passed two directories");
    let empty = || {
        assert_eq!(entries(&tmpdir), Vec::<String>::new(), "TMPDIR stays empty");
    };
    empty();

    let server = common::setup_in_dir(work, RateLimitConfig::default()).await;
    let scratch = server.state.query.pool.scratch().path().to_owned();
    assert!(
        scratch.starts_with(work),
        "the default scratch is the data root's sibling: {}",
        scratch.display()
    );
    assert!(
        entries(&scratch)
            .iter()
            .any(|name| name.starts_with("trawl-spill-")),
        "the query database spills into scratch: {:?}",
        entries(&scratch)
    );
    empty();

    let hot = server
        .state
        .query
        .hot_buffer
        .clone()
        .expect("an ingest node has a hot buffer");
    hot.insert_for_test(hot_batch("1", "fresh", 20));
    let client = common::harness_client_builder().build().unwrap();

    // A query over the hot buffer, which builds a snapshot.
    let response = client
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": "service=fresh | stats count()" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    empty();

    // An export, held where it still holds its snapshot and its staging
    // file, on a new generation so it builds its own snapshot.
    hot.insert_for_test(hot_batch("2", "fresh", 20));
    let seams = server.state.query.pool.seams();
    let finished = seams.hold(Seam::Finished);
    let export = tokio::spawn({
        let (client, url, token) = (
            client.clone(),
            server.url.clone(),
            server.analyst_token.clone(),
        );
        async move {
            client
                .post(format!("{url}/api/v1/export?format=parquet"))
                .bearer_auth(token)
                .json(&serde_json::json!({ "query": "service=fresh" }))
                .send()
                .await
                .unwrap()
        }
    });
    let start = std::time::Instant::now();
    while finished.arrivals() == 0 {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "the export never finished its work"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let held = entries(&scratch);
    assert!(
        held.iter()
            .any(|name| name.starts_with("trawl-snapshot-") && name.ends_with(".ndjson")),
        "the snapshot is in scratch: {held:?}"
    );
    assert!(
        held.iter()
            .any(|name| name.starts_with("trawl-export-") && name.ends_with(".parquet")),
        "the export staging file is in scratch: {held:?}"
    );
    empty();
    finished.release();
    let response = export.await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.bytes().await.unwrap();
    assert!(bytes.starts_with(b"PAR1"), "a Parquet export");
    empty();
    assert!(
        !entries(&scratch)
            .iter()
            .any(|name| name.starts_with("trawl-export-")),
        "the staging file is gone once the export answered"
    );
}

// -- guard ----------------------------------------------------------------------

/// No trawld production path opens a `DuckDB` database outside the query
/// database or makes a temp file in `TMPDIR` (ADR-0056).
///
/// `Executor::new` stays for embedded CLI mode and engine tests, and
/// spills into `TMPDIR` with no budgets, so trawld's sources may name it
/// only in test code. The scan drops `#[cfg(test)]` and test-support
/// items, then looks for the constructors.
#[test]
fn no_trawld_path_opens_an_unbudgeted_database_or_writes_tmpdir() {
    const FORBIDDEN: [&str; 6] = [
        "Executor::new(",
        "NamedTempFile::new(",
        "tempfile::tempfile(",
        "tempfile::tempdir(",
        ".tempfile()",
        "std::env::temp_dir(",
    ];
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files: Vec<(PathBuf, String)> = rust_files(&src)
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            (path, text)
        })
        .collect();
    // Files of modules declared under a test gate (`#[cfg(test)] mod x;`)
    // are test code whole.
    let gated: Vec<PathBuf> = files
        .iter()
        .flat_map(|(path, text)| {
            let dir = match path.file_stem().and_then(|stem| stem.to_str()) {
                Some("lib" | "main" | "mod") => path.parent().unwrap().to_owned(),
                _ => path.with_extension(""),
            };
            production_lines(text)
                .1
                .into_iter()
                .map(move |module| dir.join(module))
        })
        .collect();
    let mut offenders = Vec::new();
    for (path, text) in &files {
        if gated
            .iter()
            .any(|module| path.starts_with(module) || *path == module.with_extension("rs"))
        {
            continue;
        }
        for (line_no, line) in production_lines(text).0 {
            for needle in FORBIDDEN {
                if names(line, needle) && !line.trim_start().starts_with("//") {
                    offenders.push(format!("{}:{line_no}: {}", path.display(), line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "trawld production code opens an unbudgeted database or writes TMPDIR:\n{}",
        offenders.join("\n")
    );
}

/// Whether `line` holds `needle` as a whole path, not as the tail of a
/// longer name (`TokioExecutor::new(` is not `Executor::new(`).
fn names(line: &str, needle: &str) -> bool {
    line.match_indices(needle).any(|(at, _)| {
        needle.starts_with('.')
            || line[..at]
                .chars()
                .next_back()
                .is_none_or(|before| !(before.is_alphanumeric() || before == '_'))
    })
}

/// Every `.rs` file under `dir`.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

/// The lines of `text` outside items gated on `test` (`#[cfg(test)]`,
/// `#[cfg(any(test, ...))]`), numbered from 1.
///
/// A gated item runs from the line after its attributes to the end of the
/// item: the brace that closes its first block for a `mod`, `fn`, `impl`,
/// `struct`, `enum` or `trait`, otherwise the first `;` or `,` outside any
/// bracket, or the brace that closes a block opened outside any bracket.
/// String, character and comment text is skipped while matching, so a
/// brace inside a fixture string does not end an item.
///
/// Also returns the names of the modules declared out of line under such a
/// gate (`#[cfg(test)] mod x;`), whose files are test code whole.
fn production_lines(text: &str) -> (Vec<(usize, &str)>, Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .collect();
    let mut kept = Vec::new();
    let mut gated_modules = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if !(trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[cfg(any(test")) {
            kept.push((i + 1, lines[i]));
            i += 1;
            continue;
        }
        i += 1;
        while i < lines.len() && lines[i].trim_start().starts_with("#[") {
            i += 1;
        }
        if i == lines.len() {
            break;
        }
        if let Some(module) = lines[i]
            .trim()
            .strip_prefix("mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        {
            gated_modules.push(module.to_owned());
        }
        let end = item_end(text, starts[i]);
        // Skip every line up to and including the one holding the end.
        while i < lines.len() && starts[i] <= end {
            i += 1;
        }
    }
    (kept, gated_modules)
}

/// The byte offset of the character that ends the item starting at
/// `start` (see [`production_lines`]).
fn item_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let head = text[start..].trim_start();
    let head = head
        .strip_prefix("pub(crate) ")
        .or_else(|| head.strip_prefix("pub "))
        .unwrap_or(head);
    let block_item = [
        "mod ",
        "fn ",
        "async fn ",
        "impl",
        "struct ",
        "enum ",
        "trait ",
    ]
    .iter()
    .any(|keyword| head.starts_with(keyword));
    let mut depth = 0_usize;
    let mut at = start;
    while at < bytes.len() {
        match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at + 1 < bytes.len() && !(bytes[at] == b'*' && bytes[at + 1] == b'/') {
                    at += 1;
                }
                at += 1;
            }
            b'r' if matches!(bytes.get(at + 1), Some(b'"' | b'#'))
                && (at == 0
                    || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_')) =>
            {
                let hashes = bytes[at + 1..].iter().take_while(|&&b| b == b'#').count();
                let close: String = std::iter::once('"')
                    .chain("#".repeat(hashes).chars())
                    .collect();
                let body = at + 2 + hashes;
                at = body + text[body..].find(&close).expect("a closed raw string") + close.len()
                    - 1;
            }
            b'"' => {
                at += 1;
                while bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
            }
            // A character literal; a lifetime has no closing quote.
            b'\'' if bytes.get(at + 2) == Some(&b'\'') => at += 2,
            b'\'' if bytes.get(at + 1) == Some(&b'\\') => {
                at += 2;
                while bytes[at] != b'\'' {
                    at += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' => depth -= 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return at;
                }
            }
            b';' | b',' if depth == 0 && !block_item => return at,
            b';' if depth == 0 => return at,
            _ => {}
        }
        at += 1;
    }
    bytes.len()
}
