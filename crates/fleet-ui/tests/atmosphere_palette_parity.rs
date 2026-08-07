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
