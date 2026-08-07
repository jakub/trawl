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
//!   * `.login-shell` lost its `background: var(--bg)` declaration
//!     (jakub/coastwatch#308, ADR-0012) — a DELIBERATE delta: an opaque
//!     normal-flow block paints above the `z-index: -1` `.atmosphere`
//!     canvas and fully occluded the backdrop. Zero-visual-delta today
//!     because `body`'s `background: var(--bg)` propagates to the
//!     canvas (html declares none). `.login-shell` is not in the golden
//!     fixture, so no re-capture was needed.
//!
//! Reading the shipped stylesheet at test time turns those prose claims
//! into a `cargo nextest` guard, mirroring the `ToastKind` / `Btn`
//! variant native contract tests.
//!
//! Baseline: **Mira Blue (ADR-0007, issue #47)**. The golden fixture was
//! re-captured from the shipped CSS after the Mira Blue port (the
//! ADR-0005-sanctioned mechanism), and the targeted `rule_body` pins
//! below enforce the Mira control recipes — weight-500 buttons, tinted
//! destructive, `--on-accent` text, 2px `--ring` focus, tokenized radii
//! — so the new values are the guarded baseline, not a casualty.

mod common;

use common::rules;

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
        rule_body(".login-card .error-banner").contains("margin-bottom: 16px"),
        "`.login-card .error-banner` must keep `margin-bottom: 16px` — the \
         login form's error spacing depends on this override of the base \
         `.error-banner`"
    );
}

#[test]
fn login_shell_declares_no_background() {
    // jakub/coastwatch#308 / ADR-0012 composability: `.login-shell` is a
    // normal-flow opaque block covering the viewport — ANY background on
    // it paints above the `z-index: -1` `.atmosphere` canvas and fully
    // occludes the backdrop. The page background it used to provide comes
    // from `body { background: var(--bg) }` via root propagation instead
    // (html declares none — pinned by atmosphere_palette_parity), so
    // removing the declaration is zero visual delta without a backdrop
    // and the whole point with one.
    assert!(
        !rule_body(".login-shell").contains("background"),
        "`.login-shell` must not declare a background — an opaque \
         normal-flow block occludes the z-index:-1 .atmosphere backdrop \
         (jakub/coastwatch#308); the page floor is body's var(--bg)"
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
        sm.contains("background: transparent"),
        ".btn-sm is the Mira outline treatment (ADR-0007): transparent \
         fill, 1px line border — still self-contained, not a modifier"
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
fn mira_blue_button_recipes_pinned() {
    // ADR-0007: buttons are weight 500 (was 600) — the single most
    // visible Mira control delta. Pinned per variant so a font-weight
    // regression on any one of them turns red.
    for sel in [".btn", ".btn-pri", ".btn-danger"] {
        assert!(
            rule_body(sel).contains("font-weight: 500"),
            "`{sel}` must carry the Mira Blue weight-500 button treatment"
        );
    }
    // Destructive is TINTED (red text on the red wash), never solid red.
    let danger = rule_body(".btn-danger");
    assert!(
        danger.contains("background: var(--red-wash)"),
        ".btn-danger must be tinted: red text on var(--red-wash)"
    );
    assert!(
        !danger.contains("background: var(--red)"),
        ".btn-danger must never regress to a solid red fill"
    );
    // Tinting makes `--red` a FOREGROUND over its own wash, so the light
    // tone is contrast-bound: Mira's oklch(57.7% .245) measures 3.97:1
    // there (3.31:1 on the 20% hover wash), under the 4.5:1 AA floor for
    // normal text. The shipped light tone is the measured one — re-measure
    // the composited pixels before changing it.
    assert!(
        CSS.contains("--red:        oklch(48% .177 27.325)"),
        "the light `--red` is toned for AA over `--red-wash` (6.03:1 resting, \
         5.06:1 hover) — a lighter tone drops the tinted destructive recipe \
         below 4.5:1"
    );
    // Press feedback is a 1px translate; the scale press is retired.
    assert!(
        !CSS.contains("scale: 0.96"),
        "the scale(0.96) press is retired — Mira presses are `translate: 0 1px`"
    );
    // Accent surfaces read their text from the token, never a literal.
    assert!(
        !CSS.contains("color: #fff"),
        "hard-coded #fff text is retired — accent surfaces use var(--on-accent)"
    );
}

#[test]
fn dark_outline_hover_is_a_backdrop_independent_lift() {
    // The shared hover fill — `color-mix(in oklab, var(--panel-3) 55%,
    // transparent)` — is TRANSLUCENT, so what it renders depends on the
    // backdrop it composites over, while the dark resting fill is a fixed
    // white-alpha overlay. Measured in headless Chrome with the shipped
    // CSS at the old .04 resting alpha: over `--panel` the hover landed on
    // rgb(29) against a rgb(27) rest (a 2/255 delta — no feedback), and
    // inside a `--panel-2` container it landed DARKER than rest, rgb(33)
    // under rgb(36) — an inverted hover. Real `--panel-2` containers hold
    // secondary buttons (`.tl-bar`, `.sd-card`), so dark mode restates
    // BOTH ends in the overlay system: `--fill` -> `--fill-2` is the same
    // step on every surface (rgb(32)->rgb(44) on `--panel`,
    // rgb(41)->rgb(52) on `--panel-2`).
    let rest = rule_body("[data-theme=\"dark\"] .btn-sec,\n[data-theme=\"dark\"] .btn-sm");
    assert!(
        rest.contains("background: var(--fill)"),
        "the dark outline resting fill must be the backdrop-independent \
         `var(--fill)` overlay, got:{rest}"
    );
    let hover = rule_body(
        "[data-theme=\"dark\"] .btn-sec:hover,\n[data-theme=\"dark\"] .btn-sm:hover:not(:disabled)",
    );
    assert!(
        hover.contains("background: var(--fill-2)"),
        "the dark outline hover must step to `var(--fill-2)` — inheriting \
         the shared backdrop-dependent `--panel-3` hover makes the lift \
         imperceptible on `--panel` and a DIP on `--panel-2`, got:{hover}"
    );
    // The resting fill is (0,2,0), exactly `.btn-sec:hover`, so source
    // order is the only tie-breaker: it must still be declared BEFORE the
    // shared hover rules. (The dark hover pair above is (0,3,0) and wins
    // on specificity regardless of where it sits.)
    let fill = CSS
        .find("[data-theme=\"dark\"] .btn-sec,")
        .expect("dark outline resting fill present");
    for shared in ["\n.btn-sec:hover {", "\n.btn-sm:hover:not(:disabled) {"] {
        let hpos = CSS
            .find(shared)
            .unwrap_or_else(|| panic!("`{}` present", shared.trim()));
        assert!(
            fill < hpos,
            "the `[data-theme=\"dark\"]` resting fill must precede `{}` — \
             equal specificity means a later fill would kill the hover",
            shared.trim()
        );
    }
}

#[test]
fn mira_blue_tokens_declared() {
    // The ADR-0007 radius scale lives in fleet-ui.css :root — main.css
    // consumes it but never declares tokens (css_move_invariant).
    for decl in [
        "--radius-panel: 10px",
        "--radius-ctl: 8px",
        "--radius-sm: 6px",
        "--on-accent:",
    ] {
        assert!(
            CSS.contains(decl),
            "expected `{decl}` declared in fleet-ui.css (ADR-0007 token contract)"
        );
    }
    // --on-accent is per-theme: light near-white, dark near-black
    // (Mira's dark-mode inversion) — one declaration per theme block.
    assert_eq!(
        CSS.matches("--on-accent:").count(),
        2,
        "--on-accent must be declared exactly once per theme block"
    );
    // Focus is the 2px solid ring in BOTH themes (was a 3px soft glow).
    assert_eq!(
        CSS.matches("--shadow-glow: 0 0 0 2px var(--ring)").count(),
        2,
        "--shadow-glow must be the 2px var(--ring) ring in both theme blocks"
    );
    // Light --ink-4 is contrast-bound: it paints TEXT (the --fs-micro
    // DEBUG level pill, the DSL editor gutter numbers, .editor-hd .dim,
    // .divider), so it holds the pre-Mira tone's luminance rather than the
    // skin's oklch(70.8%), which measures 2.59:1 on --panel and 2.48:1 on
    // the editor's --fill wash. Re-measure the composited pixels before
    // lightening it.
    assert!(
        CSS.contains("--ink-4:     oklch(62% 0 0)"),
        "the light `--ink-4` is toned for the 3:1 floor as a text colour \
         (3.64:1 on --panel, 3.48:1 on the editor fill) — a lighter step \
         drops the DEBUG pill and the gutter rule below it"
    );
}

#[test]
fn control_fills_route_through_the_per_theme_token() {
    // Dark `--line` is ITSELF a 10%-alpha white overlay, and
    // `color-mix(<colour>, transparent)` MULTIPLIES alphas: a fill spelled
    // `color-mix(in oklab, var(--line) 20%, transparent)` renders at
    // .10 x .20 = 2% in dark mode — nothing on an oklch(18%) panel, so
    // dark inputs would ship effectively unfilled while the light theme
    // looked correct. Fills therefore read the per-theme `--fill` /
    // `--fill-2` tokens (light mixes the opaque line; dark states the
    // overlay alpha directly), and the ONLY `--line`-against-transparent
    // mixes left in the file are those two light declarations.
    // Declarations only — anchored on the newline + indent so prose in the
    // surrounding comments never counts.
    for token in ["\n  --fill:", "\n  --fill-2:"] {
        assert_eq!(
            CSS.matches(token).count(),
            2,
            "`{}` must be declared exactly once per theme block",
            token.trim()
        );
    }
    let dark = CSS
        .find("[data-theme=\"dark\"] {")
        .expect("dark theme token block present");
    let strays: Vec<&str> = CSS
        .match_indices("var(--line) ")
        .filter_map(|(i, m)| {
            let rest = &CSS[i + m.len()..];
            let end = rest.find(')')?;
            (rest[..end].contains("transparent") && i > dark)
                .then(|| CSS[i..i + m.len() + end].trim())
        })
        .collect();
    assert!(
        strays.is_empty(),
        "these mix the alpha dark `--line` against `transparent`, which \
         multiplies alphas and collapses the fill to a few percent — read \
         `var(--fill)` / `var(--fill-2)` instead: {strays:#?}"
    );
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
