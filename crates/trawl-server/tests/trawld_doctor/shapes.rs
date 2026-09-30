// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The shapes of the values no doctor report may show, whatever a test
//! planted.
//!
//! The `trawld --doctor` leak check applies [`forbidden_shape`] to both
//! output streams. This file depends on nothing but `std`, so another
//! crate's doctor tests, such as `trawl-web --doctor`'s, include it with
//! `#[path]` and refuse the same shapes.

/// The first value in `text` shaped like one no doctor report may show,
/// named, or `None`:
///
/// - a UUID, the form of every catalog id;
/// - 32 or more hex digits in a row, or 8 or more hex pairs joined by
///   colons, the forms of a certificate fingerprint;
/// - a dotted IPv4 address, with or without a port, or a bracketed IPv6
///   address, the forms of a listener address and a certificate's IP SAN.
pub fn forbidden_shape(text: &str) -> Option<(&'static str, String)> {
    let bytes = text.as_bytes();
    let hex = |b: u8| b.is_ascii_hexdigit();
    for start in 0..bytes.len() {
        // Only look where a token starts, so a match is not the tail of a
        // longer run already judged.
        if start > 0 && bytes[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let rest = &bytes[start..];
        let run = rest.iter().take_while(|b| hex(**b)).count();
        if run >= 32 {
            return Some(("a hex fingerprint", text[start..start + run].to_owned()));
        }
        if let Some(len) = uuid_at(rest) {
            return Some(("a catalog id", text[start..start + len].to_owned()));
        }
        if let Some(len) = colon_hex_at(rest) {
            return Some((
                "a colon-hex fingerprint",
                text[start..start + len].to_owned(),
            ));
        }
        if let Some(len) = ipv4_at(rest) {
            return Some(("an IPv4 address", text[start..start + len].to_owned()));
        }
        if rest.first() == Some(&b'[') {
            let inner = rest[1..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(**b, b':' | b'.' | b'%'))
                .count();
            if inner >= 2 && rest.get(1 + inner) == Some(&b']') && rest[1..=inner].contains(&b':') {
                return Some(("an IPv6 address", text[start..start + inner + 2].to_owned()));
            }
        }
    }
    None
}

/// The length of the UUID `bytes` starts with (8-4-4-4-12 hex digits).
fn uuid_at(bytes: &[u8]) -> Option<usize> {
    let mut at = 0;
    for (index, group) in [8, 4, 4, 4, 12].into_iter().enumerate() {
        if index > 0 {
            (bytes.get(at) == Some(&b'-')).then_some(())?;
            at += 1;
        }
        let digits = bytes[at.min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        (digits == group).then_some(())?;
        at += group;
    }
    Some(at)
}

/// The length of the colon-joined hex pairs `bytes` starts with, when
/// there are at least 8 of them.
fn colon_hex_at(bytes: &[u8]) -> Option<usize> {
    let mut pairs = 0;
    let mut at = 0;
    loop {
        let pair = bytes.get(at..at + 2)?;
        if !pair.iter().all(u8::is_ascii_hexdigit) {
            break;
        }
        pairs += 1;
        at += 2;
        if bytes.get(at) == Some(&b':') && bytes.get(at + 1).is_some_and(u8::is_ascii_hexdigit) {
            at += 1;
        } else {
            break;
        }
    }
    (pairs >= 8).then_some(at)
}

/// The length of the dotted IPv4 address, and its port if one follows,
/// that `bytes` starts with.
fn ipv4_at(bytes: &[u8]) -> Option<usize> {
    let mut at = 0;
    for octet in 0..4 {
        if octet > 0 {
            (bytes.get(at) == Some(&b'.')).then_some(())?;
            at += 1;
        }
        let digits = bytes[at.min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        (1..=3).contains(&digits).then_some(())?;
        at += digits;
    }
    if bytes.get(at).is_some_and(u8::is_ascii_alphanumeric) || bytes.get(at) == Some(&b'.') {
        return None;
    }
    if bytes.get(at) == Some(&b':') {
        at += 1 + bytes[at + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
    }
    Some(at)
}
