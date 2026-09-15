// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The pure run-status tone mapper lives in the ungated
// [`crate::tone_vocab`] module so native `cargo test` exercises it;
// re-exported here so call sites keep their existing `components::…` path.
pub(crate) use crate::tone_vocab::run_status_tone;

// Same arrangement for the pure service formatters: they live ungated at
// [`crate::service_card_fmt`] so their tests run natively, and are
// re-exported here so `components::service_card_fmt::…` still resolves.
pub(crate) use crate::service_card_fmt;

/// Percent-encode one URL query-parameter value. Shared by the Schema
/// page's own drill-in links and the query notice's: a catalog field
/// name is any ASCII-folded client JSON key, so the two must encode it
/// the same way or a deep link would miss the field it names.
pub(crate) fn enc_uri(raw: &str) -> String {
    js_sys::encode_uri_component(raw)
        .as_string()
        .unwrap_or_else(|| raw.to_string())
}

pub mod cat_chart;
pub mod chart;
pub mod degraded_notice;
pub mod editor;
pub mod editor_wrap;
pub mod exact_table;
pub mod export_modal;
pub mod facet_sidebar;
pub mod field_case_drawer;
pub mod histogram;
pub mod job_refresh;
pub mod malformed_notice;
pub mod meta_strip;
pub mod net_drawer;
pub mod repin_modal;
pub mod results_table;
pub mod save_as_net_modal;
pub mod service_drawer;
pub mod sort_th;
pub mod status_bar;
