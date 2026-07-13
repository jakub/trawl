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

/// The pre-migration bytes of every chrome rule issue #28 relocated out
/// of trawl-web-ui's `main.css`, captured verbatim (provenance in the
/// fixture header). `moved_chrome_is_byte_identical_to_premigration`
/// asserts each still lives byte-for-byte in the shipped `fleet-ui.css`.
const PREMIGRATION_CHROME: &str = include_str!("fixtures/premigration-chrome.css");

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

/// Split a flat stylesheet into its individual top-level rules, dropping
/// blank and comment lines between them. A rule runs from its selector
/// line to the line where brace depth returns to zero, so single-line
/// rules, multi-line rules, and `@keyframes` blocks each come out whole.
/// The chrome CSS is un-nested, so this stays simple.
fn rules(css: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    for line in css.lines() {
        if depth == 0 {
            let t = line.trim_start();
            if t.is_empty() || t.starts_with("/*") || t.starts_with('*') {
                continue;
            }
        }
        cur.push(line);
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth == 0 && !cur.is_empty() {
            out.push(cur.join("\n"));
            cur.clear();
        }
    }
    out
}

#[test]
fn moved_chrome_is_byte_identical_to_premigration() {
    // C5 ("zero visual change") of issue #28 asks for a before/after pixel
    // grid of every migrated surface in both themes — unobservable from a
    // native test. For a *pure CSS relocation* it has an exact structural
    // equivalent: every rule that moved must still render from byte-for-
    // byte identical CSS, and the design tokens those rules reference are
    // untouched by this slice (guarded by `css_move_invariant`), so both
    // themes follow by construction. This turns the golden fixture's
    // pre-migration bytes into that machine-checked guarantee — the
    // exhaustive backstop behind the hand-picked delta assertions below.
    let expected = rules(PREMIGRATION_CHROME);
    // Vacuous-pass guard: the fixture is the full moved set (33 rules at
    // capture). A splitter that stopped matching would pass silently.
    assert!(
        expected.len() >= 30,
        "expected the full moved-chrome set (~33 rules), split {} — the \
         fixture or the splitter regressed",
        expected.len()
    );
    let missing: Vec<&String> = expected
        .iter()
        .filter(|r| !CSS.contains(r.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these pre-migration chrome rules are no longer byte-identical in \
         fleet-ui.css — a moved rule was altered, a C5 zero-visual-change \
         regression. Restore the rule, or if the change is deliberate, pin \
         it as a documented delta and re-capture the golden fixture: \
         {missing:#?}"
    );
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
        xs.contains("font-size: var(--fs-small)") && xs.contains("padding: 3px 8px"),
        ".btn-xs modifier body matches the ADR-0005 baseline"
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
fn modal_family_classes_shipped_with_crate() {
    // Issue #28 M2: the Modal shell renders the header icon chip and
    // the promoted ConfirmWithReasonModal emits .m-field/.reason-input,
    // so their rules move from trawl's main.css into fleet-ui.css
    // (coastwatch consumes the reason modal next — app-side CSS would
    // leave it unstyled there). Bodies pinned to the moved values.
    assert!(
        rule_body(".modal .m-hd .ic").contains("background: var(--accent-wash)"),
        ".modal .m-hd .ic (header icon chip) keeps the accent-wash chip"
    );
    assert!(
        rule_body(".modal .m-field").contains("flex-direction: column"),
        ".modal .m-field wrapper moved verbatim"
    );
    // ADR-0005: label typography routes through the --label-* treatment
    // tokens (sentence case) instead of hardcoded uppercase-mono.
    assert!(
        rule_body(".modal .m-field label").contains("text-transform: var(--label-transform)"),
        ".modal .m-field label reads the --label-* treatment tokens"
    );
    assert!(
        rule_body(".modal .m-field input").contains("height: 30px"),
        ".modal .m-field input moved verbatim"
    );
    assert!(
        rule_body(".reason-input").contains("min-height: 60px"),
        ".reason-input moved verbatim"
    );
}

#[test]
fn tab_strip_families_stay_distinct() {
    // Issue #28 M3: ONE Tabs component renders BOTH strip families —
    // the workspace `.tabs > .t.active` (weight 500) and the drawer
    // `.sd-tabs > .tb.on` (weight 600). Pin the weights separately so
    // a future "simplify the CSS" pass can't silently merge them.
    let workspace = rule_body(".tabs .t.active");
    assert!(
        workspace.contains("font-weight: 500"),
        ".tabs .t.active must keep font-weight 500 (workspace strip)"
    );
    let drawer = rule_body(".sd-tabs .tb.on");
    assert!(
        drawer.contains("font-weight: 600"),
        ".sd-tabs .tb.on must keep font-weight 600 (drawer strip)"
    );
    // Count chip on workspace tabs (the Events row count).
    assert!(
        rule_body(".tabs .t .c").contains("tabular-nums"),
        ".tabs .t .c count chip moved verbatim"
    );
}

#[test]
fn drawer_shell_classes_shipped_with_crate() {
    // Drawer owns the sd-* SHELL: scrim, panel, header, actions, close,
    // body. Content selectors (.sd-overview, .sd-card, .sf-*, .sd-ttl
    // .name/.sub) stay app-side. Bodies pinned to the moved values.
    assert!(
        rule_body(".sd-scrim").contains("z-index: 50"),
        ".sd-scrim moved verbatim"
    );
    assert!(
        rule_body(".sd-drawer").contains("width: min(720px, 92vw)"),
        ".sd-drawer moved verbatim"
    );
    assert!(
        rule_body(".sd-hd").contains("background: var(--panel-2)"),
        ".sd-hd moved verbatim"
    );
    assert!(
        rule_body(".sd-x").contains("width: 28px"),
        ".sd-x close affordance moved verbatim"
    );
    assert!(
        rule_body(".sd-body").contains("padding: 16px 18px"),
        ".sd-body moved verbatim"
    );
    for name in ["sd-fade-in", "sd-slide-in"] {
        assert!(
            CSS.contains(&format!("@keyframes {name}")),
            "drawer keyframe `@keyframes {name}` missing from fleet-ui.css"
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
