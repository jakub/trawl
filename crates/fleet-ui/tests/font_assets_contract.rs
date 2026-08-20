// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract tests for the self-hosted Geist assets (issue #119).

use sha2::{Digest as _, Sha256};

const FLEET_CSS: &str = include_str!("../styles/fleet-ui.css");
const WORKBENCH_HTML: &str = include_str!("../index.html");
const SPA_HTML: &str = include_str!("../../trawl-web-ui/index.html");
const SECURITY_HEADERS: &str = include_str!("../../trawl-web/src/middleware/security_headers.rs");
const DESIGN_CARDS: &str = include_str!("../../../xtask/src/design_cards.rs");
const GEIST: &[u8] = include_bytes!("../fonts/Geist-Variable.woff2");
const GEIST_MONO: &[u8] = include_bytes!("../fonts/GeistMono-Variable.woff2");
const FONT_README: &str = include_str!("../fonts/README.md");
const FONT_LICENSE: &str = include_str!("../fonts/OFL.txt");
const CHECKSUMS: &str = include_str!("../fonts/SHA256SUMS");

#[test]
fn stylesheet_declares_the_two_local_variable_faces() {
    assert_eq!(FLEET_CSS.matches("@font-face").count(), 2);
    for needle in [
        "font-family: \"Geist\"",
        "font-family: \"Geist Mono\"",
        "url(\"fonts/Geist-Variable.woff2\")",
        "url(\"fonts/GeistMono-Variable.woff2\")",
    ] {
        assert!(
            FLEET_CSS.contains(needle),
            "missing font contract: {needle}"
        );
    }
    assert_eq!(FLEET_CSS.matches("font-weight: 400 700").count(), 2);
    assert_eq!(FLEET_CSS.matches("font-display: swap").count(), 2);
}

#[test]
fn both_trunk_distributions_copy_the_fleet_owned_font_directory() {
    assert!(WORKBENCH_HTML.contains(r#"rel="copy-dir" href="fonts" data-target-path="fonts""#));
    assert!(
        SPA_HTML.contains(r#"rel="copy-dir" href="../fleet-ui/fonts" data-target-path="fonts""#)
    );
    assert!(DESIGN_CARDS.contains("data:font/woff2;base64,"));
    assert!(DESIGN_CARDS.contains("GeistMono-Variable.woff2"));
    assert!(DESIGN_CARDS.contains("geist-font-license"));
    assert!(DESIGN_CARDS.contains("fonts_dir.join(\"OFL.txt\")"));
}

#[test]
fn runtime_surfaces_have_no_google_font_dependency_or_allowance() {
    let google_css = ["fonts", ".googleapis.com"].concat();
    let google_woff = ["fonts", ".gstatic.com"].concat();
    for (name, source) in [
        ("workbench", WORKBENCH_HTML),
        ("SPA", SPA_HTML),
        ("security headers", SECURITY_HEADERS),
        ("design cards", DESIGN_CARDS),
    ] {
        assert!(
            !source.contains(&google_css),
            "{name} still references {google_css}"
        );
        assert!(
            !source.contains(&google_woff),
            "{name} still references {google_woff}"
        );
    }
}

#[test]
fn committed_assets_carry_source_checksum_and_ofl_evidence() {
    assert!(GEIST.starts_with(b"wOF2"));
    assert!(GEIST_MONO.starts_with(b"wOF2"));
    assert!(GEIST.len() > 60_000 && GEIST_MONO.len() > 60_000);
    assert!(FONT_README.contains("Release tag: `1.8.0`"));
    assert!(FONT_README.contains("91158e012bdc4abd59fa066d0eae9fc11c2c9f24"));
    assert!(FONT_LICENSE.contains("SIL OPEN FONT LICENSE Version 1.1"));
    for (name, bytes, digest) in [
        (
            "Geist-Variable.woff2",
            GEIST,
            "c2a45610c45081b940562b38437b3128ee287c564ae759a758dbad8a90a32349",
        ),
        (
            "GeistMono-Variable.woff2",
            GEIST_MONO,
            "4e772ac3c650b07abd180140088b0a7c84ec016cc6385cb7dfc21beb9e258815",
        ),
        (
            "OFL.txt",
            FONT_LICENSE.as_bytes(),
            "f12f59163d92084b0473315ae4566cde0c1233d940ae464f5225a69c347cc691",
        ),
    ] {
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), digest);
        assert!(
            CHECKSUMS.contains(&format!("{digest}  {name}")),
            "missing checksum for {name}"
        );
    }
}
