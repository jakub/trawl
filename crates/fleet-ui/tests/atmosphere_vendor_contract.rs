// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract tests over the COMMITTED vendor artifacts
//! (jakub/coastwatch#308, ADR-0012).
//!
//! wasm-bindgen inlines the vendored `paper-shaders.js` at compile
//! time, so a missing or renamed file is already a build error. What
//! it cannot see is the bundle's CONTENTS: a malformed module shape, a
//! renamed export, or a stale attribution banner produces a backdrop
//! that silently never appears, with no compile-time signal on any
//! target. `include_str!` over the committed artifacts turns those
//! failure modes into `cargo nextest` reds. The CI vendor-drift job
//! guarantees the committed bundle matches `build.sh` output, so
//! asserting on the committed bytes IS asserting on the build.

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

const INTEROP: &str = include_str!("../src/atmosphere/interop.rs");
const WRAPPER_TS: &str = include_str!("../vendor/src/paper-shaders.ts");

#[test]
fn bundle_is_a_self_contained_esm_module() {
    // wasm-bindgen `module = "…"` loads the file as ONE browser ES
    // module with no resolver: a relative import or a CommonJS
    // `require()` escaping minification means the bundle silently fails
    // to load at runtime — the backdrop just never appears.
    let body = &BUNDLE[BUNDLE.find("*/").map_or(0, |i| i + 2)..];
    for needle in ["from\"./", "from\"../", "from \"./", "from \"../"] {
        assert!(
            !body.contains(needle),
            "bundle contains a relative import (`{needle}`) — it must \
             be a single self-contained file (esbuild --bundle)"
        );
    }
    assert!(
        !body.contains("require("),
        "bundle contains a CommonJS require() — browsers cannot resolve \
         it inside an ES module (esbuild --format=esm regressed)"
    );
}

#[test]
fn bundle_exports_the_wrapper_api() {
    // The two names wasm-bindgen binds (createShader) and the catalog
    // the vendor-side name resolution reads. esbuild keeps export
    // names verbatim in the `export{… as name}` clause.
    for export in ["createShader", "shaderCatalog"] {
        assert!(
            BUNDLE.contains(export),
            "bundle must export `{export}` — wasm-bindgen resolves it \
             by name at module load; a rename is a silent runtime break"
        );
    }
}

#[test]
fn interop_path_agrees_with_the_vendored_filename() {
    // The wasm-bindgen module path is a compile-time snippet path
    // resolved against the crate root, so it must name the committed
    // file (whose existence include_str! proves). The compiler enforces
    // this on wasm32 only; this test carries it onto native builds.
    assert!(
        INTEROP.contains(r#"module = "/vendor/paper-shaders.js""#),
        "interop.rs must bind module = \"/vendor/paper-shaders.js\" — \
         the crate-root-relative path of the committed bundle"
    );
}

#[test]
fn palette_shader_is_a_catalog_key() {
    // createShader falls through to treating an unknown name as RAW
    // GLSL source — a typo'd catalog name compiles as a broken shader
    // and renders nothing, silently. Pin the name against the
    // unminified wrapper source's catalog keys.
    let key = format!("\n  {}: ", fleet_ui::atmosphere::palette::SHADER);
    assert!(
        WRAPPER_TS.contains(&key),
        "palette::SHADER `{}` is not a shaderCatalog key in \
         vendor/src/paper-shaders.ts — the wrapper would fall through \
         to compiling the NAME as raw GLSL and render nothing",
        fleet_ui::atmosphere::palette::SHADER
    );
}
