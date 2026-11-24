// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract tests over the committed vendor artifacts
//! (ADR-0012).

const BUNDLE: &str = include_str!("../vendor/paper-shaders.js");
const PACKAGE_JSON: &str = include_str!("../vendor/package.json");

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
fn atmosphere_vendor_contract_dispose_removes_the_parent_style_marker() {
    // The upstream JS property and the DOM attribute are separate markers.
    // Removing only paperShaderMount leaves the shared stylesheet active.
    let dispose = WRAPPER_TS
        .split_once("    dispose(): void {")
        .expect("the wrapper must expose dispose")
        .1;
    assert!(
        dispose.contains(r#"parent.removeAttribute("data-paper-shader")"#),
        "wrapper disposal must remove data-paper-shader from its parent"
    );
}

#[test]
fn bundle_is_a_self_contained_esm_module() {
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

const ATMOSPHERE_MOD: &str = include_str!("../src/atmosphere/mod.rs");
const CARGO_TOML: &str = include_str!("../Cargo.toml");

#[test]
fn the_snippet_binding_stays_behind_the_default_off_atmosphere_feature() {
    assert!(
        ATMOSPHERE_MOD.contains(
            "#[cfg(all(target_arch = \"wasm32\", feature = \"atmosphere\"))]\npub mod interop;"
        ),
        "atmosphere/mod.rs must gate `pub mod interop` on \
         `all(target_arch = \"wasm32\", feature = \"atmosphere\")` — \
         linking the extern block is what ships the 142 KB bundle to \
         every consumer's dist, mounted or not"
    );
    assert!(
        CARGO_TOML.contains("[features]\natmosphere = []"),
        "fleet-ui must declare `atmosphere` as a feature with no \
         dependants of its own"
    );
    assert!(
        !CARGO_TOML.contains("default ="),
        "the `atmosphere` feature must stay OUT of fleet-ui's default \
         feature set — defaulting it on puts the shader bundle back in \
         every consumer's dist, which is the regression the gate exists \
         to prevent"
    );
}

#[test]
fn interop_path_agrees_with_the_vendored_filename() {
    // The compiler enforces this on wasm32 only; this test carries it onto
    // native builds.
    assert!(
        INTEROP.contains(r#"module = "/vendor/paper-shaders.js""#),
        "interop.rs must bind module = \"/vendor/paper-shaders.js\" — \
         the crate-root-relative path of the committed bundle"
    );
}

#[test]
fn palette_shader_is_a_catalog_key() {
    let key = format!("\n  {}: ", fleet_ui::atmosphere::palette::SHADER);
    assert!(
        WRAPPER_TS.contains(&key),
        "palette::SHADER `{}` is not a shaderCatalog key in \
         vendor/src/paper-shaders.ts — the wrapper would fall through \
         to compiling the NAME as raw GLSL and render nothing",
        fleet_ui::atmosphere::palette::SHADER
    );
}
