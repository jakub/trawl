// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract test for the chrome CSS invariants that AC3
//! ("zero visual change") of issue #27 depends on but that no rendered
//! test can observe.
//!
//! The migration moved trawl-web-ui's chrome onto fleet-ui and deleted
//! the duplicated `main.css` surface. The reviewer's residual AC3 gap:
//! the *deliberate* CSS deltas — and the compensations that keep the
//! result eyeball-identical — were asserted in prose, not guarded.
//! These are load-bearing and non-obvious:
//!
//!   * `.rail .it` and `.topbar .mode` render as `<a>` in fleet-ui
//!     (they were `<div>` in the pre-migration markup). Anchors default
//!     to `text-decoration: underline`; the explicit `none` is the sole
//!     thing keeping rail items and mode tabs from sprouting underlines.
//!     Drop it and every nav element silently regresses — invisible to
//!     the DOM-class contract tests.
//!   * `.login-card .error` re-establishes the login form's 16px error
//!     spacing via a *more specific* selector after the base `.error`
//!     moved into fleet-ui. Lose the override and login spacing shifts.
//!   * `.shell` grid rows became `var(--topbar-h) 1fr auto` (documented
//!     footer-auto-sizing change) — the row that lets a footer-less app
//!     collapse the footer to zero while trawl's statusbar sizes itself.
//!   * The six chrome keyframes must survive the move to fleet-ui.css.
//!
//! Reading the shipped stylesheet at test time turns those prose claims
//! into a `cargo nextest` guard, mirroring the `ToastKind` / `Btn`
//! variant native contract tests.

const CSS: &str = include_str!("../styles/fleet-ui.css");

/// Return the declaration body of a top-level rule, keyed on its exact
/// selector. Anchored on a preceding newline so `.rail .it` does not
/// match `.rail .it .lb` / `.rail .it:hover`, and the selector must be
/// followed by ` {` so partial selectors never collide. Chrome rules
/// have no nested braces, so the body ends at the first `}`.
fn rule_body(selector: &str) -> &'static str {
    let needle = format!("\n{selector} {{");
    let start = CSS
        .find(&needle)
        .unwrap_or_else(|| panic!("selector `{selector}` not found in fleet-ui.css"))
        + needle.len();
    let end = CSS[start..]
        .find('}')
        .unwrap_or_else(|| panic!("unterminated rule for `{selector}`"));
    &CSS[start..start + end]
}

#[test]
fn rail_items_suppress_anchor_underline() {
    // div -> anchor compensation: without this, every rail item underlines.
    assert!(
        rule_body(".rail .it").contains("text-decoration: none"),
        "`.rail .it` must keep `text-decoration: none` — rail items are \
         anchors and would otherwise render underlined (AC3 regression)"
    );
}

#[test]
fn mode_tabs_suppress_anchor_underline() {
    assert!(
        rule_body(".topbar .mode").contains("text-decoration: none"),
        "`.topbar .mode` must keep `text-decoration: none` — mode tabs are \
         anchors and would otherwise render underlined (AC3 regression)"
    );
}

#[test]
fn login_error_spacing_preserved() {
    // The more-specific override that restores the pre-migration 16px gap.
    assert!(
        rule_body(".login-card .error").contains("margin-bottom: 16px"),
        "`.login-card .error` must keep `margin-bottom: 16px` — the login \
         form's error spacing depends on this override of base `.error`"
    );
}

#[test]
fn shell_grid_has_auto_footer_row() {
    // Documented delta #3: topbar / body / auto footer row.
    assert!(
        rule_body(".shell").contains("grid-template-rows: var(--topbar-h) 1fr auto"),
        "`.shell` grid rows must be `var(--topbar-h) 1fr auto` — the `auto` \
         footer row collapses to zero footer-less and sizes trawl's statusbar"
    );
}

#[test]
fn btn_size_classes_shipped_with_crate() {
    // Issue #28: `<Btn size=…>` emits `btn-sm` / `btn-xs`, so the rules
    // must ship in fleet-ui.css (every class a fleet-ui component emits
    // exists in fleet-ui.css). Bodies pinned to the values moved verbatim
    // from trawl's main.css.
    let sm = rule_body(".btn-sm");
    assert!(
        sm.contains("padding: 3px 10px"),
        ".btn-sm padding moved verbatim"
    );
    assert!(
        sm.contains("background: var(--panel-2)"),
        ".btn-sm is a self-contained style (own background), not a modifier"
    );
    assert!(
        rule_body(".btn-sm:disabled").contains("opacity: 0.4"),
        ".btn-sm:disabled keeps its 0.4 opacity (vs .btn-sec's .5)"
    );

    let xs = rule_body(".btn-xs");
    assert!(
        xs.contains("font-size: 11px") && xs.contains("padding: 3px 8px"),
        ".btn-xs modifier body moved verbatim"
    );
    // .btn-xs must appear AFTER the variant rules: equal specificity, and
    // its padding/font-size must win over .btn-pri/.btn-sec/.btn-danger
    // exactly as it did when main.css loaded after fleet-ui.css.
    let xs_pos = CSS.find("\n.btn-xs {").expect(".btn-xs rule present");
    for variant in [".btn-pri {", ".btn-sec {", ".btn-danger {"] {
        let vpos = CSS
            .find(variant)
            .unwrap_or_else(|| panic!("{variant} present"));
        assert!(
            xs_pos > vpos,
            ".btn-xs must be declared after {variant} so the modifier wins the cascade"
        );
    }
}

#[test]
fn chrome_keyframes_present() {
    for name in [
        "blink",
        "spin",
        "toastIn",
        "fadeIn",
        "modalFadeIn",
        "modalIn",
    ] {
        assert!(
            CSS.contains(&format!("@keyframes {name}")),
            "chrome keyframe `@keyframes {name}` missing from fleet-ui.css"
        );
    }
}
