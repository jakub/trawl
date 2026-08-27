// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `cargo xtask pg-admission-guard` — prove that every pg-touching test in
//! the workspace runs inside the `postgres` nextest group.
//!
//! The group in `.config/nextest.toml` is what keeps the workspace from
//! opening more postgres connections than the server allows (ADR-0021
//! ruling 4). It is a hand-written list of binary names, so it rots the
//! moment somebody adds a test file. This guard recomputes the pg-touching
//! set from the sources and asks nextest whether those tests land in the
//! group.
//!
//! Two independent sides, on purpose:
//!
//! 1. DERIVATION, from `cargo metadata` plus a line scan of each target's
//!    sources. A target is pg-touching if it carries an `#[sqlx::test]`
//!    attribute, if it pulls in a `common` module that opens postgres pools
//!    (`fixture_pool`), or if test code names one of the connection-opening
//!    APIs itself (`KeyStore::connect`, `PgPool`, and friends). That third
//!    class is what catches a plain `#[tokio::test]` that dials postgres by
//!    hand, carrying neither marker. It reads a whole integration test
//!    file, but only the `#[cfg(test)]` regions of a `src/` tree, where the
//!    surrounding production code is where those APIs are defined.
//! 2. MEMBERSHIP, from nextest itself: `cargo nextest list -E
//!    'group(postgres)'`. cargo-nextest 0.9.143 supports `group()` as a
//!    filterset predicate, so the authority on "is this test in the group"
//!    is the same engine that will enforce it at run time. Re-parsing the
//!    override filters out of the TOML was the alternative, and it would
//!    have re-implemented nextest's filterset semantics (`kind(lib)`,
//!    `test(/regex/)`, precedence) in the guard — a second implementation
//!    to disagree with the first.
//!
//! The match is per TEST NAME only when EVERY finding for the target names
//! tests (an `#[sqlx::test]` attribute names the function beneath it). Any
//! target-level finding — the fixture module, a connection-opening API —
//! dominates: the whole binary must be in the group, and the check is that
//! every non-ignored testcase nextest listed for it carries
//! `filter-match.status == "matches"`. Dominance matters because a target
//! can carry both kinds of evidence (`trawl-server::auth_pg` has named
//! `#[sqlx::test]` cases AND the fixture module), and checking only the
//! named ones would let a narrowed group filter admit two tests out of
//! twenty-six while the other twenty-four boot a whole server unbounded.
//!
//! Name granularity survives for the targets that only ever name tests: it
//! is what lets the group filter narrow trawl-server's library target to
//! `from_saved::` and `scheduler::` instead of dragging ~600 pure unit
//! tests under an 8-thread cap.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;

/// The nextest test group every pg-touching test must land in.
const GROUP: &str = "postgres";

/// Marker of a postgres fixture module: a `common` module that defines this
/// is the shared pg harness, not just any file called `common/mod.rs`
/// (fleet-ui and trawl-engine both have a non-pg one).
const FIXTURE_MARKER: &str = "fn fixture_pool";

/// Connection-opening APIs. A test naming one of these opens a postgres
/// connection of its own, whatever attribute sits above it — a plain
/// `#[tokio::test]` calling `KeyStore::connect` is exactly the escape the
/// two marker-based classes miss.
///
/// Where they are scanned differs by tree. An integration test file is all
/// test code, so the whole file counts. A package's `src/` tree is
/// production code, where `PgPool` appears in every store module and
/// `StorageState::connect` is DEFINED, so only `#[cfg(test)]` regions count
/// there (see [`cfg_test_connection_site`]).
const CONNECTION_APIS: &[&str] = &[
    "PgConnection::connect",
    "KeyStore::connect",
    "StorageState::connect",
    "connect_lazy",
    "PgPool",
    "fixture_pool",
];

/// Why one target is pg-touching, in a form a failure message can print.
struct Evidence {
    /// File and line the finding came from.
    site: String,
    /// Test function names the evidence names, empty when it names none.
    tests: Vec<String>,
}

/// One test target and the evidence gathered for it.
struct Derived {
    binary_id: String,
    evidence: Vec<Evidence>,
}

impl Derived {
    /// Does any finding describe the TARGET rather than named tests? Such a
    /// finding dominates: the whole binary has to be in the group.
    fn has_target_evidence(&self) -> bool {
        self.evidence.iter().any(|e| e.tests.is_empty())
    }

    /// Every test name the evidence pins, across all findings.
    fn named_tests(&self) -> BTreeSet<&str> {
        self.evidence
            .iter()
            .flat_map(|e| e.tests.iter().map(String::as_str))
            .collect()
    }

    fn sites(&self) -> String {
        self.evidence
            .iter()
            .map(|e| e.site.clone())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Run the guard. `nextest_args` are appended to the `cargo nextest list`
/// invocation, so CI can list under the same feature selection it tests
/// with (a feature that compiles a pg test file out would otherwise make
/// the guard disagree with the suite it guards).
pub fn run(root: &Path, nextest_args: &[String]) -> ExitCode {
    let derived = match derive(root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("xtask: pg-admission-guard: {e}");
            return ExitCode::FAILURE;
        }
    };
    if derived.is_empty() {
        eprintln!(
            "xtask: pg-admission-guard: derived no pg-touching targets at all. \
             That is never right — the scan or `cargo metadata` is broken."
        );
        return ExitCode::FAILURE;
    }

    let grouped = match group_membership(root, nextest_args) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("xtask: pg-admission-guard: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut failures = Vec::new();
    for target in &derived {
        if let Some(failure) = check(target, grouped.get(&target.binary_id)) {
            failures.push(failure);
        }
    }

    if failures.is_empty() {
        println!(
            "pg-admission-guard: {} pg-touching target(s), all inside the `{GROUP}` group",
            derived.len()
        );
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "pg-admission-guard FAILED: postgres connections are unbounded for these tests.\n\
         Add them to the `{GROUP}` group override in .config/nextest.toml.\n{}",
        failures.join("\n")
    );
    ExitCode::FAILURE
}

/// One binary as nextest listed it: every non-ignored testcase, and the
/// subset the `group(postgres)` filterset matched.
#[derive(Default)]
struct Suite {
    all: BTreeSet<String>,
    matched: BTreeSet<String>,
}

/// How many escaped test names a failure message prints before it stops.
const NAMES_SHOWN: usize = 5;

/// Check one derived target against its nextest listing. `None` is a pass.
fn check(target: &Derived, suite: Option<&Suite>) -> Option<String> {
    if target.has_target_evidence() {
        // The whole binary drives postgres, so the whole binary belongs in
        // the group. Nothing here consults the named tests: target-level
        // evidence dominates.
        let Some(suite) = suite.filter(|s| !s.all.is_empty()) else {
            return Some(format!(
                "  {} is not in the `{GROUP}` group\n    evidence: {}",
                target.binary_id,
                target.sites()
            ));
        };
        let outside: Vec<&str> = suite
            .all
            .iter()
            .filter(|name| !suite.matched.contains(*name))
            .map(String::as_str)
            .collect();
        if outside.is_empty() {
            return None;
        }
        return Some(format!(
            "  {} is pg-touching as a whole binary, but {} of its {} test(s) resolve outside the `{GROUP}` group: {}\n    evidence: {}",
            target.binary_id,
            outside.len(),
            suite.all.len(),
            name_list(&outside),
            target.sites()
        ));
    }

    let default = Suite::default();
    let matched = &suite.unwrap_or(&default).matched;
    let missing: Vec<&str> = target
        .named_tests()
        .into_iter()
        .filter(|name| !matched.iter().any(|test| test_is(test, name)))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "  {} runs {} pg-touching case(s) outside the `{GROUP}` group: {}\n    evidence: {}",
        target.binary_id,
        missing.len(),
        name_list(&missing),
        target.sites()
    ))
}

/// Test names for a failure message, truncated so a whole 600-test binary
/// does not bury the sentence that explains it.
fn name_list(names: &[&str]) -> String {
    if names.len() <= NAMES_SHOWN {
        return names.join(", ");
    }
    format!(
        "{}, and {} more",
        names[..NAMES_SHOWN].join(", "),
        names.len() - NAMES_SHOWN
    )
}

/// Does a nextest test id name this function? Ids carry the module path
/// (`from_saved::tests::name`), so match the last segment.
fn test_is(test_id: &str, fn_name: &str) -> bool {
    test_id == fn_name || test_id.rsplit("::").next() == Some(fn_name)
}

/// Derive the pg-touching targets from `cargo metadata` and the sources.
fn derive(root: &Path) -> Result<Vec<Derived>, String> {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to run `cargo metadata`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let meta: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("`cargo metadata` output is not JSON: {e}"))?;

    let mut derived = Vec::new();
    let packages = meta["packages"].as_array().map_or(&[][..], Vec::as_slice);
    for package in packages {
        let package_name = package["name"].as_str().unwrap_or_default();
        let targets = package["targets"].as_array().map_or(&[][..], Vec::as_slice);

        // The unit-test half: a package's `src/` tree is ONE module tree
        // shared by its lib and its bins, so it is scanned once and charged
        // to the binary nextest actually runs those tests in — the lib
        // target when there is one, else the package's first bin. Charging
        // it per target would blame `trawl-server::bin/trawld` for tests
        // that only ever run in `trawl-server`'s lib binary.
        let mut unit_id = None;
        let mut unit_src = None;
        for target in targets {
            if !target["test"].as_bool().unwrap_or(false) {
                continue;
            }
            let kinds = target_kinds(target);
            let name = target["name"].as_str().unwrap_or_default();
            let src = PathBuf::from(target["src_path"].as_str().unwrap_or_default());
            if kinds.contains(&"test") {
                let sources = integration_sources(&src);
                let evidence = scan(&sources, root, Scope::Integration);
                if !evidence.is_empty() {
                    derived.push(Derived {
                        binary_id: format!("{package_name}::{name}"),
                        evidence,
                    });
                }
            } else if kinds.contains(&"lib") {
                unit_id = Some(package_name.to_string());
                unit_src = Some(src);
            } else if unit_id.is_none() {
                // nextest ids a bin's unit tests `pkg::bin/name`.
                unit_id = Some(format!("{package_name}::bin/{name}"));
                unit_src = Some(src);
            }
        }
        if let (Some(binary_id), Some(src)) = (unit_id, unit_src) {
            let mut sources = Vec::new();
            if let Some(dir) = src.parent() {
                collect_rs(dir, &mut sources);
            }
            sources.sort();
            sources.dedup();
            let evidence = scan(&sources, root, Scope::UnitTree);
            if !evidence.is_empty() {
                derived.push(Derived {
                    binary_id,
                    evidence,
                });
            }
        }
    }
    derived.sort_by(|a, b| a.binary_id.cmp(&b.binary_id));
    Ok(derived)
}

fn target_kinds(target: &Value) -> Vec<&str> {
    target["kind"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

/// The source files one integration test target's evidence may live in:
/// the test file itself plus, when it declares `mod common;`, that module.
fn integration_sources(src: &Path) -> Vec<PathBuf> {
    let mut sources = vec![src.to_path_buf()];
    if let Some(dir) = src.parent()
        && fs::read_to_string(src).is_ok_and(|text| declares_common(&text))
    {
        let common = dir.join("common").join("mod.rs");
        if common.is_file() {
            sources.push(common);
        }
    }
    sources
}

fn declares_common(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .any(|line| line == "mod common;" || line == "pub mod common;")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Which tree the sources come from. The connection-API class applies to
/// integration test files only (see [`CONNECTION_APIS`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Integration,
    UnitTree,
}

/// Scan a target's sources for pg evidence.
fn scan(sources: &[PathBuf], root: &Path, scope: Scope) -> Vec<Evidence> {
    let mut evidence = Vec::new();
    for source in sources {
        let Ok(text) = fs::read_to_string(source) else {
            continue;
        };
        let shown = source.strip_prefix(root).unwrap_or(source).display();

        let tests = sqlx_test_names(&text);
        if let Some((line, _)) = tests.first() {
            evidence.push(Evidence {
                site: format!("{shown}:{line} #[sqlx::test]"),
                tests: tests.iter().map(|(_, name)| name.clone()).collect(),
            });
        }

        // The fixture side: this file IS the pg harness (it defines the
        // pool constructor), and `integration_sources` only handed it over
        // because the target declares `mod common;`.
        if text.contains(FIXTURE_MARKER) && source.ends_with(Path::new("common/mod.rs")) {
            evidence.push(Evidence {
                site: format!("{shown} (postgres fixture module)"),
                tests: Vec::new(),
            });
        }

        // The raw-connection side: no attribute, no fixture, just a test
        // that dials postgres itself.
        match scope {
            Scope::Integration => {
                if let Some((line, api)) = connection_api_site(&text) {
                    evidence.push(Evidence {
                        site: format!("{shown}:{line} {api}"),
                        tests: Vec::new(),
                    });
                }
            }
            Scope::UnitTree => {
                if let Some((line, api, tests)) = cfg_test_connection_site(&text) {
                    evidence.push(Evidence {
                        site: format!("{shown}:{line} {api}"),
                        tests,
                    });
                }
            }
        }
    }
    evidence
}

/// The first line naming a connection-opening API, with the API it named.
///
/// Comments are cut first, for the same reason `sqlx_test_names` drops
/// them: the fixture module explains `fixture_pool` in prose, and prose is
/// not a connection. Cutting at `//` also truncates a string literal that
/// contains one (a URL), which can only lose a detection, never invent one.
fn connection_api_site(text: &str) -> Option<(usize, &'static str)> {
    text.lines().enumerate().find_map(|(index, raw)| {
        let code = raw.split("//").next().unwrap_or_default();
        CONNECTION_APIS
            .iter()
            .find(|api| code.contains(**api))
            .map(|api| (index + 1, *api))
    })
}

/// The first connection-opening API called inside a `#[cfg(test)]` region,
/// with the test functions that region declares.
///
/// A `src/` tree is production code: `StorageState::connect` is DEFINED
/// there, and every store module names `PgPool` in a signature, so scanning
/// the whole file would put ~600 connectionless unit tests in the group.
/// Scanning nothing is the other failure, and it is the one that lets a
/// plain `#[tokio::test]` under `src/` dial postgres outside the group. The
/// middle is the test code itself: everything under a `#[cfg(test)]`
/// attribute, which is where a unit test that opens a connection lives.
///
/// The names keep the finding NAME-granular, like the `#[sqlx::test]`
/// class: the connection lives in one test module, not in all 600 unit
/// tests of the binary that module compiles into. A region that opens a
/// connection and declares no test of its own returns no names, which makes
/// the finding target-level and drags the whole binary in — the
/// conservative answer for test-only code nothing in the region names.
///
/// Comments and literal contents are blanked first (see
/// [`blank_comments_and_literals`]) so a `}` inside a string cannot end the
/// region early and prose about `fixture_pool` is not a connection. Line
/// numbers are the file's real ones: blanking preserves every newline.
fn cfg_test_connection_site(text: &str) -> Option<(usize, &'static str, Vec<String>)> {
    let code = blank_comments_and_literals(text);
    let lines: Vec<&str> = code.lines().collect();
    cfg_test_spans(&lines).into_iter().find_map(|(start, end)| {
        let (line, api) = (start..=end).find_map(|index| {
            CONNECTION_APIS
                .iter()
                .find(|api| lines[index].contains(**api))
                .map(|api| (index + 1, *api))
        })?;
        Some((line, api, test_fn_names(&lines[start..=end])))
    })
}

/// Names of the test functions declared in one span: every `fn` under an
/// attribute whose last path segment is `test` (`#[test]`, `#[tokio::test]`,
/// `#[sqlx::test(migrations = false)]`). Helper `fn`s are left out — they
/// are not test ids, and asking nextest about one would fail the guard on a
/// name that can never be in the group.
fn test_fn_names(lines: &[&str]) -> Vec<String> {
    let mut names = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        if !is_test_attribute(raw.trim()) {
            continue;
        }
        if let Some(name) = lines[index + 1..]
            .iter()
            .map(|l| l.trim())
            .find_map(function_name)
        {
            names.push(name);
        }
    }
    names
}

/// `#[test]`, `#[tokio::test]`, `#[sqlx::test(...)]` — an attribute whose
/// final path segment is exactly `test`.
fn is_test_attribute(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("#[") else {
        return false;
    };
    let path: &str = rest.split(['(', ']']).next().unwrap_or_default().trim();
    path.rsplit("::").next() == Some("test")
}

/// Inclusive line ranges (0-based) of every `#[cfg(test)]` item.
///
/// The attribute is followed by the item it guards, and the item's body is
/// taken by brace matching from its first `{`. That covers `mod tests { }`
/// and a `#[cfg(test)]` on a single `fn` alike, since both are one braced
/// body. A brace-less item (`#[cfg(test)] mod tests;`, the out-of-line
/// module form) closes with `;` before any `{` and yields no span — the
/// separate file it names is scanned on its own, and its own tests carry
/// no `#[cfg(test)]` inside, which is the one residual here: a unit test
/// living in a file that is ITSELF `#[cfg(test)]`-gated from its parent is
/// invisible to this scan. Nothing in the workspace is written that way,
/// and the `#[sqlx::test]` and fixture classes still cover such a file.
fn cfg_test_spans(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !lines[index].contains("#[cfg(test)]") {
            index += 1;
            continue;
        }
        match braced_body_end(lines, index) {
            Some(end) => {
                spans.push((index, end));
                index = end + 1;
            }
            None => index += 1,
        }
    }
    spans
}

/// Line of the `}` closing the first `{` at or after `start`, or `None`
/// when the item has no braced body.
///
/// Three ways to have none, all of them real: a `;` ends the item before
/// any `{` (`#[cfg(test)] mod tests;`), the ENCLOSING block closes first
/// (an attribute written inside a function body), or the braces never
/// balance to the end of the file. All three yield no span rather than
/// swallowing the rest of the file.
fn braced_body_end(lines: &[&str], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, line) in lines[start..].iter().enumerate() {
        for c in line.chars() {
            match c {
                '{' => depth += 1,
                '}' | ';' if depth == 0 => return None,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(start + offset);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Replace comment and literal CONTENT with spaces, keeping every byte
/// position and newline. Handles `//`, `/* */` (nested, as rustc does),
/// `"..."`, `r"..."`/`r#"..."#` and `'a'`.
///
/// Blanking rather than deleting is what keeps the reported line numbers
/// honest, and it is what stops a `}` inside a string literal from closing
/// a `#[cfg(test)] mod` twenty lines early.
fn blank_comments_and_literals(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 0usize;
            while i < bytes.len() {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
        } else if b == b'r' && matches!(bytes.get(i + 1), Some(b'"' | b'#')) {
            let hashes = bytes[i + 1..].iter().take_while(|c| **c == b'#').count();
            if bytes.get(i + 1 + hashes) == Some(&b'"') {
                out.push_str(&" ".repeat(2 + hashes));
                i += 2 + hashes;
                let terminator: Vec<u8> = std::iter::once(b'"')
                    .chain(std::iter::repeat_n(b'#', hashes))
                    .collect();
                while i < bytes.len() && !bytes[i..].starts_with(&terminator) {
                    out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
                out.push_str(&" ".repeat(terminator.len().min(bytes.len() - i)));
                i += terminator.len();
            } else {
                out.push('r');
                i += 1;
            }
        } else if b == b'"' {
            out.push(' ');
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    out.push_str("  ");
                    i += 2;
                    continue;
                }
                out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                i += 1;
            }
            if i < bytes.len() {
                out.push(' ');
                i += 1;
            }
        } else if b == b'\'' && is_char_literal(&bytes[i..]) {
            let end = char_literal_len(&bytes[i..]);
            out.push_str(&" ".repeat(end));
            i += end;
        } else {
            out.push(text[i..].chars().next().unwrap_or(' '));
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

/// Is this `'` opening a char literal rather than a lifetime (`'a`)?
fn is_char_literal(rest: &[u8]) -> bool {
    char_literal_len(rest) > 0
}

/// Byte length of the char literal at `rest`, or 0 when it is not one.
fn char_literal_len(rest: &[u8]) -> usize {
    if rest.first() != Some(&b'\'') {
        return 0;
    }
    if rest.get(1) == Some(&b'\\') {
        // Escapes are at most `\u{10FFFF}`; find the closing quote.
        return rest[2..]
            .iter()
            .position(|c| *c == b'\'')
            .map_or(0, |p| p + 3);
    }
    // A single character then a quote — anything else is a lifetime.
    let len = std::str::from_utf8(rest)
        .ok()
        .and_then(|s| s[1..].chars().next())
        .map_or(0, char::len_utf8);
    if len > 0 && rest.get(1 + len) == Some(&b'\'') {
        1 + len + 1
    } else {
        0
    }
}

/// Line numbers and function names of every `#[sqlx::test]` attribute.
///
/// Comment lines are dropped first. A bare substring search would find the
/// PROSE: `crates/trawl-config/src/lib.rs` explains in a `//` comment that
/// `DATABASE_URL` is ceded to `#[sqlx::test]`, and trawl-server's store
/// modules say the same in doc comments. Dropping `//`-leading lines is
/// enough here because the attribute is only ever written at the start of
/// its own line; a `/* */` block hiding one would be a false positive, and
/// a false positive only over-groups.
///
/// An `#[ignore]`d test is skipped, and it has to be: `group_membership`
/// drops ignored cases from both the `all` and the `matched` side, so a
/// name collected here that nextest never lists reads as a test that
/// escaped the group and fails the guard over a test that never runs. The
/// two sides must agree on what counts as a test.
fn sqlx_test_names(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.starts_with("//") || !is_sqlx_test_attribute(line) {
            continue;
        }
        // Attributes above the `#[sqlx::test]`, contiguous with it.
        if lines[..index]
            .iter()
            .rev()
            .map(|l| l.trim())
            .take_while(|l| l.starts_with("#["))
            .any(is_ignore_attribute)
        {
            continue;
        }
        // The attribute names the test beneath it; skip any further
        // attributes stacked between the two, and drop the test if one of
        // them is `#[ignore]`.
        let below = lines[index + 1..].iter().map(|l| l.trim());
        let mut ignored = false;
        let mut name = None;
        for line in below {
            if let Some(found_name) = function_name(line) {
                name = Some(found_name);
                break;
            }
            ignored |= is_ignore_attribute(line);
        }
        if ignored {
            continue;
        }
        found.push((index + 1, name.unwrap_or_else(|| "<unnamed>".to_string())));
    }
    found
}

/// `#[ignore]` / `#[ignore = "reason"]`.
fn is_ignore_attribute(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("#[") else {
        return false;
    };
    let path = rest
        .split(['(', '=', ']'])
        .next()
        .unwrap_or_default()
        .trim();
    path == "ignore"
}

/// `#[sqlx::test]` / `#[sqlx::test(migrations = false)]`, tolerating the
/// whitespace rustfmt would never write but a human might.
fn is_sqlx_test_attribute(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("#[") else {
        return false;
    };
    rest.trim_start().starts_with("sqlx::test")
}

/// `async fn name(` / `fn name(` → `name`.
fn function_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("pub ").unwrap_or(line).trim_start();
    let rest = rest.strip_prefix("async ").unwrap_or(rest).trim_start();
    let rest = rest.strip_prefix("fn ")?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Ask nextest what it would run: binary id → its testcases and which of
/// them the group filterset matched.
fn group_membership(root: &Path, extra: &[String]) -> Result<BTreeMap<String, Suite>, String> {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args([
        "nextest",
        "list",
        "--workspace",
        "--message-format",
        "json",
        "-E",
    ])
    .arg(format!("group({GROUP})"))
    .args(extra)
    .current_dir(root);
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run `cargo nextest list`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`cargo nextest list -E 'group({GROUP})'` failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let listing: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("`cargo nextest list` output is not JSON: {e}"))?;
    let suites = listing["rust-suites"]
        .as_object()
        .ok_or_else(|| "`cargo nextest list` output has no rust-suites".to_string())?;
    let mut membership = BTreeMap::new();
    for suite in suites.values() {
        let Some(binary_id) = suite["binary-id"].as_str() else {
            continue;
        };
        // `--message-format json` lists EVERY test case and marks each
        // with its filter verdict, so a filtered listing is a listing with
        // mismatches in it, not a shorter listing. Reading the keys alone
        // makes every binary look like a group member — and reading only
        // the matches loses the denominator a whole-binary target needs.
        // Ignored tests never run, so they are not in either set.
        let mut parsed = Suite::default();
        if let Some(cases) = suite["testcases"].as_object() {
            for (name, case) in cases {
                if case["ignored"].as_bool().unwrap_or(false) {
                    continue;
                }
                parsed.all.insert(name.clone());
                if case["filter-match"]["status"] == "matches" {
                    parsed.matched.insert(name.clone());
                }
            }
        }
        if !parsed.all.is_empty() {
            membership.insert(binary_id.to_string(), parsed);
        }
    }
    Ok(membership)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard scans this very file, so the fixtures below assemble the
    /// attribute at run time instead of writing it at the start of a line.
    /// A line scan cannot tell an attribute from a string literal that
    /// looks like one; keeping the literal out of column zero is cheaper
    /// than teaching the scanner Rust's lexical grammar.
    fn attr(args: &str) -> String {
        format!("#[{}::test{args}]", "sqlx")
    }

    #[test]
    fn prose_about_sqlx_test_is_not_evidence() {
        // Verbatim shapes from trawl-config and trawl-server's store modules.
        let text = format!(
            "// DATABASE_URL is ceded to the sqlx test harness ({a}\n    /// tests use `{a}`-migrated pools directly).\n",
            a = attr("")
        );
        assert!(sqlx_test_names(&text).is_empty());
    }

    #[test]
    fn the_attribute_names_the_test_beneath_it() {
        let text = format!(
            "{}\nasync fn roles_are_data(pool: PgPool) {{\n    {}\n    async fn nested(pool: PgPool) {{}}\n",
            attr("(migrations = false)"),
            attr("")
        );
        let found = sqlx_test_names(&text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], (1, "roles_are_data".to_string()));
        assert_eq!(found[1], (3, "nested".to_string()));
    }

    /// An ignored test never runs, so `group_membership` leaves it out of
    /// both its sets. Collecting it here would fail the guard on a name
    /// nextest never lists.
    #[test]
    fn an_ignored_sqlx_test_is_not_collected() {
        let text = format!(
            "{a}\n#[ignore = \"needs a live cluster\"]\nasync fn skipped(pool: PgPool) {{}}\n\n#[ignore]\n{a}\nasync fn also_skipped(pool: PgPool) {{}}\n\n{a}\nasync fn runs(pool: PgPool) {{}}\n",
            a = attr("")
        );
        let found = sqlx_test_names(&text);
        assert_eq!(found, vec![(9, "runs".to_string())]);
    }

    /// Same trick for the connection APIs: writing `KeyStore::connect(`
    /// literally here would make the guard's own source pg-touching if the
    /// scan is ever pointed at a `src/` tree.
    fn call(receiver: &str, method: &str) -> String {
        format!("{receiver}{}{method}(", "::")
    }

    fn suite(all: &[&str], matched: &[&str]) -> Suite {
        Suite {
            all: all.iter().map(|s| (*s).to_string()).collect(),
            matched: matched.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn target(evidence: Vec<Evidence>) -> Derived {
        Derived {
            binary_id: "trawl-server::auth_pg".to_string(),
            evidence,
        }
    }

    fn fixture_evidence() -> Evidence {
        Evidence {
            site: "tests/common/mod.rs (postgres fixture module)".to_string(),
            tests: Vec::new(),
        }
    }

    fn named_evidence(names: &[&str]) -> Evidence {
        Evidence {
            site: "tests/auth_pg.rs:12 attribute".to_string(),
            tests: names.iter().map(|n| (*n).to_string()).collect(),
        }
    }

    #[test]
    fn target_evidence_dominates_the_named_tests_beside_it() {
        // auth_pg's shape: named `#[sqlx::test]` cases AND the fixture
        // module. A narrowed filter that matches only the named ones leaves
        // the rest of the binary booting servers unbounded.
        let mixed = target(vec![
            fixture_evidence(),
            named_evidence(&["ac1_key_roundtrip", "ac3_admin_cannot_ingest"]),
        ]);
        let narrowed = suite(
            &["ac1_key_roundtrip", "ac3_admin_cannot_ingest", "ac7_reload"],
            &["ac1_key_roundtrip", "ac3_admin_cannot_ingest"],
        );
        let failure = check(&mixed, Some(&narrowed)).expect("the third test escaped the group");
        assert!(failure.contains("1 of its 3 test(s)"), "{failure}");
        assert!(failure.contains("ac7_reload"), "{failure}");

        // Same evidence, whole binary in the group: a pass.
        let whole = suite(
            &["ac1_key_roundtrip", "ac3_admin_cannot_ingest", "ac7_reload"],
            &["ac1_key_roundtrip", "ac3_admin_cannot_ingest", "ac7_reload"],
        );
        assert!(check(&mixed, Some(&whole)).is_none());
    }

    #[test]
    fn a_named_only_target_is_still_checked_per_test() {
        let lib = target(vec![named_evidence(&["run_all", "resolve_one"])]);
        let listing = suite(
            &["from_saved::tests::run_all", "unrelated::unit_test"],
            &["from_saved::tests::run_all"],
        );
        let failure = check(&lib, Some(&listing)).expect("resolve_one is outside the group");
        assert!(failure.contains("resolve_one"), "{failure}");
        assert!(!failure.contains("unrelated"), "{failure}");
    }

    #[test]
    fn a_raw_connection_call_is_evidence_but_prose_is_not() {
        let code = format!(
            "#[tokio::test]\nasync fn boots() {{\n    let ks = {}\"...\").await;\n}}\n",
            call("KeyStore", "connect")
        );
        assert_eq!(
            connection_api_site(&code).map(|(_, api)| api),
            Some("KeyStore::connect")
        );

        let prose = format!(
            "// the fixture calls {}dsn) for us, so this test never touches a pool\n/// see {} above\nfn pure() {{}}\n",
            call("KeyStore", "connect"),
            call("PgConnection", "connect"),
        );
        assert!(connection_api_site(&prose).is_none());

        assert!(connection_api_site("fn pure() -> u8 { 1 }\n").is_none());
    }

    /// Production `src/` code that opens a pool is not test evidence: the
    /// definition sites would flag every store module in the workspace.
    #[test]
    fn production_code_outside_cfg_test_is_not_evidence() {
        let text = format!(
            "pub async fn connect(url: &str) -> Store {{\n    let pool = {}url).await;\n    Store {{ pool }}\n}}\n",
            call("PgPoolOptions", "new")
        );
        assert!(cfg_test_connection_site(&text).is_none());
    }

    /// The escape this class exists for: a plain `#[tokio::test]` under
    /// `src/`, inside the crate's own test module, dialing postgres itself.
    #[test]
    fn a_connection_inside_a_cfg_test_module_is_evidence() {
        let text = format!(
            "pub async fn connect(url: &str) -> Store {{\n    let pool = {}url).await;\n    Store {{ pool }}\n}}\n\n#[cfg(test)]\nmod tests {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("PgPoolOptions", "new"),
            call("StorageState", "connect"),
        );
        let (line, api, tests) = cfg_test_connection_site(&text).expect("the test dials postgres");
        assert_eq!((line, api), (10, "StorageState::connect"));
        assert_eq!(tests, vec!["boots".to_string()]);
    }

    /// Braces nested inside the test module, and a string literal carrying
    /// an unbalanced `}`. Neither may end the region before the call.
    #[test]
    fn nested_braces_and_a_brace_in_a_string_do_not_end_the_region() {
        let text = format!(
            "#[cfg(test)]\nmod tests {{\n    fn helper() {{\n        let s = \"}}}}}} not code\";\n    }}\n\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n\nfn after() {{}}\n",
            call("StorageState", "connect"),
        );
        let (line, api, tests) = cfg_test_connection_site(&text).expect("the test dials postgres");
        assert_eq!((line, api), (9, "StorageState::connect"));
        // `helper` carries no test attribute, so it is not a name to check.
        assert_eq!(tests, vec!["boots".to_string()]);

        // The same call AFTER the module closes is production code again.
        let outside = format!(
            "#[cfg(test)]\nmod tests {{\n    fn helper() {{}}\n}}\n\nasync fn boot() {{\n    let s = {}\"...\").await;\n}}\n",
            call("StorageState", "connect"),
        );
        assert!(cfg_test_connection_site(&outside).is_none());
    }

    /// `#[cfg(test)]` on a single fn is one braced body like a module.
    #[test]
    fn a_cfg_test_fn_is_scanned_like_a_cfg_test_mod() {
        let text = format!(
            "#[cfg(test)]\nasync fn helper() {{\n    let s = {}\"...\").await;\n}}\n",
            call("KeyStore", "connect"),
        );
        let (line, api, tests) = cfg_test_connection_site(&text).expect("cfg(test) fn is scanned");
        assert_eq!((line, api), (3, "KeyStore::connect"));
        // No test attribute inside: the finding is target-level.
        assert!(tests.is_empty());
    }

    #[test]
    fn a_test_id_matches_by_its_last_segment() {
        assert!(test_is("from_saved::tests::run_all", "run_all"));
        assert!(test_is("run_all", "run_all"));
        assert!(!test_is("from_saved::tests::run_all_but_one", "run_all"));
    }
}
