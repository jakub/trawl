// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression guard for ADR-0029's conversion: no element that acts on a
//! press is a `div`, `span`, `tr`, `th`, `td`, `li`, `p` or heading any
//! more. Every one of those 29 sites is now a `<button type="button">`
//! or an `<a href>`, which is what puts it in the tab order, gives it
//! the fleet-ui focus ring and lets it carry an accessible name.
//!
//! This is NOT the accessibility claim. It cannot see a name, a focus
//! ring or a keyboard press; the browser specs under `e2e/tests/` do
//! that. What it catches is the cheap regression: someone hanging an
//! `on:click` on a `div` again because the markup around it was already
//! a `div`.
//!
//! Two things make the scan honest rather than a `git grep` in a test:
//!
//!   1. Comments are stripped first, with the same idiom
//!      `e2e_selector_contract.rs` uses, so a commented-out example
//!      cannot fail the build and a comment cannot answer for markup.
//!   2. An opening tag is read to its real end, across newlines and
//!      through Rust expressions in attribute values. A one-line grep
//!      misses `.op`, `.x` and `.tool` outright: those tags carry an
//!      `aria-label` on one line and the handler three lines further
//!      down.
//!
//! The one allowed site is the date picker's scrim, a full-screen
//! transparent `div` that dismisses on `mousedown`. It is not a control:
//! it has no name, does nothing a keyboard user needs (Escape closes the
//! dialog and the overlay hook traps Tab inside the panel), and giving
//! it a role would put a nameless button in the tab order. The
//! allow-list is asserted to MATCH something, so deleting the scrim
//! silently widens nothing.

use std::fs;
use std::path::{Path, PathBuf};

/// Elements that are not controls. A press handler on one of these is
/// the defect, whatever else the tag carries.
const PSEUDO_BUTTON_TAGS: &[&str] = &[
    "div", "span", "tr", "th", "td", "li", "p", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// The press handlers a pseudo-button uses. `on:mousedown` is here
/// because the scrim uses it, and an author reaching for it to dodge
/// this guard would be writing the same defect one event earlier.
const PRESS_HANDLERS: &[&str] = &["on:click", "on:mousedown"];

/// One opening tag that carries a press handler.
#[derive(Debug)]
struct Site {
    /// Path relative to the crate root, for a message that can be acted on.
    file: String,
    /// 1-based line of the tag's `<`.
    line: usize,
    tag: &'static str,
    /// The opening tag's source text, comments already stripped.
    text: String,
}

impl Site {
    /// The date picker's scrim: `editor_wrap.rs`, a `div`, dismissing on
    /// `mousedown` and on nothing else. Every clause is load-bearing —
    /// an `on:click` added to it, or a second `class="scrim"` div
    /// elsewhere, is not this site.
    fn is_allowed_scrim(&self) -> bool {
        self.file.ends_with("components/editor_wrap.rs")
            && self.tag == "div"
            && self.text.contains("class=\"scrim\"")
            && self.text.contains("on:mousedown")
            && !self.text.contains("on:click")
    }
}

/// Every pseudo-button site in one file's source text.
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
/// also refuses the `>` of `=>`, `->`, `>=` and `>>`, which are the
/// operator spellings that reach depth zero in practice. A bare `a > b`
/// at depth zero would end the tag early and could hide a handler that
/// follows it; no attribute in this crate is written that way, and the
/// alternative — parsing Rust — is not what a drift guard should be.
fn opening_tag_end(src: &str) -> Option<usize> {
    let b = src.as_bytes();
    let mut depth = 0usize;
    let mut i = 1usize;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    // A backslash escape cannot end the literal.
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

/// `src` with its comments removed, so a commented-out example cannot
/// fail the build and a comment describing markup cannot stand in for
/// it. Same idiom as `e2e_selector_contract.rs`: nested block comments
/// go first, then any line whose first non-space characters are `//`.
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

/// Every `.rs` file under `src/`, sorted so a failure reads the same way
/// twice.
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

/// Recursive half of [`source_files`].
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
fn no_pseudo_button_survives_outside_the_allow_list() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut allowed = 0usize;
    let mut offenders = Vec::new();
    for path in source_files() {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        for site in pseudo_button_sites(&rel, &src) {
            if site.is_allowed_scrim() {
                allowed += 1;
            } else {
                offenders.push(format!("{}:{} <{}>", site.file, site.line, site.tag));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these elements act on a press but are not controls (ADR-0029): {}\n\
         Make each one a `<button type=\"button\">` or an `<a href>` with an \
         accessible name, and prove it in a spec under e2e/tests/.",
        offenders.join(", "),
    );
    // Non-vacuous: the allow-list has to be matching the scrim it was
    // written for, or it is silently permitting nothing and would keep
    // passing after someone widened it.
    assert_eq!(
        allowed, 1,
        "expected exactly one allow-listed site, the date picker's scrim \
         in src/components/editor_wrap.rs — found {allowed}",
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
    // The shape a one-line grep misses: the tag name, the attributes and
    // the handler are all on different lines.
    let src = "view! {\n    <span\n        class=\"op\"\n        aria-label=inc_label\n        \
               on:click=move |_| on_add.run(f)\n    >\"+\"</span>\n}\n";
    let sites = pseudo_button_sites("src/fixture.rs", src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    assert_eq!(sites[0].tag, "span");
    assert_eq!(sites[0].line, 2);

    // And the same shape on a real control is not a site at all.
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
    // `=>`, `->` and a `>` inside braces all sit in attribute values in
    // this crate; a scan that stopped at the first `>` would read the
    // handler below as being outside the tag.
    let src = "<div\n    class=move || match k.get() { X => \"a\", _ => \"b\" }\n    \
               aria-expanded=move || (n.get() > 0).to_string()\n    on:click=go\n>\n";
    let sites = pseudo_button_sites("src/fixture.rs", src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    assert!(sites[0].text.contains("on:click=go"));
}

#[test]
fn the_scrim_is_allowed_and_an_ordinary_dismisser_is_not() {
    let scrim = "<div\n    class=\"scrim\"\n    node_ref=scrim_ref\n    \
                 on:mousedown=on_scrim_mousedown\n/>\n";
    let sites = pseudo_button_sites("src/components/editor_wrap.rs", scrim);
    assert_eq!(sites.len(), 1);
    assert!(
        sites[0].is_allowed_scrim(),
        "the picker's scrim is the one exemption"
    );

    // Same markup somewhere else, or the same file with a click handler
    // added: not the exempt site.
    let elsewhere = pseudo_button_sites("src/components/other.rs", scrim);
    assert!(!elsewhere[0].is_allowed_scrim());
    let clicking = scrim.replace("on:mousedown=on_scrim_mousedown", "on:click=close");
    let clicking = pseudo_button_sites("src/components/editor_wrap.rs", &clicking);
    assert!(!clicking[0].is_allowed_scrim());
}
