// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Atmosphere palette ↔ stylesheet parity (jakub/coastwatch#308).
//!
//! The shader palettes live in Rust (`atmosphere::palette`) because
//! `getShaderColorFromString` cannot parse the `oklch()`/`color-mix()`
//! tokens the Mira Blue stylesheet uses — the palettes MIRROR the CSS
//! by necessity instead of reading it. Mirrors drift, so this test
//! pins the two literal-hex anchors the palettes are seeded from (the
//! only literal-hex blue tokens in fleet-ui.css: `--accent` per theme)
//! and asserts the `.atmosphere` CSS fallback floor stays `var()`-only —
//! the floor must re-theme through the token system, never through a
//! second hard-coded color that could drift from `--bg`.

use fleet_ui::atmosphere::palette;

const FLEET_CSS: &str = include_str!("../styles/fleet-ui.css");

/// Extract the first `--accent:` declaration value after `marker`.
///
/// Markers are the selector lines at line start (`\n`-prefixed) — the
/// header comment also mentions the `[data-theme=...]` attribute
/// names, and matching prose would silently read the wrong block.
fn accent_after(marker: &str) -> String {
    let block_start = FLEET_CSS
        .find(marker)
        .unwrap_or_else(|| panic!("marker `{marker}` not found in fleet-ui.css"));
    let rest = &FLEET_CSS[block_start..];
    let decl_start = rest
        .find("--accent:")
        .unwrap_or_else(|| panic!("no --accent: declaration after `{marker}`"));
    let value = &rest[decl_start + "--accent:".len()..];
    let end = value.find(';').expect("--accent declaration unterminated");
    value[..end].trim().to_string()
}

#[test]
fn light_palette_carries_the_css_light_accent() {
    let accent = accent_after("\n:root, [data-theme=\"light\"]");
    assert!(
        palette::LIGHT_COLORS.contains(&accent.as_str()),
        "fleet-ui.css light --accent is `{accent}` but the light \
         atmosphere palette {:?} does not carry it — the Rust mirror \
         drifted from the stylesheet",
        palette::LIGHT_COLORS
    );
}

#[test]
fn dark_palette_carries_the_css_dark_accent() {
    let accent = accent_after("\n[data-theme=\"dark\"]");
    assert!(
        palette::DARK_COLORS.contains(&accent.as_str()),
        "fleet-ui.css dark --accent is `{accent}` but the dark \
         atmosphere palette {:?} does not carry it — the Rust mirror \
         drifted from the stylesheet",
        palette::DARK_COLORS
    );
}

#[test]
fn atmosphere_css_floor_is_var_only() {
    let rule_start = FLEET_CSS
        .find(".atmosphere {")
        .expect(".atmosphere rule missing from fleet-ui.css");
    let rest = &FLEET_CSS[rule_start..];
    let body_end = rest.find('}').expect(".atmosphere rule unterminated");
    let body = &rest[..body_end];
    assert!(
        body.contains("background: var(--bg)"),
        ".atmosphere must paint the theme-reactive var(--bg) floor — \
         it is the WebGL-unavailable fallback (AC 6) and must be \
         painted before/without the shader"
    );
    assert!(
        !body.contains('#'),
        ".atmosphere floor must stay var()-only — a literal color \
         here would not re-theme and could drift from --bg: {body}"
    );
}

#[test]
fn atmosphere_geometry_is_pinned() {
    // The layering contract every consumer composes against: fixed
    // full-viewport, painted BELOW normal flow, and transparent to
    // input. Losing any one of these silently breaks the backdrop —
    // content stops scrolling over it, it occludes clicks, or it paints
    // above the login card.
    let rule_start = FLEET_CSS
        .find(".atmosphere {")
        .expect(".atmosphere rule missing from fleet-ui.css");
    let body = &FLEET_CSS[rule_start..];
    let body = &body[..body.find('}').expect(".atmosphere rule unterminated")];
    for decl in [
        "position: fixed",
        "inset: 0",
        "z-index: -1",
        "pointer-events: none",
    ] {
        assert!(
            body.contains(decl),
            ".atmosphere must declare `{decl}` — the backdrop layering \
             contract (fixed full-viewport, below normal flow, input- \
             transparent) that consumers compose against: {body}"
        );
    }
}

#[test]
fn html_selector_declares_no_background() {
    // The .login-shell background removal (css_chrome_parity) depends on
    // an invariant nothing else states: `body`'s `background: var(--bg)`
    // PROPAGATES to the canvas only while `html` declares no background
    // of its own. A future `html { background: … }` would silently
    // re-occlude nothing but ALSO stop body's floor from reaching the
    // viewport behind the z-index:-1 canvas. Scan every html-selector
    // rule body for a background declaration.
    let mut search_from = 0;
    while let Some(pos) = FLEET_CSS[search_from..].find("html") {
        let abs = search_from + pos;
        search_from = abs + 4;
        // Only selector positions: preceded by newline/space/comma and
        // part of a selector list that opens a block before a `;`.
        let prev = FLEET_CSS[..abs].chars().next_back();
        if !matches!(prev, None | Some('\n' | ' ' | ',')) {
            continue; // prose in a comment, e.g. "…html"
        }
        let rest = &FLEET_CSS[abs..];
        let Some(open) = rest.find('{') else { continue };
        let head = &rest[..open];
        if head.contains(';') || head.contains("/*") || head.contains('\n') && head.contains('.') {
            continue; // not a selector for this occurrence
        }
        let block = &rest[open..=open + rest[open..].find('}').unwrap_or(rest.len() - open - 1)];
        assert!(
            !block.contains("background"),
            "an `html` selector rule declares a background — this stops \
             body's var(--bg) from propagating to the viewport canvas \
             and breaks the .atmosphere degradation floor: {block}"
        );
    }
}
