// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Trawl's sign-in return protocol. Only the outer component is decoded;
//! destination query and fragment bytes belong to their destination page.
//! These client parsing limits do not guarantee that a deployment's proxy
//! accepts the larger, encoded sign-in request URI.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

pub const MAX_DESTINATION_BYTES: usize = 64 * 1024;
pub const MAX_LOGIN_QUERY_BYTES: usize = 3 * MAX_DESTINATION_BYTES + 64;
pub const FALLBACK: &str = "/search";

/// A closed-route destination, with its original query and fragment suffix.
/// Private fields prevent navigation callers from bypassing validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnDestination {
    relative: String,
    pathname: &'static str,
}

impl ReturnDestination {
    pub fn as_str(&self) -> &str {
        &self.relative
    }

    pub fn pathname(&self) -> &'static str {
        self.pathname
    }

    fn fallback() -> Self {
        Self {
            relative: FALLBACK.into(),
            pathname: FALLBACK,
        }
    }
}

/// Admit literal protected paths and the explicitly supported aliases only.
/// Split the fragment first: the question mark in `/search#a?b` is data.
pub fn validate_destination(raw: &str) -> Option<ReturnDestination> {
    if raw.len() > MAX_DESTINATION_BYTES || raw.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    let before_fragment = raw.split_once('#').map_or(raw, |(prefix, _)| prefix);
    let path = before_fragment
        .split_once('?')
        .map_or(before_fragment, |(prefix, _)| prefix);
    if path.starts_with("//") {
        return None;
    }
    let candidate = if path == "/" {
        path
    } else {
        path.strip_suffix('/').unwrap_or(path)
    };
    // This closed match also rejects schemes, authorities, credentials,
    // percent escapes, backslashes, dot segments, and repeated slashes.
    let pathname = match candidate {
        "/" | "/search" => "/search",
        "/search/history" => "/search/history",
        "/search/schema" => "/search/schema",
        "/jobs" | "/jobs/nets" => "/jobs/nets",
        "/jobs/runs" => "/jobs/runs",
        "/settings" | "/settings/health" => "/settings/health",
        _ => return None,
    };
    let suffix = &raw[path.len()..];
    if pathname.len() + suffix.len() > MAX_DESTINATION_BYTES {
        return None;
    }
    Some(ReturnDestination {
        relative: format!("{pathname}{suffix}"),
        pathname,
    })
}

/// Read `location.search()`, requiring exactly one component-decoded name.
/// Malformed encoding anywhere invalidates the outer query, even in ignored
/// parameters. Unlike Search's form decoder, a literal plus stays a plus.
pub fn read_return_to(raw_search: &str) -> ReturnDestination {
    read_return_to_inner(raw_search).unwrap_or_else(ReturnDestination::fallback)
}

fn read_return_to_inner(raw_search: &str) -> Option<ReturnDestination> {
    let query = raw_search.strip_prefix('?').unwrap_or(raw_search);
    if query.len() > MAX_LOGIN_QUERY_BYTES {
        return None;
    }
    let mut destination = None;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = decode_component(name, MAX_LOGIN_QUERY_BYTES)?;
        let limit = if name == "return_to" {
            MAX_DESTINATION_BYTES
        } else {
            MAX_LOGIN_QUERY_BYTES
        };
        let value = decode_component(value, limit)?;
        if name == "return_to" {
            if destination.is_some() {
                return None;
            }
            destination = Some(validate_destination(&value)?);
        }
    }
    destination
}

/// Encode with Search's browser-stable component encoder, but never its form
/// decoder. A validated destination fits the outer bound even if every byte
/// expands to three bytes, including the `return_to=` name.
pub fn login_href(destination: &ReturnDestination) -> String {
    format!(
        "/login?return_to={}",
        crate::search_url::percent_encode(destination.as_str())
    )
}

fn captured_destination(raw: &str) -> ReturnDestination {
    validate_destination(raw).unwrap_or_else(ReturnDestination::fallback)
}

#[cfg(target_arch = "wasm32")]
std::thread_local! {
    // The WASM instance belongs to this document. Component cleanup must not
    // reset the guard: another live Jobs helper or a queued /me response can
    // otherwise replace the first destination before the document unloads.
    static LOGIN_REDIRECT_STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // Explicit logout takes precedence from the user's click until failure
    // or document departure, including while its HTTP response is pending.
    static EXPLICIT_LOGOUT_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Claim logout for this document, including across shell remounts.
#[cfg(target_arch = "wasm32")]
pub fn begin_explicit_logout() -> bool {
    EXPLICIT_LOGOUT_PENDING.with(|pending| !pending.replace(true))
}

/// A failed logout must allow subsequent session expiry to redirect normally.
#[cfg(target_arch = "wasm32")]
pub fn cancel_explicit_logout() {
    EXPLICIT_LOGOUT_PENDING.with(|pending| pending.set(false));
}

/// Replace an interrupted protected page with sign-in, capturing its current
/// URL only when the first authorized automatic redirect reaches this call.
/// Return whether navigation started, so suppressed Jobs reads keep polling.
#[cfg(target_arch = "wasm32")]
pub fn redirect_to_login() -> bool {
    if LOGIN_REDIRECT_STARTED.with(std::cell::Cell::get)
        || EXPLICIT_LOGOUT_PENDING.with(std::cell::Cell::get)
    {
        return false;
    }
    let Some(window) = web_sys::window() else {
        return false;
    };
    let location = window.location();
    let raw = (|| {
        Ok::<_, wasm_bindgen::JsValue>(format!(
            "{}{}{}",
            location.pathname()?,
            location.search()?,
            location.hash()?
        ))
    })()
    .unwrap_or_default();
    let candidate = captured_destination(&raw);
    let destination = browser_destination(&window, candidate);
    LOGIN_REDIRECT_STARTED.with(|started| started.set(true));
    if location.replace(&login_href(&destination)).is_err() {
        LOGIN_REDIRECT_STARTED.with(|started| started.set(false));
        return false;
    }
    true
}

/// Read the current sign-in query after authentication succeeds. Failures do
/// not consume the URL, and a copied or reloaded sign-in link needs no storage.
#[cfg(target_arch = "wasm32")]
pub fn finish_login() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let candidate = read_return_to(&window.location().search().unwrap_or_default());
    let destination = browser_destination(&window, candidate);
    // Navigate with the validated relative string, not the URL parser's
    // reserialization, so the destination still owns its original suffix.
    let _ = window.location().replace(destination.as_str());
}

#[cfg(target_arch = "wasm32")]
fn browser_destination(
    window: &web_sys::Window,
    candidate: ReturnDestination,
) -> ReturnDestination {
    let accepted = (|| {
        let current = web_sys::Url::new(&window.location().href().ok()?).ok()?;
        if !matches!(current.protocol().as_str(), "http:" | "https:") {
            return None;
        }
        let parsed = web_sys::Url::new_with_base(candidate.as_str(), &current.href()).ok()?;
        // URL origins normalize scheme/host/default ports. A path mismatch
        // means the browser interpreted syntax that the pure codec did not.
        Some(parsed.origin() == current.origin() && parsed.pathname() == candidate.pathname())
    })() == Some(true);
    if accepted {
        candidate
    } else {
        ReturnDestination::fallback()
    }
}

fn decode_component(raw: &str, max_bytes: usize) -> Option<String> {
    let mut bytes = Vec::with_capacity(raw.len().min(max_bytes));
    let mut input = raw.bytes();
    while let Some(byte) = input.next() {
        if bytes.len() == max_bytes {
            return None;
        }
        bytes.push(if byte == b'%' {
            let high = hex(input.next()?)?;
            let low = hex(input.next()?)?;
            high * 16 + low
        } else {
            byte
        });
    }
    String::from_utf8(bytes).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(raw: &str) -> ReturnDestination {
        let destination = validate_destination(raw).expect("valid destination");
        let href = login_href(&destination);
        assert!(href.strip_prefix("/login?").unwrap().len() <= MAX_LOGIN_QUERY_BYTES);
        let restored = read_return_to(href.strip_prefix("/login").unwrap());
        assert_eq!(restored, destination);
        restored
    }

    #[test]
    fn producer_encodes_search_fallback_for_unadmitted_capture() {
        for raw in [
            "/login?return_to=/jobs/runs",
            "//other.example/search",
            "/unknown",
        ] {
            assert_eq!(
                login_href(&captured_destination(raw)),
                "/login?return_to=%2Fsearch"
            );
        }
        let oversized = format!("/search?{}", "x".repeat(MAX_DESTINATION_BYTES));
        assert_eq!(
            login_href(&captured_destination(&oversized)),
            "/login?return_to=%2Fsearch"
        );
    }

    #[test]
    fn closed_routes_aliases_and_single_trailing_slashes_preserve_suffixes() {
        for (path, canonical) in [
            ("/", "/search"),
            ("/search", "/search"),
            ("/search/history", "/search/history"),
            ("/search/schema", "/search/schema"),
            ("/jobs", "/jobs/nets"),
            ("/jobs/nets", "/jobs/nets"),
            ("/jobs/runs", "/jobs/runs"),
            ("/settings", "/settings/health"),
            ("/settings/health", "/settings/health"),
        ] {
            for suffix in ["", "?net=1&ntab=runs#row", "#a?b", "?#"] {
                let result = round_trip(&format!("{path}{suffix}"));
                assert_eq!(result.pathname(), canonical);
                assert_eq!(result.as_str(), format!("{canonical}{suffix}"));
                if path != "/" {
                    assert_eq!(round_trip(&format!("{path}/{suffix}")), result);
                }
            }
        }
    }

    #[test]
    fn exactly_one_outer_layer_preserves_inner_data_and_malformed_search() {
        for suffix in [
            "?q=%252F+a%2Bb%26c&f=v1.!&r=garbage#row%23x",
            "?next=https://other.example/a?b=c&value=%00%0D%FF#fragment",
            "?q=日本語+é&x=%&x=%GG#日?x",
            "?q=service%3Dnginx&page=85899346&r=1h",
        ] {
            let raw = format!("/search{suffix}");
            assert_eq!(round_trip(&raw).as_str(), raw);
        }
        assert_eq!(
            read_return_to("return_to=%2Fsearch%3Fq=a+b").as_str(),
            "/search?q=a+b"
        );
        assert_eq!(
            decode_component("%252F+%E6%97%A5", 20).as_deref(),
            Some("%2F+日")
        );
    }

    #[test]
    fn hostile_paths_and_raw_controls_are_rejected() {
        for raw in [
            "",
            "search",
            "https://same.example/search",
            "https://other.example/search",
            "//other.example/search",
            "//",
            "///search",
            "/search//",
            "/jobs//",
            "https://user:pass@same.example/search",
            "javascript:alert(1)",
            "/search/../login",
            "/./search",
            "/%73earch",
            "/search%2F",
            "/search\\",
            "\\search",
            "/api/auth/me",
            "/login",
            "/unknown",
            "/Search",
            "/search/child",
        ] {
            assert!(validate_destination(raw).is_none(), "{raw:?}");
        }
        for byte in (0..=31).chain(std::iter::once(127)) {
            for raw in [
                format!("/search?x={}", char::from(byte)),
                format!("/search#{}", char::from(byte)),
            ] {
                assert!(validate_destination(&raw).is_none());
                let query = format!("return_to={}", crate::search_url::percent_encode(&raw));
                assert_eq!(read_return_to(&query).as_str(), FALLBACK);
            }
        }
    }

    #[test]
    fn missing_empty_duplicate_and_malformed_targets_fall_back() {
        for query in [
            "",
            "?",
            "other=/jobs/runs",
            "return_to",
            "return_to=",
            "return_to=%",
            "return_to=%2",
            "return_to=%GG",
            "return_to=%FF",
            "return_to=%C0%AF",
            "return_to=%ED%A0%80",
            "return_to=%F4%90%80%80",
            "return_to=/jobs/runs&return_to=/jobs/nets",
            "return_to=/jobs/runs&%72eturn%5Fto=/jobs/runs",
            "return_to=/jobs/runs&return_to=",
            "return_to=/login",
            "return_to=%252Fsearch",
        ] {
            assert_eq!(read_return_to(query).as_str(), FALLBACK, "{query}");
        }
        assert_eq!(
            read_return_to("?%72eturn%5fto=%2fjobs%2fruns").as_str(),
            "/jobs/runs"
        );
        assert_eq!(
            read_return_to("return+to=/login&return_to=/jobs/runs").as_str(),
            "/jobs/runs"
        );
    }

    #[test]
    fn malformed_unrelated_keys_or_values_invalidate_the_outer_query() {
        for pair in [
            "%=ok",
            "%FF=ok",
            "other=%",
            "other=%GG",
            "other=%FF",
            "other=%C0%AF",
        ] {
            for query in [
                format!("{pair}&return_to=/jobs/runs"),
                format!("return_to=/jobs/runs&{pair}"),
            ] {
                assert_eq!(read_return_to(&query).as_str(), FALLBACK, "{query}");
            }
        }
        assert_eq!(
            read_return_to("other=%252F+ok&return_to=/jobs/runs&x=https%3A%2F%2Fexample.com")
                .as_str(),
            "/jobs/runs"
        );
    }

    #[test]
    fn destination_byte_limit_is_inclusive_before_and_after_alias_mapping() {
        let at = format!(
            "/jobs/runs?{}",
            "x".repeat(MAX_DESTINATION_BYTES - "/jobs/runs?".len())
        );
        assert_eq!(round_trip(&at).as_str().len(), MAX_DESTINATION_BYTES);
        assert!(validate_destination(&format!("{at}x")).is_none());
        let too_long = format!(
            "return_to={}",
            crate::search_url::percent_encode(&format!("{at}x"))
        );
        assert_eq!(read_return_to(&too_long).as_str(), FALLBACK);

        let alias_at = format!(
            "/settings?{}",
            "x".repeat(MAX_DESTINATION_BYTES - "/settings/health?".len())
        );
        assert_eq!(round_trip(&alias_at).as_str().len(), MAX_DESTINATION_BYTES);
        assert!(validate_destination(&format!("{alias_at}x")).is_none());
        // A trailing slash cannot rescue an already oversized input.
        let long_slash = format!(
            "/jobs/runs/?{}",
            "x".repeat(MAX_DESTINATION_BYTES - "/jobs/runs?".len())
        );
        assert!(validate_destination(&long_slash).is_none());
        let unicode = format!("/search?{}", "日".repeat(MAX_DESTINATION_BYTES / 3));
        assert!(validate_destination(&unicode).is_none());
    }

    #[test]
    fn raw_query_limit_is_inclusive_and_checked_before_decoding() {
        let prefix = "return_to=/jobs/runs&ignored=";
        let at = format!(
            "{prefix}{}",
            "x".repeat(MAX_LOGIN_QUERY_BYTES - prefix.len())
        );
        assert_eq!(read_return_to(&at).as_str(), "/jobs/runs");
        assert_eq!(read_return_to(&format!("?{at}")).as_str(), "/jobs/runs");
        assert_eq!(read_return_to(&format!("{at}x")).as_str(), FALLBACK);
        assert_eq!(decode_component("%61%62", 2).as_deref(), Some("ab"));
        assert!(decode_component("%61%62%63", 2).is_none());
    }

    #[test]
    fn existing_search_query_boundary_and_maximum_encoding_expansion_survive() {
        let query = format!("q={}", "+".repeat(crate::search_url::MAX_SEARCH_BYTES - 2));
        let destination = format!("/search?{query}#row%23one");
        assert_eq!(round_trip(&destination).as_str(), destination);
        let large = format!(
            "/search?{}",
            "+".repeat(MAX_DESTINATION_BYTES - "/search?".len())
        );
        let restored = round_trip(&large);
        assert_eq!(restored.as_str(), large);
        assert!(login_href(&restored).len() > 190 * 1024);
    }
}
