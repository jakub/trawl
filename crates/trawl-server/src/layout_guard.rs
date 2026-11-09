//! Guards the data-root grammar against local copies.
//!
//! [`crate::env_dirs`] owns what a date and an hour directory are, and
//! [`crate::pool::ExecutorPool::base_dir`] holds the configured data root.
//! This test scans the crate's production source for the two shapes that
//! used to stand in for them: a date checked by its byte shape (`[4] ==
//! b'-'`), and a root recovered by cutting a glob at its first `*`.

use std::path::{Path, PathBuf};

/// Byte-shape date checks and glob cuts, spelled without whitespace.
const FORBIDDEN: &[(&str, &str)] = &[
    ("[4]==b'-'", "a date checked by its byte shape"),
    ("[7]==b'-'", "a date checked by its byte shape"),
    ("get(4)==Some(&b'-')", "a date checked by its byte shape"),
    ("get(7)==Some(&b'-')", "a date checked by its byte shape"),
    ("find('*')", "a data root cut out of a glob"),
    ("find(\"*\")", "a data root cut out of a glob"),
];

/// Files allowed to spell the grammar: the layout owner and this guard.
const OWNERS: &[&str] = &["env_dirs.rs", "layout_guard.rs"];

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

/// The byte offset of the file's trailing test module: the last
/// `#[cfg(test)]` followed, past whitespace and other attributes, by
/// `mod tests`. Not the first `#[cfg(test)]`, because test-only helpers
/// can sit above production code.
fn test_module_start(source: &str) -> Option<usize> {
    source
        .match_indices("#[cfg(test)]")
        .map(|(at, attr)| (at, &source[at + attr.len()..]))
        .filter(|&(_, mut rest)| {
            loop {
                rest = rest.trim_start();
                if rest.starts_with("#[") {
                    let Some(end) = rest.find(']') else {
                        return false;
                    };
                    rest = &rest[end + 1..];
                } else {
                    break;
                }
            }
            let rest = rest
                .strip_prefix("pub(crate) ")
                .or_else(|| rest.strip_prefix("pub "))
                .unwrap_or(rest);
            rest.strip_prefix("mod tests")
                .is_some_and(|after| after.starts_with([' ', '{', ';']))
        })
        .map(|(at, _)| at)
        .last()
}

/// Every forbidden shape in `source`'s production code, as `line: reason`.
fn violations(source: &str) -> Vec<String> {
    let production = &source[..test_module_start(source).unwrap_or(source.len())];
    let mut found = Vec::new();
    for (index, line) in production.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let squeezed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        for (shape, reason) in FORBIDDEN {
            if squeezed.contains(shape) {
                found.push(format!("{}: {reason}: {}", index + 1, line.trim()));
            }
        }
    }
    found
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
        if OWNERS.iter().any(|owner| relative == Path::new(owner)) {
            continue;
        }
        let source = std::fs::read_to_string(path).unwrap();
        for violation in violations(&source) {
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
fn the_scan_flags_each_shape_and_stops_at_the_trailing_test_module() {
    let source = "\
#[cfg(test)]
fn helper() {}

fn production(bytes: &[u8], base: &str) {
    let _ = bytes[4] == b'-';
    let _ = bytes.get(7) == Some(&b'-');
    let _ = base.find('*');
    let _ = chrono::NaiveDate::parse_from_str(base, \"%Y-%m-%d\");
    // base.find('*') in a comment is prose.
}

#[cfg(test)]
#[allow(clippy::all)]
mod tests {
    fn _t(bytes: &[u8]) -> bool { bytes[4] == b'-' }
}
";
    let found = violations(source);
    let lines: Vec<&str> = found.iter().map(|v| v.split_once(':').unwrap().0).collect();
    assert_eq!(lines, ["5", "6", "7"], "{found:#?}");
}
