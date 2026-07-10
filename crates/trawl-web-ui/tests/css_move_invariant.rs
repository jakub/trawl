// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract test for the "move, not copy" invariant of the
//! issue #28 CSS split (issue #27 AC4). The chrome that moved into
//! fleet-ui must live in `fleet-ui.css` ONLY — `main.css` may keep
//! more-specific app overrides (`.dr-pop .tabs`, `.login-card .error`)
//! but must not re-declare the base rules or collide on custom
//! properties. `css_chrome_parity.rs` (in fleet-ui) already guards the
//! fleet-SIDE presence of the moved rules; this guards the app-SIDE
//! absence, which fleet-ui cannot see without reaching into its own
//! consumer.
//!
//! This replaces the orphaned `scripts/css-crossgrep.sh` heuristic with
//! a single source of truth that runs under `cargo nextest` on every
//! push. `main.css` is read locally; `fleet-ui.css` is read across the
//! sibling-crate boundary exactly as `index.html`'s trunk `<link>`
//! already references it (`../fleet-ui/styles/fleet-ui.css`).

use std::collections::BTreeSet;

const APP_CSS: &str = include_str!("../styles/main.css");
const FLEET_CSS: &str = include_str!("../../fleet-ui/styles/fleet-ui.css");

/// Top-level selector strings declared in a stylesheet.
///
/// Mirrors the `css-crossgrep.sh` heuristic: a rule-opening line begins
/// at column 0 with a selector char and contains its `{` on the same
/// line; the prefix before that `{` is the selector group, split on `,`
/// into individual selectors. Continuation lines of a multi-line
/// selector group don't open a brace at column 0 and are skipped —
/// good enough for the flat style both files use.
fn top_level_selectors(css: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in css.lines() {
        let Some(first) = line.chars().next() else {
            continue;
        };
        // Rule-opening lines start flush-left with a selector char.
        if !(first.is_ascii_alphabetic() || matches!(first, '.' | '#' | ':' | '*' | '[')) {
            continue;
        }
        let Some((prefix, _)) = line.split_once('{') else {
            continue;
        };
        for sel in prefix.split(',') {
            let sel = sel.trim();
            if !sel.is_empty() {
                out.insert(sel.to_owned());
            }
        }
    }
    out
}

/// Custom-property names (`--foo`) declared anywhere in a stylesheet.
fn custom_properties(css: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in css.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("--") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        // Must be a declaration (`--name:`), not a `var(--name)` usage.
        let after = rest[name.len()..].trim_start();
        if !name.is_empty() && after.starts_with(':') {
            out.insert(format!("--{name}"));
        }
    }
    out
}

/// The class families fleet-ui components render, which moved out of
/// `main.css`. Kept in sync with `css_chrome_parity.rs`'s fleet-side
/// presence assertions; here we assert their app-side absence.
const MOVED_SELECTORS: &[&str] = &[
    ".btn-sm",
    ".btn-xs",
    ".modal .m-hd .ic",
    ".modal .m-field",
    ".reason-input",
    ".tabs",
    ".tabs .t",
    ".tabs .t.active",
    ".tabs .t .c",
    ".tabs .sp",
    ".sd-scrim",
    ".sd-drawer",
    ".sd-hd",
    ".sd-ttl",
    ".sd-actions",
    ".sd-x",
    ".sd-tabs",
    ".sd-tabs .tb",
    ".sd-tabs .tb.on",
    ".sd-tabs .sp",
    ".sd-tabs .meta",
    ".sd-body",
    // Issue #31 small-widget sweep — moved verbatim (or lightly
    // generalized) into fleet-ui.css as each widget's consumers
    // switched to the fleet component.
    ".sc-spark",
    ".status-dot",
    ".status-dot.success",
    ".status-dot.error",
    ".status-dot.running",
    ".status-dot.timeout",
    ".toggle",
    ".toggle input",
    ".toggle-slider",
    ".toggle-slider::before",
    ".toggle input:checked + .toggle-slider",
    ".toggle input:checked + .toggle-slider::before",
    ".kbd",
    ".kbd-inline",
    ".results-footer",
    ".results-summary",
    ".results-pager",
    ".inp-wrap",
    ".inp-wrap input",
    ".btn-icon",
    ".btn-icon:hover",
    ".actions-menu",
    ".actions-menu .item",
    ".actions-menu .item:hover",
    ".actions-menu .item.danger",
    ".actions-menu .item.danger:hover",
];

/// Selector families the issue #31 unification RETIRED outright: their
/// markup now renders a fleet-ui component with a DIFFERENT canonical
/// class (`.bdg`, `.seg`, `.results-footer`, `.status-dot`,
/// `.load-hint`), or was dead (`.live-badge`). Unlike
/// [`MOVED_SELECTORS`] these must not exist in EITHER stylesheet —
/// reappearing anywhere means per-site drift is growing back.
const RETIRED_SELECTORS: &[&str] = &[
    ".live-badge",
    ".live-badge.live",
    ".live-badge.lagged",
    ".sd-dot",
    ".sd-dot.errors",
    ".run .kbd-inline",
    ".results-loading",
    ".results-error",
    ".export-formats",
    ".export-formats .fmt-btn",
    ".export-formats .fmt-btn:last-child",
    ".export-formats .fmt-btn:hover",
    ".export-formats .fmt-btn.active",
    ".seg-mini",
    ".seg-mini > span",
    ".seg-mini > span:last-child",
    ".seg-mini > span.on",
    ".dr-pop .tabs",
    ".dr-pop .tabs .t",
    ".dr-pop .tabs .t.on",
    ".tbl-foot",
    ".tbl-foot .pager",
];

#[test]
fn moved_selectors_absent_from_app_css() {
    let app = top_level_selectors(APP_CSS);
    let stray: Vec<&str> = MOVED_SELECTORS
        .iter()
        .copied()
        .filter(|sel| app.contains(*sel))
        .collect();
    assert!(
        stray.is_empty(),
        "these moved selectors are still defined in main.css (must live only \
         in fleet-ui.css; more-specific app overrides are fine): {stray:?}"
    );
}

#[test]
fn retired_selectors_absent_from_both_stylesheets() {
    let app = top_level_selectors(APP_CSS);
    let fleet = top_level_selectors(FLEET_CSS);
    let stray: Vec<&str> = RETIRED_SELECTORS
        .iter()
        .copied()
        .filter(|sel| app.contains(*sel) || fleet.contains(*sel))
        .collect();
    assert!(
        stray.is_empty(),
        "these selectors were retired by the issue #31 unification (their \
         surfaces render fleet-ui components with canonical classes) but \
         are defined again: {stray:?}"
    );
}

#[test]
fn no_selector_defined_in_both_stylesheets() {
    let app = top_level_selectors(APP_CSS);
    let fleet = top_level_selectors(FLEET_CSS);
    // Guard against a vacuous pass if the parser ever stops matching.
    assert!(!app.is_empty(), "parsed no selectors from main.css");
    assert!(!fleet.is_empty(), "parsed no selectors from fleet-ui.css");
    let dups: Vec<&String> = app.intersection(&fleet).collect();
    assert!(
        dups.is_empty(),
        "top-level selectors defined in BOTH fleet-ui.css and main.css \
         (the split must be a move, not a copy): {dups:?}"
    );
}

#[test]
fn no_custom_property_defined_in_both_stylesheets() {
    let app = custom_properties(APP_CSS);
    let fleet = custom_properties(FLEET_CSS);
    // fleet-ui.css owns the design tokens; a parser that stops matching
    // would let a real collision slip through as a vacuous pass.
    assert!(
        !fleet.is_empty(),
        "parsed no custom properties from fleet-ui.css"
    );
    let dups: Vec<&String> = app.intersection(&fleet).collect();
    assert!(
        dups.is_empty(),
        "custom properties defined in BOTH fleet-ui.css and main.css \
         (collisions risk cross-stylesheet override drift): {dups:?}"
    );
}
