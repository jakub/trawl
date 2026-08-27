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
//! 1. DERIVATION, from `cargo metadata` plus one item-bounded lexer pass
//!    over each target's sources (see [`file_facts`]). A target is pg-touching if it carries an `#[sqlx::test]`
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

/// Does a nextest test id name this derived test?
///
/// An id carries the full module path (`from_saved::pg_tests::run_all`)
/// while a derived name carries only the nesting the lexer could see inside
/// the file (`pg_tests::run_all`), so the match is on a whole trailing run
/// of path segments. Matching the LEAF alone was unsound: a pg-touching
/// `pg_tests::connects` outside the group was masked by an unrelated
/// `pure_tests::connects` inside it.
fn test_is(test_id: &str, derived: &str) -> bool {
    test_id == derived || test_id.ends_with(&format!("::{derived}"))
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
    let parsed: Vec<(PathBuf, FileFacts)> = sources
        .iter()
        .filter_map(|source| {
            fs::read_to_string(source)
                .ok()
                .map(|text| (source.clone(), file_facts(&text)))
        })
        .collect();

    let mut evidence = Vec::new();
    for (source, facts) in &parsed {
        let shown = source.strip_prefix(root).unwrap_or(source).display();
        let tests = sqlx_tests(facts);
        if let Some((line, _)) = tests.first() {
            evidence.push(Evidence {
                site: format!("{shown}:{line} #[sqlx::test]"),
                tests: tests.iter().map(|(_, name)| name.clone()).collect(),
            });
        }

        // The fixture side: this file IS the pg harness (it defines the
        // pool constructor), and `integration_sources` only handed it over
        // because the target declares `mod common;`.
        if facts.code.contains(FIXTURE_MARKER) && source.ends_with(Path::new("common/mod.rs")) {
            evidence.push(Evidence {
                site: format!("{shown} (postgres fixture module)"),
                tests: Vec::new(),
            });
        }

        // The raw-connection side: no attribute, no fixture, just a test
        // that dials postgres itself. An integration file is all test code;
        // a `src/` file is production code, where only the `#[cfg(test)]`
        // bodies count.
        //
        // Residual: an out-of-line test module (`#[cfg(test)] mod tests;`)
        // puts the test code in a file that carries no `#[cfg(test)]` of
        // its own, so no span of it is scanned at all.
        if scope == Scope::Integration {
            // An integration target's finding is target-level on purpose:
            // nothing there says WHICH tests the connection belongs to, so
            // the whole binary is charged.
            if let Some((line, api)) = connection_site(facts, 1, usize::MAX) {
                evidence.push(Evidence {
                    site: format!("{shown}:{line} {api}"),
                    tests: Vec::new(),
                });
            }
        } else {
            for span in &facts.cfg_test_spans {
                if let Some((line, api)) = connection_site(facts, span.0, span.1) {
                    evidence.push(Evidence {
                        site: format!("{shown}:{line} {api}"),
                        tests: test_names(facts, Some(*span)),
                    });
                }
            }
        }
    }
    evidence
}

/// Line numbers and names of every non-ignored `#[sqlx::test]` in a file.
///
/// An `#[ignore]`d test is skipped, and it has to be: `group_membership`
/// drops ignored cases from both the `all` and the `matched` side, so a
/// name collected here that nextest never lists reads as a test that
/// escaped the group and fails the guard over a test that never runs. The
/// two sides must agree on what counts as a test.
fn sqlx_tests(facts: &FileFacts) -> Vec<(usize, String)> {
    facts
        .fns
        .iter()
        .filter(|item| item.is_sqlx_test && !item.ignored)
        .map(|item| (item.line, item.name.clone()))
        .collect()
}

/// Names of the non-ignored test functions of a file, or of one span of it.
///
/// Helper `fn`s are left out — they are not test ids, and asking nextest
/// about one would fail the guard on a name that can never be in the group.
/// Ignored ones are left out for the same reason `sqlx_tests` drops them.
fn test_names(facts: &FileFacts, span: Option<Span>) -> Vec<String> {
    facts
        .fns
        .iter()
        .filter(|item| item.is_test && !item.ignored)
        .filter(|item| span.is_none_or(|(start, end)| item.line >= start && item.line <= end))
        .map(|item| item.name.clone())
        .collect()
}

/// The first line in the inclusive range `[start, end]` naming a
/// connection-opening API, with the API it named.
///
/// Reads the blanked text, so prose about `fixture_pool` is not a
/// connection and a DSN inside a string literal is not one either. Losing a
/// detection that way is impossible: a call is code, and code survives
/// blanking intact.
fn connection_site(facts: &FileFacts, start: usize, end: usize) -> Option<(usize, &'static str)> {
    facts
        .code
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line))
        .filter(|(number, _)| *number >= start && *number <= end)
        .find_map(|(number, line)| {
            CONNECTION_APIS
                .iter()
                .find(|api| line.contains(**api))
                .map(|api| (number, *api))
        })
}

/// One `fn` item, with the facts derived from the attribute block rustc
/// attaches to it.
struct FnItem {
    /// Real 1-based line of the first attribute in the block, or of the
    /// signature itself when the function carries no attributes.
    line: usize,
    /// The enclosing inline `mod` names joined onto the function's own
    /// (`pg_tests::resolve_all`). The file's path from the crate root is
    /// NOT part of it; [`test_is`] matches by trailing segments.
    name: String,
    /// Does the block carry an attribute whose last path segment is `test`
    /// (`#[test]`, `#[tokio::test]`, `#[sqlx::test]`)?
    is_test: bool,
    is_sqlx_test: bool,
    ignored: bool,
}

/// Inclusive 1-based line range.
type Span = (usize, usize);

/// One attribute, as the lexer read it.
struct Attr {
    /// The path before any arguments, whitespace removed: `cfg`, `ignore`,
    /// `sqlx::test`.
    path: String,
    /// The arguments, BLANKED — a string literal's contents are spaces.
    args: String,
}

/// Everything the guard reads out of one source file.
struct FileFacts {
    /// The comment- and literal-blanked text: byte for byte as long as the
    /// original, with every newline in place.
    code: String,
    fns: Vec<FnItem>,
    cfg_test_spans: Vec<Span>,
}

/// Read one source file with a single item-bounded pass over its blanked
/// text.
///
/// Every per-test fact the guard uses — is this a test, what is it called,
/// is it `#[ignore]`d — comes from here, because deriving them from
/// adjacent lines gets all three wrong in ways that matter:
///
/// * An `#[ignore]` written inside a `/* */` block comment is not an
///   attribute. Blanking turns it into spaces before the lexer sees it, so
///   it cannot be read as one.
/// * An attribute block belongs to the NEXT ITEM and stops there. A scan
///   that walks downward until it finds a `fn` reads the `#[ignore]` of the
///   next test as this one's.
/// * Blank lines, doc comments and multi-line attributes between the block
///   and its item change nothing. Probed with rustc: `#[test]`, a blank
///   line, `/// doc`, a blank line, `#[ignore]`, a blank line, then `fn
///   t()` compiles to one IGNORED test. So the block accumulates across all
///   of them and is discarded only when some other item takes it.
///
/// Line numbers stay the file's real ones: blanking preserves every byte
/// position and newline.
fn file_facts(text: &str) -> FileFacts {
    let code = blank_comments_and_literals(text);
    let mut lexer = ItemLexer {
        bytes: code.as_bytes(),
        i: 0,
        line: 1,
        depth: 0,
        mods: Vec::new(),
        open_cfg: Vec::new(),
        pending: None,
        pending_cfg: None,
        fns: Vec::new(),
        spans: Vec::new(),
    };
    lexer.run();
    let (fns, spans) = (lexer.fns, lexer.spans);
    FileFacts {
        code,
        fns,
        cfg_test_spans: spans,
    }
}

/// The item lexer. It knows four things — attributes, braces, `fn` and
/// `mod` — and treats everything else as "some item that takes the pending
/// attribute block".
struct ItemLexer<'a> {
    /// The blanked text, as long as the file it came from.
    bytes: &'a [u8],
    i: usize,
    /// 1-based line of `i`.
    line: usize,
    depth: usize,
    /// Enclosing inline `mod` names, each with the depth its body opened at.
    mods: Vec<(String, usize)>,
    /// Open `#[cfg(test)]` bodies: start line, and the depth each opened at.
    open_cfg: Vec<(usize, usize)>,
    /// The attribute block being accumulated: the line it started on, and
    /// the attributes in it.
    pending: Option<(usize, Vec<Attr>)>,
    /// A `cfg(test)` block already taken by an item whose braced body has
    /// not opened yet.
    pending_cfg: Option<usize>,
    fns: Vec<FnItem>,
    spans: Vec<Span>,
}

impl ItemLexer<'_> {
    fn run(&mut self) {
        loop {
            self.skip_trivia();
            let Some(byte) = self.peek() else { break };
            match byte {
                b'#' => self.attribute(),
                b'{' => {
                    self.pending = None;
                    if let Some(start) = self.pending_cfg.take() {
                        self.open_cfg.push((start, self.depth));
                    }
                    self.depth += 1;
                    self.bump();
                }
                b'}' => self.close_block(),
                b';' | b',' => {
                    self.pending = None;
                    self.pending_cfg = None;
                    self.bump();
                }
                b'_' | b'a'..=b'z' | b'A'..=b'Z' => self.word(),
                _ => {
                    self.pending = None;
                    self.bump();
                }
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.i).copied()
    }

    fn bump(&mut self) {
        if self.peek() == Some(b'\n') {
            self.line += 1;
        }
        self.i += 1;
    }

    /// Comments are already spaces, so trivia is whitespace.
    fn skip_trivia(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.bump();
        }
    }

    fn ident(&mut self) -> Option<String> {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            self.bump();
        }
        (self.i > start).then(|| self.slice(start, self.i).to_string())
    }

    fn slice(&self, from: usize, to: usize) -> &str {
        std::str::from_utf8(self.bytes.get(from..to).unwrap_or_default()).unwrap_or_default()
    }

    /// Consume a balanced group and return the byte range INSIDE it. The
    /// cursor must be on `open`; it ends past the matching `close`.
    fn group(&mut self, open: u8, close: u8) -> (usize, usize) {
        self.bump();
        let start = self.i;
        let mut depth = 1usize;
        while let Some(byte) = self.peek() {
            if byte == open {
                depth += 1;
            } else if byte == close {
                depth -= 1;
                if depth == 0 {
                    let end = self.i;
                    self.bump();
                    return (start, end);
                }
            }
            self.bump();
        }
        (start, self.i)
    }

    fn attribute(&mut self) {
        let line = self.line;
        let at = self.i;
        self.bump();
        let inner = self.peek() == Some(b'!');
        if inner {
            self.bump();
        }
        if self.peek() != Some(b'[') {
            // Not an attribute. Rust has no other `#`, but a blanked byte
            // could leave one behind; step over it and drop the block.
            self.i = at;
            self.line = line;
            self.bump();
            self.pending = None;
            return;
        }
        let (from, to) = self.group(b'[', b']');
        if inner {
            // `#![...]` belongs to the ENCLOSING item, not the next one.
            return;
        }
        let attr = self.parse_attr(from, to);
        self.pending
            .get_or_insert_with(|| (line, Vec::new()))
            .1
            .push(attr);
    }

    fn parse_attr(&self, from: usize, to: usize) -> Attr {
        let inner = self.slice(from, to);
        let split = inner.find(['(', '=']).unwrap_or(inner.len());
        Attr {
            path: inner[..split]
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect(),
            args: inner[split..].to_string(),
        }
    }

    fn close_block(&mut self) {
        self.pending = None;
        self.pending_cfg = None;
        self.depth = self.depth.saturating_sub(1);
        while self
            .mods
            .last()
            .is_some_and(|(_, depth)| *depth >= self.depth)
        {
            self.mods.pop();
        }
        while let Some(&(start, depth)) = self.open_cfg.last() {
            if depth < self.depth {
                break;
            }
            self.open_cfg.pop();
            self.spans.push((start, self.line));
        }
        self.bump();
    }

    fn word(&mut self) {
        let line = self.line;
        let Some(word) = self.ident() else {
            self.bump();
            return;
        };
        match word.as_str() {
            // Visibility and function modifiers do NOT take the attribute
            // block: `#[sqlx::test] pub(crate) async fn live()` is one item,
            // and the block is the `fn`'s.
            "pub" => {
                self.skip_trivia();
                if self.peek() == Some(b'(') {
                    self.group(b'(', b')');
                }
            }
            "async" | "const" | "unsafe" | "extern" | "default" => {}
            "fn" => self.function(line),
            "mod" => self.module(line),
            // Any other item or expression takes the block. A `cfg(test)`
            // one still opens a span when its body does, which is what
            // covers shapes like `#[cfg(test)] impl Fixture { .. }`.
            _ => {
                self.take_pending(line);
            }
        }
    }

    /// Take the pending attribute block, remembering a `cfg(test)` in it so
    /// the item's braced body becomes a span.
    fn take_pending(&mut self, line: usize) -> (usize, Vec<Attr>) {
        let (attr_line, attrs) = self.pending.take().unwrap_or((line, Vec::new()));
        if attrs.iter().any(is_cfg_test) {
            self.pending_cfg = Some(attr_line);
        }
        (attr_line, attrs)
    }

    fn function(&mut self, line: usize) {
        self.skip_trivia();
        let name = self.ident();
        let (attr_line, attrs) = self.take_pending(line);
        let Some(name) = name else { return };
        self.fns.push(FnItem {
            line: attr_line,
            name: self.qualified(&name),
            is_test: attrs.iter().any(is_test_attr),
            is_sqlx_test: attrs.iter().any(|attr| attr.path == "sqlx::test"),
            ignored: attrs.iter().any(|attr| attr.path == "ignore"),
        });
    }

    fn module(&mut self, line: usize) {
        self.skip_trivia();
        let name = self.ident().unwrap_or_default();
        let (attr_line, attrs) = self.pending.take().unwrap_or((line, Vec::new()));
        let cfg_test = attrs.iter().any(is_cfg_test);
        self.skip_trivia();
        if self.peek() == Some(b'{') {
            self.bump();
            if cfg_test {
                self.open_cfg.push((attr_line, self.depth));
            }
            self.mods.push((name, self.depth));
            self.depth += 1;
        }
    }

    /// A function's name under the inline modules enclosing it.
    fn qualified(&self, name: &str) -> String {
        let mut out = String::new();
        for (module, _) in &self.mods {
            out.push_str(module);
            out.push_str("::");
        }
        out.push_str(name);
        out
    }
}

/// `#[test]`, `#[tokio::test]`, `#[sqlx::test(...)]` — an attribute whose
/// final path segment is exactly `test`.
fn is_test_attr(attr: &Attr) -> bool {
    attr.path.rsplit("::").next() == Some("test")
}

/// `#[cfg(test)]`, and only a bare `test` predicate: the arguments are read
/// from the BLANKED text, so `#[cfg(feature = "test")]` has no `test` token
/// left in it to find.
fn is_cfg_test(attr: &Attr) -> bool {
    attr.path == "cfg" && has_ident(&attr.args, "test")
}

/// Does `text` contain `word` as a whole identifier?
fn has_ident(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices(word).any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + word.len()).copied();
        !before.is_some_and(is_ident_byte) && !after.is_some_and(is_ident_byte)
    })
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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
                    // A `\` before a newline is a line-continuation escape.
                    // Blanking the newline away with it would shift every
                    // later line number by one, so the escaped byte is
                    // blanked only when it is not a newline.
                    out.push(' ');
                    out.push(if bytes[i + 1] == b'\n' { '\n' } else { ' ' });
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
    /// Blanking makes a literal that looks like an attribute harmless, but
    /// an attribute written in column zero here would be a real one.
    fn attr(args: &str) -> String {
        format!("#[{}::test{args}]", "sqlx")
    }

    /// Every `#[sqlx::test]` a file declares.
    fn sqlx_names(text: &str) -> Vec<(usize, String)> {
        sqlx_tests(&file_facts(text))
    }

    /// The span half of `scan`, as one call: the first connection each
    /// `#[cfg(test)]` span opens, with the tests that span declares.
    fn cfg_test_sites(text: &str) -> Vec<(usize, &'static str, Vec<String>)> {
        let facts = file_facts(text);
        facts
            .cfg_test_spans
            .iter()
            .filter_map(|span| {
                connection_site(&facts, span.0, span.1)
                    .map(|(line, api)| (line, api, test_names(&facts, Some(*span))))
            })
            .collect()
    }

    #[test]
    fn prose_about_sqlx_test_is_not_evidence() {
        // Verbatim shapes from trawl-config and trawl-server's store modules.
        let text = format!(
            "// DATABASE_URL is ceded to the sqlx test harness ({a}\n    /// tests use `{a}`-migrated pools directly).\n",
            a = attr("")
        );
        assert!(sqlx_names(&text).is_empty());
    }

    #[test]
    fn the_attribute_names_the_test_beneath_it() {
        let text = format!(
            "{}\nasync fn roles_are_data(pool: PgPool) {{\n    {}\n    async fn nested(pool: PgPool) {{}}\n",
            attr("(migrations = false)"),
            attr("")
        );
        let found = sqlx_names(&text);
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
        let found = sqlx_names(&text);
        assert_eq!(found, vec![(9, "runs".to_string())]);
    }

    /// An `#[ignore]` inside a block comment is prose, and prose does not
    /// ignore a test. The lexer reads the BLANKED text, where the whole
    /// comment is spaces, so the live test below it stays collected.
    #[test]
    fn an_ignore_inside_a_block_comment_is_not_an_attribute() {
        let text = format!(
            "{a}\n/* the shelved variant was\n#[ignore]\nuntil the fixture landed */\nasync fn live(pool: PgPool) {{}}\n",
            a = attr("")
        );
        assert_eq!(sqlx_names(&text), vec![(1, "live".to_string())]);
    }

    /// The block ends at the item it attributes. Reading downward "until a
    /// fn appears" walked past `live`'s signature and charged it with the
    /// NEXT test's `#[ignore]`, dropping a live pg test from the derived
    /// set. `pub(crate) async fn` is the shape that made the old scan miss
    /// the signature in the first place.
    #[test]
    fn the_attribute_block_stops_at_the_fn_it_attributes() {
        let text = format!(
            "{a}\npub(crate) async fn live(pool: PgPool) {{}}\n\n#[test]\n#[ignore]\nfn offline() {{}}\n",
            a = attr("")
        );
        assert_eq!(sqlx_names(&text), vec![(1, "live".to_string())]);
    }

    /// Visibility and modifier spellings the old `strip_prefix("pub ")`
    /// scan could not read.
    #[test]
    fn every_visibility_and_modifier_ordering_names_its_fn() {
        for signature in [
            "pub(crate) async fn probe(pool: PgPool) {}",
            "pub(super) async fn probe(pool: PgPool) {}",
            "pub(in crate::store) async fn probe(pool: PgPool) {}",
            "pub async unsafe fn probe(pool: PgPool) {}",
            "const fn probe() {}",
            "pub  (  crate  )  fn probe() {}",
        ] {
            let text = format!("{}\n{signature}\n", attr(""));
            assert_eq!(
                sqlx_names(&text),
                vec![(1, "probe".to_string())],
                "{signature}"
            );
        }
    }

    /// rustc attaches an attribute across blank lines, doc comments and
    /// other attributes; probed with rustc, which reports exactly one
    /// IGNORED test for this shape. The reverse line scan stopped at the
    /// first line that was not an attribute, so the `#[ignore]` above the
    /// doc comment was invisible and an ignored test was collected.
    #[test]
    fn an_attribute_block_carries_across_blank_lines_and_docs() {
        let text = format!(
            "#[ignore]\n\n/// why it is shelved\n\n#[allow(\n    dead_code\n)]\n\n{}\nasync fn shelved(pool: PgPool) {{}}\n",
            attr("")
        );
        assert!(sqlx_names(&text).is_empty(), "{:?}", sqlx_names(&text));
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
        let facts = file_facts(&code);
        assert_eq!(
            connection_site(&facts, 1, usize::MAX).map(|(_, api)| api),
            Some("KeyStore::connect")
        );

        let prose = format!(
            "// the fixture calls {}dsn) for us, so this test never touches a pool\n/// see {} above\n/* or {} */\nfn pure() {{}}\n",
            call("KeyStore", "connect"),
            call("PgConnection", "connect"),
            call("StorageState", "connect"),
        );
        assert!(connection_site(&file_facts(&prose), 1, usize::MAX).is_none());

        assert!(connection_site(&file_facts("fn pure() -> u8 { 1 }\n"), 1, usize::MAX).is_none());
    }

    /// Production `src/` code that opens a pool is not test evidence: the
    /// definition sites would flag every store module in the workspace.
    #[test]
    fn production_code_outside_cfg_test_is_not_evidence() {
        let text = format!(
            "pub async fn connect(url: &str) -> Store {{\n    let pool = {}url).await;\n    Store {{ pool }}\n}}\n",
            call("PgPoolOptions", "new")
        );
        assert!(cfg_test_sites(&text).is_empty());
    }

    /// The escape this class exists for: a plain `#[tokio::test]` under
    /// `src/`, inside the crate's own test module, dialing postgres itself.
    /// The name it reports carries the module, because a leaf name is
    /// ambiguous across modules (see [`test_is`]).
    #[test]
    fn a_connection_inside_a_cfg_test_module_is_evidence() {
        let text = format!(
            "pub async fn connect(url: &str) -> Store {{\n    let pool = {}url).await;\n    Store {{ pool }}\n}}\n\n#[cfg(test)]\nmod pg_tests {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("PgPoolOptions", "new"),
            call("StorageState", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!(sites.len(), 1);
        assert_eq!((sites[0].0, sites[0].1), (10, "StorageState::connect"));
        assert_eq!(sites[0].2, vec!["pg_tests::boots".to_string()]);
    }

    /// One `#[cfg(test)]` region per file was the old rule (`find_map`), so
    /// a second test module in the same file opened connections unseen.
    #[test]
    fn every_cfg_test_span_is_scanned() {
        let text = format!(
            "#[cfg(test)]\nmod pure_tests {{\n    #[test]\n    fn arithmetic() {{}}\n}}\n\n#[cfg(test)]\nmod pg_tests {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!((sites[0].0, sites[0].1), (11, "StorageState::connect"));
        assert_eq!(sites[0].2, vec!["pg_tests::boots".to_string()]);
    }

    /// An ignored test inside a connecting span is not named evidence
    /// either: nextest lists no such case, so the guard would fail on a
    /// name that can never be in the group.
    #[test]
    fn an_ignored_test_in_a_connecting_span_is_not_named() {
        let text = format!(
            "#[cfg(test)]\nmod pg_tests {{\n    #[tokio::test]\n    #[ignore]\n    async fn shelved() {{\n        let s = {}\"...\").await;\n    }}\n\n    #[tokio::test]\n    async fn live() {{}}\n}}\n",
            call("StorageState", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].2, vec!["pg_tests::live".to_string()]);
    }

    /// Braces nested inside the test module, and a string literal carrying
    /// an unbalanced `}`. Neither may end the region before the call.
    #[test]
    fn nested_braces_and_a_brace_in_a_string_do_not_end_the_region() {
        let text = format!(
            "#[cfg(test)]\nmod tests {{\n    fn helper() {{\n        let s = \"}}}}}} not code\";\n    }}\n\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n\nfn after() {{}}\n",
            call("StorageState", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!((sites[0].0, sites[0].1), (9, "StorageState::connect"));
        // `helper` carries no test attribute, so it is not a name to check.
        assert_eq!(sites[0].2, vec!["tests::boots".to_string()]);

        // The same call AFTER the module closes is production code again.
        let outside = format!(
            "#[cfg(test)]\nmod tests {{\n    fn helper() {{}}\n}}\n\nasync fn boot() {{\n    let s = {}\"...\").await;\n}}\n",
            call("StorageState", "connect"),
        );
        assert!(cfg_test_sites(&outside).is_empty());
    }

    /// `#[cfg(test)]` on a single fn is one braced body like a module.
    #[test]
    fn a_cfg_test_fn_is_scanned_like_a_cfg_test_mod() {
        let text = format!(
            "#[cfg(test)]\nasync fn helper() {{\n    let s = {}\"...\").await;\n}}\n",
            call("KeyStore", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!((sites[0].0, sites[0].1), (3, "KeyStore::connect"));
        // No test attribute inside: the finding is target-level.
        assert!(sites[0].2.is_empty());
    }

    /// `#[cfg(feature = "test")]` is not `#[cfg(test)]`. The argument text
    /// is read blanked, so the string has no `test` token left in it.
    #[test]
    fn a_feature_named_test_does_not_open_a_span() {
        let text = format!(
            "#[cfg(feature = \"test\")]\nmod helpers {{\n    async fn boot() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
        );
        assert!(cfg_test_sites(&text).is_empty());
    }

    #[test]
    fn a_test_id_matches_by_a_whole_trailing_path() {
        assert!(test_is("from_saved::tests::run_all", "run_all"));
        assert!(test_is("run_all", "run_all"));
        assert!(!test_is("from_saved::tests::run_all_but_one", "run_all"));
        // The whole point of qualifying: a leaf match would let an
        // in-group `pure_tests::connects` mask an out-of-group one.
        assert!(test_is("store::pg_tests::connects", "pg_tests::connects"));
        assert!(!test_is(
            "store::pure_tests::connects",
            "pg_tests::connects"
        ));
    }

    /// Blanking has to be byte-for-byte length preserving and newline
    /// exact, because every reported line number and every `#[path]` read
    /// out of the raw text depends on the two texts lining up. A `\`
    /// before a newline inside a string used to eat the newline, and every
    /// line after it was reported one too low.
    #[test]
    fn blanking_preserves_length_and_line_numbers() {
        let text = "let s = \"a \\\n    b\";\nlet t = \"ünïcødé\";\nlet r = r#\"raw \" }\"#;\nlet c = '}';\n// tail ü\n";
        let blanked = blank_comments_and_literals(text);
        assert_eq!(blanked.len(), text.len());
        assert_eq!(blanked.lines().count(), text.lines().count());
        for (index, (before, after)) in text.lines().zip(blanked.lines()).enumerate() {
            assert_eq!(before.len(), after.len(), "line {}", index + 1);
        }

        // The line number a continuation escape used to shift.
        let code = format!(
            "fn a() {{\n    let msg = \"one \\\n        two\";\n}}\n\n#[cfg(test)]\nmod pg_tests {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
        );
        let sites = cfg_test_sites(&code);
        assert_eq!((sites[0].0, sites[0].1), (10, "StorageState::connect"));
    }
}
