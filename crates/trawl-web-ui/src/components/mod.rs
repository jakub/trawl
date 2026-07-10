// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
        "--amber" | "--yellow" => fleet_ui::Tone::Warn,
        "--blue" | "--teal" => fleet_ui::Tone::Info,
        _ => fleet_ui::Tone::Neutral,
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max.min(s.len());
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\u{2026}", &s[..end])
    }
}

pub mod chart;
pub mod editor;
pub mod editor_wrap;
pub mod export_modal;
pub mod facet_sidebar;
pub mod histogram;
pub mod lineage_tree;
pub mod linkage_graph;
pub mod meta_strip;
pub mod net_drawer;
pub mod results_table;
pub mod save_as_net_modal;
pub mod service_card;
pub mod service_card_fmt;
pub mod service_drawer;
pub mod status_bar;
