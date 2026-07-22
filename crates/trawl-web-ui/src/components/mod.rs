// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The pure tone-vocabulary mappers (`run_status_tone`, `tone_for_var`)
// live in the ungated [`crate::tone_vocab`] module so native `cargo test`
// exercises them; re-exported here so call sites keep their existing
// `components::…` / `super::…` paths.
pub(crate) use crate::tone_vocab::{run_status_tone, tone_for_var};

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
pub mod service_card_fmt;
pub mod service_drawer;
pub mod sort_th;
pub mod status_bar;
