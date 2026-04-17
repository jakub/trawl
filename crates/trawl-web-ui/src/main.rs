// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web-ui`: Leptos SPA entry point.

// offset helpers are pure str manipulation — keep them available on both
// targets so their tests run under plain `cargo test`.
mod offset;

#[cfg(target_arch = "wasm32")]
mod api;

#[cfg(target_arch = "wasm32")]
mod app;

#[cfg(target_arch = "wasm32")]
mod components;

#[cfg(target_arch = "wasm32")]
mod interop;

#[cfg(target_arch = "wasm32")]
mod pages;

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
