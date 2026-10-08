// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The shipped Debian Vector configs and trawld's service rule (ADR-0053).
//!
//! VRL has no functions, so every transform that derives `service` from
//! event data carries its own copy of the gate regex and falls back to a
//! marked name when no candidate passes. These tests keep every copy equal
//! to one pinned literal, keep that literal equal to
//! `trawl_config::is_valid_service_name`, and notice a transform that
//! derives `service` without a gate. The behaviour of the configs runs
//! under the real Vector binary in `scripts/test-vector-collector.py`.

use std::collections::BTreeSet;

use trawl_config::is_valid_service_name;

/// The gate, as it appears in every deriving transform. VRL `match` and
/// this test use the same `regex` crate.
const GATE: &str = r"^[A-Za-z0-9_-][A-Za-z0-9._-]{0,127}$";

const SHIPPED: [(&str, &str); 9] = [
    (
        "apache.toml",
        include_str!("../../../config/vector/debian/apache.toml"),
    ),
    (
        "base.toml",
        include_str!("../../../config/vector/debian/base.toml"),
    ),
    (
        "docker.toml",
        include_str!("../../../config/vector/debian/docker.toml"),
    ),
    (
        "fail2ban.toml",
        include_str!("../../../config/vector/debian/fail2ban.toml"),
    ),
    (
        "mysql.toml",
        include_str!("../../../config/vector/debian/mysql.toml"),
    ),
    (
        "nginx.toml",
        include_str!("../../../config/vector/debian/nginx.toml"),
    ),
    (
        "postgresql.toml",
        include_str!("../../../config/vector/debian/postgresql.toml"),
    ),
    (
        "redis.toml",
        include_str!("../../../config/vector/debian/redis.toml"),
    ),
    (
        "unifi-syslog.toml",
        include_str!("../../../config/vector/debian/unifi-syslog.toml"),
    ),
];

/// The VRL source of every remap transform: `(file, transform, source)`.
fn remap_sources() -> Vec<(&'static str, String, String)> {
    let mut sources = Vec::new();
    for (file, text) in SHIPPED {
        let parsed: toml::Table = toml::from_str(text).unwrap_or_else(|e| panic!("{file}: {e}"));
        let Some(transforms) = parsed.get("transforms").and_then(|t| t.as_table()) else {
            continue;
        };
        for (name, transform) in transforms {
            if transform.get("type").and_then(|t| t.as_str()) != Some("remap") {
                continue;
            }
            let source = transform
                .get("source")
                .and_then(|s| s.as_str())
                .unwrap_or_else(|| panic!("{file}: transform {name} has no source"));
            sources.push((file, name.clone(), source.to_owned()));
        }
    }
    sources
}

/// The contents of every `r'...'` regex literal in `source`.
fn regex_literals(source: &str) -> Vec<String> {
    let literal = regex::Regex::new(r"r'([^']*)'").expect("literal pattern");
    literal
        .captures_iter(source)
        .map(|c| c[1].to_owned())
        .collect()
}

#[test]
fn every_service_deriving_transform_ends_in_the_one_gate() {
    let mut deriving = BTreeSet::new();
    let mut remaps = 0;
    for (file, name, source) in remap_sources() {
        remaps += 1;
        let at = format!("{file}: transform {name}");
        let mut derives = false;
        for line in source.lines().map(str::trim) {
            if let Some(rhs) = line.strip_prefix(".service =") {
                let rhs = rhs.trim();
                if let Some(literal) = rhs.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
                    assert!(
                        is_valid_service_name(literal),
                        "{at}: literal service {literal:?} is a name trawld refuses"
                    );
                    assert_ne!(
                        literal, "unknown",
                        "{at}: `unknown` is a host fallback, never a service"
                    );
                } else {
                    assert_eq!(
                        rhs, "service",
                        "{at}: `.service` is assigned something other than a string \
                         literal or the gated `service` variable: {line}"
                    );
                    derives = true;
                }
            }
            if line.contains("\"unknown\"") {
                assert!(
                    line.starts_with(".host ="),
                    "{at}: `\"unknown\"` may only fall back a host: {line}"
                );
            }
        }

        let gates: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("gate = r'"))
            .collect();
        if derives {
            assert_eq!(
                gates.len(),
                1,
                "{at}: a transform deriving service holds exactly one gate: {gates:?}"
            );
            assert_eq!(
                gates[0],
                format!("gate = r'{GATE}'"),
                "{at}: the gate differs from trawld's service rule"
            );
            deriving.insert(name);
        } else {
            assert!(gates.is_empty(), "{at}: a gate without a derived service");
        }

        for literal in regex_literals(&source) {
            if literal.contains("{0,127}") {
                assert_eq!(literal, GATE, "{at}: a stray copy of the gate differs");
            }
        }
    }
    assert!(remaps > 0, "no remap transform was parsed");

    let expected: BTreeSet<String> = [
        "journal_enriched",
        "trawl_docker",
        "trawl_unifi",
        "trawl_varlog",
    ]
    .map(String::from)
    .into();
    assert_eq!(
        deriving, expected,
        "the transforms deriving service from event data changed; give each its gate \
         and fallback, then list it here"
    );
}

#[test]
fn the_gate_agrees_with_is_valid_service_name() {
    let bytes = regex::bytes::Regex::new(GATE).expect("gate compiles");
    for b in 0..=u8::MAX {
        let gate = bytes.is_match(&[b]);
        let rule = std::str::from_utf8(&[b]).is_ok_and(is_valid_service_name);
        assert_eq!(gate, rule, "byte {b:#04x}");
    }

    let text = regex::Regex::new(GATE).expect("gate compiles");
    for c in '\u{80}'..='\u{ff}' {
        let s = c.to_string();
        assert_eq!(
            text.is_match(&s),
            is_valid_service_name(&s),
            "char {:#06x}",
            c as u32
        );
    }

    let mut samples: Vec<String> = ["a", "-", "_"]
        .iter()
        .flat_map(|unit| [0, 1, 127, 128, 129].map(|n| unit.repeat(n)))
        .collect();
    samples.extend(
        [
            ".", "..", ".x", "-a", "_a", "a.", "a\n", "a b", "a/b", "a.b-c_d",
        ]
        .map(String::from),
    );
    for s in samples {
        assert_eq!(
            text.is_match(&s),
            is_valid_service_name(&s),
            "{:?} (len {})",
            if s.len() > 16 { &s[..16] } else { &s },
            s.len()
        );
    }
}
