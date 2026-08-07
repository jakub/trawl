// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract tests over the COMMITTED vendor artifacts
//! (jakub/coastwatch#308, ADR-0012).
//!
//! The vendored `paper-shaders.js` bundle is consumed by wasm-bindgen
//! at RUNTIME — a malformed bundle, a missing export, or a broken
//! interop path agreement produces a backdrop that silently never
//! appears, with no compile-time signal on any target. `include_str!`
//! over the committed artifacts turns those failure modes into
//! `cargo nextest` reds. The CI vendor-drift job guarantees the
//! committed bundle matches `build.sh` output, so asserting on the
//! committed bytes IS asserting on the build.

const BUNDLE: &str = include_str!("../vendor/paper-shaders.js");
const PACKAGE_JSON: &str = include_str!("../vendor/package.json");

/// The `@paper-design/shaders` version pinned in `vendor/package.json`.
fn pinned_version() -> String {
    let key = "\"@paper-design/shaders\":";
    let start = PACKAGE_JSON
        .find(key)
        .expect("vendor/package.json must pin @paper-design/shaders")
        + key.len();
    let rest = &PACKAGE_JSON[start..];
    let open = rest.find('"').expect("version string opens") + 1;
    let close = open + rest[open..].find('"').expect("version string closes");
    let version = &rest[open..close];
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "the @paper-design/shaders pin must be exact (no ^/~ range): \
         the committed bundle and the Apache-2.0 banner both cite it \
         verbatim, got `{version}`"
    );
    version.to_string()
}

#[test]
fn bundle_carries_the_apache_attribution_banner() {
    // The upstream package is Apache-2.0: redistribution must carry
    // attribution IN the artifact actually served to browsers. Only an
    // esbuild `--banner:js` comment (the `/*!` form minifiers keep)
    // survives minification — LICENSE/NOTICE next to the bundle never
    // leave the repo checkout.
    let banner_end = BUNDLE.find("*/").map_or(0, |i| i + 2);
    let banner = &BUNDLE[..banner_end];
    assert!(
        banner.starts_with("/*!"),
        "vendor/paper-shaders.js must open with a `/*!` attribution \
         banner (esbuild --banner:js in vendor/build.sh) — the bundle \
         is the only vendored byte a browser ever receives"
    );
    for needle in ["@paper-design/shaders", "Apache-2.0", "NOTICE"] {
        assert!(
            banner.contains(needle),
            "attribution banner must cite `{needle}`; got: {banner}"
        );
    }
}

#[test]
fn banner_version_matches_the_package_json_pin() {
    let version = pinned_version();
    let banner_end = BUNDLE.find("*/").map_or(0, |i| i + 2);
    let banner = &BUNDLE[..banner_end];
    assert!(
        banner.contains(&format!("v{version}")),
        "banner must cite the pinned upstream version v{version} — a \
         mismatch means build.sh's banner drifted from the \
         package.json pin; got: {banner}"
    );
}
