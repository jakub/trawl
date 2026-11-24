// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drift guard: the configured allowlist and the `Origin` header go
//! through the same parser (ADR-0016).

#![cfg(feature = "session")]

use fleet_auth::{Origin, OriginParseError, PublicOrigins, PublicOriginsError};

fn config_error(entry: &str) -> OriginParseError {
    match PublicOrigins::parse([entry]) {
        Err(PublicOriginsError::Entry { index, source, .. }) => {
            assert_eq!(index, 0, "single-entry list reports index 0");
            source
        }
        other => panic!("{entry:?} should be refused as a config entry, got {other:?}"),
    }
}

const EQUIVALENT_SPELLINGS: &[(&str, &str)] = &[
    ("https://trawl.example.com", "https://trawl.example.com:443"),
    (
        "https://TRAWL.example.com.",
        "https://trawl.example.com.:443",
    ),
    ("https://trawl.example.com:443", "https://trawl.example.com"),
    ("http://localhost", "http://localhost:80"),
    ("https://TRAWL.example.com", "https://trawl.example.com"),
    ("HTTPS://trawl.example.com", "https://trawl.example.com"),
    ("http://localhost:08090", "http://localhost:8090"),
    ("http://[::1]:8090", "http://[0:0:0:0:0:0:0:1]:8090"),
    ("http://[0:0:0:0:0:0:0:1]:8090", "http://[::1]:8090"),
    ("http://[::1]:80", "http://[::1]"),
    ("http://[::FFFF:127.0.0.1]", "http://[::ffff:127.0.0.1]"),
];

const DISTINCT_PAIRS: &[(&str, &str)] = &[
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
    ("https://trawl.example.com", "https://trawl.example.org"),
    // The rooted name and the unrooted one are two origins, because they
    // are two origins to the browser that sends them.
    ("https://trawl.example.com.", "https://trawl.example.com"),
    ("https://trawl.example.com", "https://trawl.example.com."),
    ("https://trawl.example.com", "https://sub.trawl.example.com"),
];

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
    let rooted = Origin::parse("https://trawl.example.com.").expect("a rooted host parses");
    assert_eq!(rooted.to_string(), "https://trawl.example.com.");
    assert_eq!(Origin::parse(&rooted.to_string()), Ok(rooted.clone()));

    let configured = PublicOrigins::parse(["https://trawl.example.com."]).expect("and configures");
    assert!(configured.contains(&rooted));
    assert!(!configured.contains(&Origin::parse("https://trawl.example.com").unwrap()));

    let unrooted = PublicOrigins::parse(["https://trawl.example.com"]).expect("parses");
    assert!(!unrooted.contains(&rooted));

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
        let allowed =
            PublicOrigins::parse([&rendered]).unwrap_or_else(|e| panic!("{rendered}: {e}"));
        assert!(allowed.contains(&origin));
    }
}
