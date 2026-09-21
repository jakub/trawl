// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web-ui`: Leptos SPA entry point.

// Pure modules that don't touch Leptos/web-sys — keep them ungated so
// their tests run under plain `cargo test` on native.
mod auth_return;
mod categorical;
mod completion;
mod context_query;
mod dashboard_state;
mod drawer_query;
mod facets;
mod fetch_plan;
mod filter_codec;
mod histogram;
pub mod history_export;
mod notice_key;
mod offset;
mod perms;
mod query_merge;
mod repin_flow;
mod repin_hint;
mod result_actions;
mod results_layout;
mod schedule_edit;
mod schema_nav;
mod search_status;
mod search_url;
mod service_card_fmt;
mod severity_cell;
mod sort_label;
mod tone_vocab;

#[cfg(target_arch = "wasm32")]
mod api;

#[cfg(target_arch = "wasm32")]
mod download;

#[cfg(target_arch = "wasm32")]
mod app;

#[cfg(target_arch = "wasm32")]
mod components;

#[cfg(target_arch = "wasm32")]
mod interop;

#[cfg(target_arch = "wasm32")]
mod pages;

// `state` is ungated: its `app_mode` + `section` submodules carry pure
// `&'static` data whose contracts are exercised by native `cargo test`;
// the leptos-backed submodules inside it stay wasm32-gated.
mod state;

#[cfg(target_arch = "wasm32")]
fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!(
        "trawl-web-ui is a wasm-only crate — build with `trunk build` or \
         `cargo check -p trawl-web-ui --target wasm32-unknown-unknown`"
    );
}
