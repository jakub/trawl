// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub(crate) use crate::service_card_fmt;

/// Percent-encode one URL query-parameter value.
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
pub mod evidence_drawer;
pub mod exact_table;
pub mod export_modal;
pub mod facet_sidebar;
pub mod field_case_drawer;
pub mod histogram;
pub mod job_refresh;
pub mod live_cap_notice;
pub mod malformed_notice;
pub mod meta_strip;
pub mod net_drawer;
pub mod query_error_notice;
pub mod read_turn;
pub mod recovering_notice;
pub mod repin_modal;
pub mod resource_notice;
pub mod results_table;
pub mod save_as_net_modal;
pub mod search_quick_start;
pub mod send_to_investigation;
pub mod service_drawer;
pub mod sort_th;
pub mod status_bar;
