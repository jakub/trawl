// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The component tables, the executable counts and the source map are
//! packaging artifacts: they say what a release ships. The Linux tarball
//! holds `trawl`, `trawld`, `trawl-web` and `fleet-admin`, as
//! `scripts/release/verify-distribution.py` checks. A shipped executable
//! that a page leaves out, or a deleted one it still lists, fails here
//! before it reaches a reader.

use std::path::Path;

const README: &str = include_str!("../../../README.md");
const OVERVIEW: &str = include_str!("../../../docs/src/content/docs/start/overview.md");
const GETTING_STARTED: &str =
    include_str!("../../../docs/src/content/docs/getting-started/index.md");
const DEPLOYMENT: &str = include_str!("../../../docs/src/content/docs/operate/deployment.md");
const SOURCE_MAP: &str = include_str!("../../../docs/src/content/docs/contribute/source-map.md");
const RELEASE_V1: &str = include_str!("../../../docs/releases/v1.0.0.md");
const RELEASE_DRAFT: &str = include_str!("../../../docs/launch/1.0-release-notes.md");

/// The Linux tarball's executables, in the order the tables list them.
const EXECUTABLES: [&str; 4] = ["trawl", "trawld", "trawl-web", "fleet-admin"];
/// The executables in the `trawl-server` package and the container image.
const SERVER_EXECUTABLES: [&str; 3] = ["trawld", "trawl-web", "fleet-admin"];

const COMPONENT_HEADER: &str = "| Component | Responsibility |";

/// The first cells of a markdown table that starts at `header`, one table
/// per header line in `doc`.
fn tables<'a>(doc: &'a str, header: &str) -> Vec<Vec<&'a str>> {
    let lines: Vec<&str> = doc.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == header)
        .map(|(at, _)| {
            lines[at + 2..]
                .iter()
                .take_while(|line| line.starts_with('|'))
                .map(|row| row.split('|').nth(1).unwrap_or_default().trim())
                .collect()
        })
        .collect()
}

/// The backticked first cells of each component table. PostgreSQL is a
/// component but not an executable, so it is not backticked and drops out.
fn component_tables(doc: &str) -> Vec<Vec<&str>> {
    tables(doc, COMPONENT_HEADER)
        .into_iter()
        .map(|cells| {
            cells
                .into_iter()
                .filter_map(|cell| cell.strip_prefix('`')?.strip_suffix('`'))
                .collect()
        })
        .collect()
}

#[test]
fn component_tables_list_exactly_the_four_executables() {
    for (name, doc) in [
        ("README.md", README),
        ("start/overview.md", OVERVIEW),
        ("getting-started/index.md", GETTING_STARTED),
    ] {
        let found = component_tables(doc);
        assert_eq!(found.len(), 1, "{name} has one component table");
        assert_eq!(found[0], EXECUTABLES, "{name}'s component table");
    }
}

const NUMBER_WORDS: [&str; 10] = [
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
];

/// Every number, as a word or in digits, that comes right before
/// "executables" in `doc`.
fn executable_counts(doc: &str) -> Vec<String> {
    let words: Vec<String> = doc
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .collect();
    words
        .windows(2)
        .filter(|pair| pair[1] == "executables")
        .map(|pair| pair[0].clone())
        .filter(|word| {
            NUMBER_WORDS.contains(&word.as_str())
                || (!word.is_empty() && word.chars().all(|c| c.is_ascii_digit()))
        })
        .collect()
}

#[test]
fn every_executable_count_reads_four() {
    // Each of these sentences counts what a Linux tarball holds. The minimum
    // keeps the test from passing when a sentence is reworded away.
    for (name, doc, at_least) in [
        ("getting-started/index.md", GETTING_STARTED, 2),
        ("operate/deployment.md", DEPLOYMENT, 1),
        ("docs/releases/v1.0.0.md", RELEASE_V1, 1),
        ("docs/launch/1.0-release-notes.md", RELEASE_DRAFT, 1),
    ] {
        let counts = executable_counts(doc);
        assert!(
            counts.len() >= at_least,
            "{name} counts executables {at_least} or more times, found {counts:?}"
        );
        assert!(
            counts.iter().all(|count| count == "four"),
            "{name} counts executables as {counts:?}"
        );
    }
}

/// The `/usr/bin/<name>` paths in `cell`.
fn usr_bin_names(cell: &str) -> Vec<&str> {
    cell.split('`')
        .filter_map(|part| part.strip_prefix("/usr/bin/"))
        .collect()
}

#[test]
fn the_server_package_lists_its_three_executables() {
    let rows: Vec<&str> = DEPLOYMENT
        .lines()
        .filter(|line| line.starts_with("| `/usr/bin/trawld`"))
        .collect();
    assert_eq!(rows.len(), 1, "deployment.md has one executables row");
    let cell = rows[0].split('|').nth(1).unwrap_or_default();
    assert_eq!(usr_bin_names(cell), SERVER_EXECUTABLES);
    assert!(!rows[0].contains("trawl-admin"), "{}", rows[0]);

    let paragraph = GETTING_STARTED
        .split("\n\n")
        .find(|paragraph| paragraph.starts_with("`trawl-cli` installs `trawl`."))
        .expect("getting-started says what each package installs");
    let installs = paragraph
        .split_once("`trawl-server` installs")
        .expect("getting-started says what trawl-server installs")
        .1
        .split_once(" with ")
        .expect("the trawl-server list ends at its systemd units")
        .0;
    let named: Vec<&str> = installs.split('`').skip(1).step_by(2).collect();
    assert_eq!(named, SERVER_EXECUTABLES, "{paragraph}");
}

#[test]
fn every_source_map_crate_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let crates: Vec<&str> = tables(SOURCE_MAP, "| Crate | Owns |")
        .into_iter()
        .flatten()
        .filter_map(|cell| cell.strip_prefix('`')?.strip_suffix('`'))
        .filter(|path| path.starts_with("crates/"))
        .collect();
    assert!(crates.len() >= 10, "source-map.md lists crates: {crates:?}");
    for path in crates {
        assert!(
            root.join(path).is_dir(),
            "source-map.md lists {path}, which is not a directory"
        );
    }
}
