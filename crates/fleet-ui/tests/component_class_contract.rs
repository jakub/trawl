// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every class hook a fleet-ui component emits, paired with the CSS rule
//! that styles it.
//!
//! `css_chrome_parity` proves each chrome rule still lives byte-for-byte
//! in `fleet-ui.css` and `css_move_invariant` proves its design tokens are
//! untouched, so a hook that *is* emitted paints as intended. Neither
//! observes whether the component still emits it: a primitive renamed to
//! `class="modal-overlay"` leaves the byte-identical `.modal-scrim` rule
//! green while dropping every pixel it painted, invisible to compile, to
//! clippy, and to the CSS tests.
//!
//! fleet-ui compiles leptos only under `cfg(target_arch = "wasm32")`
//! (natively it is `serde_json` + `chrono`), so no native test can *render*
//! these components. Instead each primitive's source is read with
//! `include_str!` and scanned for its hooks, the same trick
//! `css_chrome_parity` uses on the stylesheet. Rule ships plus hook
//! emitted is as close to "renders identically" as a native test gets;
//! the screenshot grid stays the PR-time deliverable for the rest.

const MODAL_SHELL: &str = include_str!("../src/modal/shell.rs");
const CONFIRM_REASON: &str = include_str!("../src/modal/confirm_reason.rs");
const DRAWER: &str = include_str!("../src/drawer.rs");
const TABS: &str = include_str!("../src/tabs.rs");
const ERROR_BANNER: &str = include_str!("../src/error_banner.rs");

// Small widgets. Their tone/class *composition* is pinned by native
// unit tests in the pure layers (badge::tone, status_dot::tone,
// segmented::class); these source scans pin the base class hooks the
// wasm components emit.
const BADGE: &str = include_str!("../src/badge/component.rs");
const BADGE_TONE: &str = include_str!("../src/badge/tone.rs");
const STATUS_DOT_TONE: &str = include_str!("../src/status_dot/tone.rs");
const SPARKLINE: &str = include_str!("../src/sparkline/component.rs");
const LOADED: &str = include_str!("../src/loaded/component.rs");
const SEGMENTED: &str = include_str!("../src/segmented/component.rs");
const SEGMENTED_CLASS: &str = include_str!("../src/segmented/class.rs");
const PAGER: &str = include_str!("../src/pager.rs");
const SEARCH_INPUT: &str = include_str!("../src/search_input.rs");
const TOGGLE: &str = include_str!("../src/toggle.rs");
const KBD: &str = include_str!("../src/kbd.rs");
const ACTIONS_MENU: &str = include_str!("../src/actions_menu.rs");
const MENU: &str = include_str!("../src/menu.rs");
const TOPBAR: &str = include_str!("../src/topbar.rs");
const ROVING: &str = include_str!("../src/roving.rs");
const COPY_BUTTON: &str = include_str!("../src/copy_button.rs");
const ICON: &str = include_str!("../src/icon.rs");
const LIB: &str = include_str!("../src/lib.rs");
const LOAD_MORE: &str = include_str!("../src/load_more.rs");
const WHEN: &str = include_str!("../src/time/when.rs");
const CLOCK: &str = include_str!("../src/time/clock.rs");
const ATMOSPHERE: &str = include_str!("../src/atmosphere/component.rs");
const FLEET_CSS: &str = include_str!("../styles/fleet-ui.css");

/// The source with its comment lines removed, for the negative scans:
/// a module doc that explains why an affordance was retired names the
/// affordance, and prose is not markup. Same trick as trawl-core's
/// `now_anchor_contract`.
fn markup_only(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Assert `src` contains `hook` (a class literal or class-idiom substring),
/// blaming the CSS rule that hook must line up with.
fn emits(src: &str, hook: &str, styled_by: &str) {
    assert!(
        src.contains(hook),
        "class hook `{hook}` is no longer emitted — its byte-identical CSS \
         rule `{styled_by}` (pinned in css_chrome_parity) would then style \
         nothing, a C5 zero-visual-change regression invisible to compile \
         and to the CSS parity tests. Restore the hook, or if the markup \
         change is deliberate re-pin both sides."
    );
}

#[test]
fn modal_shell_emits_the_hooks_its_css_styles() {
    // `.modal-scrim > .modal[.modal-sm] > .m-hd(.ic/.t/.x) / .m-body / .m-ft`
    // — the frame `modal_family_classes_shipped_with_crate` pins in
    // css_chrome_parity.
    emits(MODAL_SHELL, r#"class="modal-scrim""#, ".modal-scrim");
    // narrow => `modal modal-sm`, else `modal`; both selectors are styled.
    emits(MODAL_SHELL, r#""modal modal-sm""#, ".modal-sm");
    emits(MODAL_SHELL, r#""modal""#, ".modal");
    emits(MODAL_SHELL, r#"class="m-hd""#, ".modal .m-hd");
    emits(MODAL_SHELL, r#"class="ic""#, ".modal .m-hd .ic");
    emits(MODAL_SHELL, r#"class="m-body""#, ".modal .m-body");
    emits(MODAL_SHELL, r#"class="m-ft""#, ".modal .m-ft");
}

#[test]
fn confirm_reason_emits_field_and_reason_input() {
    // ConfirmWithReasonModal: `.m-field` wrapper (via Field's class
    // override) + `.reason-input` textarea, both pinned in the CSS
    // parity test's `modal_family_classes_shipped_with_crate`.
    emits(CONFIRM_REASON, r#"class="m-field""#, ".modal .m-field");
    emits(CONFIRM_REASON, r#"class="reason-input""#, ".reason-input");
}

#[test]
fn drawer_emits_the_sd_shell_hooks_its_css_styles() {
    // The `sd-*` shell css_chrome_parity's `drawer_shell_classes_shipped_
    // with_crate` pins: scrim, panel, header (title + actions + close),
    // body.
    emits(DRAWER, r#"class="sd-scrim""#, ".sd-scrim");
    emits(DRAWER, r#"class="sd-drawer""#, ".sd-drawer");
    emits(DRAWER, r#"class="sd-hd""#, ".sd-hd");
    emits(DRAWER, r#"class="sd-ttl""#, ".sd-ttl");
    emits(DRAWER, r#"class="sd-actions""#, ".sd-actions");
    emits(DRAWER, r#"class="sd-x""#, ".sd-x");
    emits(DRAWER, r#"class="sd-body""#, ".sd-body");
}

#[test]
fn tabs_emits_both_strip_families_with_distinct_active_idioms() {
    // Workspace family: `.tabs > div.t.active > span.c`. The active
    // modifier must be `active` (weight-500 `.tabs .t.active`) and the
    // count chip `.c` — both pinned in the CSS test.
    emits(TABS, r#"class="tabs""#, ".tabs");
    emits(TABS, r#"class="t""#, ".tabs .t");
    emits(TABS, "class:active", ".tabs .t.active");
    emits(TABS, r#"class="c""#, ".tabs .t .c");

    // Drawer family: `.sd-tabs > span.tb.on`. The active modifier here is
    // `on` (weight-600 `.sd-tabs .tb.on`) — a different idiom from the
    // workspace `active`. `tab_strip_families_stay_distinct` pins the two
    // weights apart on the CSS side; pin the two active-class idioms apart
    // on the emission side so a "unify the tab strips" refactor can't
    // collapse one family onto the other's class and shift its weight.
    emits(TABS, r#"class="sd-tabs""#, ".sd-tabs");
    emits(TABS, r#""tb on""#, ".sd-tabs .tb.on");
    assert!(
        !TABS.contains(r#""tb active""#) && !TABS.contains("class:on"),
        "the drawer strip's active idiom must stay `\"tb on\"` and the \
         workspace strip's `class:active` — merging them (e.g. `class:on` \
         on the workspace tab, or `\"tb active\"` on the drawer tab) points \
         a strip at the other family's font-weight rule, a C5 regression"
    );
}

#[test]
fn badge_emits_the_bdg_class_and_not_the_rail_chip_class() {
    // <Badge> composes `bdg {tone}` via the natively-tested badge_class.
    emits(BADGE, "badge_class(tone)", ".bdg");
    emits(BADGE_TONE, r#"format!("bdg {}""#, ".bdg");
    // The base class must stay `bdg`: a top-level `.badge` rule would
    // leak display/text-transform into the rail count chip, which owns
    // `.rail .it .badge` in the same stylesheet.
    assert!(
        !BADGE_TONE.contains(r#""badge {}""#),
        "Badge must not adopt the `badge` base class — it collides with \
         the rail count chip's `.rail .it .badge` family"
    );
}

#[test]
fn status_dot_emits_the_status_dot_class() {
    emits(STATUS_DOT_TONE, r#""status-dot""#, ".status-dot");
    emits(
        STATUS_DOT_TONE,
        r#"format!("status-dot {suffix}")"#,
        ".status-dot.success",
    );
}

#[test]
fn sparkline_emits_the_spark_class() {
    emits(SPARKLINE, r#"class="sc-spark""#, ".sc-spark");
}

#[test]
fn loaded_emits_the_tri_state_hint_hooks() {
    emits(LOADED, r#"class="load-hint""#, ".load-hint");
    emits(LOADED, r#"class="load-hint error""#, ".load-hint.error");
    // The Missing arm renders on the neutral hint tone (a missing
    // resource is not a failure — no `.error` modifier), with the
    // subtitle hook for the explanatory second line.
    emits(LOADED, "missing_copy(label)", ".load-hint");
    emits(LOADED, r#"class="load-sub""#, ".load-hint .load-sub");
    assert!(
        !LOADED.contains(r#"<div class="load-hint error">{missing"#),
        "the Missing arm must stay on the neutral .load-hint tone, not \
         the red .load-hint.error treatment"
    );
}

#[test]
fn segmented_emits_one_strip_family_with_a_single_active_treatment() {
    // <Segmented> composes the container class via the natively-tested
    // segmented_class (Default/Sm/Xs x full table in segmented::class);
    // the pure layer owns the `seg`/`seg-sm`/`seg-full` fragment.
    emits(SEGMENTED, "segmented_class(size, full)", ".seg");
    emits(SEGMENTED_CLASS, r#"String::from("seg")"#, ".seg");
    emits(SEGMENTED, r#"class="seg-opt""#, ".seg .seg-opt");
    emits(SEGMENTED, "class:on", ".seg .seg-opt.on");
    // ADR-0003: one accent-wash active treatment. The size axis must not
    // grow its own active rule — exactly one `.seg-opt.on` declaration
    // ships in the stylesheet.
    let active_rules = FLEET_CSS.matches(".seg-opt.on").count();
    assert_eq!(
        active_rules, 1,
        "expected exactly one `.seg-opt.on` active rule in fleet-ui.css \
         (single accent-wash treatment across sizes), found {active_rules}"
    );
}

#[test]
fn pager_emits_the_results_footer_family() {
    emits(PAGER, r#"class="results-footer""#, ".results-footer");
    emits(PAGER, r#"class="results-summary""#, ".results-summary");
    emits(PAGER, r#"class="results-pager""#, ".results-pager");
}

#[test]
fn search_input_emits_the_inp_wrap_hook() {
    emits(SEARCH_INPUT, r#"class="inp-wrap""#, ".inp-wrap");
}

#[test]
fn toggle_emits_the_switch_hooks() {
    emits(TOGGLE, r#"class="toggle""#, ".toggle");
    emits(TOGGLE, r#"class="toggle-slider""#, ".toggle-slider");
}

#[test]
fn kbd_emits_both_chip_treatments() {
    emits(KBD, r#""kbd-inline""#, ".kbd-inline");
    emits(KBD, r#""kbd""#, ".kbd");
}

#[test]
fn actions_menu_emits_its_hooks_and_registers_with_the_overlay_stack() {
    emits(ACTIONS_MENU, r#"class="actions-wrap""#, ".actions-wrap");
    emits(ACTIONS_MENU, r#"class="btn-icon""#, ".btn-icon");
    // The panel class is a prop of the shared menu panel now, so the
    // hook that `.actions-menu` styles is the value ActionsMenu passes;
    // the item classes it styles are emitted in menu.rs.
    emits(
        ACTIONS_MENU,
        r#"panel_class="actions-menu""#,
        ".actions-menu",
    );
    emits(MENU, r#""item danger""#, ".actions-menu .item.danger");
    assert!(
        ACTIONS_MENU.contains("MenuPanel"),
        "ActionsMenu must mount the shared menu panel — a second local \
         panel is how the two menus drifted apart before ADR-0028"
    );
    // The open panel arbitrates Escape through the overlay stack
    // (topmost-only), like Modal and Drawer. That registration moved
    // into menu.rs with the panel, so assert it where it lives.
    assert!(
        MENU.contains("use_overlay_layer()") && MENU.contains("is_topmost()"),
        "the shared menu panel must register an overlay layer and gate \
         its window Escape on is_topmost() — otherwise Escape under a \
         stacked ConfirmModal closes both (the issue #28 bug class the \
         overlay stack exists to prevent)"
    );
    assert!(
        !ACTIONS_MENU.contains(r#"query_selector(".item")"#),
        "the local initial-focus scan is gone: initial focus is the \
         shared overlay::focus_initial scan, which respects the roving \
         tabindex instead of grabbing the first .item"
    );
    assert!(
        ACTIONS_MENU.contains(r#"aria-label="Actions""#),
        "the ⋯ trigger has no text content — without aria-label its \
         accessible name is the glyph"
    );
}

#[test]
fn the_menu_contract_is_shared_and_walks_by_index() {
    // Both menus mount one panel, and that panel is the only place the
    // lifecycle lives: no second set of window listeners anywhere.
    assert!(
        !markup_only(ACTIONS_MENU).contains("use_event_listener"),
        "menu lifecycle (Escape, outside mousedown) belongs to menu.rs \
         alone — a second listener is a second contract"
    );
    // The walk indexes the queried [role="menuitem"] list. The element
    // sibling walk it replaced hopped whatever came next, so a header
    // or a separator could take focus.
    assert!(
        MENU.contains(r#"[role="menuitem"]"#) && MENU.contains("next_index"),
        "the arrow walk must index the queried menuitem list through \
         roving::next_index"
    );
    assert!(
        !MENU.contains("next_element_sibling"),
        "no element-sibling walk: a header or separator would take focus"
    );
    assert!(
        MENU.contains("restores_trigger") && ROVING.contains("fn next_index"),
        "restore-by-cause and the index arithmetic are the two pure \
         halves this contract is native-tested through"
    );
    // One tab stop: exactly one item at 0, the rest at -1.
    assert!(
        MENU.contains(r#"if focused.get() == index { "0" } else { "-1" }"#),
        "menu items carry a true roving tabindex — every item tabbable \
         puts N tab stops in the page and defeats the arrow walk"
    );
}

#[test]
fn topbar_menu_is_native_and_registers_with_the_stack() {
    // The trigger paints through `.topbar .user` and the panel through
    // `.user-menu`; the header/name/mail hooks are the identity block
    // the panel renders outside role="menu".
    emits(TOPBAR, r#"class="user""#, ".topbar .user");
    emits(TOPBAR, r#"class="avatar""#, ".topbar .user .avatar");
    emits(TOPBAR, r#"class="who""#, ".topbar .user .who");
    emits(TOPBAR, r#"panel_class="user-menu""#, ".user-menu");
    emits(TOPBAR, r#"class="hdr""#, ".user-menu .hdr");
    emits(TOPBAR, r#"class="mail""#, ".user-menu .hdr .mail");
    emits(MENU, r#"class="sep""#, ".user-menu .sep");

    // A native trigger that reports its state, not a div with on:click.
    assert!(
        TOPBAR.contains("<button") && TOPBAR.contains(r#"type="button""#),
        "the account trigger must be a native <button type=\"button\"> \
         — a div reaches neither the keyboard nor the focus ring"
    );
    assert!(
        TOPBAR.contains(r#"aria-haspopup="menu""#) && TOPBAR.contains("aria-expanded"),
        "the trigger must announce that it opens a menu and whether it \
         is open"
    );
    assert!(
        TOPBAR.contains("MenuPanel"),
        "the account menu mounts the shared menu panel — its own panel \
         is how it ended up with no layer, no Escape and no restore"
    );
    assert!(
        !markup_only(TOPBAR).contains("use_event_listener"),
        "the menu lifecycle belongs to menu.rs; a listener here is a \
         second contract"
    );

    // ADR-0025's retirements, and the one box that stays. Scanned
    // against the markup: the module doc names each retired affordance
    // to explain why it left.
    let markup = markup_only(TOPBAR);
    assert!(
        !markup.contains("iconbtn"),
        "the notifications bell is retired — it was never wired"
    );
    assert!(
        !markup.contains("⌘⇧L"),
        "the theme chord is not bound (it collides with Bitwarden's \
         autofill and Safari's own binding), so the hint chip would be \
         a lie"
    );
    assert!(
        !markup.contains("API tokens") && !markup.contains("Profile"),
        "the two disabled rows are retired — a menu item that cannot be \
         activated is not a menu item"
    );
    assert!(
        !markup.contains(r#"class="overlay""#),
        "the menu's private full-viewport scrim is gone: dismissal is \
         the shared outside-mousedown check, so a modal above the menu \
         arbitrates instead of being covered"
    );
    assert!(
        TOPBAR.contains("Command palette"),
        "the ⌘K stub stays exactly as it is until the palette slice \
         (ADR-0028, human ruling)"
    );
}

#[test]
fn modal_family_traps_focus_and_keeps_aria_modal() {
    // The modal family renders aria-modal="true" and earns it: the shell
    // registers FocusPolicy::Trap with the overlay stack (initial focus,
    // Tab/Shift+Tab cycle, restore-to-opener) and hands the glue its
    // panel element with a tabindex="-1" fallback.
    assert!(
        MODAL_SHELL.contains(r#"aria-modal="true""#),
        "the modal shell must keep aria-modal=\"true\" — it traps focus, \
         so the semantics are honest"
    );
    assert!(
        MODAL_SHELL.contains("FocusPolicy::Trap") && MODAL_SHELL.contains("use_overlay_layer_with"),
        "the modal shell must register FocusPolicy::Trap via \
         use_overlay_layer_with — dropping it reopens the issue #33 \
         defect (aria-modal with zero focus management)"
    );
    assert!(
        MODAL_SHELL.contains(r#"tabindex="-1""#),
        "the modal panel needs tabindex=\"-1\" so the initial-focus \
         fallback can land on the panel itself"
    );
}

#[test]
fn drawer_is_an_honest_non_modal_dialog() {
    // The drawer is non-modal by design (background stays interactive;
    // modal-over-live-drawer is a supported stack), so it must not claim
    // aria-modal. It keeps role="dialog" and registers
    // FocusPolicy::Capture (initial focus + restore, no trap).
    assert!(
        DRAWER.contains(r#"role="dialog""#),
        "the drawer keeps role=\"dialog\""
    );
    // Scan for the attribute form (`aria-modal=`) rather than the bare
    // token: the drawer's comments legitimately explain WHY the
    // attribute is absent.
    assert!(
        !DRAWER.contains("aria-modal="),
        "the drawer must not render an aria-modal attribute — screen \
         readers would be told the background is gone while keyboard \
         users tab straight out (the issue #33 defect)"
    );
    assert!(
        DRAWER.contains("FocusPolicy::Capture") && DRAWER.contains("use_overlay_layer_with"),
        "the drawer must register FocusPolicy::Capture via \
         use_overlay_layer_with — initial focus on open, restore on \
         close, and no Tab trap"
    );
    assert!(
        DRAWER.contains(r#"tabindex="-1""#),
        "the drawer panel needs tabindex=\"-1\" so the initial-focus \
         fallback can land on the panel itself"
    );
}

#[test]
fn icon_ships_the_slice_d_glyphs_and_the_crate_doc_is_honest() {
    // Document / Upload / Copy belong to the closed enum, each with an
    // icon_body arm in house style.
    for glyph in ["Document", "Upload", "Copy"] {
        assert!(
            ICON.contains(&format!("    {glyph},\n"))
                && ICON.contains(&format!("Icon::{glyph} =>")),
            "Icon::{glyph} must exist as a variant with an icon_body arm"
        );
    }
    // The lib.rs crate doc must describe its exports by category, never
    // by count: a hard-coded number rots the next time one lands.
    assert!(
        !LIB.contains("four typed components"),
        "lib.rs crate doc must not hard-code a component count (doc rot)"
    );
}

#[test]
fn drawer_and_tabs_meta_is_reactive() {
    // `meta` is a reactive optional (MaybeProp) on both Tabs and the
    // Drawer that forwards to it — live counts must tick. Static Strings
    // still convert via `into`, so call sites with snapshot copy compile
    // unchanged.
    assert!(
        TABS.contains("MaybeProp<String>"),
        "Tabs meta must be a reactive MaybeProp<String>"
    );
    assert!(
        DRAWER.contains("MaybeProp<String>"),
        "Drawer meta must stay a reactive MaybeProp<String> forwarded \
         to Tabs"
    );
}

#[test]
fn copy_button_reports_through_the_shared_toast_bus() {
    // One click-to-copy component, wired to the Shell's ToastBus (never
    // a second bus), with the canonical "Copied" / "Copy failed" toast
    // titles.
    assert!(
        COPY_BUTTON.contains("expect_context::<ToastBus>()"),
        "CopyButton must resolve the Shell-owned ToastBus from context"
    );
    assert!(
        COPY_BUTTON.contains(r#""Copied""#) && COPY_BUTTON.contains(r#""Copy failed""#),
        "the toast titles are canonical copy — apps customize only the \
         success detail line"
    );
    assert!(
        COPY_BUTTON.contains("stop_propagation"),
        "copy triggers sit inside clickable rows — the click must not \
         bubble into the host row handler"
    );
}

#[test]
fn load_more_emits_its_hooks_and_the_canonical_busy_label() {
    // Cursor-driven list footer. The three terminal states (button /
    // end-of-list / empty) are pinned natively in load_more::phase
    // tests; these pin the class hooks and that the busy label is the
    // canonical loading copy, not a bespoke string.
    emits(LOAD_MORE, r#"class="load-more""#, ".load-more");
    emits(
        LOAD_MORE,
        r#"class="load-more-end""#,
        ".load-more .load-more-end",
    );
    assert!(
        LOAD_MORE.contains("loading_copy(None)"),
        "the busy label must be the canonical loading_copy — slice C \
         normalized generic status copy, LoadMore must not fork it"
    );
}

#[test]
fn when_renders_both_modes_off_the_shared_clock() {
    // <When> renders relative (time_ago buckets) and absolute
    // ("%Y-%m-%d %H:%M UTC" — explicit zone marker) modes, always with
    // the full RFC 3339 form in the title attr.
    emits(WHEN, r#"class="when""#, ".when");
    assert!(
        WHEN.contains("title=") && WHEN.contains("to_rfc3339"),
        "<When> must carry the full RFC 3339 timestamp in its title attr"
    );
    assert!(
        WHEN.contains("%Y-%m-%d %H:%M UTC"),
        "absolute mode renders \"%Y-%m-%d %H:%M UTC\" — nothing else in \
         the app signals timezone, the explicit marker is the point"
    );
    // One shared tick drives every instance: <When> subscribes to
    // clock::now_ms and must never own a timer of its own.
    assert!(
        WHEN.contains("clock::now_ms") && !WHEN.contains("Interval"),
        "<When> must subscribe to the shared clock tick, never a \
         per-instance interval"
    );
    assert!(
        CLOCK.contains("Interval::new(30_000") && CLOCK.contains("is_some"),
        "the shared clock is a single 30s interval installed once \
         (install() re-entry is a no-op)"
    );
}

#[test]
fn error_banner_emits_error_class_with_alert_role() {
    // ErrorBanner paints via `.error-banner`, not a bare `.error`: that
    // selector would leak banner padding onto every element using
    // `error` as a state token (`.status-dot.error`, `.load-hint.error`).
    // Pin both — the class so it paints, the role so the sanctioned a11y
    // delta isn't silently dropped.
    emits(ERROR_BANNER, r#"class="error-banner""#, ".error-banner");
    assert!(
        ERROR_BANNER.contains(r#"role="alert""#),
        "ErrorBanner must keep role=\"alert\" — the one sanctioned DOM \
         delta of the issue #28 migration (attribute-only, zero pixels)"
    );
}

#[test]
fn atmosphere_emits_its_hook_and_is_hidden_from_the_accessibility_tree() {
    // The shader backdrop mounts into `.atmosphere`, the fixed
    // full-viewport layer whose CSS paints the var(--bg) fallback floor
    // for browsers without WebGL.
    emits(ATMOSPHERE, r#"class="atmosphere""#, ".atmosphere");
    // Pure decoration: never in the accessibility tree.
    assert!(
        ATMOSPHERE.contains(r#"aria-hidden="true""#),
        "the atmosphere layer is decorative — it must carry \
         aria-hidden=\"true\" so screen readers skip the canvas"
    );
}

#[test]
fn atmosphere_disposes_its_shader_mount_on_cleanup() {
    // A leaked ShaderHandle keeps a rAF loop + WebGL context alive for
    // the life of the page, one per route transition.
    assert!(
        ATMOSPHERE.contains("on_cleanup") && ATMOSPHERE.contains("dispose"),
        "Atmosphere must dispose its ShaderHandle in on_cleanup — \
         otherwise every route transition leaks a canvas and its rAF \
         loop (the mount/dispose contract of jakub/coastwatch#308 AC 3)"
    );
}
