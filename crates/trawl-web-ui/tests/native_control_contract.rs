// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression guard for ADR-0029: no element that acts on a
//! press is a `div`, `span`, `tr`, `th`, `td`, `li`, `p` or heading.

use std::fs;
use std::path::{Path, PathBuf};

const PSEUDO_BUTTON_TAGS: &[&str] = &[
    "div", "span", "tr", "th", "td", "li", "p", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// `on:mousedown` is here because an author reaching for it to dodge
/// this guard would be writing the same defect one event earlier.
const PRESS_HANDLERS: &[&str] = &["on:click", "on:mousedown"];

#[derive(Debug)]
struct Site {
    file: String,
    /// 1-based line of the tag's `<`.
    line: usize,
    tag: &'static str,
    text: String,
}

fn pseudo_button_sites(file: &str, src: &str) -> Vec<Site> {
    let src = comment_stripped(src);
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &src[i + 1..];
        let Some(tag) = PSEUDO_BUTTON_TAGS.iter().copied().find(|tag| {
            rest.strip_prefix(tag).is_some_and(|after| {
                // `<div>`, `<div …`, `<div/>` — but never `<divider …`.
                after
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_whitespace() || ch == '>' || ch == '/')
            })
        }) else {
            i += 1;
            continue;
        };
        let end = opening_tag_end(&src[i..]).map_or(src.len(), |len| i + len);
        let text = &src[i..end];
        if PRESS_HANDLERS.iter().any(|h| text.contains(h)) {
            out.push(Site {
                file: file.to_owned(),
                line: src[..i].matches('\n').count() + 1,
                tag,
                text: text.to_owned(),
            });
        }
        i = end.max(i + 1);
    }
    out
}

/// Byte length of the opening tag starting at `src[0] == '<'`, up to and
/// including its `>`.
///
/// Leptos attribute values are Rust expressions, so the scan tracks
/// nesting and string literals and only accepts a `>` outside both. It
/// also refuses the `>` of `=>`, `->`, `>=` and `>>`. A bare `a > b`
/// at depth zero would end the tag early and could hide a handler that
/// follows it.
fn opening_tag_end(src: &str) -> Option<usize> {
    let b = src.as_bytes();
    let mut depth = 0usize;
    let mut i = 1usize;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'>' if depth == 0 => {
                let prev = b[i - 1];
                let next = b.get(i + 1).copied();
                if matches!(prev, b'=' | b'-' | b'>') || next == Some(b'=') || next == Some(b'>') {
                    i += 1;
                } else {
                    return Some(i + 1);
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn comment_stripped(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut kept: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if depth > 0 && bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
        } else {
            if depth == 0 {
                kept.push(bytes[i]);
            } else if bytes[i] == b'\n' {
                kept.push(b'\n');
            }
            i += 1;
        }
    }
    let out = String::from_utf8(kept).expect("dropping whole comment spans keeps the rest valid");
    out.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn source_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    collect(&root, &mut out);
    out.sort();
    assert!(
        out.len() > 20,
        "only {} source files found under {} — the walk is looking in the wrong place",
        out.len(),
        root.display(),
    );
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_app_pseudo_button_survives() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for path in source_files() {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        for site in pseudo_button_sites(&rel, &src) {
            offenders.push(format!("{}:{} <{}>", site.file, site.line, site.tag));
        }
    }

    assert!(
        offenders.is_empty(),
        "these elements act on a press but are not controls (ADR-0029): {}\n\
         Make each one a `<button type=\"button\">` or an `<a href>` with an \
         accessible name, and prove it in a spec under e2e/tests/.",
        offenders.join(", "),
    );
}

#[test]
fn a_commented_out_pseudo_button_does_not_trip_the_scan() {
    let line = "// <div on:click=move |_| open.set(true)>\"x\"</div>\n";
    let block = "/* <div on:click=move |_| open.set(true)> */\n";
    let doc = "/// A `<span on:click=…>` is what this replaced.\n";
    for src in [line, block, doc] {
        assert!(
            pseudo_button_sites("src/fixture.rs", src).is_empty(),
            "a pseudo-button in a comment is prose, not markup: {src}",
        );
    }
}

#[test]
fn a_multi_line_opening_tag_is_read_whole() {
    let src = "view! {\n    <span\n        class=\"op\"\n        aria-label=inc_label\n        \
               on:click=move |_| on_add.run(f)\n    >\"+\"</span>\n}\n";
    let sites = pseudo_button_sites("src/fixture.rs", src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    assert_eq!(sites[0].tag, "span");
    assert_eq!(sites[0].line, 2);

    let native = src
        .replace("<span", "<button type=\"button\"")
        .replace("</span>", "</button>");
    assert!(
        pseudo_button_sites("src/fixture.rs", &native).is_empty(),
        "a button is a control, whatever it renders",
    );
}

#[test]
fn a_rust_expression_in_an_attribute_does_not_end_the_tag_early() {
    let src = "<div\n    class=move || match k.get() { X => \"a\", _ => \"b\" }\n    \
               aria-expanded=move || (n.get() > 0).to_string()\n    on:click=go\n>\n";
    let sites = pseudo_button_sites("src/fixture.rs", src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    assert!(sites[0].text.contains("on:click=go"));
}

#[test]
fn a_scrim_in_the_app_is_still_detected() {
    let sites = pseudo_button_sites(
        "src/fixture.rs",
        "<div class=\"scrim\" on:mousedown=close/>",
    );
    assert_eq!(sites.len(), 1);
}
