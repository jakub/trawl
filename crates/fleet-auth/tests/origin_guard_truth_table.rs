// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The origin guard's truth table and its rejection log (ADR-0016).
//!
//! These ran as unit tests inside `session.rs` until the structural guard
//! in `no_forwarded_trust.rs` started scanning whole source files. That
//! scan has to read every line of `src/` to be worth anything, and one of
//! the tests below deliberately plants `Host`, `Forwarded` and
//! `X-Forwarded-*` headers to prove they move no verdict. That is evidence
//! for the rule, not a breach of it, but a text scanner cannot tell the
//! two apart. So the table lives out here, where the scan does not reach,
//! and still exercises exactly the surface a caller has: `check_origin`,
//! `origin_allowed` and `PublicOrigins`.

#![cfg(feature = "session")]

use fleet_auth::{
    PublicOrigins, REASON_MULTIPLE_HEADERS, REASON_NON_UTF8, check_origin, origin_allowed,
};

/// The allowlist every guard test runs against: a public HTTPS
/// deployment, the packaged loopback bind, and that bind's IPv6
/// spelling. Three entries, so "allowed" cannot be an accident of a
/// single-element list.
fn test_origins() -> PublicOrigins {
    PublicOrigins::parse([
        "https://trawl.example.com",
        "http://localhost:8090",
        "http://[::1]:8090",
    ])
    .expect("valid test allowlist")
}

fn headers_with_origin(origin: &str) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ORIGIN,
        http::HeaderValue::from_str(origin).expect("test origin is a header value"),
    );
    headers
}

/// Every row is a verdict the deployment has to get right, and most of
/// the `false` rows are a CSRF bypass if they ever flip. The `true`
/// rows matter just as much in the other direction: each is a spelling
/// a real browser sends for an origin the operator configured, and a
/// wrong `false` is a 403 nobody can explain.
const ORIGIN_VERDICTS: &[(&str, bool, &str)] = &[
    ("https://trawl.example.com", true, "the configured origin"),
    (
        "https://trawl.example.com:443",
        true,
        "the default port spelled out is the same origin",
    ),
    (
        "https://TRAWL.Example.COM",
        true,
        "scheme and DNS host are case-insensitive",
    ),
    (
        "http://trawl.example.com",
        false,
        "cross-scheme forgery: the defect ADR-0016 closes",
    ),
    (
        "https://trawl.example.com:8443",
        false,
        "another port of the same name is another origin",
    ),
    (
        "https://evil.example.com",
        false,
        "an unrelated host, the classic forged form POST",
    ),
    (
        "https://coastwatch.example.com",
        false,
        "a sibling fleet app: sharing the cookie's domain is not authority",
    ),
    (
        "https://sub.trawl.example.com",
        false,
        "a subdomain of a configured origin is not that origin",
    ),
    (
        "https://trawl.example.com.evil.com",
        false,
        "suffix forgery",
    ),
    (
        "null",
        false,
        "the opaque origin (sandboxed iframe, data: URL)",
    ),
    ("http://[::1]:8090", true, "the configured IPv6 loopback"),
    (
        "http://[0:0:0:0:0:0:0:1]:8090",
        true,
        "the expanded spelling of that same address",
    ),
    (
        "http://[::1]:9090",
        false,
        "IPv6 with another port is another origin",
    ),
    (
        "http://127.0.0.1:8090",
        false,
        "three loopback spellings, three distinct origins",
    ),
    ("http://localhost:8090", true, "the packaged loopback bind"),
    (
        "http://localhost",
        false,
        "port 80 is not the configured 8090",
    ),
    (
        "https://trawl.example.com/",
        false,
        "a trailing slash makes it a URL, not a serialized origin",
    ),
];

#[test]
fn the_guard_answers_the_truth_table() {
    let allowed = test_origins();
    for (origin, expected, why) in ORIGIN_VERDICTS {
        let verdict = check_origin(&headers_with_origin(origin), &allowed, "login").is_ok();
        assert_eq!(verdict, *expected, "{origin:?}: {why}");
        // Same claim through the string-level door, so the two cannot
        // disagree about a row.
        assert_eq!(
            origin_allowed(Some(origin), &allowed),
            *expected,
            "{origin:?}: {why}"
        );
    }
}

#[test]
fn an_absent_origin_is_allowed() {
    // A browser CSRF control, not client authentication: curl, the CLI
    // and every scripted client send no Origin and must keep working.
    let allowed = test_origins();
    assert!(check_origin(&http::HeaderMap::new(), &allowed, "logout").is_ok());
    assert!(origin_allowed(None, &allowed));
}

#[test]
fn two_origin_headers_are_rejected_even_when_identical() {
    // `HeaderMap::get` would hand back the first of the two and the
    // guard would answer about a request nobody sent. Byte-identical
    // copies are refused as well: the request still passed through
    // something that appends Origin headers, and the next one it
    // forwards may not be a copy.
    let allowed = test_origins();
    for second in ["https://trawl.example.com", "https://evil.example.com"] {
        let mut headers = headers_with_origin("https://trawl.example.com");
        headers.append(
            http::header::ORIGIN,
            http::HeaderValue::from_str(second).unwrap(),
        );
        assert!(
            check_origin(&headers, &allowed, "login").is_err(),
            "two Origin fields must be refused (second: {second})"
        );
    }
}

#[test]
fn a_non_utf8_origin_is_rejected() {
    // Header values are bytes; a serialized origin is ASCII. There is
    // nothing to parse and nothing safe to log.
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ORIGIN,
        http::HeaderValue::from_bytes(&[0xff, 0xfe, 0x00_u8.wrapping_add(0x41)]).unwrap(),
    );
    assert!(check_origin(&headers, &test_origins(), "login").is_err());
}

#[test]
fn forwarding_headers_and_host_change_no_verdict() {
    // The structural half of ADR-0016: these headers are free text to
    // anyone who can reach the port, and the old rule let them move the
    // answer. Now they are inert in both directions — they cannot open
    // the guard for a foreign origin, and they cannot close it against
    // a configured one.
    let allowed = test_origins();
    let forged = [
        (http::header::HOST, "evil.example.com"),
        (
            http::HeaderName::from_static("x-forwarded-host"),
            "evil.example.com",
        ),
        (http::HeaderName::from_static("x-forwarded-proto"), "http"),
        (http::HeaderName::from_static("x-forwarded-port"), "8443"),
        (
            http::HeaderName::from_static("forwarded"),
            "host=evil.example.com;proto=http",
        ),
    ];

    for (origin, expected) in [
        (Some("https://trawl.example.com"), true),
        (Some("https://evil.example.com"), false),
        (None, true),
    ] {
        let mut headers = origin.map_or_else(http::HeaderMap::new, headers_with_origin);
        for (name, value) in &forged {
            headers.insert(name.clone(), http::HeaderValue::from_static(value));
        }
        assert_eq!(
            check_origin(&headers, &allowed, "logout").is_ok(),
            expected,
            "forged forwarding headers must not move the verdict for {origin:?}"
        );
    }
}

// -- the rejection log's contract -----------------------------------

/// Collects one formatted `field=value` line per event, so the tests
/// below can assert on what the guard actually recorded rather than on
/// what it meant to record.
#[derive(Clone, Default)]
struct CaptureLayer {
    lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

struct FieldWriter<'a>(&'a mut String);

impl tracing::field::Visit for FieldWriter<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{}={value} ", field.name());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut line = format!("{} ", event.metadata().level());
        event.record(&mut FieldWriter(&mut line));
        self.lines.lock().expect("capture mutex").push(line);
    }
}

/// Serializes the capture tests against each other.
///
/// `tracing` caches every callsite's `Interest` process-wide and
/// rebuilds that cache whenever a subscriber registers or dies. Two of
/// these tests running at once can leave the guard's `warn!` callsite
/// cached as "never interested" for the thread that is about to emit,
/// so the event vanishes and the test reads "the guard did not log"
/// — which is exactly the failure it exists to catch, arriving at
/// random. One lock over register/emit/read makes the rebuild
/// deterministic. Poisoning is ignored deliberately: a panic in one
/// capture test must not turn the other three into cascade failures
/// that hide their own result.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run the guard under a capturing subscriber and hand back the lines
/// it emitted.
fn captured_lines(headers: &http::HeaderMap, handler: &'static str) -> Vec<String> {
    use tracing_subscriber::layer::SubscriberExt as _;

    let _serialized = CAPTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let capture = CaptureLayer::default();
    let lines = std::sync::Arc::clone(&capture.lines);
    let subscriber = tracing_subscriber::registry().with(capture);
    let _guard = tracing::subscriber::set_default(subscriber);
    let _ = check_origin(headers, &test_origins(), handler);
    lines.lock().expect("capture mutex").clone()
}

#[test]
fn a_hostile_origin_header_never_reaches_the_log() {
    // 10 KiB of attacker-chosen bytes. Whatever else this line says, it
    // must not say this: a log field an attacker fills is a way to bury
    // other evidence, blow up log storage, or smuggle terminal escapes
    // into whoever greps for it.
    let hostile = format!("https://{}.example.com", "a".repeat(10 * 1024));
    let lines = captured_lines(&headers_with_origin(&hostile), "login");

    assert_eq!(lines.len(), 1, "exactly one warn per rejection: {lines:?}");
    let line = &lines[0];
    assert!(line.starts_with("WARN"), "got: {line}");
    assert!(line.contains("handler=login"), "got: {line}");
    assert!(line.contains("reason=too_long"), "got: {line}");
    assert!(
        !line.contains("origin="),
        "nothing parsed, so there is no safe origin text to print: {line}"
    );
    assert!(!line.contains(&hostile), "the raw header is in the log");
    assert!(
        !line.contains("aaaaaaaa"),
        "not even a slice of the raw header: {line}"
    );
    assert!(line.len() < 200, "bounded line, got {} bytes", line.len());
}

#[test]
fn a_foreign_origin_logs_its_normalized_text() {
    // The one case with safe text: it parsed, so what gets logged is
    // the parser's own canonical form. That is what makes a
    // misconfigured public_origins debuggable — the operator can paste
    // the logged text straight into the config.
    let lines = captured_lines(
        &headers_with_origin("https://EVIL.example.com:443"),
        "logout",
    );

    assert_eq!(lines.len(), 1, "got: {lines:?}");
    let line = &lines[0];
    assert!(line.contains("handler=logout"), "got: {line}");
    assert!(line.contains("reason=not_allowed"), "got: {line}");
    assert!(
        line.contains("origin=https://evil.example.com "),
        "normalized, not echoed: {line}"
    );
}

#[test]
fn every_rejection_reason_is_from_the_closed_vocabulary() {
    let mut two = headers_with_origin("https://trawl.example.com");
    two.append(
        http::header::ORIGIN,
        http::HeaderValue::from_static("https://trawl.example.com"),
    );
    let mut non_utf8 = http::HeaderMap::new();
    non_utf8.insert(
        http::header::ORIGIN,
        http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
    );

    for (headers, expected) in [
        (two, REASON_MULTIPLE_HEADERS),
        (non_utf8, REASON_NON_UTF8),
        (headers_with_origin("null"), "not_serialized_origin"),
        (
            headers_with_origin("ws://trawl.example.com"),
            "unsupported_scheme",
        ),
        (
            headers_with_origin("https://evil.example.com"),
            "not_allowed",
        ),
    ] {
        let lines = captured_lines(&headers, "login");
        assert_eq!(lines.len(), 1, "got: {lines:?}");
        assert!(
            lines[0].contains(&format!("reason={expected} ")),
            "expected reason={expected}, got: {}",
            lines[0]
        );
        // The two field-less refusals must stay field-less.
        if expected == REASON_MULTIPLE_HEADERS || expected == REASON_NON_UTF8 {
            assert!(!lines[0].contains("origin="), "got: {}", lines[0]);
        }
    }
}

#[test]
fn an_allowed_origin_logs_nothing() {
    assert!(captured_lines(&headers_with_origin("https://trawl.example.com"), "login").is_empty());
    assert!(captured_lines(&http::HeaderMap::new(), "login").is_empty());
}
