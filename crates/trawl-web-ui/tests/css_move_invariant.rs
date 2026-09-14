// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract test for the "move, not copy" split between the two
//! stylesheets. The chrome that fleet-ui components render lives in
//! `fleet-ui.css` alone: `main.css` may keep more-specific app overrides
//! (`.tabs .action`, `.svc-cell .status-dot`) but must not re-declare a
//! base rule or collide on a custom property. `css_chrome_parity.rs`
//! (in fleet-ui) guards the fleet-side presence of those rules; this
//! guards the app-side absence, which fleet-ui cannot see without
//! reaching into its own consumer.
//!
//! `main.css` is read locally; `fleet-ui.css` is read across the
//! sibling-crate boundary exactly as `index.html`'s trunk `<link>`
//! references it (`../fleet-ui/styles/fleet-ui.css`).

use std::collections::BTreeSet;

const APP_CSS: &str = include_str!("../styles/main.css");
const FLEET_CSS: &str = include_str!("../../fleet-ui/styles/fleet-ui.css");

/// Top-level selector strings declared in a stylesheet.
///
/// A rule-opening line begins at column 0 with a selector char and
/// contains its `{` on the same line; the prefix before that `{` is the
/// selector group, split on `,` into individual selectors. Continuation
/// lines of a multi-line selector group don't open a brace at column 0
/// and are skipped, which is good enough for the flat style both files
/// use.
fn top_level_selectors(css: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in css.lines() {
        let Some(first) = line.chars().next() else {
            continue;
        };
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

/// The class families fleet-ui components render; this asserts their
/// app-side absence. The fleet-side presence guards live in two places:
/// the chrome rows in `css_chrome_parity.rs`, the small-widget rows in
/// `fleet-ui/tests/component_class_contract.rs`'s `emits(...)`
/// assertions.
const MOVED_SELECTORS: &[&str] = &[
    // Sidebar and command bar (ADR-0032).
    ".rail",
    ".rail .brand",
    ".rail .brand .accent",
    ".rail .grp",
    ".rail .grp + .grp",
    ".rail .grp-lb",
    ".rail .it",
    ".rail .it > svg",
    ".rail .it .lb",
    ".rail .it:hover",
    ".rail .it.active",
    ".rail .it .badge",
    ".rail .bot",
    ".rail.collapsed",
    ".rail.overlay",
    ".nav-scrim",
    ".shell-content",
    ".topbar .nav-toggle",
    ".topbar .crumb",
    ".seg",
    ".seg .seg-opt",
    ".seg .seg-opt.on",
    ".daterange",
    ".daterange .dr-trigger",
    ".dr-trigger",
    ".dr-trigger span",
    ".dr-trigger:hover",
    ".dr-trigger.open",
    ".scrim",
    ".dr-pop",
    ".dr-pop .seg",
    ".dr-pop .grid",
    ".dr-pop .opt",
    ".dr-pop button.opt",
    ".dr-pop .opt:hover",
    ".dr-pop .opt:disabled:hover",
    ".dr-pop .opt.on",
    ".dr-pop .cust",
    ".dr-pop .cust .fld",
    ".dr-pop .cust .fld .lb",
    ".dr-pop .cust .fld input",
    ".dr-pop .cust .fld input:focus",
    ".dr-pop .foot",
    ".dr-pop .foot .btns",
    ".dr-apply",
    ".dr-pop .dr-err",
    ".dr-pop .rt-hint",
    ".dr-pop .rt-hint p",
    ".dr-pop .opt:disabled",
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
    // The docked presentation's two host classes (ADR-0032). App-side
    // placement rules are written through the layout that placed the
    // panel (`.page-split > .sd-host …`), never as these bare names.
    ".sd-host",
    ".sd-drawer.sd-docked",
    // Small widgets.
    ".sc-spark",
    ".status-dot",
    ".status-dot.success",
    ".status-dot.error",
    ".status-dot.running",
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

/// Retired selector families: the markup renders a fleet-ui component
/// under a different canonical class (`.bdg`, `.seg`,
/// `.results-footer`, `.status-dot`, `.load-hint`), or nothing renders
/// them at all (`.live-badge`). Unlike [`MOVED_SELECTORS`] these must
/// not exist in either stylesheet; reappearing anywhere means per-site
/// drift is growing back.
const RETIRED_SELECTORS: &[&str] = &[
    // The command bar's brand, mode tabs and app links left with the
    // sidebar (ADR-0032). `.rail .it .lb` did not: the sidebar still
    // renders a label, visible or screen-reader-only.
    ".topbar .brand",
    ".topbar .brand .accent",
    ".topbar .modes",
    ".topbar .mode",
    ".topbar .mode:hover",
    ".topbar .mode.active",
    ".topbar .app-links",
    // The meta strip became the executed-scope strip (ADR-0032): the
    // chips kept their `.meta-chips` wrapper and their own classes, but
    // every rule that scoped them under `.meta` is re-homed under
    // `.scope`, and the truncation note left for the result header.
    ".meta",
    ".meta .dim",
    ".meta .chip",
    ".meta .chip .x",
    ".meta .chip .x:hover",
    ".meta .chip.excl",
    ".meta .chip.excl .x",
    ".meta .chip.excl .x:hover",
    ".meta .chip.bad",
    // The nets list says a schedule's cadence in words and leaves the
    // enabled/paused judgement to fleet_ui::Badge (ADR-0032), so the
    // glyph pills it used to render are gone.
    ".sched-badge",
    ".sched-badge.active",
    ".sched-badge.disabled",
    ".live-badge",
    ".live-badge.live",
    ".live-badge.lagged",
    ".sd-dot",
    ".sd-dot.errors",
    // `run_status_tone` folds timeout into `StatusTone::Error`, so
    // there is no `.status-dot.timeout` markup to style.
    ".status-dot.timeout",
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
    ".intel-badge",
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

#[test]
fn app_control_fills_read_the_fill_token() {
    // fleet-ui's dark `--line` is a 10%-alpha white overlay and
    // `color-mix(<colour>, transparent)` multiplies alphas, so an app-side
    // fill written as `color-mix(in oklab, var(--line) N%, transparent)`
    // renders at .10 x N in dark mode (a 20% fill lands at 2%). App fills
    // read fleet-ui's per-theme `--fill` / `--fill-2` tokens instead.
    // Either step of the ramp satisfies the vacuous-pass guard: the
    // editor well moved onto `--well` with ADR-0032, so the app's
    // remaining control fill is the `--fill-2` hover step.
    assert!(
        APP_CSS.contains("var(--fill)") || APP_CSS.contains("var(--fill-2)"),
        "expected the app control fills to read fleet-ui's --fill / --fill-2 tokens"
    );
    let strays: Vec<&str> = APP_CSS
        .match_indices("var(--line) ")
        .filter_map(|(i, m)| {
            let rest = &APP_CSS[i + m.len()..];
            let end = rest.find(')')?;
            rest[..end]
                .contains("transparent")
                .then(|| APP_CSS[i..i + m.len() + end].trim())
        })
        .collect();
    assert!(
        strays.is_empty(),
        "these mix fleet-ui's alpha dark `--line` against `transparent`, \
         collapsing the fill to a few percent in dark mode — read \
         `var(--fill)` / `var(--fill-2)` instead: {strays:#?}"
    );
}
