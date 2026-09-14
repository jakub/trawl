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
const LOGIN: &str = include_str!("../src/login/component.rs");

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
const SHELL: &str = include_str!("../src/shell.rs");
const SIDEBAR: &str = include_str!("../src/sidebar.rs");
const COMMAND_PALETTE: &str = include_str!("../src/command_palette.rs");
const TOAST_RUNTIME: &str = include_str!("../src/toast/runtime.rs");
const ROVING: &str = include_str!("../src/roving.rs");
const COPY_BUTTON: &str = include_str!("../src/copy_button.rs");
const ICON: &str = include_str!("../src/icon.rs");
const LIB: &str = include_str!("../src/lib.rs");
const LOAD_MORE: &str = include_str!("../src/load_more.rs");
const WHEN: &str = include_str!("../src/time/when.rs");
const CLOCK: &str = include_str!("../src/time/clock.rs");
const ATMOSPHERE: &str = include_str!("../src/atmosphere/component.rs");
const FLEET_CSS: &str = include_str!("../styles/fleet-ui.css");

/// The `type="button"` attribute every converted control carries: a
/// type-less button inside a form submits it.
const TYPE_BUTTON: &str = "type=\"button\"";

/// The source with its comment lines removed, for the negative scans:
/// a module doc that explains why an affordance was retired names the
/// affordance, and prose is not markup. Same trick as trawl-core's
/// `now_anchor_contract`.
///
/// Block comments go first, counting nesting the way rustc does, then
/// any line whose first non-space characters are `//`, which covers
/// `//`, `///` and `//!` alike. A trailing comment after code on the
/// same line stays: removing it would need to know where string
/// literals end, and a literal is where hooks like `title="Close
/// (Esc)"` legitimately live. `view!` markup is not a string literal,
/// so nothing here strips quoted text.
fn markup_only(src: &str) -> String {
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
            // Byte-wise: outside a comment every byte is copied, so a
            // multi-byte character survives intact; inside one only the
            // newline is kept, which cannot split a character.
            if depth == 0 {
                kept.push(bytes[i]);
            } else if bytes[i] == b'\n' {
                kept.push(b'\n');
            }
            i += 1;
        }
    }
    let stripped =
        String::from_utf8(kept).expect("dropping whole comment spans keeps the rest valid");
    stripped
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn markup_only_hides_prose_and_keeps_markup() {
    for (label, src) in [
        ("///", "/// class=\"iconbtn\"\nfn f() {}\n"),
        ("//!", "//! class=\"iconbtn\"\nfn f() {}\n"),
        ("//", "// class=\"iconbtn\"\nfn f() {}\n"),
        ("/* */", "/* class=\"iconbtn\" */\nfn f() {}\n"),
        ("nested /* */", "/* a /* b */ class=\"iconbtn\" */\n"),
    ] {
        assert!(
            !markup_only(src).contains("class=\"iconbtn\""),
            "a hook living only in a {label} comment must not satisfy a \
             positive scan, nor defeat a negative one — prose is not markup"
        );
    }
    assert!(
        markup_only("view! { <span class=\"iconbtn\"></span> } // why\n")
            .contains("class=\"iconbtn\""),
        "real markup must survive stripping, trailing comment and all"
    );
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
    //
    // The host's two classes are conditional on `docked` (ADR-0032):
    // the scrim paints the overlay presentation, `.sd-host` collapses
    // the host to `display: contents` so the docked panel becomes a
    // child of the layout that placed it.
    emits(DRAWER, "class:sd-scrim=", ".sd-scrim");
    emits(DRAWER, "class:sd-host=", ".sd-host");
    emits(DRAWER, "class:sd-docked=", ".sd-drawer.sd-docked");
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
    // The trailing slot's wrapper: `.tabs .tabs-actions` is what keeps
    // the consumer's actions on one line when the strip wraps.
    emits(TABS, r#"class="tabs-actions""#, ".tabs .tabs-actions");

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
fn tabs_render_a_named_tablist() {
    // ADR-0028: the tabs are native buttons inside a named tablist,
    // and only the tabs are inside it — the spacer, the trailing
    // action slot and the drawer's meta text are siblings of that
    // node, so a Save link is never announced as a tab. Scanned
    // against the markup, so prose about a role cannot stand in for
    // one.
    let tabs = markup_only(TABS);
    assert!(
        tabs.contains(r#"class="tablist""#) && tabs.contains("role=role"),
        "the strip must render a `.tablist` node whose role is the \
         computed one — an unconditional role would name an empty strip \
         (the field case drawer passes no tabs at all)"
    );
    assert!(
        tabs.contains(r#"role="tab""#) && tabs.contains("aria-selected"),
        "each tab must carry role=\"tab\" and aria-selected — without \
         them the strip is a row of buttons with no relationship"
    );
    assert!(
        tabs.contains(r#"type="button""#),
        "tabs are <button type=\"button\">: inside a form, a type-less \
         button submits it"
    );
    // Selection-derived roving tabindex: one predicate over
    // roving::resolve_selected, no focus state of its own (the
    // asymmetry roving.rs documents). Going through that helper is
    // what makes an id matching no tab select the first tab instead of
    // leaving the strip with no tab stop at all.
    assert!(
        tabs.contains(r#"if selected.get() == Some(i) { "0" } else { "-1" }"#)
            && TABS.contains("resolve_selected"),
        "the strip's single tab stop is derived from the selected tab, \
         resolved through roving::resolve_selected — every tab tabbable \
         puts N stops in the page and defeats the arrow walk, and no tab \
         tabbable (an unknown ?ntab= id) leaves the strip unreachable"
    );
    assert!(
        TABS.contains("horizontal_nav") && TABS.contains("next_index"),
        "the arrow walk must go through roving::horizontal_nav and \
         roving::next_index — a second key map is a second contract"
    );
    assert!(
        !TABS.contains("next_element_sibling"),
        "no element-sibling walk: the spacer and the trailing slot sit \
         in the same container and would take focus"
    );
    // Activation is manual: arrows move focus and nothing else. The
    // walk lives in its own function that is not handed the callback,
    // so it cannot select — Enter and Space reach on_change through the
    // native button's click.
    let walk = TABS
        .split_once("fn tab_keydown(")
        .expect("the arrow walk is its own function")
        .1;
    assert!(
        !walk.contains("on_change"),
        "the arrow walk must never run on_change — arrowing across \
         trawl's ?ntab= strip would rewrite the URL under the user"
    );
    // Both drawer families name their strip.
    assert!(
        DRAWER.contains("tabs_label") && DRAWER.contains("label=tabs_label"),
        "Drawer must require a tabs_label and forward it verbatim — the \
         drawer is the only thing that knows what its strip lists"
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
        markup_only(ACTIONS_MENU).contains(r#"aria-label="Actions""#),
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
        markup_only(MENU).contains(r#"[role="menuitem"]"#) && MENU.contains("next_index"),
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
        markup_only(MENU).contains(r#"if focused.get() == index { "0" } else { "-1" }"#),
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
    // Every tag and attribute below is scanned against the markup: the
    // module doc names each affordance it explains.
    let markup = markup_only(TOPBAR);
    assert!(
        markup.contains("<button") && markup.contains(r#"type="button""#),
        "the account trigger must be a native <button type=\"button\"> \
         — a div reaches neither the keyboard nor the focus ring"
    );
    assert!(
        markup.contains(r#"aria-haspopup="menu""#) && markup.contains("aria-expanded"),
        "the trigger must announce that it opens a menu and whether it \
         is open"
    );
    // One predicate for the mount and for the reported state. Split
    // them and a lost identity unmounts the panel while the trigger
    // still reads aria-expanded="true".
    assert!(
        markup.contains("<Show when=move || panel_open.get()>")
            && markup.contains("aria-expanded=move || panel_open.get().to_string()"),
        "the panel's <Show when=> and the trigger's aria-expanded must \
         read the same derived predicate"
    );
    assert!(
        markup.contains("Effect::new(move |_| {")
            && markup.contains("if user.get().is_none() {")
            && markup.contains("menu_open.set(false);"),
        "an effect must clear the open flag when the identity is lost — \
         otherwise the next identity remounts the menu, and runs its \
         initial-focus effect, with no user activation behind it"
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

    // ADR-0025's retired controls stay absent.
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
}

#[test]
fn sidebar_emits_an_unconditional_group_wrapper_and_names_labelled_groups() {
    // The wrapper is unconditional so `.rail .grp > a[title]` addresses
    // every destination, labelled group or not; `role`/`aria-label` ride
    // only the labelled ones, because a nameless role="group" is an axe
    // defect (ADR-0032).
    let sidebar = markup_only(SIDEBAR);
    for required in [
        r#"class="grp""#,
        "role=",
        "aria-label=",
        r#"class="grp-lb""#,
        "attr:title=label_attr",
        "aria-current=",
        r#"class="bot""#,
        r#"class="it collapse""#,
        r#"id="fleet-sidebar""#,
        r#"aria-label="Primary""#,
    ] {
        assert!(sidebar.contains(required), "Sidebar lost {required}");
    }
    emits(SIDEBAR, r#"class="rail""#, "nav.rail");
    emits(SIDEBAR, r#"class="lb""#, ".rail .it .lb");
    emits(SIDEBAR, r#"class="badge""#, ".rail .it .badge");
    assert!(
        sidebar.contains("<button") && sidebar.contains(TYPE_BUTTON),
        "the collapse control must be a native <button type=\"button\"> \
         — it changes presentation, it is not a destination"
    );
}

#[test]
fn shell_mounts_the_nav_overlay_as_a_capture_layer() {
    // Below 900px navigation is an overlay, not a docked column: same
    // policy the Drawer uses, so Escape only closes the topmost layer,
    // the background stays interactive and the palette chord is inert
    // while it is open.
    let shell = markup_only(SHELL);
    for required in [
        "FocusPolicy::Capture",
        r#"class="nav-scrim""#,
        r#"use_media_query("(max-width: 899.98px)")"#,
        "is_topmost",
        r#"class="shell-content""#,
    ] {
        assert!(
            shell.contains(required),
            "Shell nav overlay lost {required}"
        );
    }
    let topbar = markup_only(TOPBAR);
    for required in [
        r#"class="nav-toggle""#,
        r#"aria-controls="fleet-sidebar""#,
        r#"class="crumb""#,
        r#"<header class="topbar">"#,
    ] {
        assert!(topbar.contains(required), "command bar lost {required}");
    }
}

#[test]
fn command_palette_trigger_is_a_live_native_button() {
    let topbar = markup_only(TOPBAR);
    let start = topbar
        .find("<Show when=move || palette_available.get()>")
        .unwrap();
    let trigger = topbar[start..].split("</Show>").next().unwrap();
    for required in [
        "<button",
        "type=\"button\"",
        "class=\"jump\"",
        "aria-haspopup=\"dialog\"",
        "aria-expanded=move || palette_open.get().to_string()",
        "aria-keyshortcuts=hint.aria_keyshortcuts",
        "node_ref=palette_trigger",
        "on_open_palette.run(())",
        "<Kbd>{hint.label}</Kbd>",
        "Go to…",
    ] {
        assert!(
            trigger.contains(required),
            "palette trigger lost {required}"
        );
    }
    for required in [
        "on_open_palette: Callback<()>",
        "palette_open: Signal<bool>",
        "palette_available: Signal<bool>",
        "platform_kbd_hint(",
    ] {
        assert!(topbar.contains(required), "TopBar lost {required}");
    }
    assert!(!topbar.contains("coming soon"));
    assert!(trigger.contains("aria-label=\"Go to… Command palette\""));
    assert!(!trigger.contains("Search…"));
}

#[test]
fn command_palette_dialog_uses_trap_combobox_and_real_router_options() {
    let palette = markup_only(COMMAND_PALETTE);
    for required in [
        "use_overlay_layer_with(FocusPolicy::Trap",
        "role=\"dialog\"",
        "aria-label=\"Command palette\"",
        "aria-modal=\"true\"",
        "role=\"combobox\"",
        "aria-controls=\"fleet-command-palette-list\"",
        "aria-activedescendant=",
        "role=\"listbox\"",
        "id=\"fleet-command-palette-list\"",
        "role=\"group\"",
        "<A",
        "attr:role=\"option\"",
        "attr:tabindex=\"-1\"",
        "attr:aria-selected=",
        "anchor.click()",
        "is_composing()",
        "next_index(",
        "ScrollLogicalPosition::Nearest",
        "command.is_current(&pathname.get())",
        "role=\"status\"",
        "aria-live=\"polite\"",
        "aria-label=\"Close command palette\"",
        "type=\"button\"",
        "ordinary_click(&event)",
        "layer.is_topmost()",
    ] {
        assert!(
            palette.contains(required),
            "command palette lost {required}"
        );
    }
    assert!(!palette.contains("on:mouseover") && !palette.contains("on:mouseenter"));
    assert!(!palette.contains("Nav::First") && !palette.contains("Nav::Last"));
}

#[test]
fn command_palette_shell_gates_chords_and_retains_anchors_through_dispatch() {
    let shell = markup_only(SHELL);
    for required in [
        "commands_from(",
        "palette_available(items)",
        "has_layers()",
        "is_palette_chord(facts, is_macos)",
        "event.default_prevented()",
        "event.is_composing()",
        "event.repeat()",
        "editable_target(&event)",
        "UseEventListenerOptions::default().capture(false)",
        "is_some_and(OverlayLayer::is_topmost)",
        "if palette_open.get_untracked()",
        "trigger.focus()",
        "palette_open.set(false)",
        "queue_microtask(",
        "palette_open.try_get_untracked() == Some(false)",
        "palette_mounted.try_set(false)",
        "<Show when=move || palette_mounted.get() && available.get()>",
    ] {
        assert!(
            shell.contains(required),
            "Shell palette lifecycle lost {required}"
        );
    }
    let palette = markup_only(COMMAND_PALETTE);
    assert!(palette.contains("hidden=move || !open.get()"));
    assert!(palette.contains("is_content_editable"));
    assert!(
        palette.contains("text_input_is_editable(&input.type_(), disabled, input.read_only())")
    );
    assert!(palette.contains("!disabled && !textarea.read_only()"));
    assert!(palette.contains("matches(\":disabled\")"));
    assert!(palette.contains("closest(\"input, textarea\")"));
}

#[test]
fn command_palette_class_hooks_have_viewport_and_control_styles() {
    let palette = markup_only(COMMAND_PALETTE);
    for class in [
        "command-palette-scrim",
        "command-palette",
        "command-palette-search",
        "command-palette-input",
        "command-palette-close",
        "command-palette-list",
        "command-palette-group",
        "command-palette-group-label",
        "command-palette-option",
        "command-palette-label",
        "command-palette-path",
        "command-palette-current",
        "command-palette-empty",
        "command-palette-status",
    ] {
        assert!(
            palette.contains(&format!("class=\"{class}\"")),
            "missing markup hook {class}"
        );
        assert!(
            FLEET_CSS.contains(&format!(".{class} {{"))
                || FLEET_CSS.contains(&format!(".{class} +")),
            "missing CSS for {class}"
        );
    }
    for required in [
        ".command-palette-scrim[hidden] { display: none; }",
        "max-height: calc(100dvh",
    ] {
        assert!(FLEET_CSS.contains(required), "fleet-ui.css lost {required}");
    }
    assert!(
        palette.contains("ScrollLogicalPosition::Nearest"),
        "the palette lost its nearest-scroll keyboard follow"
    );
    let jump = FLEET_CSS
        .split(".topbar .jump {")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    for reset in [
        "appearance: none",
        "font: inherit",
        "text-align: left",
        "margin: 0",
        "cursor: pointer",
    ] {
        assert!(jump.contains(reset), "jump button reset lost {reset}");
    }
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
        DRAWER.contains("FocusPolicy::Capture") && DRAWER.contains("push_overlay_with"),
        "the drawer must register a FocusPolicy::Capture layer — initial \
         focus on open, restore on close, and no Tab trap. It pushes the \
         layer itself rather than through use_overlay_layer_with because \
         `docked` releases it at runtime (ADR-0032), so the registration \
         is pinned on the push, not on the hook"
    );
    assert!(
        DRAWER.contains(r#"tabindex="-1""#),
        "the drawer panel needs tabindex=\"-1\" so the initial-focus \
         fallback can land on the panel itself"
    );
    // A05: role="dialog" needs a host element that allows it, and
    // `<aside>` is a complementary landmark, which does not.
    assert!(
        DRAWER.contains(r#"<div class="sd-drawer""#),
        "the drawer panel must be a <div> host for role=\"dialog\" (A05)"
    );
    assert!(
        !DRAWER.contains("<aside"),
        "the drawer must render no <aside> — a complementary landmark is \
         not an allowed host for role=\"dialog\" (A05)"
    );
}

#[test]
fn icon_ships_the_slice_d_glyphs_and_the_crate_doc_is_honest() {
    // Document / Upload / Copy, and the sidebar's Menu / PanelLeft,
    // belong to the closed enum, each with an icon_body arm in house
    // style.
    for glyph in ["Document", "Upload", "Copy", "Menu", "PanelLeft"] {
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
fn modal_close_toast_dismiss_and_bare_copy_are_native_buttons() {
    // The last three pseudo-buttons in the crate (ADR-0028). Each has
    // to be reachable by Tab and operable by Enter and Space, which no
    // amount of CSS gives a <span on:click>.
    for (src, what) in [
        (MODAL_SHELL, "the modal close"),
        (TOAST_RUNTIME, "the toast dismiss"),
        (COPY_BUTTON, "the bare copy trigger"),
    ] {
        assert!(
            markup_only(src).contains("<button") && markup_only(src).contains(TYPE_BUTTON),
            "{what} must be a native <button type=\"button\"> — inside a \
             form a type-less button submits it, and a span reaches \
             neither the keyboard nor the ADR-0007 focus ring"
        );
    }
    // Both glyph-only controls take their whole accessible name from an
    // aria-label: a stroked X and a multiplication sign announce as
    // nothing useful. The class hooks stay `.x` either way.
    emits(MODAL_SHELL, "class=\"x\"", ".modal .m-hd .x");
    emits(TOAST_RUNTIME, "class=\"x\"", ".toast .x");
    assert!(
        markup_only(MODAL_SHELL).contains("aria-label=\"Close dialog\"")
            && markup_only(MODAL_SHELL).contains("title=\"Close (Esc)\""),
        "the modal close keeps a named label and the Esc hint in its \
         tooltip"
    );
    assert!(
        markup_only(TOAST_RUNTIME).contains("aria-label=\"Dismiss notification\"")
            && markup_only(TOAST_RUNTIME).contains("<span aria-hidden=\"true\">"),
        "the toast dismiss is named by aria-label, and its × is hidden \
         decoration — read aloud, \"times\" is not a dismissal"
    );
    // The bare copy trigger keeps the propagation stop it had as a
    // span: it sits inside clickable rows and header bars.
    assert!(
        COPY_BUTTON.contains("e.stop_propagation();"),
        "bare-mode copy must keep e.stop_propagation() — the click would \
         otherwise also open the row the trigger sits in"
    );
    // Neither glyph control may go back to a span with a click handler.
    for (src, name) in [
        (MODAL_SHELL, "modal/shell.rs"),
        (TOAST_RUNTIME, "toast/runtime.rs"),
        (COPY_BUTTON, "copy_button.rs"),
    ] {
        assert!(
            !markup_only(src).contains("<span\n                    class=cls")
                && !markup_only(src).contains("<span class=\"x\""),
            "{name} must not reopen a span with an on:click — that is the \
             shape ADR-0028 closed"
        );
    }
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
fn login_associates_its_error_with_the_input() {
    // A04: the login error rides a sibling banner, so `role="alert"`
    // announces it once and nothing associates it with the field it is
    // about. The input carries aria-invalid and points
    // aria-describedby at the banner's id, which ErrorBanner renders
    // from its `id` prop.
    assert!(
        LOGIN.contains("aria-invalid=") && LOGIN.contains("aria-describedby="),
        "the login input must carry aria-invalid and aria-describedby — \
         without them the alert is announced once and is unreachable \
         from the invalid field (A04)"
    );
    // One binding feeds the banner's id and the association, so the two
    // cannot drift apart; the test pins the value and both uses.
    assert!(
        LOGIN.contains(r#""login-error""#)
            && LOGIN.contains("id=ERROR_ID")
            && LOGIN.contains("then_some(ERROR_ID)"),
        "the banner id and the aria-describedby must be the same \
         `login-error` binding"
    );
    assert!(
        ERROR_BANNER.contains("id=id"),
        "ErrorBanner must render its `id` prop — an unrendered id leaves \
         login's aria-describedby pointing at nothing"
    );
    // A05/landmarks: /login routes outside the app shell, so the card
    // is the page's only chance at a <main>.
    assert!(
        LOGIN.contains(r#"<main class="login-shell">"#),
        "the login card must sit in a <main> landmark — /login never \
         reaches Shell's <main class=\"main\">"
    );
    assert!(
        !LOGIN.contains("<aside"),
        "the login card renders no complementary landmark"
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

#[test]
fn range_control_owns_its_markup_and_styles() {
    let source = markup_only(include_str!("../src/range_dialog.rs"));
    for (hook, selector) in [
        ("class=\"daterange\"", ".daterange"),
        ("class=\"dr-trigger\"", ".dr-trigger"),
        ("class=\"dr-pop\"", ".dr-pop"),
        ("class=\"scrim\"", ".scrim"),
        ("class=\"grid\"", ".dr-pop .grid"),
        ("class=\"opt\"", ".dr-pop .opt"),
        ("class=\"cust\"", ".dr-pop .cust"),
        ("class=\"fld\"", ".dr-pop .cust .fld"),
        ("class=\"lb\"", ".dr-pop .cust .fld .lb"),
        ("class=\"dr-from\"", ".dr-pop .cust .fld input"),
        ("class=\"dr-to\"", ".dr-pop .cust .fld input"),
        ("class=\"foot\"", ".dr-pop .foot"),
        ("class=\"btns\"", ".dr-pop .foot .btns"),
        ("class=\"dr-apply\"", ".dr-apply"),
        ("class=\"dr-err\"", ".dr-pop .dr-err"),
        ("class=\"rt-hint\"", ".dr-pop .rt-hint"),
    ] {
        assert!(source.contains(hook), "range hook missing: {hook}");
        assert!(
            FLEET_CSS.contains(&format!("{selector} {{")),
            "range style missing: {selector}"
        );
    }
    for hook in [
        "role=\"dialog\"",
        "aria-modal=\"true\"",
        "aria-label=\"Time range\"",
        "for=ids.get_value().0",
        "for=ids.get_value().1",
        "\"Relative\"",
        "\"Absolute\"",
        "\"Real-time\"",
    ] {
        assert!(source.contains(hook), "range semantic hook missing: {hook}");
    }
}

fn range_scrim_openings(source: &str) -> Vec<&str> {
    source
        .split("<div")
        .skip(1)
        .filter_map(|tail| {
            if !tail.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
                return None;
            }
            let mut quoted = false;
            let mut escaped = false;
            let mut depth = 0usize;
            for (index, ch) in tail.char_indices() {
                if quoted {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        quoted = false;
                    }
                    continue;
                }
                match ch {
                    '"' => quoted = true,
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth = depth.saturating_sub(1),
                    '>' if depth == 0 => return Some(&tail[..=index]),
                    _ => {}
                }
            }
            None
        })
        .filter(|tag| tag.contains("class=\"scrim\""))
        .collect()
}

#[test]
fn range_scrim_is_exactly_one_mousedown_dismisser() {
    let source = markup_only(include_str!("../src/range_dialog.rs"));
    let scrims = range_scrim_openings(&source);
    assert_eq!(scrims.len(), 1);
    assert!(scrims[0].contains("on:mousedown="));
    assert!(!scrims[0].contains("on:click="));
}

#[test]
fn range_scrim_scan_counts_paired_tags_and_duplicate_dismissers() {
    let paired = r#"<div class="scrim" on:mousedown=close></div>"#;
    assert_eq!(range_scrim_openings(paired).len(), 1);
    let duplicate =
        r#"<div class="scrim" on:mousedown=close/> <div class="scrim" on:click=bad></div>"#;
    let scrims = range_scrim_openings(duplicate);
    assert_eq!(scrims.len(), 2);
    assert!(scrims[1].contains("on:click="));
}
