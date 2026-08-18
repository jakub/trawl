// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native route contract for the removed cross-app surface.
//!
//! The Leptos router only compiles on wasm32. This source-level contract
//! follows the existing native UI contract-test pattern: it proves the
//! retired root has no registered route and that unmatched routes render
//! the application's 404 component.

const APP: &str = include_str!("../src/app.rs");

#[test]
fn retired_surface_falls_through_to_the_not_found_view() {
    let retired_root = concat!("/", "in", "tel");
    let registered_pattern = format!("path!(\"{retired_root}");

    assert!(
        !APP.contains(&registered_pattern),
        "the retired SPA root must have no registered route"
    );
    assert!(
        APP.contains("<Routes fallback=|| view! { <NotFound/> }>"),
        "unmatched SPA routes must render the 404 view"
    );
}
