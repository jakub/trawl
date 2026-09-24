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

/// Human-readable execution outcome. Unknown server states stay visible verbatim.
pub(crate) fn run_status_label(status: &str) -> &str {
    match status {
        "running" => "Running",
        "success" => "Succeeded",
        "error" => "Failed",
        "timeout" => "Timed out",
        _ => status,
    }
}

/// The marker a run's history row carries for how the run started, or
/// `None` for no marker. Only a manual run is marked: a scheduled run is
/// the ordinary case, and a run recorded before origins were has none to
/// show. Unknown server origins stay visible verbatim.
pub(crate) fn run_origin_label(origin: Option<&str>) -> Option<&str> {
    match origin? {
        "manual" => Some("Manual"),
        "scheduled" => None,
        other => Some(other),
    }
}

/// Badge counterpart of the shared execution status-dot vocabulary.
pub(crate) fn run_badge_tone(status: &str) -> fleet_ui::Tone {
    match run_status_tone(status) {
        fleet_ui::StatusTone::Success => fleet_ui::Tone::Success,
        fleet_ui::StatusTone::Error => fleet_ui::Tone::Danger,
        fleet_ui::StatusTone::Running => fleet_ui::Tone::Info,
        fleet_ui::StatusTone::Neutral => fleet_ui::Tone::Neutral,
    }
}

/// Repin refusal and blocking are distinct from execution failure.
/// Callers sanitize the returned text before displaying unknown wire values.
pub(crate) fn repin_status_label(status: &str) -> &str {
    match status {
        "running" => "Running",
        "succeeded" => "Succeeded",
        "refused_needs_force" => "Refused: force required",
        "failed" => "Failed",
        "blocked" => "Blocked",
        _ => status,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        repin_status_label, run_badge_tone, run_origin_label, run_status_label, run_status_tone,
    };

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

    #[test]
    fn execution_labels_and_badge_tones_preserve_outcomes() {
        for (wire, label, tone) in [
            ("running", "Running", fleet_ui::Tone::Info),
            ("success", "Succeeded", fleet_ui::Tone::Success),
            ("error", "Failed", fleet_ui::Tone::Danger),
            ("timeout", "Timed out", fleet_ui::Tone::Danger),
            ("queued", "queued", fleet_ui::Tone::Neutral),
            ("échec", "échec", fleet_ui::Tone::Neutral),
        ] {
            assert_eq!(run_status_label(wire), label);
            assert_eq!(run_badge_tone(wire), tone);
        }
    }

    #[test]
    fn only_a_manual_run_is_marked() {
        assert_eq!(run_origin_label(Some("manual")), Some("Manual"));
        assert_eq!(run_origin_label(Some("scheduled")), None);
        assert_eq!(run_origin_label(None), None);
        assert_eq!(run_origin_label(Some("replayed")), Some("replayed"));
    }

    #[test]
    fn repin_labels_preserve_refusal_and_unknown_states() {
        for (wire, label) in [
            ("running", "Running"),
            ("succeeded", "Succeeded"),
            ("refused_needs_force", "Refused: force required"),
            ("failed", "Failed"),
            ("blocked", "Blocked"),
            ("future_status", "future_status"),
        ] {
            assert_eq!(repin_status_label(wire), label);
        }
    }
}
