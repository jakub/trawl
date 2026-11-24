// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ingest preview's code path names nothing that stores, counts or
//! logs (ADR-0049).

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "wal_writer",
    "pipeline",
    "reserve(",
    "publish(",
    "ensure_free_space",
    "publication(",
    "metrics::counter!",
    "metrics::",
    "repair_service_label",
    "total_events",
    "total_rejected",
    // Tracing, whose events trawld persists as `service:trawld` rows.
    "tracing",
    "info!",
    "warn!",
    "error!",
    "debug!",
    "trace!",
    "event!",
];

const SECOND_PARSER: &[&str] = &["from_str(", "from_slice("];

const HTTP_PRODUCER: &str = "Producer::Http {";

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
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

/// `source` with every inline `#[cfg(test)]` module blanked out, line
/// numbers kept.
fn production_lines(source: &str) -> Result<Vec<&str>, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        out.push(line);
        at += 1;
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let mut item = at;
        while item < lines.len() && lines[item].trim_start().starts_with("#[") {
            item += 1;
        }
        let Some(head) = lines.get(item) else { break };
        let trimmed = head.trim_start();
        let is_inline_mod = (trimmed.starts_with("mod ") || trimmed.starts_with("pub mod "))
            && trimmed.ends_with('{');
        if !is_inline_mod {
            continue;
        }
        let indent = &head[..head.len() - trimmed.len()];
        let close = format!("{indent}}}");
        let Some(end) = lines[item + 1..].iter().position(|l| *l == close) else {
            return Err(format!("the test module at line {} never closes", item + 1));
        };
        let end = item + 1 + end;
        out.extend(std::iter::repeat_n("", end + 1 - at));
        at = end + 1;
    }
    Ok(out)
}

fn hits(lines: &[&str], needles: &[&'static str]) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
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

fn http_producer_constructions(lines: &[&str]) -> Vec<usize> {
    let text = lines.join("\n");
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find(HTTP_PRODUCER) {
        let start = from + offset;
        let open = start + HTTP_PRODUCER.len() - 1;
        from = open + 1;
        let line_start = text[..start].rfind('\n').map_or(0, |n| n + 1);
        if text[line_start..start].trim_start().starts_with("//") {
            continue;
        }
        let mut depth = 0_usize;
        let mut close = None;
        for (i, ch) in text[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            found.push(text[..start].matches('\n').count() + 1);
            continue;
        };
        let inside = &text[open + 1..close];
        let after = text[close + 1..].trim_start();
        let is_pattern = inside.contains("..")
            || after.starts_with("=>")
            || after.starts_with("if ")
            || after.starts_with('|')
            || (after.starts_with('=') && !after.starts_with("=="));
        if !is_pattern {
            found.push(text[..start].matches('\n').count() + 1);
        }
    }
    found
}

fn scan(relative: &str, needles: &[&'static str]) -> Vec<String> {
    let path = src_root().join(relative);
    let source = read(&path);
    let lines = production_lines(&source).unwrap_or_else(|why| panic!("src/{relative}: {why}"));
    hits(&lines, needles)
        .into_iter()
        .map(|(line, needle)| format!("  src/{relative}:{line}: {needle}"))
        .collect()
}

#[test]
fn the_preview_path_stores_counts_and_logs_nothing() {
    let mut violations = scan("ingest/preview.rs", FORBIDDEN);
    violations.extend(scan("ingest/preview.rs", SECOND_PARSER));
    violations.extend(scan("ingest/body.rs", FORBIDDEN));
    assert!(
        violations.is_empty(),
        "the ingest preview may not store, count or log the sample, nor parse \
         it a second way (ADR-0049); these lines name what it may not:\n{}",
        violations.join("\n")
    );
}

#[test]
fn only_the_shared_step_builds_the_http_producer() {
    let root = src_root();
    let mut builders = Vec::new();
    for path in rust_sources(&root) {
        let source = read(&path);
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let lines = production_lines(&source).unwrap_or_else(|why| panic!("src/{relative}: {why}"));
        for line in http_producer_constructions(&lines) {
            builders.push(format!("src/{relative}:{line}"));
        }
    }
    assert_eq!(
        builders.len(),
        1,
        "the HTTP producer's context is built once, in src/ingest/body.rs, so \
         real ingest and the preview read a peer the same way: {builders:#?}"
    );
    assert!(
        builders[0].starts_with("src/ingest/body.rs:"),
        "{builders:#?}"
    );
}

#[test]
fn the_scan_sees_each_needle() {
    for needle in FORBIDDEN.iter().chain(SECOND_PARSER) {
        let source = format!("fn f() {{\n    // {needle} in prose\n    x.{needle}y;\n}}\n");
        let lines = production_lines(&source).unwrap();
        assert!(
            hits(&lines, &[needle]).contains(&(3, needle)),
            "{needle} must be seen on a code line"
        );
        assert!(
            !hits(&lines, &[needle]).iter().any(|(line, _)| *line == 2),
            "{needle} in a comment is prose"
        );
    }
}

#[test]
fn test_modules_are_skipped_and_production_after_them_is_not() {
    let source = "\
fn a() {}

#[cfg(test)]
mod tests {
    fn t() { tracing::info!(\"x\"); }
}

fn b() { pipeline.publish(x); }

#[cfg(test)]
#[allow(dead_code)]
mod pg_tests {
    fn u() { wal_writer(); }
}
";
    let lines = production_lines(source).unwrap();
    assert_eq!(lines.len(), source.lines().count(), "line numbers kept");
    assert_eq!(hits(&lines, FORBIDDEN), [(8, "pipeline"), (8, "publish(")]);
    assert!(production_lines("#[cfg(test)]\nmod tests {\n    fn t() {}\n").is_err());
}

#[test]
fn constructions_are_told_from_patterns() {
    let source = "\
fn f(p: &Producer<'_>) {
    let built = Producer::Http {
        peer_host: \"h\",
        peer_is_trusted_relay: false,
    };
    // Producer::Http { in prose
    match p {
        Producer::Http {
            peer_host,
            peer_is_trusted_relay,
        } if relay => {}
        Producer::Http { .. } => {}
        _ => {}
    }
    call(Producer::Http { peer_host, peer_is_trusted_relay });
}
";
    let lines: Vec<&str> = source.lines().collect();
    assert_eq!(http_producer_constructions(&lines), [2, 15]);
}
