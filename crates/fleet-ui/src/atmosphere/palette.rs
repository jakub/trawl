// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Atmosphere knobs site.
//!
//! Every tunable of the shader backdrop lives here and only here:
//! palette arrays, shader selection, motion and texture knobs, so
//! aesthetic iteration touches exactly one file. The current look is
//! an explicit placeholder.
//!
//! Palettes are Rust-owned hex mirrors of the Mira Blue ramp rather
//! than `getComputedStyle` reads: the stylesheet's tokens are
//! `oklch()`/`color-mix()` expressions the vendored
//! `getShaderColorFromString` cannot parse, and resolve-at-mount
//! plumbing would freeze the first theme anyway. The two `--accent`
//! anchors are the only literal-hex blue tokens in fleet-ui.css and
//! are machine-pinned by `tests/atmosphere_palette_parity.rs`; the
//! surrounding mesh stops are hand-approximated from the `oklch`
//! bg/panel ramp (documented, not machine-pinned).

use crate::theme::Theme;

/// Catalog name resolved by the vendored wrapper — see
/// `vendor/src/paper-shaders.ts` for the full catalog.
pub const SHADER: &str = "meshGradient";

/// Light mesh stops: bg-white base (`--bg` `oklch(98.5%)` ≈ `#fafafa`),
/// two panel-derived blue washes, and the `--accent` / `--accent-soft`
/// literals (#2a5c8a / #7ea6cc).
pub const LIGHT_COLORS: [&str; 5] = ["#fafafa", "#e9eff6", "#c8d9ea", "#7ea6cc", "#2a5c8a"];

/// Dark mesh stops: near-black base (`--bg` `oklch(14.5%)` ≈ `#0a0a0a`),
/// two deep blue washes, and the dark `--accent` / `--accent-soft`
/// literals (#5a9fd4 / #8fb8dc).
pub const DARK_COLORS: [&str; 5] = ["#0a0a0a", "#12202e", "#1d3a57", "#5a9fd4", "#8fb8dc"];

/// Slow drift; the intended range is 0.1 to 0.2.
pub const SPEED: f64 = 0.15;

/// Organic noise distortion (0..=1).
pub const DISTORTION: f64 = 0.8;

/// Vortex distortion (0..=1) — kept subtle.
pub const SWIRL: f64 = 0.1;

/// Grain applied to shape edges (0..=1) — off for the placeholder look.
pub const GRAIN_MIXER: f64 = 0.0;

/// Post-processing black/white grain overlay (0..=1) — off.
pub const GRAIN_OVERLAY: f64 = 0.0;

/// The mesh stops for a theme, in wrapper-ready `#rrggbb` form.
#[must_use]
pub fn colors(theme: Theme) -> &'static [&'static str] {
    match theme {
        Theme::Light => &LIGHT_COLORS,
        Theme::Dark => &DARK_COLORS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_plain_hex(s: &str) -> bool {
        s.len() == 7
            && s.starts_with('#')
            && s[1..]
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }

    #[test]
    fn every_stop_is_lowercase_six_digit_hex() {
        // The wrapper feeds these to getShaderColorFromString, which
        // handles hex fast and oklch()/color-mix() not at all.
        for stop in LIGHT_COLORS.iter().chain(DARK_COLORS.iter()) {
            assert!(
                is_plain_hex(stop),
                "palette stop `{stop}` must be lowercase #rrggbb — the \
                 vendored color parser has no oklch()/color-mix() path"
            );
        }
    }

    #[test]
    fn stop_counts_fit_the_mesh_gradient_uniform() {
        // meshGradientMeta.maxColorCount is 10; below 3 the mesh
        // degenerates into a flat wash.
        for palette in [&LIGHT_COLORS[..], &DARK_COLORS[..]] {
            assert!(
                (3..=10).contains(&palette.len()),
                "mesh palettes must carry 3..=10 stops, got {}",
                palette.len()
            );
        }
    }

    #[test]
    fn themes_have_distinct_palettes() {
        assert_ne!(
            LIGHT_COLORS, DARK_COLORS,
            "the theme toggle must actually re-color the mesh"
        );
    }

    #[test]
    fn palette_values_are_pinned() {
        // The accent anchors (cross-checked against fleet-ui.css by
        // atmosphere_palette_parity) plus the hand-approximated ramp
        // stops. Deliberate re-tuning re-pins here.
        assert_eq!(
            LIGHT_COLORS,
            ["#fafafa", "#e9eff6", "#c8d9ea", "#7ea6cc", "#2a5c8a"]
        );
        assert_eq!(
            DARK_COLORS,
            ["#0a0a0a", "#12202e", "#1d3a57", "#5a9fd4", "#8fb8dc"]
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // pinning consts is the point
    fn knobs_stay_in_shader_range() {
        assert!(SPEED > 0.0 && SPEED <= 0.2, "slow drift, per the issue");
        for (name, knob) in [
            ("DISTORTION", DISTORTION),
            ("SWIRL", SWIRL),
            ("GRAIN_MIXER", GRAIN_MIXER),
            ("GRAIN_OVERLAY", GRAIN_OVERLAY),
        ] {
            assert!(
                (0.0..=1.0).contains(&knob),
                "{name} must stay in the shader's 0..=1 range, got {knob}"
            );
        }
    }

    #[test]
    fn theme_lookup_matches_the_arrays() {
        assert_eq!(colors(Theme::Light), &LIGHT_COLORS);
        assert_eq!(colors(Theme::Dark), &DARK_COLORS);
    }
}
