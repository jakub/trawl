// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drift guard: the configured allowlist and the `Origin` header go
//! through the same parser (ADR-0016).
//!
//! Full-origin comparison is only sound if both sides normalize
//! identically. A second parser for config entries — or a config path that
//! skipped a rule the header path applies — would turn every spelling
//! difference into either a silent allowlist miss (the operator's browser
//! gets a 403 nobody can explain) or a silent hit (the guard is decorative).
//! These tests run one table through both doors and assert the answers are
//! the same value, not merely the same shape.
//!
//! The module's own unit tests own the rule table. This file owns the
//! claim that there is only one of it.

#![cfg(feature = "session")]

use fleet_auth::{Origin, OriginParseError, PublicOrigins, PublicOriginsError};

/// Parse one entry as a configured origin, returning the error the config
/// door produced for it.
fn config_error(entry: &str) -> OriginParseError {
    match PublicOrigins::parse([entry]) {
        Err(PublicOriginsError::Entry { index, source, .. }) => {
            assert_eq!(index, 0, "single-entry list reports index 0");
            source
        }
        other => panic!("{entry:?} should be refused as a config entry, got {other:?}"),
    }
}

/// Every spelling pair the two doors must agree on: what an operator
/// plausibly writes in `public_origins`, and what a browser actually sends.
const EQUIVALENT_SPELLINGS: &[(&str, &str)] = &[
    // Default ports are implied on the wire and often written in config.
    ("https://trawl.example.com", "https://trawl.example.com:443"),
    // The root dot survives case folding and port defaulting like any
    // other host text.
    (
        "https://TRAWL.example.com.",
        "https://trawl.example.com.:443",
    ),
    ("https://trawl.example.com:443", "https://trawl.example.com"),
    ("http://localhost", "http://localhost:80"),
    // Case: an operator pasting from a browser address bar gets mixed case.
    ("https://TRAWL.example.com", "https://trawl.example.com"),
    ("HTTPS://trawl.example.com", "https://trawl.example.com"),
    // Leading zeros in the port digit string.
    ("http://localhost:08090", "http://localhost:8090"),
    // IPv6: compressed in config, expanded on the wire, or the reverse.
    ("http://[::1]:8090", "http://[0:0:0:0:0:0:0:1]:8090"),
    ("http://[0:0:0:0:0:0:0:1]:8090", "http://[::1]:8090"),
    ("http://[::1]:80", "http://[::1]"),
    ("http://[::FFFF:127.0.0.1]", "http://[::ffff:127.0.0.1]"),
];

/// Pairs that must stay distinct. Every one of these is a CSRF bypass if
/// normalization ever collapses it.
const DISTINCT_PAIRS: &[(&str, &str)] = &[
    // The defect ADR-0016 closes.
    ("https://trawl.example.com", "http://trawl.example.com"),
    (
        "https://trawl.example.com",
        "https://trawl.example.com:8443",
    ),
    ("http://localhost:8090", "http://localhost:8091"),
    ("http://localhost:8090", "http://localhost"),
    // Loopback spellings are three different origins to a browser.
    ("http://localhost:8090", "http://127.0.0.1:8090"),
    ("http://127.0.0.1:8090", "http://[::1]:8090"),
    ("http://[::ffff:127.0.0.1]:8090", "http://127.0.0.1:8090"),
    // Neighbouring names.
    ("https://trawl.example.com", "https://trawl.example.org"),
    // The rooted name and the unrooted one are two origins, because they
    // are two origins to the browser that sends them.
    ("https://trawl.example.com.", "https://trawl.example.com"),
    ("https://trawl.example.com", "https://trawl.example.com."),
    ("https://trawl.example.com", "https://sub.trawl.example.com"),
];

/// One refusal table, asserted through both doors.
const REFUSED: &[(&str, OriginParseError)] = &[
    ("null", OriginParseError::NotSerializedOrigin),
    ("", OriginParseError::NotSerializedOrigin),
    ("trawl.example.com", OriginParseError::NotSerializedOrigin),
    (
        "https://trawl.example.com/",
        OriginParseError::NotSerializedOrigin,
    ),
    (
        "https://trawl.example.com/path",
        OriginParseError::NotSerializedOrigin,
    ),
    (
        "https://trawl.example.com?q=1",
        OriginParseError::NotSerializedOrigin,
    ),
    (
        "https://trawl.example.com#f",
        OriginParseError::NotSerializedOrigin,
    ),
    (
        "https://user:pw@trawl.example.com",
        OriginParseError::NotSerializedOrigin,
    ),
    (
        "https://trawl.example.com\r\n",
        OriginParseError::NotSerializedOrigin,
    ),
    (
        "https://trawl.example.com ",
        OriginParseError::NotSerializedOrigin,
    ),
    (
        "ws://trawl.example.com",
        OriginParseError::UnsupportedScheme,
    ),
    ("file:///etc/passwd", OriginParseError::UnsupportedScheme),
    ("https://", OriginParseError::EmptyHost),
    ("https://:8090", OriginParseError::EmptyHost),
    ("https://tráwl.example.com", OriginParseError::NonAsciiHost),
    ("https://trawl_example.com", OriginParseError::InvalidHost),
    ("https://trawl.example.com..", OriginParseError::InvalidHost),
    ("https://.trawl.example.com", OriginParseError::InvalidHost),
    ("http://127.0.0.1.", OriginParseError::InvalidHost),
    ("https://*.example.com", OriginParseError::InvalidHost),
    ("http://::1", OriginParseError::InvalidHost),
    ("http://[fe80::1%eth0]", OriginParseError::InvalidHost),
    ("http://010.1.1.1", OriginParseError::InvalidHost),
    ("http://0x7f000001:8090", OriginParseError::InvalidHost),
    ("http://127.0.0.0x1:8090", OriginParseError::InvalidHost),
    ("https://trawl.example.com:0", OriginParseError::InvalidPort),
    (
        "https://trawl.example.com:65536",
        OriginParseError::InvalidPort,
    ),
    ("https://trawl.example.com:", OriginParseError::InvalidPort),
    (
        "https://trawl.example.com:-1",
        OriginParseError::InvalidPort,
    ),
];

#[test]
fn a_configured_entry_matches_every_equivalent_header_spelling() {
    for (entry, header) in EQUIVALENT_SPELLINGS {
        let allowed = PublicOrigins::parse([entry]).unwrap_or_else(|e| panic!("{entry:?}: {e}"));
        let sent = Origin::parse(header).unwrap_or_else(|e| panic!("{header:?}: {e}"));
        assert!(
            allowed.contains(&sent),
            "config {entry:?} must accept header {header:?}"
        );
    }
}

#[test]
fn distinct_origins_stay_distinct_through_both_doors() {
    for (entry, header) in DISTINCT_PAIRS {
        let allowed = PublicOrigins::parse([entry]).unwrap_or_else(|e| panic!("{entry:?}: {e}"));
        let sent = Origin::parse(header).unwrap_or_else(|e| panic!("{header:?}: {e}"));
        assert!(
            !allowed.contains(&sent),
            "config {entry:?} must NOT accept header {header:?}"
        );
        // Same claim at the value level, so a `contains` bug and an
        // equality bug can't cover for each other.
        assert_ne!(Origin::parse(entry).unwrap(), sent);
    }
}

#[test]
fn both_doors_refuse_the_same_inputs_for_the_same_reason() {
    for (input, expected) in REFUSED {
        let header_error = Origin::parse(input).expect_err(&format!("{input:?} must be refused"));
        assert_eq!(header_error, *expected, "header door on {input:?}");
        assert_eq!(config_error(input), *expected, "config door on {input:?}");
    }
}

#[test]
fn a_rooted_host_is_one_spelling_through_both_doors() {
    // A browser preserves the DNS root dot when it serializes an origin,
    // so `https://trawl.example.com./` browses as
    // `https://trawl.example.com.` and an install reached that way must be
    // able to state exactly that. Neither door normalizes it in either
    // direction, and Display keeps it, which is what makes the rejection
    // log's text pasteable back into the config.
    let rooted = Origin::parse("https://trawl.example.com.").expect("a rooted host parses");
    assert_eq!(rooted.to_string(), "https://trawl.example.com.");
    assert_eq!(Origin::parse(&rooted.to_string()), Ok(rooted.clone()));

    let configured = PublicOrigins::parse(["https://trawl.example.com."]).expect("and configures");
    assert!(configured.contains(&rooted));
    assert!(!configured.contains(&Origin::parse("https://trawl.example.com").unwrap()));

    // And the other way round: configuring the unrooted name does not
    // silently admit the rooted one.
    let unrooted = PublicOrigins::parse(["https://trawl.example.com"]).expect("parses");
    assert!(!unrooted.contains(&rooted));

    // Two spellings, so an operator serving both states both, and the
    // duplicate check does not collapse them.
    assert!(
        PublicOrigins::parse(["https://trawl.example.com", "https://trawl.example.com."]).is_ok()
    );
}

#[test]
fn a_bad_entry_is_reported_at_its_own_index() {
    let err = PublicOrigins::parse(["https://trawl.example.com", "http://localhost:8090", "null"])
        .expect_err("the third entry is not an origin");
    assert_eq!(
        err,
        PublicOriginsError::Entry {
            index: 2,
            entry: "null".to_string(),
            source: OriginParseError::NotSerializedOrigin,
        }
    );
}

#[test]
fn the_canonical_form_round_trips_through_the_parser() {
    // Anything that parses must re-parse from its own Display to the same
    // value: that is what lets the rejection log print a normalized origin
    // an operator can paste straight into public_origins.
    let inputs = EQUIVALENT_SPELLINGS
        .iter()
        .flat_map(|(a, b)| [*a, *b])
        .chain(DISTINCT_PAIRS.iter().flat_map(|(a, b)| [*a, *b]));
    for input in inputs {
        let origin = Origin::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
        let rendered = origin.to_string();
        assert_eq!(
            Origin::parse(&rendered).unwrap_or_else(|e| panic!("{rendered:?}: {e}")),
            origin,
            "{input:?} rendered as {rendered:?}"
        );
        // And the config door accepts what the log prints.
        let allowed =
            PublicOrigins::parse([&rendered]).unwrap_or_else(|e| panic!("{rendered}: {e}"));
        assert!(allowed.contains(&origin));
    }
}
