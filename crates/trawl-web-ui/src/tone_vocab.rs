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
/// fleet-ui status-dot tone. App-side because the status vocabulary is
/// trawl's; only the dot is generic.
pub(crate) fn run_status_tone(status: &str) -> fleet_ui::StatusTone {
    match status {
        "success" => fleet_ui::StatusTone::Success,
        // `timeout` is a failed execution, not a soft warning, so it shares
        // the red `Error` tone. Do not split it onto its own yellow tone.
        "error" | "timeout" => fleet_ui::StatusTone::Error,
        "running" => fleet_ui::StatusTone::Running,
        _ => fleet_ui::StatusTone::Neutral,
    }
}

#[cfg(test)]
mod tests {
    use super::run_status_tone;

    #[test]
    fn run_status_tone_maps_every_arm() {
        assert_eq!(run_status_tone("success"), fleet_ui::StatusTone::Success);
        assert_eq!(run_status_tone("error"), fleet_ui::StatusTone::Error);
        // Load-bearing: `timeout` is a failed execution and must share the
        // red `Error` tone rather than a soft-warning one. A future edit
        // moving it elsewhere has to turn this assertion red.
        assert_eq!(run_status_tone("timeout"), fleet_ui::StatusTone::Error);
        assert_eq!(run_status_tone("running"), fleet_ui::StatusTone::Running);
    }

    #[test]
    fn run_status_tone_unknown_falls_back_to_neutral() {
        assert_eq!(run_status_tone("queued"), fleet_ui::StatusTone::Neutral);
        assert_eq!(run_status_tone(""), fleet_ui::StatusTone::Neutral);
    }
}
