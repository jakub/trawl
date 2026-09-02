// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native route contract: the `/intel` root has no registered route and
//! unmatched routes render the app's 404 component.
//!
//! The Leptos router only compiles on wasm32, so this reads `app.rs` as
//! source text, following the native UI contract-test pattern.

const APP: &str = include_str!("../src/app.rs");

#[test]
fn retired_surface_falls_through_to_the_not_found_view() {
    let retired_root = "/intel";
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
