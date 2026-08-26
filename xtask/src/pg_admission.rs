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
//!    attribute, or if it pulls in a `common` module that opens postgres
//!    pools (`fixture_pool`).
//! 2. MEMBERSHIP, from nextest itself: `cargo nextest list -E
//!    'group(postgres)'`. cargo-nextest 0.9.143 supports `group()` as a
//!    filterset predicate, so the authority on "is this test in the group"
//!    is the same engine that will enforce it at run time. Re-parsing the
//!    override filters out of the TOML was the alternative, and it would
//!    have re-implemented nextest's filterset semantics (`kind(lib)`,
//!    `test(/regex/)`, precedence) in the guard — a second implementation
//!    to disagree with the first.
//!
//! The match is per TEST NAME where the evidence gives names (an
//! `#[sqlx::test]` attribute names the function beneath it), and per BINARY
//! where it does not (a fixture-driven binary has no marker attribute). The
//! name granularity is what lets the group filter narrow trawl-server's
//! library target to `from_saved::` and `scheduler::` instead of dragging
//! ~600 pure unit tests under an 8-thread cap.

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
        let in_group = grouped.get(&target.binary_id);
        let named = target.named_tests();
        if named.is_empty() {
            // No attribute to name a test: the whole binary drives the pg
            // fixture, so the whole binary belongs in the group.
            if in_group.is_none_or(BTreeSet::is_empty) {
                failures.push(format!(
                    "  {} is not in the `{GROUP}` group\n    evidence: {}",
                    target.binary_id,
                    target.sites()
                ));
            }
            continue;
        }
        let empty = BTreeSet::new();
        let in_group = in_group.unwrap_or(&empty);
        let missing: Vec<&str> = named
            .into_iter()
            .filter(|name| !in_group.iter().any(|test| test_is(test, name)))
            .collect();
        if !missing.is_empty() {
            failures.push(format!(
                "  {} runs {} `#[sqlx::test]` case(s) outside the `{GROUP}` group: {}\n    evidence: {}",
                target.binary_id,
                missing.len(),
                missing.join(", "),
                target.sites()
            ));
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
                let evidence = scan(&sources, root);
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
            let evidence = scan(&sources, root);
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

/// Scan a target's sources for pg evidence.
fn scan(sources: &[PathBuf], root: &Path) -> Vec<Evidence> {
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
    }
    evidence
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
fn sqlx_test_names(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.starts_with("//") || !is_sqlx_test_attribute(line) {
            continue;
        }
        // The attribute names the test beneath it; skip any further
        // attributes stacked between the two.
        let name = lines[index + 1..]
            .iter()
            .map(|l| l.trim())
            .find_map(function_name)
            .unwrap_or_else(|| "<unnamed>".to_string());
        found.push((index + 1, name));
    }
    found
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

/// Ask nextest which tests are in the group: binary id → test names.
fn group_membership(
    root: &Path,
    extra: &[String],
) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
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
        // makes every binary look like a group member.
        let names: BTreeSet<String> = suite["testcases"]
            .as_object()
            .map(|cases| {
                cases
                    .iter()
                    .filter(|(_, case)| case["filter-match"]["status"] == "matches")
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        if !names.is_empty() {
            membership.insert(binary_id.to_string(), names);
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

    #[test]
    fn a_test_id_matches_by_its_last_segment() {
        assert!(test_is("from_saved::tests::run_all", "run_all"));
        assert!(test_is("run_all", "run_all"));
        assert!(!test_is("from_saved::tests::run_all_but_one", "run_all"));
    }
}
