// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! App-side mapping of trawl's domain vocabularies onto fleet-ui's
//! generic tone enums. Pure (no `leptos`/`web_sys`), so it lives ungated
//! and its contract is exercised by native `cargo test` — mirroring the
//! sibling helper module (`facets`) and fleet-ui's own
//! `status_dot::tone`. Keeping the string→tone mapping app-side is
//! deliberate (ADR-0002): the status/color vocabularies are trawl's, only
//! the rendered dot/badge is generic.
//!
//! Only the wasm32 build consumes these mappers — on native they exist
//! purely so their tests run under plain `cargo test`. Matches the
//! `facets.rs` / `offset.rs` pattern.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// Map a saved-query run status string (trawl-api vocabulary) onto the
/// fleet-ui status-dot tone. App-side on purpose (ADR-0002): the
/// status vocabulary is trawl's, only the dot is generic.
pub(crate) fn run_status_tone(status: &str) -> fleet_ui::StatusTone {
    match status {
        "success" => fleet_ui::StatusTone::Success,
        // `timeout` is a failed execution, not a soft warning — it shares the
        // red `Error` tone, exactly as the pre-unification `"error" | "timeout"`
        // match arms rendered it. Do not split it onto its own yellow tone.
        "error" | "timeout" => fleet_ui::StatusTone::Error,
        "running" => fleet_ui::StatusTone::Running,
        _ => fleet_ui::StatusTone::Neutral,
    }
}

/// Map the intel color-var vocabulary (`--green`, `--red`, …) that the
/// badge helper fns share with non-badge accents (timeline dots, text
/// colors) onto the closed fleet-ui badge [`fleet_ui::Tone`] set. Known
/// visible narrowing (sanctioned by ADR-0003, flagged in the PR): teal →
/// Info, ink-2/ink-3/ink-4 → Neutral.
pub(crate) fn tone_for_var(var: &str) -> fleet_ui::Tone {
    match var {
        "--green" => fleet_ui::Tone::Success,
        "--red" => fleet_ui::Tone::Danger,
        "--yellow" => fleet_ui::Tone::Warn,
        "--blue" | "--teal" => fleet_ui::Tone::Info,
        _ => fleet_ui::Tone::Neutral,
    }
}

#[cfg(test)]
mod tests {
    use super::{run_status_tone, tone_for_var};

    #[test]
    fn run_status_tone_maps_every_arm() {
        assert_eq!(run_status_tone("success"), fleet_ui::StatusTone::Success);
        assert_eq!(run_status_tone("error"), fleet_ui::StatusTone::Error);
        // Load-bearing: `timeout` is a failed execution and must share the
        // red `Error` tone, NOT split onto a soft-warning tone. A future
        // edit moving it elsewhere has to turn this assertion red.
        assert_eq!(run_status_tone("timeout"), fleet_ui::StatusTone::Error);
        assert_eq!(run_status_tone("running"), fleet_ui::StatusTone::Running);
    }

    #[test]
    fn run_status_tone_unknown_falls_back_to_neutral() {
        assert_eq!(run_status_tone("queued"), fleet_ui::StatusTone::Neutral);
        assert_eq!(run_status_tone(""), fleet_ui::StatusTone::Neutral);
    }

    #[test]
    fn tone_for_var_maps_known_vars() {
        assert_eq!(tone_for_var("--green"), fleet_ui::Tone::Success);
        assert_eq!(tone_for_var("--red"), fleet_ui::Tone::Danger);
        assert_eq!(tone_for_var("--yellow"), fleet_ui::Tone::Warn);
        assert_eq!(tone_for_var("--blue"), fleet_ui::Tone::Info);
        // Sanctioned narrowing (ADR-0003): teal collapses onto Info.
        assert_eq!(tone_for_var("--teal"), fleet_ui::Tone::Info);
    }

    #[test]
    fn tone_for_var_unknown_falls_back_to_neutral() {
        // Sanctioned narrowing (ADR-0003): ink-* accents → Neutral.
        assert_eq!(tone_for_var("--ink-2"), fleet_ui::Tone::Neutral);
        assert_eq!(tone_for_var("--whatever"), fleet_ui::Tone::Neutral);
    }
}
