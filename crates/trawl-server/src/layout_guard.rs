//! Guards the data-root grammar against local copies.
//!
//! [`crate::env_dirs`] owns what a date and an hour directory are, and
//! [`crate::pool::ExecutorPool::base_dir`] holds the configured data root.
//! This test scans the crate's production source for the two shapes that
//! used to stand in for them: a date checked by its byte shape (`[4] ==
//! b'-'`), and a root recovered by cutting a glob at its first `*`.

use std::path::{Path, PathBuf};

const DATE_SHAPE: &str = "a date checked by its byte shape";
const GLOB_CUT: &str = "a data root cut out of a glob";

/// Byte-shape date checks and glob cuts, spelled without whitespace. The
/// flag marks a date shape, which the layout owner may spell.
const FORBIDDEN: &[(&str, &str, bool)] = &[
    ("[4]==b'-'", DATE_SHAPE, true),
    ("[7]==b'-'", DATE_SHAPE, true),
    ("get(4)==Some(&b'-')", DATE_SHAPE, true),
    ("get(7)==Some(&b'-')", DATE_SHAPE, true),
    ("find('*')", GLOB_CUT, false),
    ("find(\"*\")", GLOB_CUT, false),
    // rustfmt's vertical argument layout adds a trailing comma.
    ("find('*',)", GLOB_CUT, false),
    ("find(\"*\",)", GLOB_CUT, false),
];

/// The layout owner, which may spell date shapes but not glob cuts.
const OWNER: &str = "env_dirs.rs";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// `source`'s lines with every top-level `#[cfg(test)] mod name { .. }`
/// blanked, so line numbers still match the file. rustfmt closes a
/// top-level module with a `}` alone on its line. Production code after a
/// test module is still scanned.
fn production_lines(source: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = source.lines().collect();
    let mut at = 0;
    while at < lines.len() {
        if lines[at] != "#[cfg(test)]" {
            at += 1;
            continue;
        }
        let mut item = at + 1;
        while lines.get(item).is_some_and(|line| line.starts_with("#[")) {
            item += 1;
        }
        let opens_module = lines.get(item).is_some_and(|line| {
            let line = line
                .strip_prefix("pub(crate) ")
                .or_else(|| line.strip_prefix("pub "))
                .unwrap_or(line);
            line.starts_with("mod ") && line.ends_with('{')
        });
        if !opens_module {
            at += 1;
            continue;
        }
        let end = (item + 1..lines.len())
            .find(|&line| lines[line] == "}")
            .unwrap_or(lines.len() - 1);
        for line in &mut lines[at..=end] {
            *line = "";
        }
        at = end + 1;
    }
    lines
}

/// Every forbidden shape in `source`'s production code, as `line: reason`.
/// Whitespace is dropped across line breaks before matching, so a shape
/// split over several lines still matches; each kept byte remembers its
/// line. `owner` exempts the date shapes.
fn violations(source: &str, owner: bool) -> Vec<String> {
    let lines = production_lines(source);
    let mut squeezed = String::new();
    let mut line_of = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for c in line.chars().filter(|c| !c.is_whitespace()) {
            squeezed.push(c);
            for _ in 0..c.len_utf8() {
                line_of.push(index);
            }
        }
    }
    let mut found = Vec::new();
    for (shape, reason, date_shape) in FORBIDDEN {
        if owner && *date_shape {
            continue;
        }
        for (at, _) in squeezed.match_indices(shape) {
            found.push((line_of[at], *reason));
        }
    }
    found.sort_unstable();
    found
        .into_iter()
        .map(|(index, reason)| format!("{}: {reason}: {}", index + 1, lines[index].trim()))
        .collect()
}

#[test]
fn no_date_shape_or_glob_cut_outside_the_layout_owner() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();

    let mut offenders = Vec::new();
    for path in &files {
        let relative = path.strip_prefix(&src).unwrap();
        if relative == Path::new("layout_guard.rs") {
            continue;
        }
        let source = std::fs::read_to_string(path).unwrap();
        for violation in violations(&source, relative == Path::new(OWNER)) {
            offenders.push(format!("src/{}:{violation}", relative.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "use crate::env_dirs for date and hour names and ExecutorPool::base_dir \
         for the data root:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scan_flags_each_shape_and_skips_only_test_modules() {
    let source = "\
#[cfg(test)]
fn helper() {}

fn production(bytes: &[u8], base: &str) {
    let _ = bytes[4] == b'-';
    let _ = bytes.get(7) == Some(&b'-');
    let _ = base.find('*');
    let _ = chrono::NaiveDate::parse_from_str(base, \"%Y-%m-%d\");
    // base.find('*') in a comment is prose.
    let _ = bytes[4]
        == b'-';
    let _ = base
        .find(
            '*',
        );
}

#[cfg(test)]
#[allow(clippy::all)]
mod tests {
    fn _t(bytes: &[u8]) -> bool { bytes[4] == b'-' }
}

fn after_tests(base: &str) -> Option<usize> { base.find('*') }

#[cfg(test)]
mod pg_tests {
    fn _t(base: &str) -> Option<usize> { base.find('*') }
}
";
    let lines = |found: Vec<String>| -> Vec<String> {
        found
            .iter()
            .map(|v| v.split_once(':').unwrap().0.to_owned())
            .collect()
    };
    assert_eq!(
        lines(violations(source, false)),
        ["5", "6", "7", "10", "13", "24"]
    );
    // The owner may spell a date shape, never a glob cut.
    assert_eq!(lines(violations(source, true)), ["7", "13", "24"]);
}
