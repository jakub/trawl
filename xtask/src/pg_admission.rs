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
//!    file, but only the TEST REGIONS of a `src/` tree, where the
//!    surrounding production code is where those APIs are defined: the body
//!    of a `#[cfg(test)]` item, and the body of any fn carrying a test
//!    attribute.
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
//!
//! # What this guard defends against, and what it does not
//!
//! It is a CI tripwire for an ACCIDENTAL escape: a reasonable author adds
//! a test that opens a postgres connection, the code passes `cargo fmt`
//! and compiles in the feature set CI builds with, and nobody remembers to
//! extend the group override. That is the whole threat model. It is NOT a
//! security boundary. Anyone editing the workspace can also edit
//! `.config/nextest.toml`, or write a test whose connection this lexer
//! cannot see, and no amount of static reading closes that.
//!
//! The fail direction is fixed, and every judgement call below follows it:
//!
//! * Uncertain whether something is a pg test? Collect it. A test named
//!   here that nextest never lists fails the guard by name, and a human
//!   reads the sentence.
//! * Uncertain which target owns a file? Charge it to both. Two loud
//!   failures beat one silent pass.
//! * A SILENT FALSE GREEN is the only defect class this module treats as a
//!   bug. A false failure is noise; a pass over a live escape is the thing
//!   the group exists to prevent.
//!
//! Accepted residuals, each of them loud or over-collecting:
//!
//! * CONDITIONAL COMPILATION is not statically resolvable. The guard does
//!   not know which cfgs CI compiled under, so a `cfg_attr`-wrapped
//!   `sqlx::test` counts as one, a `cfg_attr` whose expansion contains
//!   `cfg(test)` opens a test span, a `cfg_attr`-wrapped `ignore` does not
//!   ignore, and a `#[cfg(not(test))]` region is scanned like any other
//!   attributed item. All of them over-collect.
//! * A `#[cfg(test)]` region nested inside another one yields two spans
//!   over the same code, so one connection can be reported twice. Two
//!   findings for one call site, never zero.
//! * An attributed item that ends at a `;` with no body spans its own
//!   whole extent, first attribute line through the semicolon. That is
//!   what covers a `#[cfg(test)] static POOL: LazyLock<PgPool> =
//!   LazyLock::new(|| { .. });`, whose connection lives in a closure the
//!   signature walk consumes as a group. It over-collects for an item with
//!   nothing in it, `#[cfg(test)] use x;`, which is harmless.
//! * A brace at the item's own depth that is not a body is still read as
//!   one, so a `use a::{b, c};` opens a span ending at that brace's `}`
//!   rather than at the semicolon. The span covers the item's own text and
//!   stops there: it neither loses a line of the item nor reaches into the
//!   next one. An initializer brace is not misread at all, since a `=` at
//!   the item's own depth switches the walk to skipping brace groups
//!   whole.
//! * Token spacing inside an attribute is not read: `# [test]` is invisible
//!   to the lexer. `cargo fmt` rejects that spelling at the pre-commit
//!   hook and in CI, so the shape cannot reach a merge.
//! * Raw identifiers lex whole (`r#mod` is one token), but nothing else
//!   about raw keywords is modelled. A raw keyword used where the lexer
//!   expects a real one is unsupported.
//!
//! Eight further shapes can hide a connection outright, or name the test
//! that opens one wrongly. All eight stay open, because each needs a
//! spelling nobody in this workspace writes by accident, and reading them
//! buys nothing against an author who wants the connection unseen. That is
//! the "NOT a security boundary" sentence above, spent.
//!
//! Which of the two it costs depends on the tree. Under `src/`,
//! [`unit_files`] sweeps in every `.rs` file the module walk never reached,
//! under every path that reaches it (a symlink can give one file two), so a
//! file the walk misses is still SCANNED, under the module prefix its PATH
//! implies. Where that is not the prefix rustc gave the file, the derived
//! name is one nextest never lists and the guard fails loudly. An
//! integration target has no sweep. `tests/` is the module walk and nothing
//! else, so a file the walk misses there is a file nobody reads.
//!
//! * An `include!("cases.rs")` edge is invisible to the module walk, which
//!   follows `mod` declarations and nothing else. In a `src/` tree the
//!   sweep opens the file anyway, under the prefix its path implies rather
//!   than the module the `include!` put it in, so the derived name fails
//!   the guard on a test nextest never lists. Three places keep it silent:
//!   an integration target, which has no sweep; a file reachable only
//!   through a directory this target excludes, `src/bin` for a library;
//!   and the test scope a file INHERITS from its inclusion site, which the
//!   sweep cannot see. A `#[cfg(test)] mod cases { include!("cases.rs"); }`
//!   makes all of `cases.rs` test code to rustc, but the sweep reads it as
//!   production code, so a plain helper fn in it that opens a connection,
//!   called from a `#[test]` beside it, is collected nowhere. No crate here
//!   includes Rust source.
//! * An identifier starting with a non-ASCII character is not lexed as a
//!   word. On a FN that is loud rather than silent: the attribute block is
//!   taken before the name is read, so `#[tokio::test] async fn 東京()`
//!   still opens a span over its body, the connection inside is collected,
//!   and a finding naming no test charges the whole binary. On an inline
//!   `mod 東京 { .. }` the name stops [`ItemLexer::module`] before the body
//!   brace, so a `#[cfg(test)]` above it opens nothing. A test fn inside
//!   still spans its own body and is collected, under a name missing the
//!   module, which fails loudly. A connection anywhere ELSE in that module,
//!   a helper fn or a `LazyLock` pool a test acquires from, is production
//!   code as far as the guard can see, and that is the silent half. Every
//!   identifier in this workspace is ASCII.
//! * A `#[cfg_attr(target_os = "linux", path = "linux.rs")] mod m;` path
//!   override is not read. The guard probes `m.rs` and `m/mod.rs`, so it
//!   scans the wrong file when one of them exists and fails loudly when
//!   neither does. Under `src/` the sweep still opens the real file, under
//!   a path-derived prefix. No crate here has a per-target module file.
//! * A `#[cfg(test)]` macro INVOCATION (`db_test!();`) is a bodyless item,
//!   so its span is its own line and the tests it expands to are neither
//!   named nor scanned. Reading inside one means running macro expansion.
//! * `#[::sqlx::test]`, with the leading `::`, is not the spelling
//!   `is_sqlx_test` matches, so it names no test. The fn body is still
//!   scanned (it carries a test attribute), and the injected `pool:
//!   PgPool` argument on its signature line is evidence on its own, so
//!   what the leading `::` actually costs is an sqlx test that takes NO
//!   argument and opens nothing by hand.
//! * `#[cfg(test)] if probe() { .. } else { .. }` as an attributed
//!   STATEMENT spans the first arm only, so a connection in the `else` arm
//!   falls outside it. It bites in production code alone: written inside a
//!   test fn or a `#[cfg(test)]` region, the enclosing span already covers
//!   both arms. An attribute on an `if` is exotic; the same attribute on an
//!   ITEM is covered, initializer arms included.
//! * In `skip_to_body`, a `>>` at angle depth 1 is consumed as two generic
//!   closers, one more than the signature opened, so a `{` further along
//!   can be taken for the body and the span ends in the wrong place. The
//!   angle walk is textual by design (see that function's own note), and
//!   provoking the miscount takes a right shift written at angle depth 1,
//!   outside the braces a const-generic argument is written in, which the
//!   walk skips whole.
//! * `#[path = /* "decoy.rs" */ "real.rs"]` resolves to `decoy.rs`. The
//!   `#[path]` value is the one thing read from the RAW text, comments and
//!   all, because blanking eats the very string it names. The cost is the
//!   wrong file scanned, or a loud failure when `decoy.rs` is not there.
//!
//! A future review that finds a new hole should adjudicate it against this
//! section: does it produce a silent pass, or only noise?

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
/// `StorageState::connect` is DEFINED, so only the test regions count
/// there: the body of a `#[cfg(test)]` item, and the body of a fn carrying
/// a test attribute (see [`scan`]).
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
    // Exact, not a trailing-segment match. Every derived name is rooted at
    // the target's crate root (see [`module_tree`]), so it is spelled the
    // way nextest spells it. Suffix matching let an in-group
    // `b::tests::connects` satisfy an out-of-group `a::tests::connects`,
    // and the guard reported a pass while the pg test ran unbounded.
    let missing: Vec<&str> = target
        .named_tests()
        .into_iter()
        .filter(|name| !matched.contains(*name))
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

        // The unit-test half. A package's `src/` tree is NOT one binary:
        // nextest runs `src/lib.rs`'s tests in `pkg` and each
        // `src/bin/tool.rs`'s in `pkg::bin/tool`, so scanning the whole
        // tree and charging it to the lib let a bin's pg test be satisfied
        // by a same-named lib test that was in the group.
        let mut lib_src = None;
        let mut bins = Vec::new();
        for target in targets {
            if !target["test"].as_bool().unwrap_or(false) {
                continue;
            }
            let kinds = target_kinds(target);
            let name = target["name"].as_str().unwrap_or_default();
            let src = PathBuf::from(target["src_path"].as_str().unwrap_or_default());
            if kinds.contains(&"test") {
                let files = module_tree(&src, true)?;
                let evidence = scan(&files, root, Scope::Integration);
                if !evidence.is_empty() {
                    derived.push(Derived {
                        binary_id: format!("{package_name}::{name}"),
                        evidence,
                    });
                }
            } else if kinds.contains(&"lib") {
                lib_src = Some(src);
            } else {
                bins.push((name.to_string(), src));
            }
        }

        let mut charge = |binary_id: String, files: Vec<(SourceFile, FileFacts)>| {
            let evidence = scan(&files, root, Scope::UnitTree);
            if !evidence.is_empty() {
                derived.push(Derived {
                    binary_id,
                    evidence,
                });
            }
        };

        if let Some(src) = &lib_src {
            // `src/bin/` is nobody's lib module. Sweeping it into the lib
            // scan is what made a bin's test look like a lib test.
            let bin_dir = src.parent().map(|dir| dir.join("bin"));
            let files = unit_files(src, src.parent(), bin_dir.as_deref())?;
            charge(package_name.to_string(), files);
        }
        for (name, src) in bins {
            // A bin under `src/bin/` owns `src/bin/<stem>/` and nothing
            // else. A bin at `src/main.rs` shares the directory with the
            // lib, so only the modules it actually declares are its own —
            // a module BOTH roots declare is scanned into both targets,
            // which over-collects loudly rather than dropping it.
            let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let net = match src.parent() {
                Some(dir) if dir.ends_with("bin") => Some(dir.join(stem)),
                // No lib: the bin IS the package's unit-test target, so
                // the whole `src/` tree is its safety net.
                dir if lib_src.is_none() => dir.map(Path::to_path_buf),
                _ => None,
            };
            let files = unit_files(&src, net.as_deref(), None)?;
            charge(format!("{package_name}::bin/{name}"), files);
        }
    }
    derived.sort_by(|a, b| a.binary_id.cmp(&b.binary_id));
    Ok(derived)
}

/// Every file of a unit-test tree: the module walk from the crate root,
/// plus any other `.rs` under `net_root` the walk never reached, minus
/// anything under `exclude`.
///
/// The walk is the authority on module paths; the leftovers are a safety
/// net. A file no `mod` declaration reaches does not compile into the
/// binary at all, so including it can only cost a false failure — loud,
/// and preferable to trusting the walk to be complete.
fn unit_files(
    src: &Path,
    net_root: Option<&Path>,
    exclude: Option<&Path>,
) -> Result<Vec<(SourceFile, FileFacts)>, String> {
    let mut files = module_tree(src, false)?;
    let Some(source_root) = net_root else {
        return Ok(files);
    };
    let mut all = Vec::new();
    collect_rs(source_root, &mut all)?;
    all.sort();
    all.dedup();
    let seen: BTreeSet<PathBuf> = files.iter().map(|(file, _)| file.path.clone()).collect();
    for path in all {
        if seen.contains(&path) || exclude.is_some_and(|dir| path.starts_with(dir)) {
            continue;
        }
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let facts = file_facts(&text);
        files.push((
            SourceFile {
                prefix: path_prefix(source_root, &path),
                path,
                all_test: false,
            },
            facts,
        ));
    }
    files.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    Ok(files)
}

fn target_kinds(target: &Value) -> Vec<&str> {
    target["kind"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

/// One source file of a target, with the module path its contents live
/// under.
struct SourceFile {
    path: PathBuf,
    /// Module path from the crate root, `::`-terminated (`store::pg_tests::`),
    /// empty for the root file itself. This is what makes a derived test
    /// name spelled the way nextest spells it.
    prefix: String,
    /// Is the whole file test code? True for every file of an integration
    /// target, and for a `src/` file reached through a `#[cfg(test)] mod`
    /// declaration.
    all_test: bool,
}

/// Walk a target's module tree from its root file, following every
/// out-of-line `mod x;` declaration.
///
/// Following only `mod common;` was a silent hole: a `tests/root.rs` that
/// declared `mod cases;` put its postgres tests in a file the guard never
/// opened. Resolution follows rustc's rules, each one probed against rustc
/// rather than assumed:
///
/// * Children of the crate root file, and of a `mod.rs`, resolve in that
///   file's own directory. A `tests/root.rs` declaring `mod cases;` wants
///   `tests/cases.rs`, NOT `tests/root/cases.rs`.
/// * Children of any other file `foo.rs` resolve in `foo/`.
/// * An inline `mod outer { mod inner; }` adds `outer/` to that directory.
/// * `#[path = "..."]` resolves in the same directory, with ONE quirk: at
///   the TOP level of a non-root `foo.rs` it resolves against the file's
///   own directory instead of `foo/`. Nested inside an inline `mod`, it
///   goes back to the nesting rule.
///
/// A file already visited is skipped, but the key is the pair (path,
/// module prefix), not the path. One file can be mounted under two module
/// names, `#[path = "shared.rs"] mod a;` beside `#[path = "shared.rs"] mod
/// b;`, and rustc compiles its tests twice, once under each name. Keying on
/// the path alone scanned the text once and derived only one of the two
/// name sets.
///
/// A declaration that resolves to no file on disk is a hard error naming
/// the module. Skipping it silently is the one outcome this guard may not
/// have: the file it could not find is exactly where a pg test would hide.
fn module_tree(root_file: &Path, all_test: bool) -> Result<Vec<(SourceFile, FileFacts)>, String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = vec![(
        SourceFile {
            path: root_file.to_path_buf(),
            prefix: String::new(),
            all_test,
        },
        true,
    )];
    while let Some((file, is_root)) = queue.pop() {
        // Keyed on the module identity, not the file alone. `#[path =
        // "shared.rs"]` on two `mod` declarations compiles the same text
        // twice, and nextest names the tests once per mounting, so a
        // path-only key scans the file once and derives only whichever
        // mounting came off the stack first. The other mounting's names are
        // then never checked against the group. Test scope is part of the
        // identity too: cfg-alternated mountings share a path AND a name
        // (`#[cfg(test)] mod shared;` beside `#[cfg(feature = "x")] mod
        // shared;`), and a key without `all_test` lets whichever mounting
        // pops last discard the test-scoped scan of the other.
        if !seen.insert((file.path.clone(), file.prefix.clone(), file.all_test)) {
            continue;
        }
        let text = fs::read_to_string(&file.path)
            .map_err(|e| format!("cannot read {}: {e}", file.path.display()))?;
        let facts = file_facts(&text);
        for decl in &facts.mods {
            let children = resolve_mod(&file.path, is_root, decl);
            if children.is_empty() {
                return Err(format!(
                    "{} declares `mod {};` and no file resolves it. \
                     The guard cannot scan what it cannot find.",
                    file.path.display(),
                    decl.name
                ));
            }
            let mut prefix = file.prefix.clone();
            for module in &decl.chain {
                prefix.push_str(module);
                prefix.push_str("::");
            }
            prefix.push_str(&decl.name);
            prefix.push_str("::");
            for path in children {
                queue.push((
                    SourceFile {
                        path,
                        prefix: prefix.clone(),
                        all_test: file.all_test || decl.cfg_test,
                    },
                    false,
                ));
            }
        }
        out.push((file, facts));
    }
    out.sort_by(|a, b| {
        (&a.0.path, &a.0.prefix, a.0.all_test).cmp(&(&b.0.path, &b.0.prefix, b.0.all_test))
    });
    Ok(out)
}

/// The file(s) an out-of-line `mod name;` can live in. Both candidates are
/// returned when both exist — that is a compile error in the crate, and
/// scanning both is the conservative answer.
fn resolve_mod(parent: &Path, is_root: bool, decl: &ModDecl) -> Vec<PathBuf> {
    let own_dir = parent.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut dir = if decl.path.is_some() && decl.chain.is_empty() {
        own_dir
    } else {
        child_dir(parent, is_root)
    };
    for module in &decl.chain {
        dir = dir.join(module);
    }
    if let Some(path) = &decl.path {
        let child = dir.join(path);
        return if child.is_file() {
            vec![child]
        } else {
            Vec::new()
        };
    }
    [
        dir.join(format!("{}.rs", decl.name)),
        dir.join(&decl.name).join("mod.rs"),
    ]
    .into_iter()
    .filter(|candidate| candidate.is_file())
    .collect()
}

/// The directory a file's out-of-line children resolve in, before inline
/// `mod` nesting is applied.
fn child_dir(file: &Path, is_root: bool) -> PathBuf {
    let dir = file.parent().unwrap_or(Path::new(""));
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if is_root || stem == "mod" {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    }
}

/// The module prefix a file's PATH implies, for a `src/` file the module
/// walk never reached: `src/foo.rs` is `foo::`, `src/foo/bar.rs` is
/// `foo::bar::`, `src/foo/mod.rs` is `foo::`, the root file is empty.
///
/// This is the safety net under [`module_tree`], not a substitute for it:
/// it cannot see `#[path]` or inline nesting. A file it names wrongly costs
/// a false failure, which is loud. A file left out costs a silent escape,
/// which is the one thing the guard may not do.
fn path_prefix(source_root: &Path, file: &Path) -> String {
    let Ok(rest) = file.strip_prefix(source_root) else {
        return String::new();
    };
    let mut prefix = String::new();
    for component in rest.parent().into_iter().flat_map(Path::components) {
        prefix.push_str(&component.as_os_str().to_string_lossy());
        prefix.push_str("::");
    }
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if !matches!(stem, "mod" | "lib" | "main") {
        prefix.push_str(stem);
        prefix.push_str("::");
    }
    prefix
}

/// Every `.rs` file under `dir`, following directory symlinks, under EVERY
/// path that reaches it.
///
/// The problem a symlink causes here is a CYCLE, not the symlink itself: a
/// link pointing at one of its own ancestors is a directory forever and the
/// walk descends it until the OS refuses a longer path. Refusing to follow
/// links instead drops whatever lives behind one, and behind one is exactly
/// where the safety net has to look. Rustc compiles
/// `include!("linked/mod.rs")` through a symlinked directory, and the module
/// walk cannot see an `include!` edge at all, so nobody else opens that file.
///
/// The cycle check is the ANCESTOR PATH, not a set of everything visited. A
/// global set keeps whichever lexical path happened to reach a real directory
/// first, and [`unit_files`] decides target ownership on the lexical path
/// afterwards: a library excludes `src/bin`, so if `read_dir` yielded `bin`
/// before the `src/shared -> bin/shared` link, the only recorded path was the
/// excluded one and the file was scanned by nobody. Every acyclic alias is
/// walked, and a file reachable two ways is collected twice, once per path.
/// Duplicate lexical paths cost a duplicate finding at worst, which is loud;
/// a dropped alias is a pg test nobody counted.
///
/// A ROOT that does not exist is an empty sweep, by design: [`unit_files`]
/// derives one sweep directory per target, and a bin whose source is a
/// single file has none. Every other filesystem failure, at the root or
/// below it, is an error, never an empty directory: a directory the walk
/// cannot canonicalize or list is one whose tests nobody scans, and a
/// silent skip is the one defect this module refuses.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    match fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(format!(
                "pg-admission-guard: cannot stat {}: {e}",
                dir.display()
            ));
        }
        Ok(_) => {}
    }
    let mut ancestors = Vec::new();
    collect_rs_acyclic(dir, &mut ancestors, out)
}

fn collect_rs_acyclic(
    dir: &Path,
    ancestors: &mut Vec<PathBuf>,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let real = fs::canonicalize(dir).map_err(|e| {
        format!(
            "pg-admission-guard: cannot canonicalize {}: {e}",
            dir.display()
        )
    })?;
    if ancestors.contains(&real) {
        return Ok(());
    }
    ancestors.push(real);
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("pg-admission-guard: cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("pg-admission-guard: cannot list {}: {e}", dir.display()))?
            .path();
        if path.is_dir() {
            collect_rs_acyclic(&path, ancestors, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    ancestors.pop();
    Ok(())
}

/// Which tree the sources come from. The connection-API class applies to
/// integration test files only (see [`CONNECTION_APIS`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Integration,
    UnitTree,
}

/// Scan a target's sources for pg evidence.
fn scan(files: &[(SourceFile, FileFacts)], root: &Path, scope: Scope) -> Vec<Evidence> {
    let mut evidence = Vec::new();
    for (source, facts) in files {
        let shown = source
            .path
            .strip_prefix(root)
            .unwrap_or(&source.path)
            .display();
        let prefix = source.prefix.as_str();

        let tests = sqlx_tests(facts, prefix);
        if let Some((line, _)) = tests.first() {
            evidence.push(Evidence {
                site: format!("{shown}:{line} #[sqlx::test]"),
                tests: tests.iter().map(|(_, name)| name.clone()).collect(),
            });
        }

        // The fixture side: this file IS the pg harness (it defines the
        // pool constructor), and the module walk only reached it because
        // the target declares `mod common;`.
        if facts.code.contains(FIXTURE_MARKER) && source.path.ends_with(Path::new("common/mod.rs"))
        {
            evidence.push(Evidence {
                site: format!("{shown} (postgres fixture module)"),
                tests: Vec::new(),
            });
        }

        // The raw-connection side: no attribute, no fixture, just a test
        // that dials postgres itself. An integration file is all test code
        // and so is an out-of-line test module's file; a `src/` file is
        // production code, where only the `#[cfg(test)]` bodies count.
        if source.all_test {
            // An integration target's finding stays target-level on
            // purpose: nothing there says WHICH tests the connection
            // belongs to, so the whole binary is charged.
            let tests = if scope == Scope::Integration {
                Vec::new()
            } else {
                test_names(facts, prefix, None)
            };
            if let Some((line, api)) = connection_site(facts, 1, usize::MAX) {
                evidence.push(Evidence {
                    site: format!("{shown}:{line} {api}"),
                    tests,
                });
            }
        } else {
            // A test fn's own body counts too. Without it a `#[test]` or
            // `#[tokio::test]` written at a `src/` file's top level, under
            // no `#[cfg(test)]` at all, had its connection read as
            // production code and nothing was collected.
            for span in facts.cfg_test_spans.iter().chain(&facts.test_fn_spans) {
                if let Some((line, api)) = connection_site(facts, span.0, span.1) {
                    evidence.push(Evidence {
                        site: format!("{shown}:{line} {api}"),
                        tests: test_names(facts, prefix, Some(*span)),
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
fn sqlx_tests(facts: &FileFacts, prefix: &str) -> Vec<(usize, String)> {
    facts
        .fns
        .iter()
        .filter(|item| item.is_sqlx_test && !item.ignored)
        .map(|item| (item.line, format!("{prefix}{}", item.name)))
        .collect()
}

/// Names of the non-ignored test functions of a file, or of one span of it.
///
/// Helper `fn`s are left out — they are not test ids, and asking nextest
/// about one would fail the guard on a name that can never be in the group.
/// Ignored ones are left out for the same reason `sqlx_tests` drops them.
fn test_names(facts: &FileFacts, prefix: &str, span: Option<Span>) -> Vec<String> {
    facts
        .fns
        .iter()
        .filter(|item| item.is_test && !item.ignored)
        .filter(|item| span.is_none_or(|(start, end)| item.line >= start && item.line <= end))
        .map(|item| format!("{prefix}{}", item.name))
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
    /// (`pg_tests::resolve_all`). The file's own path from the crate root
    /// is added later, by [`SourceFile::prefix`], which is what makes the
    /// full name comparable to a nextest id.
    name: String,
    /// Does the block carry an attribute whose last path segment is `test`
    /// (`#[test]`, `#[tokio::test]`, `#[sqlx::test]`)?
    is_test: bool,
    is_sqlx_test: bool,
    ignored: bool,
}

/// An out-of-line `mod name;` declaration.
struct ModDecl {
    name: String,
    /// The `#[path = "..."]` override, when one is written.
    path: Option<String>,
    /// The inline `mod` names it is nested inside, outermost first. They
    /// are part of both the module path and the directory the child file
    /// resolves in.
    chain: Vec<String>,
    /// Is the declared module test-only? True for its own `#[cfg(test)]`
    /// and for a declaration sitting anywhere inside a `#[cfg(test)]`
    /// region, which inherits the gate without carrying the attribute.
    cfg_test: bool,
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
    /// The same arguments as WRITTEN, for the one attribute whose value has
    /// to be read back (`#[path = "child.rs"]`).
    raw_args: String,
}

/// Everything the guard reads out of one source file.
struct FileFacts {
    /// The comment- and literal-blanked text: byte for byte as long as the
    /// original, with every newline in place.
    code: String,
    fns: Vec<FnItem>,
    cfg_test_spans: Vec<Span>,
    /// Bodies of fns carrying a test attribute, minus the ones a
    /// `#[cfg(test)]` span already covers. Keeping the covered ones would
    /// report one connection twice for the ordinary `#[cfg(test)] mod tests`
    /// shape.
    test_fn_spans: Vec<Span>,
    /// Every out-of-line `mod name;` in the file, test-only or not.
    mods: Vec<ModDecl>,
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
        raw: text,
        bytes: code.as_bytes(),
        i: 0,
        line: 1,
        depth: 0,
        mods: Vec::new(),
        open_spans: Vec::new(),
        pending: None,
        fns: Vec::new(),
        spans: Vec::new(),
        test_fn_spans: Vec::new(),
        decls: Vec::new(),
    };
    lexer.run();
    let (fns, spans, decls) = (lexer.fns, lexer.spans, lexer.decls);
    let test_fn_spans = lexer
        .test_fn_spans
        .into_iter()
        .filter(|(start, end)| {
            !spans
                .iter()
                .any(|(outer, close)| outer <= start && end <= close)
        })
        .collect();
    FileFacts {
        code,
        fns,
        cfg_test_spans: spans,
        test_fn_spans,
        mods: decls,
    }
}

/// The item lexer. It knows four things — attributes, braces, `fn` and
/// `mod` — and treats everything else as "some item that takes the pending
/// attribute block".
struct ItemLexer<'a> {
    /// The file as written. Only `#[path = "..."]` reads it, because
    /// blanking eats the very string it needs.
    raw: &'a str,
    /// The blanked text, the same length as `raw`.
    bytes: &'a [u8],
    i: usize,
    /// 1-based line of `i`.
    line: usize,
    depth: usize,
    /// Enclosing inline `mod` names, each with the depth its body opened at.
    mods: Vec<(String, usize)>,
    /// Open test regions: the line each started on, the depth it opened at,
    /// and the list it closes into.
    open_spans: Vec<(usize, usize, SpanKind)>,
    /// The attribute block being accumulated: the line it started on, and
    /// the attributes in it.
    pending: Option<(usize, Vec<Attr>)>,
    fns: Vec<FnItem>,
    spans: Vec<Span>,
    test_fn_spans: Vec<Span>,
    decls: Vec<ModDecl>,
}

/// Which list a closed span belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpanKind {
    /// A `#[cfg(test)]` item's body.
    CfgTest,
    /// The body of a fn carrying a test attribute.
    TestFn,
}

/// How [`ItemLexer::skip_to_body`] left an attributed item.
enum ItemHead {
    /// A body brace was found and consumed. The item's extent is the block
    /// that now follows, and the main loop's depth bookkeeping closes it.
    Body,
    /// The item ended at a `;` with no body, on the line reported here.
    Bodyless(usize),
    /// The enclosing block closed first, or the file ended mid-item.
    None,
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
                    self.depth += 1;
                    self.bump();
                }
                b'}' => self.close_block(),
                b'_' | b'a'..=b'z' | b'A'..=b'Z' => self.word(),
                // Anything else ends the attribute block without taking
                // it. A `cfg(test)` one is already spent by then: the item
                // that took it walked its own signature to its body (see
                // [`ItemLexer::take_pending`]), so a separator here can no
                // longer clear a span that has not opened yet.
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

    /// One identifier, raw ones (`r#type`) read whole.
    ///
    /// The `r#` prefix is part of the token: `r#fn` is a name, not the `fn`
    /// keyword, and lexing it as `r` followed by `#` would hand the `#` to
    /// [`ItemLexer::attribute`]. A raw STRING cannot be confused with one,
    /// because blanking already turned `r#"..."#` into spaces.
    fn ident(&mut self) -> Option<String> {
        let start = self.i;
        if self.peek() == Some(b'r')
            && self.bytes.get(self.i + 1) == Some(&b'#')
            && self
                .bytes
                .get(self.i + 2)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        {
            self.bump();
            self.bump();
        }
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
            // `#![...]` belongs to the ENCLOSING item, not the next one, so
            // it never joins the pending block. A `#![cfg(test)]` at the top
            // of an inline `mod m { .. }` is how that module says its whole
            // body is test code, and the cursor is already inside that body:
            // open the span here, at the depth the body was entered on.
            // Without it the module was scanned as production code and every
            // connection in it was invisible.
            if self.depth > 0 && is_cfg_test(&self.parse_attr(from, to)) {
                self.open_spans
                    .push((line, self.depth - 1, SpanKind::CfgTest));
            }
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
            path: despace(&inner[..split]),
            args: inner[split..].to_string(),
            raw_args: self
                .raw
                .get(from + split..to)
                .unwrap_or_default()
                .to_string(),
        }
    }

    fn close_block(&mut self) {
        self.pending = None;
        self.depth = self.depth.saturating_sub(1);
        while self
            .mods
            .last()
            .is_some_and(|(_, depth)| *depth >= self.depth)
        {
            self.mods.pop();
        }
        while let Some(&(start, depth, kind)) = self.open_spans.last() {
            if depth < self.depth {
                break;
            }
            self.open_spans.pop();
            self.record_span(kind, (start, self.line));
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

    /// Take the pending attribute block and, when it carries `#[cfg(test)]`
    /// or a test attribute, open a span over the attributed item's BODY.
    ///
    /// The cursor sits inside the item's signature here (just past `fn
    /// name`, past `impl`, past `use`), so the body has to be found by
    /// walking that signature. Letting the main loop do it lost spans two
    /// ways, both of them silent: a comma or semicolon anywhere in the
    /// signature (`fn helper(_: u8, _: u8)`, `fn f(x: [u8; 4])`) cleared
    /// the remembered `cfg(test)`, and the first `{` was taken as the body
    /// even when it was a const-generic argument (`fn f() -> Foo<{ 1 }>`).
    fn take_pending(&mut self, line: usize) -> (usize, Vec<Attr>) {
        let (attr_line, attrs) = self.pending.take().unwrap_or((line, Vec::new()));
        // A test fn's body is test code wherever it is written, `#[cfg(test)]`
        // above it or not, so it opens a span of its own kind.
        let kind = if attrs.iter().any(is_cfg_test) {
            Some(SpanKind::CfgTest)
        } else if attrs.iter().any(is_test_attr) {
            Some(SpanKind::TestFn)
        } else {
            None
        };
        if let Some(kind) = kind {
            match self.skip_to_body() {
                ItemHead::Body => {
                    self.open_spans.push((attr_line, self.depth, kind));
                    self.depth += 1;
                }
                // No body to bound the span with, so the item's own extent
                // is the span: first attribute line through the semicolon.
                // A `#[cfg(test)] static POOL: LazyLock<PgPool> =
                // LazyLock::new(|| { .. connect_lazy .. });` has its whole
                // connection inside a closure the signature walk consumed
                // as a group, and reporting no span at all made that pool
                // invisible.
                ItemHead::Bodyless(end) => self.record_span(kind, (attr_line, end)),
                ItemHead::None => {}
            }
        }
        (attr_line, attrs)
    }

    /// File one closed span under the list its kind names.
    fn record_span(&mut self, kind: SpanKind, span: Span) {
        match kind {
            SpanKind::CfgTest => self.spans.push(span),
            SpanKind::TestFn => self.test_fn_spans.push(span),
        }
    }

    /// Walk an item's signature to its body brace, consuming it. An item
    /// that ends at a `;` instead reports the line that semicolon sits on,
    /// which is where its extent stops.
    ///
    /// `(`/`[` depth is what keeps a signature comma or semicolon from
    /// reading as the end of the item. Angle depth is what keeps a
    /// const-generic brace from reading as the body: at angle depth > 0 a
    /// `{` is an argument and is skipped as a balanced group.
    ///
    /// Angle tracking is deliberately crude — `->` and `=>` are stepped
    /// over so their `>` does not close a bracket that never opened, and
    /// `>>` closes two. A signature-level `<` that is not a generic opener
    /// does not occur in Rust, so nothing else is tracked.
    ///
    /// A `=` at group and angle depth 0 says the rest of the item is an
    /// INITIALIZER, so every brace after it is an expression brace and is
    /// skipped as a balanced group rather than read as a body. No item
    /// with a real body carries such a `=` first: an `impl`, `struct`,
    /// `trait` or `fn` head has none, and an associated type's `=` sits at
    /// angle depth 1 (`Iterator<Item = u8>`). Without the rule an
    /// initializer like `= if c { a } else { connect_lazy(..) };` had its
    /// span closed at the first `}`, leaving the second arm uncovered.
    fn skip_to_body(&mut self) -> ItemHead {
        let mut group = 0usize;
        let mut angle = 0usize;
        let mut initializer = false;
        loop {
            self.skip_trivia();
            let Some(byte) = self.peek() else {
                return ItemHead::None;
            };
            match byte {
                b'(' | b'[' => {
                    group += 1;
                    self.bump();
                }
                b')' | b']' => {
                    group = group.saturating_sub(1);
                    self.bump();
                }
                b'{' => {
                    if group > 0 || angle > 0 || initializer {
                        self.group(b'{', b'}');
                    } else {
                        self.bump();
                        return ItemHead::Body;
                    }
                }
                // The enclosing block ended before this item did. Leave the
                // brace for the main loop, which owns the depth bookkeeping.
                b'}' => return ItemHead::None,
                b';' if group == 0 => {
                    let line = self.line;
                    self.bump();
                    return ItemHead::Bodyless(line);
                }
                b'-' | b'=' if self.bytes.get(self.i + 1) == Some(&b'>') => {
                    self.bump();
                    self.bump();
                }
                b'=' => {
                    if group == 0 && angle == 0 {
                        initializer = true;
                    }
                    self.bump();
                }
                b'<' => {
                    angle += 1;
                    self.bump();
                }
                b'>' => {
                    self.bump();
                    if angle >= 2 && self.peek() == Some(b'>') {
                        angle -= 2;
                        self.bump();
                    } else {
                        angle = angle.saturating_sub(1);
                    }
                }
                _ => self.bump(),
            }
        }
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
            is_sqlx_test: attrs.iter().any(is_sqlx_test),
            // `#[ignore]` and nothing else. A `#[cfg_attr(slow, ignore)]`
            // is ignored under some cfgs and not others, and no static
            // reading can say which one CI compiled, so the test is KEPT:
            // a name nextest never lists fails the guard loudly, while a
            // name dropped here is a pg test running outside the group
            // with nobody to say so.
            ignored: attrs.iter().any(|attr| attr.path == "ignore"),
        });
    }

    fn module(&mut self, line: usize) {
        self.skip_trivia();
        let name = self.ident().unwrap_or_default();
        let (attr_line, attrs) = self.pending.take().unwrap_or((line, Vec::new()));
        let cfg_test = attrs.iter().any(is_cfg_test);
        self.skip_trivia();
        match self.peek() {
            Some(b'{') => {
                self.bump();
                if cfg_test {
                    self.open_spans
                        .push((attr_line, self.depth, SpanKind::CfgTest));
                }
                self.mods.push((name, self.depth));
                self.depth += 1;
            }
            Some(b';') => {
                self.bump();
                self.decls.push(ModDecl {
                    name,
                    path: path_attr(&attrs),
                    chain: self.mods.iter().map(|(name, _)| name.clone()).collect(),
                    // An open test region above the declaration gates it
                    // just as its own attribute would, and a declaration
                    // inside `#[cfg(test)] mod outer { .. }` carries no
                    // attribute of its own.
                    cfg_test: cfg_test || !self.open_spans.is_empty(),
                });
            }
            _ => {}
        }
    }

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

/// Does this attribute install the sqlx test harness, which builds a
/// postgres pool for the function beneath it?
///
/// Written directly, or wrapped in a `cfg_attr` at any depth. Textual
/// containment is enough on purpose: the guard cannot know which cfgs CI
/// compiled under, so it treats a conditional `sqlx::test` as a real one.
/// That over-collects when the cfg is off, and over-collecting names a
/// test nextest may not list, which fails the guard loudly. The reverse
/// reading would be a pg pool nobody counted.
fn is_sqlx_test(attr: &Attr) -> bool {
    attr.path == "sqlx::test" || despace(&attr.args).contains("sqlx::test")
}

/// The text with every whitespace byte removed, so `sqlx :: test` reads
/// the same as `sqlx::test`.
fn despace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `#[cfg(test)]`, and only a bare `test` predicate: the arguments are read
/// from the BLANKED text, so `#[cfg(feature = "test")]` has no `test` token
/// left in it to find.
///
/// A `cfg_attr` whose expansion contains `cfg(test)` counts as one, for the
/// same reason [`is_sqlx_test`] reads a wrapped `sqlx::test`: the guard
/// cannot know which cfgs CI compiled under, and the item is test code
/// whenever the predicate holds. Reading it as production code is a
/// connection nobody scanned. `#[cfg_attr(test, ignore)]` is untouched by
/// this: its expansion is `ignore`, not `cfg(test)`.
fn is_cfg_test(attr: &Attr) -> bool {
    if attr.path == "cfg" {
        return has_ident(&attr.args, "test");
    }
    attr.path == "cfg_attr" && despace(&attr.args).contains("cfg(test)")
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

/// The `#[path = "child.rs"]` value, read from the attribute AS WRITTEN
/// (blanking eats the string it names).
fn path_attr(attrs: &[Attr]) -> Option<String> {
    let attr = attrs.iter().find(|attr| attr.path == "path")?;
    let start = attr.raw_args.find('"')? + 1;
    let end = attr.raw_args[start..].find('"')? + start;
    Some(attr.raw_args[start..end].to_string())
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

    /// Every `#[sqlx::test]` a file declares, unqualified.
    fn sqlx_names(text: &str) -> Vec<(usize, String)> {
        sqlx_tests(&file_facts(text), "")
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
                    .map(|(line, api)| (line, api, test_names(&facts, "", Some(*span))))
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
        let lib = target(vec![named_evidence(&[
            "from_saved::tests::run_all",
            "from_saved::tests::resolve_one",
        ])]);
        let listing = suite(
            &["from_saved::tests::run_all", "unrelated::unit_test"],
            &["from_saved::tests::run_all"],
        );
        let failure = check(&lib, Some(&listing)).expect("resolve_one is outside the group");
        assert!(failure.contains("resolve_one"), "{failure}");
        assert!(!failure.contains("unrelated"), "{failure}");
    }

    /// The masking escape, end to end through `check`: two files declare
    /// the same `tests::connects`, only one of them opens a pool. A
    /// trailing-segment match let the grouped one satisfy the ungrouped
    /// one and the guard reported a pass.
    #[test]
    fn a_same_named_test_in_another_module_does_not_satisfy_this_one() {
        let masked = target(vec![named_evidence(&["a::tests::connects"])]);
        let listing = suite(
            &["a::tests::connects", "b::tests::connects"],
            &["b::tests::connects"],
        );
        let failure = check(&masked, Some(&listing)).expect("a::tests::connects escaped");
        assert!(failure.contains("a::tests::connects"), "{failure}");

        // The right one in the group is a pass.
        let listing = suite(
            &["a::tests::connects", "b::tests::connects"],
            &["a::tests::connects"],
        );
        assert!(check(&masked, Some(&listing)).is_none());
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
    /// ambiguous across modules.
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

    /// Two connecting regions in one file, both reported. The old
    /// `find_map` stopped at the first, so a second test module could dial
    /// postgres unseen behind a module that already had a finding.
    #[test]
    fn two_connecting_spans_are_two_findings() {
        let text = format!(
            "#[cfg(test)]\nmod first_tests {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n\n#[cfg(test)]\nmod second_tests {{\n    #[tokio::test]\n    async fn dials() {{\n        let k = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
            call("KeyStore", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!(sites.len(), 2, "{sites:?}");
        assert_eq!((sites[0].0, sites[0].1), (5, "StorageState::connect"));
        assert_eq!(sites[0].2, vec!["first_tests::boots".to_string()]);
        assert_eq!((sites[1].0, sites[1].1), (13, "KeyStore::connect"));
        assert_eq!(sites[1].2, vec!["second_tests::dials".to_string()]);
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

    /// A comma in the attributed item's OWN signature used to clear the
    /// remembered `#[cfg(test)]`, so the body never opened a span and a
    /// helper that dials postgres was invisible. Any signature separator
    /// does it: a comma between parameters, a `;` inside an array type.
    #[test]
    fn a_signature_separator_does_not_lose_the_cfg_test_span() {
        for signature in [
            "async fn helper(_: u8, _: u8)",
            "async fn helper(_: [u8; 4])",
            "async fn helper<A, B>(_: A, _: B)",
            "async fn helper(_: (u8, u8)) -> Result<Store, Error>",
        ] {
            let text = format!(
                "#[cfg(test)]\n{signature} {{\n    let s = {}\"...\").await;\n}}\n",
                call("KeyStore", "connect"),
            );
            let sites = cfg_test_sites(&text);
            assert_eq!(sites.len(), 1, "{signature}: {sites:?}");
            assert_eq!(
                (sites[0].0, sites[0].1),
                (3, "KeyStore::connect"),
                "{signature}"
            );
        }
    }

    /// A const-generic argument in the return type is a `{` that is not the
    /// body. Taking it as one ended the span before the real body, so the
    /// connection below it was outside every span.
    #[test]
    fn a_const_generic_brace_is_not_the_body() {
        let text = format!(
            "#[cfg(test)]\nasync fn helper() -> Fixed<{{ 1 + 1 }}> {{\n    let s = {}\"...\").await;\n    Fixed\n}}\n",
            call("KeyStore", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!((sites[0].0, sites[0].1), (3, "KeyStore::connect"));
    }

    /// A raw identifier is one token. Lexing `r#mod` as `r` then `#` fed
    /// the `#` to the attribute reader and left a bare `mod` keyword behind
    /// it, so the module never opened a span and the connection inside was
    /// invisible.
    #[test]
    fn a_raw_identifier_module_name_still_opens_a_span() {
        let text = format!(
            "#[cfg(test)]\nmod r#mod {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!((sites[0].0, sites[0].1), (5, "StorageState::connect"));
        assert_eq!(sites[0].2, vec!["r#mod::boots".to_string()]);
    }

    /// A bodyless `#[cfg(test)]` item ends at its own `;`. It must not
    /// swallow the span of the module that follows it.
    #[test]
    fn a_bodyless_cfg_test_item_does_not_eat_the_next_span() {
        let text = format!(
            "#[cfg(test)]\nuse std::sync::Arc;\n\n#[cfg(test)]\nmod pg_tests {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!((sites[0].0, sites[0].1), (8, "StorageState::connect"));
        assert_eq!(sites[0].2, vec!["pg_tests::boots".to_string()]);
    }

    /// A `LazyLock` pool is a bodyless item, and its whole connection sits
    /// inside a closure the signature walk consumes as a balanced group.
    /// The walk used to reach the terminating `;` and report no span at
    /// all, so the pool was invisible: a test that acquires from it names
    /// no connection API of its own, and nothing else in the file does
    /// either.
    #[test]
    fn a_lazy_static_pool_is_covered_by_its_own_span() {
        let pool = format!("Pg{}", "Pool");
        let lazy = format!("connect{}", "_lazy");
        let text = format!(
            "#[cfg(test)]\nstatic POOL: LazyLock<{pool}> = LazyLock::new(|| {{\n    {lazy}(\"postgres://x\")\n}});\n",
        );
        let facts = file_facts(&text);
        // Attribute line through the semicolon, and no further.
        assert_eq!(facts.cfg_test_spans, vec![(1, 4)]);
        let span = facts.cfg_test_spans[0];
        // The type on line 2 names `PgPool`, so ask the span about the
        // initializer line specifically.
        assert_eq!(
            connection_site(&facts, 3, span.1),
            Some((3, "connect_lazy"))
        );
    }

    /// An initializer can open more than one brace group at the item's own
    /// depth. Reading the first one as a body closed the span at its `}`,
    /// so the second arm fell outside every span.
    #[test]
    fn an_initializer_with_two_brace_groups_is_covered_whole() {
        let lazy = format!("connect{}", "_lazy");
        let text = format!(
            "#[cfg(test)]\nstatic POOL: Lazy = if stubbed() {{\n    stub()\n}} else {{\n    {lazy}(\"postgres://x\")\n}};\n",
        );
        let facts = file_facts(&text);
        assert_eq!(facts.cfg_test_spans, vec![(1, 6)]);
        assert_eq!(cfg_test_sites(&text), vec![(5, "connect_lazy", Vec::new())]);
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

    /// An inline module can gate itself from the INSIDE. The attribute is
    /// an inner one, so it never joins the pending block, and the module was
    /// read as production code with every connection in it invisible.
    #[test]
    fn an_inner_cfg_test_attribute_opens_the_module_body_as_a_span() {
        let text = format!(
            "mod pg_tests {{\n    #![cfg(test)]\n\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n\nasync fn after() {{\n    let s = {}\"...\").await;\n}}\n",
            call("StorageState", "connect"),
            call("KeyStore", "connect"),
        );
        let sites = cfg_test_sites(&text);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!((sites[0].0, sites[0].1), (6, "StorageState::connect"));
        assert_eq!(sites[0].2, vec!["pg_tests::boots".to_string()]);
    }

    /// A `cfg_attr` that expands to `cfg(test)` gates its item as a written
    /// `#[cfg(test)]` does whenever the predicate holds, and the guard cannot
    /// know whether CI compiled with it. Over-collecting is the fail
    /// direction; reading it as production code was a connection nobody
    /// scanned.
    #[test]
    fn a_cfg_attr_expanding_to_cfg_test_opens_a_span() {
        for wrapper in [
            "#[cfg_attr(feature = \"pg\", cfg(test))]",
            "#[cfg_attr(unix, cfg_attr(feature = \"pg\", cfg(test)))]",
        ] {
            let text = format!(
                "{wrapper}\nmod pg_tests {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
                call("StorageState", "connect"),
            );
            let sites = cfg_test_sites(&text);
            assert_eq!(sites.len(), 1, "{wrapper}: {sites:?}");
            assert_eq!(
                (sites[0].0, sites[0].1),
                (5, "StorageState::connect"),
                "{wrapper}"
            );
        }

        // `#[cfg_attr(test, ignore)]` expands to `ignore`, not to
        // `cfg(test)`, so it gates nothing and opens no span.
        let text = format!(
            "#[cfg_attr(test, ignore)]\nasync fn helper() {{\n    let s = {}\"...\").await;\n}}\n",
            call("StorageState", "connect"),
        );
        assert!(cfg_test_sites(&text).is_empty());
    }

    /// A non-ASCII fn name never lexes, so the guard cannot name the test.
    /// The connection is still COLLECTED: the attribute block is taken
    /// before the name is read, so the body opens a span, and a finding that
    /// names no test charges the whole binary. Loud, which is the accepted
    /// direction.
    #[test]
    fn a_non_ascii_test_fn_is_collected_without_a_name() {
        let text = format!(
            "#[tokio::test]\nasync fn \u{6771}\u{4eac}() {{\n    let s = {}\"...\").await;\n}}\n",
            call("StorageState", "connect"),
        );
        let facts = file_facts(&text);
        assert!(facts.fns.is_empty(), "the name is not lexable");
        assert_eq!(facts.test_fn_spans, vec![(1, 4)]);
        assert_eq!(
            connection_site(&facts, 1, 4),
            Some((3, "StorageState::connect"))
        );
        assert!(
            test_names(&facts, "", Some((1, 4))).is_empty(),
            "no name to charge, so the finding is target-level"
        );
    }

    /// The other half of the same residual, and this one IS silent: a
    /// non-ASCII module name stops `module` before it can see the body
    /// brace, so the `#[cfg(test)]` above it opens nothing and the
    /// connection inside reads as production code. Pinned so a later fix
    /// has to come here and delete this test.
    #[test]
    fn a_non_ascii_module_name_opens_no_span() {
        let text = format!(
            "#[cfg(test)]\nmod \u{6771}\u{4eac} {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
        );
        let facts = file_facts(&text);
        assert!(
            facts.cfg_test_spans.is_empty(),
            "{:?}",
            facts.cfg_test_spans
        );
        // The inner `#[tokio::test]` fn is still a span of its own, so the
        // connection is collected even here. What the module name costs is
        // the `#[cfg(test)]` region, and the qualification of the name.
        assert_eq!(facts.test_fn_spans, vec![(3, 6)]);
        assert_eq!(
            facts
                .fns
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["boots"]
        );

        // The silent half: a connection in that module that sits outside a
        // test fn body is in no span at all, so nothing collects it.
        let helper = format!(
            "#[cfg(test)]\nmod \u{6771}\u{4eac} {{\n    async fn helper() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
        );
        let facts = file_facts(&helper);
        assert!(facts.cfg_test_spans.is_empty());
        assert!(facts.test_fn_spans.is_empty());
    }

    /// The path a file sits at is the module path its tests are named
    /// under, and that is what makes an exact match possible.
    #[test]
    fn a_files_path_gives_its_module_prefix() {
        let src = Path::new("/w/src");
        assert_eq!(path_prefix(src, Path::new("/w/src/lib.rs")), "");
        assert_eq!(path_prefix(src, Path::new("/w/src/foo.rs")), "foo::");
        assert_eq!(path_prefix(src, Path::new("/w/src/foo/mod.rs")), "foo::");
        assert_eq!(
            path_prefix(src, Path::new("/w/src/foo/bar.rs")),
            "foo::bar::"
        );
    }

    /// A scratch directory that removes itself. The out-of-line module
    /// class resolves a declaration to a FILE, so it needs real ones.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("pg-admission-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }

    fn derived_sites(derived: &[Derived]) -> Vec<String> {
        derived
            .iter()
            .map(|d| format!("{}: {}", d.binary_id, d.sites()))
            .collect()
    }

    fn sites(evidence: &[Evidence]) -> Vec<&str> {
        evidence.iter().map(|e| e.site.as_str()).collect()
    }

    fn write(path: &Path, text: &str) {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).expect("scratch subdir");
        }
        fs::write(path, text).expect("scratch file");
    }

    /// `#[cfg(test)] mod pg_tests;` puts the tests in another file, and
    /// that file carries no `#[cfg(test)]` of its own — so a span scan of
    /// it finds nothing at all and the connection is invisible. The child
    /// is all test code, so it is scanned whole, under the module name its
    /// parent gave it.
    #[test]
    fn an_out_of_line_cfg_test_module_is_scanned_whole() {
        let dir = scratch("out-of-line");
        let root = &dir.0;
        let parent = root.join("src").join("store.rs");
        let child = root.join("src").join("store").join("pg_tests.rs");
        write(
            &parent,
            "pub struct Store;\n\n#[cfg(test)]\nmod pg_tests;\n",
        );
        write(
            &child,
            &format!(
                "#[tokio::test]\nasync fn connects() {{\n    let s = {}\"...\").await;\n}}\n\n#[tokio::test]\n#[ignore]\nasync fn shelved() {{}}\n",
                call("StorageState", "connect"),
            ),
        );

        write(&root.join("src").join("lib.rs"), "mod store;\n");
        let files = unit_files(&root.join("src").join("lib.rs"), None, None).expect("module tree");
        let evidence = scan(&files, root, Scope::UnitTree);
        assert_eq!(
            evidence.len(),
            1,
            "{:?}",
            evidence.iter().map(|e| &e.site).collect::<Vec<_>>()
        );
        assert_eq!(
            evidence[0].site,
            "src/store/pg_tests.rs:3 StorageState::connect"
        );
        assert_eq!(
            evidence[0].tests,
            vec!["store::pg_tests::connects".to_string()]
        );
        let _ = (parent, child);
    }

    /// The `mod.rs` form of the same thing, and a `#[path]` override. The
    /// child is discovered from the declaration, so it does not have to be
    /// in the source list at all.
    #[test]
    fn an_out_of_line_module_resolves_mod_rs_and_path_overrides() {
        let dir = scratch("out-of-line-paths");
        let root = &dir.0;
        let lib = root.join("src").join("lib.rs");
        write(
            &lib,
            "#[cfg(test)]\nmod pg_tests;\n\n#[cfg(test)]\n#[path = \"elsewhere/other.rs\"]\nmod other;\n",
        );
        write(
            &root.join("src").join("pg_tests").join("mod.rs"),
            &format!(
                "#[tokio::test]\nasync fn connects() {{\n    let s = {}\"...\").await;\n}}\n",
                call("StorageState", "connect"),
            ),
        );
        write(
            &root.join("src").join("elsewhere").join("other.rs"),
            &format!(
                "#[tokio::test]\nasync fn also_connects() {{\n    let s = {}\"...\").await;\n}}\n",
                call("KeyStore", "connect"),
            ),
        );

        let files = unit_files(&lib, None, None).expect("module tree");
        let evidence = scan(&files, root, Scope::UnitTree);
        let sites: Vec<&str> = evidence.iter().map(|e| e.site.as_str()).collect();
        assert_eq!(
            sites,
            vec![
                "src/elsewhere/other.rs:3 KeyStore::connect",
                "src/pg_tests/mod.rs:3 StorageState::connect",
            ]
        );
        assert_eq!(evidence[0].tests, vec!["other::also_connects".to_string()]);
        assert_eq!(evidence[1].tests, vec!["pg_tests::connects".to_string()]);
    }

    /// Two files, the same `tests::connects`, only one of them opening a
    /// pool. The derived names have to differ, or the grouped one satisfies
    /// the ungrouped one and the guard passes over a live escape.
    #[test]
    fn same_named_tests_in_two_files_derive_two_different_names() {
        let dir = scratch("same-name");
        let root = &dir.0;
        let src = root.join("src");
        write(&src.join("lib.rs"), "mod a;\nmod b;\n");
        let body = |api: &str| {
            format!(
                "#[cfg(test)]\nmod tests {{\n    #[tokio::test]\n    async fn connects() {{\n        let s = {api}\"...\").await;\n    }}\n}}\n",
            )
        };
        write(&src.join("a.rs"), &body(&call("StorageState", "connect")));
        write(
            &src.join("b.rs"),
            "#[cfg(test)]\nmod tests {\n    #[tokio::test]\n    async fn connects() {}\n}\n",
        );

        let files = unit_files(&src.join("lib.rs"), None, None).expect("module tree");
        let evidence = scan(&files, root, Scope::UnitTree);
        assert_eq!(evidence.len(), 1, "{:?}", sites(&evidence));
        assert_eq!(evidence[0].tests, vec!["a::tests::connects".to_string()]);
    }

    /// An integration root declaring anything other than `mod common;` used
    /// to be invisible: only the root file and a literal `mod common;` were
    /// ever opened, so a postgres test one module down never existed.
    #[test]
    fn an_integration_root_follows_every_module_it_declares() {
        let dir = scratch("integration-mods");
        let root = &dir.0;
        let tests = root.join("tests");
        write(&tests.join("root.rs"), "mod cases;\nmod common;\n");
        write(
            &tests.join("cases.rs"),
            &format!(
                "#[tokio::test]\nasync fn connects() {{\n    let s = {}\"...\").await;\n}}\n",
                call("KeyStore", "connect"),
            ),
        );
        write(&tests.join("common").join("mod.rs"), "pub fn helper() {}\n");

        let files = module_tree(&tests.join("root.rs"), true).expect("module tree");
        // rustc resolves a crate root's children in the root file's own
        // directory, probed: `tests/cases.rs`, not `tests/root/cases.rs`.
        let paths: Vec<&Path> = files.iter().map(|(f, _)| f.path.as_path()).collect();
        assert!(
            paths.iter().any(|path| path.ends_with("cases.rs")),
            "{paths:?}"
        );
        let evidence = scan(&files, root, Scope::Integration);
        assert_eq!(sites(&evidence), vec!["tests/cases.rs:3 KeyStore::connect"]);
    }

    /// An out-of-line module declared INSIDE an inline `#[cfg(test)] mod`
    /// carries no attribute of its own, and its file sits one directory
    /// deeper. Both halves have to be right or the child is either skipped
    /// as production code or looked for in the wrong place.
    #[test]
    fn an_out_of_line_module_inside_an_inline_test_mod_is_followed() {
        let dir = scratch("nested-decl");
        let root = &dir.0;
        let src = root.join("src");
        write(
            &src.join("lib.rs"),
            "#[cfg(test)]\nmod outer {\n    mod pg_tests;\n}\n",
        );
        write(
            &src.join("outer").join("pg_tests.rs"),
            &format!(
                "#[tokio::test]\nasync fn connects() {{\n    let s = {}\"...\").await;\n}}\n",
                call("KeyStore", "connect"),
            ),
        );

        let files = unit_files(&src.join("lib.rs"), None, None).expect("module tree");
        let evidence = scan(&files, root, Scope::UnitTree);
        assert_eq!(
            sites(&evidence),
            vec!["src/outer/pg_tests.rs:3 KeyStore::connect"]
        );
        assert_eq!(
            evidence[0].tests,
            vec!["outer::pg_tests::connects".to_string()]
        );
    }

    /// One file, two `#[path]` mountings. rustc compiles the text twice
    /// and nextest lists `alias_a::connects` and `alias_b::connects`, so
    /// deriving only one of them leaves the other unchecked: the group
    /// filter can hold the derived name while the alias runs outside it.
    #[test]
    fn a_file_mounted_under_two_module_names_derives_both() {
        let dir = scratch("path-alias");
        let root = &dir.0;
        let src = root.join("src");
        write(
            &src.join("lib.rs"),
            "#[cfg(test)]\n#[path = \"shared.rs\"]\nmod alias_a;\n\n#[cfg(test)]\n#[path = \"shared.rs\"]\nmod alias_b;\n",
        );
        write(
            &src.join("shared.rs"),
            &format!(
                "#[tokio::test]\nasync fn connects() {{\n    let s = {}\"...\").await;\n}}\n",
                call("KeyStore", "connect"),
            ),
        );

        let files = unit_files(&src.join("lib.rs"), None, None).expect("module tree");
        let evidence = scan(&files, root, Scope::UnitTree);
        assert_eq!(
            sites(&evidence),
            vec![
                "src/shared.rs:3 KeyStore::connect",
                "src/shared.rs:3 KeyStore::connect"
            ]
        );
        let derived: Vec<String> = evidence.iter().flat_map(|e| e.tests.clone()).collect();
        assert_eq!(
            derived,
            vec![
                "alias_a::connects".to_string(),
                "alias_b::connects".to_string()
            ]
        );
    }

    /// One file, one module name, two cfg-alternated mountings. Only one
    /// exists per build, but the walk follows both declarations, and a
    /// dedup key without the test scope lets the non-test mounting (popped
    /// last off the LIFO) discard the `#[cfg(test)]` one -- the whole-file
    /// scan that would have seen the connection never runs.
    #[test]
    fn a_cfg_alternated_mounting_keeps_the_test_scoped_scan() {
        let dir = scratch("cfg-alternated");
        let root = &dir.0;
        let src = root.join("src");
        write(
            &src.join("lib.rs"),
            "#[cfg(test)]\n#[path = \"shared.rs\"]\nmod shared;\n\n#[cfg(feature = \"prod\")]\n#[path = \"shared.rs\"]\nmod shared;\n",
        );
        write(
            &src.join("shared.rs"),
            &format!(
                "#[tokio::test]\nasync fn escapes() {{\n    let s = {}\"...\").await;\n}}\n",
                call("KeyStore", "connect"),
            ),
        );

        let files = unit_files(&src.join("lib.rs"), None, None).expect("module tree");
        let evidence = scan(&files, root, Scope::UnitTree);
        // One call site, two findings: the test-scoped mounting is scanned
        // whole, and the production-scoped one now reports the test fn's own
        // body as a span. Twice is the fail direction; zero is not.
        assert_eq!(
            sites(&evidence),
            vec![
                "src/shared.rs:3 KeyStore::connect",
                "src/shared.rs:3 KeyStore::connect"
            ]
        );
        let derived: Vec<String> = evidence.iter().flat_map(|e| e.tests.clone()).collect();
        assert_eq!(derived, vec!["shared::escapes".to_string(); 2]);
    }

    /// A declaration the walk cannot resolve is the shape a pg test hides
    /// behind, so it fails the guard by name rather than being skipped.
    #[test]
    fn an_unresolvable_module_declaration_is_a_loud_error() {
        let dir = scratch("unresolvable");
        let root = &dir.0;
        write(&root.join("src").join("lib.rs"), "mod nowhere;\n");
        let Err(error) = module_tree(&root.join("src").join("lib.rs"), false) else {
            panic!("nowhere resolves to no file")
        };
        assert!(error.contains("mod nowhere;"), "{error}");
    }

    /// A bin's unit tests run in `pkg::bin/tool`, not in the lib binary.
    /// Scanning all of `src/` and charging it to the lib let a pg test in
    /// `src/bin/tool.rs` be satisfied by a same-named lib test that WAS in
    /// the group. This one goes through `derive`, so `cargo metadata` gets
    /// the last word on which target owns which file.
    #[test]
    fn a_bins_pg_test_is_charged_to_the_bin_not_the_lib() {
        let dir = scratch("bin-partition");
        let root = &dir.0;
        write(
            &root.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        );
        // The lib carries a same-named test that never opens a pool.
        write(
            &root.join("src").join("lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn connects() {}\n}\n",
        );
        write(
            &root.join("src").join("bin").join("tool.rs"),
            &format!(
                "fn main() {{}}\n\n#[cfg(test)]\nmod tests {{\n    #[tokio::test]\n    async fn connects() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
                call("KeyStore", "connect"),
            ),
        );

        let derived = derive(root).expect("cargo metadata");
        let ids: Vec<&str> = derived.iter().map(|d| d.binary_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["probe::bin/tool"],
            "{:?}",
            derived_sites(&derived)
        );
        assert_eq!(
            derived[0].named_tests().into_iter().collect::<Vec<_>>(),
            vec!["tests::connects"]
        );
    }

    /// A `cfg_attr`-wrapped `sqlx::test` builds a postgres pool whenever
    /// its cfg holds, and the guard cannot know whether CI compiled with
    /// it. Reading it as a real one over-collects; reading it as no
    /// attribute at all is a pool nobody counted.
    #[test]
    fn a_cfg_attr_wrapped_sqlx_test_still_counts() {
        for wrapper in [
            "#[cfg_attr(feature = \"pg\", {a})]",
            "#[cfg_attr(all(unix, feature = \"pg\"), cfg_attr(test, {a}))]",
            "#[cfg_attr(feature = \"pg\", sqlx :: test)]",
        ] {
            let text = format!(
                "{}\nasync fn live(pool: PgPool) {{}}\n",
                wrapper.replace("{a}", "sqlx::test"),
            );
            assert_eq!(
                sqlx_names(&text),
                vec![(1, "live".to_string())],
                "{wrapper}"
            );
        }
    }

    /// A `cfg_attr`-wrapped `ignore` runs under some cfgs and not others,
    /// so the test is kept. Dropping it would take a real pg test out of
    /// the derived set on a guess.
    #[test]
    fn a_cfg_attr_wrapped_ignore_does_not_drop_the_test() {
        let text = format!(
            "{}\n#[cfg_attr(slow, ignore)]\nasync fn live(pool: PgPool) {{}}\n",
            attr(""),
        );
        assert_eq!(sqlx_names(&text), vec![(1, "live".to_string())]);
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

    /// A test fn at a `src/` file's top level sits inside no `#[cfg(test)]`
    /// region at all, so the span scan found nothing and the connection was
    /// read as production code. Its own body is the span.
    #[test]
    fn a_test_fn_outside_any_cfg_test_region_is_scanned() {
        for marker in ["#[test]", "#[tokio::test]", "#[::tokio::test]", &attr("")] {
            let text = format!(
                "{marker}\nasync fn boots() {{\n    let s = {}\"...\").await;\n}}\n",
                call("StorageState", "connect"),
            );
            let facts = file_facts(&text);
            assert!(facts.cfg_test_spans.is_empty(), "{marker}");
            assert_eq!(facts.test_fn_spans, vec![(1, 4)], "{marker}");
        }

        let dir = scratch("root-test-fn");
        let root = &dir.0;
        let src = root.join("src");
        write(
            &src.join("lib.rs"),
            &format!(
                "pub struct Store;\n\n#[tokio::test]\nasync fn boots() {{\n    let s = {}\"...\").await;\n}}\n",
                call("StorageState", "connect"),
            ),
        );
        let files = unit_files(&src.join("lib.rs"), None, None).expect("module tree");
        let evidence = scan(&files, root, Scope::UnitTree);
        assert_eq!(sites(&evidence), vec!["src/lib.rs:5 StorageState::connect"]);
        assert_eq!(evidence[0].tests, vec!["boots".to_string()]);
    }

    /// The ordinary shape stays ONE finding: a test fn body inside a
    /// `#[cfg(test)]` region is already covered by that region's span, and
    /// reporting it twice would print the same connection twice.
    #[test]
    fn a_test_fn_inside_a_cfg_test_region_is_not_a_second_span() {
        let text = format!(
            "#[cfg(test)]\nmod pg_tests {{\n    #[tokio::test]\n    async fn boots() {{\n        let s = {}\"...\").await;\n    }}\n}}\n",
            call("StorageState", "connect"),
        );
        let facts = file_facts(&text);
        assert_eq!(facts.cfg_test_spans, vec![(1, 7)]);
        assert!(facts.test_fn_spans.is_empty(), "{:?}", facts.test_fn_spans);
    }

    /// Three halves of the symlink rule, and they pull against each other. A
    /// link pointing at its own ancestor is a cycle, and the walk used to
    /// descend it once per path component the OS still allowed. A link
    /// pointing anywhere else is a real subtree that only the safety net
    /// reaches, since `module_tree` follows `mod` declarations and cannot see
    /// an `include!` edge. And two links to ONE directory are two aliases,
    /// both of which have to come out: which lexical path a file is recorded
    /// under is what decides, later, whether a target owns it.
    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_terminates_and_every_alias_is_collected() {
        let dir = scratch("symlink-walk");
        let root = &dir.0;
        let src = root.join("src");
        write(&src.join("lib.rs"), "pub struct Store;\n");
        write(&root.join("shared").join("mod.rs"), "pub struct Shared;\n");
        std::os::unix::fs::symlink(&src, src.join("cycle")).expect("cycle link");
        std::os::unix::fs::symlink(root.join("shared"), src.join("linked")).expect("dir link");
        std::os::unix::fs::symlink(root.join("shared"), src.join("aliased")).expect("alias link");

        let mut found = Vec::new();
        collect_rs(&src, &mut found).expect("walk");
        found.sort();
        assert_eq!(
            found,
            vec![
                src.join("aliased").join("mod.rs"),
                src.join("lib.rs"),
                src.join("linked").join("mod.rs"),
            ]
        );
    }

    /// The alias rule with the ownership rule behind it, which is where a
    /// global visited set turns into a silent pass. `src/shared` and
    /// `src/bin/shared` are one directory, the library target excludes
    /// `src/bin`, and `module_tree` cannot follow the `include!` that mounts
    /// the file. Record only the `src/bin` alias, whichever `read_dir`
    /// happens to yield first, and `unit_files` drops it as another target's
    /// file. Nothing is scanned, and rustc still runs `cases::escapes`
    /// against a postgres pool.
    ///
    /// The derived name is `shared::cases::escapes`, not the `cases::escapes`
    /// rustc gives it: the sweep names a file by its PATH. That mismatch
    /// fails the guard loudly, which is the documented cost of an `include!`
    /// edge. Loud is not what this test is about. Evidence at all is.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_fails_the_walk_instead_of_reading_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("sealed-walk");
        let src = dir.0.join("src");
        let sealed = src.join("sealed");
        write(&sealed.join("cases.rs"), "");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).expect("seal");
        let outcome = collect_rs(&src, &mut Vec::new());
        // Restore before asserting so the scratch directory can remove itself.
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).expect("unseal");
        if fs::read_dir(&sealed).is_ok() && outcome.is_ok() {
            // Root reads anything: the mode bits cannot make the directory
            // unreadable here, so there is nothing for this test to see.
            return;
        }
        let err = outcome.expect_err("an unlistable directory must fail the walk");
        assert!(err.contains("sealed"), "error names the directory: {err}");
    }

    #[test]
    fn a_file_aliased_into_another_targets_directory_is_still_scanned() {
        let dir = scratch("alias-ownership");
        let root = &dir.0;
        let src = root.join("src");
        write(
            &src.join("lib.rs"),
            "#[cfg(test)]\nmod cases {\n    include!(\"shared/cases.rs\");\n}\n",
        );
        write(
            &src.join("bin").join("shared").join("cases.rs"),
            &format!(
                "#[tokio::test]\nasync fn escapes() {{\n    let s = {}\"...\").await;\n}}\n",
                call("KeyStore", "connect"),
            ),
        );
        std::os::unix::fs::symlink(src.join("bin").join("shared"), src.join("shared"))
            .expect("alias link");

        // Both lexical paths, so the assertion does not depend on the order
        // `read_dir` returns `bin` and `shared` in.
        let mut found = Vec::new();
        collect_rs(&src, &mut found).expect("walk");
        found.sort();
        assert_eq!(
            found,
            vec![
                src.join("bin").join("shared").join("cases.rs"),
                src.join("lib.rs"),
                src.join("shared").join("cases.rs"),
            ]
        );

        // The library's own view: net root `src/`, `src/bin` excluded.
        let files = unit_files(&src.join("lib.rs"), Some(&src), Some(&src.join("bin")))
            .expect("module tree");
        let evidence = scan(&files, root, Scope::UnitTree);
        assert_eq!(
            sites(&evidence),
            vec!["src/shared/cases.rs:3 KeyStore::connect"]
        );
        assert_eq!(
            evidence[0].tests,
            vec!["shared::cases::escapes".to_string()]
        );
    }
}
