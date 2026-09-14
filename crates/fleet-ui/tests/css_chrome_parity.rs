// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract test for the chrome CSS invariants that "zero visual
//! change" depends on but that no rendered test can observe.
//!
//! Each of these is load-bearing and non-obvious:
//!
//!   * `.rail .it` and `.topbar .mode` render as `<a>`. Anchors default
//!     to `text-decoration: underline`; the explicit `none` is the sole
//!     thing keeping rail items and mode tabs from sprouting underlines.
//!     Drop it and every nav element silently regresses — invisible to
//!     the DOM-class contract tests.
//!   * `.login-card .error-banner` re-establishes the login form's 16px
//!     error spacing via a *more specific* selector over the base
//!     `.error-banner`. Lose the override and login spacing shifts.
//!   * `.shell` grid rows are `auto minmax(0, 1fr) auto` — the `auto`
//!     row lets a footer-less app collapse the footer to zero while
//!     trawl's statusbar sizes itself.
//!   * The six chrome keyframes ship from fleet-ui.css.
//!   * `.login-shell` declares no background (ADR-0012): an opaque
//!     normal-flow block paints above the `z-index: -1` `.atmosphere`
//!     canvas and fully occludes the backdrop. The page floor comes
//!     from `body`'s `background: var(--bg)` propagating to the canvas
//!     instead (html declares none), so the pixels are unchanged.
//!
//! Reading the shipped stylesheet at test time turns those claims into
//! a `cargo nextest` guard, mirroring the `ToastKind` / `Btn`
//! variant native contract tests.
//!
//! Baseline: Mira Blue (ADR-0007). The golden fixture is captured from
//! the shipped CSS (the ADR-0005-sanctioned mechanism), and the targeted
//! `rule_body` pins below enforce the Mira control recipes — weight-500
//! buttons, tinted destructive, `--on-accent` text, 2px `--ring` focus,
//! tokenized radii.

mod common;

use common::rules;

const CSS: &str = include_str!("../styles/fleet-ui.css");

/// The pre-migration bytes of every chrome rule relocated out of
/// trawl-web-ui's `main.css`, captured verbatim (provenance in the
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
    // "Zero visual change" wants a before/after pixel grid of every
    // surface in both themes, which a native test cannot observe. For a
    // pure CSS relocation there is an exact structural equivalent: every
    // moved rule must still render from byte-for-byte identical CSS, and
    // the design tokens those rules reference are untouched (guarded by
    // `css_move_invariant`), so both themes follow by construction. The
    // golden fixture's bytes are that guarantee — the exhaustive backstop
    // behind the hand-picked delta assertions below.
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
    assert!(
        rule_body(".login-card .error-banner").contains("margin-bottom: 16px"),
        "`.login-card .error-banner` must keep `margin-bottom: 16px` — the \
         login form's error spacing depends on this override of the base \
         `.error-banner`"
    );
}

#[test]
fn login_shell_declares_no_background() {
    // ADR-0012 composability: `.login-shell` is a normal-flow opaque
    // block covering the viewport, so any background on it paints above
    // the `z-index: -1` `.atmosphere` canvas and fully occludes the
    // backdrop. The page background comes from
    // `body { background: var(--bg) }` via root propagation instead
    // (html declares none — pinned by atmosphere_palette_parity), which
    // looks identical without a backdrop and is the whole point with one.
    assert!(
        !rule_body(".login-shell").contains("background"),
        "`.login-shell` must not declare a background — an opaque \
         normal-flow block occludes the z-index:-1 .atmosphere backdrop \
         (jakub/coastwatch#308); the page floor is body's var(--bg)"
    );
}

#[test]
fn shell_grid_has_auto_footer_row() {
    assert!(
        rule_body(".shell").contains("grid-template-rows: auto minmax(0, 1fr) auto"),
        "`.shell` grid rows must be `auto minmax(0, 1fr) auto` — the `auto` \
         footer row collapses to zero footer-less and sizes trawl's statusbar"
    );
}

#[test]
fn btn_size_classes_shipped_with_crate() {
    // `<Btn size=…>` emits `btn-sm` / `btn-xs`, so those rules must ship
    // in fleet-ui.css: every class a fleet-ui component emits exists in
    // fleet-ui.css.
    let sm = rule_body(".btn-sm");
    assert!(
        sm.contains("padding: 3px 10px"),
        ".btn-sm padding moved verbatim"
    );
    assert!(
        sm.contains("background: var(--fill)"),
        "the Mira outline treatment is now the --fill plane with an edge \
         highlight (ADR-0032) — still self-contained, not a modifier"
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
    // .btn-xs must appear after the variant rules: equal specificity, so
    // source order is what makes its padding/font-size win over
    // .btn-pri/.btn-sec/.btn-danger.
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
    // The Modal shell renders the header icon chip and
    // ConfirmWithReasonModal emits .m-field/.reason-input, so their rules
    // live in fleet-ui.css — app-side CSS would leave the reason modal
    // unstyled in every other consumer.
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
    // One Tabs component renders both strip families: the workspace
    // `.tabs > .t.active` (weight 500) and the drawer `.sd-tabs > .tb.on`
    // (weight 600). Pin the weights separately so a future "simplify the
    // CSS" pass can't silently merge them.
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
fn native_control_rules_carry_the_button_reset() {
    // ADR-0028 converted these pseudo-buttons to real ones. A browser
    // paints a <button> with its own background and border, so each
    // rule needs its own reset — the stylesheet has no global one
    // beyond `button { font: inherit }`, deliberately (a global reset
    // would strip <Btn> too). Miss one and the control ships with the
    // UA's grey fill and bevel, which no DOM contract test can see.
    for sel in [
        ".tabs .t",
        ".sd-tabs .tb",
        ".topbar .user",
        ".user-menu .item",
        ".modal .m-hd .x",
        ".toast .x",
    ] {
        let body = rule_body(sel);
        assert!(
            body.contains("background: none"),
            "`{sel}` is a <button> now and must reset its background — \
             the UA fill would paint over the strip, got:{body}"
        );
        assert!(
            body.contains("border: 0") || body.contains("border: none"),
            "`{sel}` is a <button> now and must reset its border — the \
             UA bevel would box the control, got:{body}"
        );
    }
    // The reset must not cost the tab strips their declared type: the
    // weights are pinned in tab_strip_families_stay_distinct, the
    // count chip's tabular figures here.
    assert!(
        rule_body(".tabs .t .c").contains("tabular-nums"),
        ".tabs .t .c must keep its tabular figures through the conversion"
    );
    // `border: none` resets all four sides, so each strip's own
    // underline has to be declared after it inside the same rule.
    for (sel, decl) in [
        (".tabs .t", "border-bottom: 2px solid transparent"),
        (".sd-tabs .tb", "border-bottom: 2px solid transparent"),
    ] {
        let body = rule_body(sel);
        let reset = body.find("border: none").expect("reset present");
        let underline = body
            .find(decl)
            .unwrap_or_else(|| panic!("`{sel}` must keep `{decl}`"));
        assert!(
            reset < underline,
            "`{sel}`'s button reset must precede its `{decl}` — declared \
             after, the reset erases the tab underline the active tab \
             colours in"
        );
    }
}

#[test]
fn the_tablist_is_a_flex_row_inside_each_strip() {
    // The tabs moved into a nested role="tablist" node, so that node
    // has to be the flex row the tabs used to sit in directly, and the
    // drawer family's 4px inter-tab gap has to travel with them.
    let tablist = rule_body(".tablist");
    assert!(
        tablist.contains("display: flex"),
        "`.tablist` must be a flex row — as a plain block the tabs stack \
         vertically, got:{tablist}"
    );
    assert!(
        rule_body(".sd-tabs .tablist").contains("gap: 4px"),
        "the drawer strip's 4px inter-tab gap must live on the nested \
         tablist now that .sd-tabs' own gap falls either side of the \
         flex:1 spacer"
    );
}

#[test]
fn drawer_shell_classes_shipped_with_crate() {
    // Drawer owns the sd-* shell: scrim, panel, header, actions, close,
    // body. Content selectors (.sd-overview, .sd-card, .sf-*, .sd-ttl
    // .name/.sub) stay app-side.
    assert!(
        rule_body(".sd-scrim").contains("z-index: 50"),
        ".sd-scrim moved verbatim"
    );
    assert!(
        rule_body(".sd-drawer").contains("width: min(720px, 92vw)"),
        ".sd-drawer moved verbatim"
    );
    assert!(
        rule_body(".sd-hd").contains("background: var(--panel)"),
        ".sd-hd is the drawer's sheet header (ADR-0032): --panel with an \
         edge highlight over the floor-toned body, not the support tone"
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
    // ADR-0007: buttons are weight 500, pinned per variant so a
    // font-weight regression on any one of them turns red.
    for sel in [".btn", ".btn-pri", ".btn-danger"] {
        assert!(
            rule_body(sel).contains("font-weight: 500"),
            "`{sel}` must carry the Mira Blue weight-500 button treatment"
        );
    }
    // Destructive is tinted (red text on the red wash), never solid red.
    let danger = rule_body(".btn-danger");
    assert!(
        danger.contains("background: var(--red-wash)"),
        ".btn-danger must be tinted: red text on var(--red-wash)"
    );
    assert!(
        !danger.contains("background: var(--red)"),
        ".btn-danger must never regress to a solid red fill"
    );
    // Tinting makes `--red` a foreground over its own wash, so the light
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
    // Press feedback is a 1px translate, never a scale.
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
    // transparent)` — is translucent, so what it renders depends on the
    // backdrop it composites over, while the dark resting fill is a fixed
    // white-alpha overlay. Measured in headless Chrome with the shipped
    // CSS at a .04 resting alpha: over `--panel` the hover landed on
    // rgb(29) against a rgb(27) rest (a 2/255 delta, no feedback), and
    // inside a `--panel-2` container it landed darker than rest, rgb(33)
    // under rgb(36) — an inverted hover. Real `--panel-2` containers hold
    // secondary buttons (`.tl-bar`, `.sd-card`), so dark mode restates
    // both ends in the overlay system: `--fill` -> `--fill-2` is the same
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
    // order is the only tie-breaker: it must still be declared before the
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
    // Focus is the 2px --ring outline in both themes (ADR-0032 retired
    // --shadow-glow: every focusable element now carries the outline, so
    // a glow consumer would be a second focus idiom).
    assert_eq!(
        CSS.matches("\n  --ring:").count(),
        2,
        "--ring must be declared exactly once per theme block"
    );
    // Accent-coloured TEXT reads --accent-ink: the dark brand blue
    // measures 4.40:1 on --panel-3, the raised segmented option's floor.
    assert_eq!(
        CSS.matches("\n  --accent-ink:").count(),
        2,
        "--accent-ink must be declared exactly once per theme block"
    );
    // The plane and elevation ramp: a well inset, three raised steps and
    // the 1px edge highlight, one declaration per theme block at the
    // two-space indent the token blocks use.
    for token in [
        "\n  --well:",
        "\n  --inset:",
        "\n  --elev-1:",
        "\n  --elev-2:",
        "\n  --elev-3:",
        "\n  --edge-hi:",
    ] {
        assert_eq!(
            CSS.matches(token).count(),
            2,
            "`{}` must be declared exactly once per theme block (ADR-0032)",
            token.trim()
        );
    }
    // Light --ink-4 is contrast-bound: it paints TEXT (the --fs-micro
    // DEBUG level pill, the DSL editor gutter numbers, .divider), so it
    // clears AA on every surface that reads it rather than sitting at the
    // 3:1 non-text floor. Re-measure the composited pixels before
    // lightening it.
    assert!(
        CSS.contains("--ink-4:     oklch(53% .024 253)"),
        "5.27:1 on --panel, 4.82:1 on --panel-2, 4.68:1 on --well, \
         4.60:1 on --bg — a lighter step drops the DEBUG pill and the \
         gutter rule below AA"
    );
}

#[test]
fn control_fills_route_through_the_per_theme_token() {
    // Dark `--line` is itself a 10%-alpha white overlay, and
    // `color-mix(<colour>, transparent)` multiplies alphas: a fill spelled
    // `color-mix(in oklab, var(--line) 20%, transparent)` renders at
    // .10 x .20 = 2% in dark mode — nothing on an oklch(18%) panel, so
    // dark inputs would ship effectively unfilled while the light theme
    // looked correct. Fills therefore read the per-theme `--fill` /
    // `--fill-2` tokens (light mixes the opaque line; dark states the
    // overlay alpha directly), and the only `--line`-against-transparent
    // mixes in the file are those two light declarations.
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

#[test]
fn the_focus_ring_reaches_anchors() {
    // ADR-0029 makes the one stretched control on a navigating row an
    // `<a href>`, and an anchor outside this group keeps the browser's
    // own blue outline instead of the ring every other control gets.
    // Pinning the whole selector list also pins its shape: the anchors
    // have to ride the same rule body, not a copy beside it.
    let group = rule_body(
        "button:focus-visible,\na[href]:focus-visible,\ninput:focus-visible,\ntextarea:focus-visible,\nselect:focus-visible,\n[tabindex]:focus-visible",
    );
    assert!(
        group.contains("outline: 2px solid var(--ring)") && group.contains("outline-offset: 2px"),
        "the focus-visible group must carry the 2px --ring outline at a \
         2px offset (ADR-0032: an outline never competes with the \
         elevation box-shadow a plane already paints), got:{group}"
    );
    // The ring is an outline now, so a `:focus-visible` rule that painted
    // it as a box-shadow would be a second focus idiom — and --shadow-glow
    // no longer exists to paint it with.
    let glow_groups: Vec<String> = rules(CSS)
        .into_iter()
        .filter(|r| {
            let (selector, body) = r.split_once('{').unwrap_or((r.as_str(), ""));
            selector.contains(":focus-visible") && body.contains("box-shadow: var(--shadow-glow)")
        })
        .collect();
    assert!(
        glow_groups.is_empty(),
        "the accent glow is retired: focus is the --ring outline, \
         found: {glow_groups:#?}"
    );
    // And a rule that resets `outline: none` on a focused control erases
    // the ring outright — `.actions-menu .item:focus-visible`
    // (fleet-ui.css:1390) did exactly that before ADR-0032.
    let suppressed: Vec<String> = rules(CSS)
        .into_iter()
        .filter(|r| {
            let (selector, body) = r.split_once('{').unwrap_or((r.as_str(), ""));
            selector.contains(":focus-visible") && body.contains("outline: none")
        })
        .collect();
    assert!(
        suppressed.is_empty(),
        "no `:focus-visible` rule may reset the outline — that erases the \
         ring instead of restyling it, found: {suppressed:#?}"
    );
}
