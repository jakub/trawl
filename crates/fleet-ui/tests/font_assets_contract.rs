// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract tests for the self-hosted Albert Sans / Chivo Mono assets.

use sha2::{Digest as _, Sha256};

const FLEET_CSS: &str = include_str!("../styles/fleet-ui.css");
const WORKBENCH_HTML: &str = include_str!("../index.html");
const SPA_HTML: &str = include_str!("../../trawl-web-ui/index.html");
const SECURITY_HEADERS: &str = include_str!("../../trawl-web/src/middleware/security_headers.rs");
const DESIGN_CARDS: &str = include_str!("../../../xtask/src/design_cards.rs");
const ALBERT_SANS: &[u8] = include_bytes!("../fonts/AlbertSans-Variable.ttf");
const CHIVO_MONO: &[u8] = include_bytes!("../fonts/ChivoMono-Variable.ttf");
const FONT_README: &str = include_str!("../fonts/README.md");
const ALBERT_SANS_LICENSE: &str = include_str!("../fonts/OFL-AlbertSans.txt");
const CHIVO_MONO_LICENSE: &str = include_str!("../fonts/OFL-ChivoMono.txt");
const CHECKSUMS: &str = include_str!("../fonts/SHA256SUMS");

/// TrueType's `sfnt` version tag for glyf-outline fonts.
const TRUETYPE_MAGIC: &[u8] = &[0x00, 0x01, 0x00, 0x00];

#[test]
fn stylesheet_declares_the_two_local_variable_faces() {
    assert_eq!(FLEET_CSS.matches("@font-face").count(), 2);
    for needle in [
        "font-family: \"Albert Sans\"",
        "font-family: \"Chivo Mono\"",
        "url(\"fonts/AlbertSans-Variable.ttf\") format(\"truetype\")",
        "url(\"fonts/ChivoMono-Variable.ttf\") format(\"truetype\")",
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
fn the_body_rule_slashes_the_monospace_zero() {
    // Chivo Mono's default zero is an unslashed oval; a log viewer has to
    // keep 0 and O apart, and the feature inherits from one declaration.
    let body = FLEET_CSS
        .split("\nbody {")
        .nth(1)
        .expect("fleet-ui.css declares a `body` rule")
        .split('}')
        .next()
        .expect("the `body` rule closes");
    assert!(
        body.contains("font-feature-settings: \"zero\";"),
        "body rule is missing the slashed-zero feature: {body}"
    );
}

#[test]
fn both_trunk_distributions_copy_the_fleet_owned_font_directory() {
    assert!(WORKBENCH_HTML.contains(r#"rel="copy-dir" href="fonts" data-target-path="fonts""#));
    assert!(
        SPA_HTML.contains(r#"rel="copy-dir" href="../fleet-ui/fonts" data-target-path="fonts""#)
    );
    assert!(DESIGN_CARDS.contains("data:font/ttf;base64,"));
    assert!(DESIGN_CARDS.contains("AlbertSans-Variable.ttf"));
    assert!(DESIGN_CARDS.contains("ChivoMono-Variable.ttf"));
    assert!(DESIGN_CARDS.contains("\"OFL-AlbertSans.txt\""));
    assert!(DESIGN_CARDS.contains("\"OFL-ChivoMono.txt\""));
    assert!(DESIGN_CARDS.contains("id=\"font-licenses\""));
    assert!(
        !DESIGN_CARDS.contains("Geist"),
        "design cards still name the retired typefaces"
    );
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
    assert!(ALBERT_SANS.starts_with(TRUETYPE_MAGIC));
    assert!(CHIVO_MONO.starts_with(TRUETYPE_MAGIC));
    assert!(ALBERT_SANS.len() > 100_000 && CHIVO_MONO.len() > 100_000);

    // The google/fonts pins: the packaging commit per family.
    assert!(FONT_README.contains("6b612533e5b14370fea6f524095f4d01cdfee18b"));
    assert!(FONT_README.contains("5b62bc464227fcadef4d4acebd73153598d3e05e"));
    assert!(FONT_README.contains("https://github.com/usted/Albert-Sans"));
    assert!(FONT_README.contains("https://github.com/Omnibus-Type/Chivo"));

    for (family, license, copyright) in [
        (
            "Albert Sans",
            ALBERT_SANS_LICENSE,
            "Copyright 2021 The Albert Sans Project Authors",
        ),
        (
            "Chivo Mono",
            CHIVO_MONO_LICENSE,
            "Copyright 2019 The Chivo Project Authors",
        ),
    ] {
        assert!(
            license.contains("SIL OPEN FONT LICENSE Version 1.1"),
            "{family} license text is not OFL 1.1"
        );
        assert!(
            license.contains(copyright),
            "{family} license is missing its copyright line"
        );
    }

    for (name, bytes, digest) in [
        (
            "AlbertSans-Variable.ttf",
            ALBERT_SANS,
            "8fe5d4cf5822d7096d4d17ad781c90f97c745ac13a22be619db74966fba45fda",
        ),
        (
            "ChivoMono-Variable.ttf",
            CHIVO_MONO,
            "725256f30b7b1b25dd001a96ff8d4a23773197bb886cd847a97ff8eabc9c1d9d",
        ),
        (
            "OFL-AlbertSans.txt",
            ALBERT_SANS_LICENSE.as_bytes(),
            "5c856c086e8743b84932aae46ced424729a703932b601e9eb8aaeac15a617ec6",
        ),
        (
            "OFL-ChivoMono.txt",
            CHIVO_MONO_LICENSE.as_bytes(),
            "c9b69fa18c372df2b187b49efc57b1ea643b86a938e5af32f6b5a7af1017c891",
        ),
    ] {
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), digest);
        assert!(
            CHECKSUMS.contains(&format!("{digest}  {name}")),
            "missing checksum for {name}"
        );
    }
}
