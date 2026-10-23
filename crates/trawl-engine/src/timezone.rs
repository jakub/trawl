// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Timezone offset resolution for timestamp display.
//!
//! Converts user-facing timezone configuration strings into UTC offset
//! seconds for use by the executor's timestamp formatter.

use chrono::Local;

/// Resolve a timezone configuration string to a UTC offset in seconds.
///
/// Supported values:
/// - `"UTC"` → 0
/// - `"local"` → current system timezone offset (respects `TZ` env var)
/// - `"+HH:MM"` / `"-HH:MM"` → fixed offset (e.g. `"+05:30"` → 19800)
///
/// Returns an error for unrecognized formats.
pub fn resolve_utc_offset(tz: &str) -> Result<i32, String> {
    match tz.trim() {
        s if s.eq_ignore_ascii_case("utc") => Ok(0),
        s if s.eq_ignore_ascii_case("local") => Ok(Local::now().offset().local_minus_utc()),
        s if s.starts_with('+') || s.starts_with('-') => parse_fixed_offset(s),
        other => Err(format!(
            "unrecognized timezone: {other:?} \
             (expected \"UTC\", \"local\", or a fixed offset like \"+05:30\")"
        )),
    }
}

/// Parse a fixed offset string like `"+05:30"` or `"-06:00"` into seconds.
fn parse_fixed_offset(s: &str) -> Result<i32, String> {
    let (sign, rest) = s.split_at(1);
    let sign: i32 = if sign == "+" { 1 } else { -1 };

    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 2 {
        return Err(format!(
            "invalid timezone offset format: {s:?} (expected \"+HH:MM\" or \"-HH:MM\")"
        ));
    }

    let hours: i32 = parts[0]
        .parse()
        .map_err(|_| format!("invalid hours in timezone offset: {s:?}"))?;
    let minutes: i32 = parts[1]
        .parse()
        .map_err(|_| format!("invalid minutes in timezone offset: {s:?}"))?;

    if hours > 23 || minutes > 59 {
        return Err(format!("timezone offset out of range: {s:?}"));
    }

    Ok(sign * (hours * 3600 + minutes * 60))
}

/// A result-wire timestamp re-rendered at the display offset
/// `utc_offset_secs`: the same instant in the wire's own spelling, so the
/// text a query run at that offset would have answered.
///
/// The live tail's frames carry UTC, and a client applies its own display
/// offset to them with this. `None` when the text is not RFC 3339 that
/// chrono reads (an infinity, a year past 9999) or the offset is a day or
/// more; the caller keeps such a cell as it came.
#[must_use]
pub fn rfc3339_at_offset(ts: &str, utc_offset_secs: i32) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let offset = chrono::FixedOffset::east_opt(utc_offset_secs)?;
    Some(crate::executor::wire_timestamp(
        at.timestamp_micros(),
        offset,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_resolves_to_zero() {
        assert_eq!(resolve_utc_offset("UTC").unwrap(), 0);
        assert_eq!(resolve_utc_offset("utc").unwrap(), 0);
        assert_eq!(resolve_utc_offset("Utc").unwrap(), 0);
    }

    #[test]
    fn local_resolves_to_system_offset() {
        // Just verify it doesn't error — actual value depends on system.
        let offset = resolve_utc_offset("local").unwrap();
        // Offset must be within ±14 hours (the real-world range).
        assert!(offset.abs() <= 14 * 3600);
    }

    #[test]
    fn positive_offset_parses() {
        assert_eq!(resolve_utc_offset("+05:30").unwrap(), 19800);
        assert_eq!(resolve_utc_offset("+00:00").unwrap(), 0);
        assert_eq!(resolve_utc_offset("+12:00").unwrap(), 43200);
    }

    #[test]
    fn negative_offset_parses() {
        assert_eq!(resolve_utc_offset("-06:00").unwrap(), -21600);
        assert_eq!(resolve_utc_offset("-05:30").unwrap(), -19800);
        assert_eq!(resolve_utc_offset("-00:00").unwrap(), 0);
    }

    #[test]
    fn invalid_format_errors() {
        assert!(resolve_utc_offset("America/Chicago").is_err());
        assert!(resolve_utc_offset("EST").is_err());
        assert!(resolve_utc_offset("+5").is_err());
        assert!(resolve_utc_offset("").is_err());
    }

    #[test]
    fn out_of_range_errors() {
        assert!(resolve_utc_offset("+25:00").is_err());
        assert!(resolve_utc_offset("+00:60").is_err());
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(resolve_utc_offset("  UTC  ").unwrap(), 0);
        assert_eq!(resolve_utc_offset(" +05:30 ").unwrap(), 19800);
    }

    // ── rfc3339_at_offset ─────────────────────────────────────────

    #[test]
    fn at_offset_renders_the_wire_spelling() {
        let at = |ts, secs| rfc3339_at_offset(ts, secs);
        assert_eq!(
            at("2026-02-18T12:00:00Z", 0).as_deref(),
            Some("2026-02-18T12:00:00.000000Z")
        );
        // +05:30 = 19800 seconds
        assert_eq!(
            at("2026-02-18T12:00:00.000000Z", 19800).as_deref(),
            Some("2026-02-18T17:30:00.000000+05:30")
        );
        // -06:00 = -21600 seconds
        assert_eq!(
            at("2026-02-18T12:00:00.123456Z", -21600).as_deref(),
            Some("2026-02-18T06:00:00.123456-06:00")
        );
        // 23:00 UTC + 2h offset = 01:00 next day
        assert_eq!(
            at("2026-02-18T23:00:00.000000Z", 7200).as_deref(),
            Some("2026-02-19T01:00:00.000000+02:00")
        );
        // Already at an offset: the instant moves, not the wall clock.
        assert_eq!(
            at("2026-02-18T12:00:00.000000+05:30", 0).as_deref(),
            Some("2026-02-18T06:30:00.000000Z")
        );
    }

    #[test]
    fn at_offset_leaves_what_it_cannot_read() {
        assert_eq!(rfc3339_at_offset("infinity", 3600), None);
        assert_eq!(
            rfc3339_at_offset("+10000-01-01T00:00:00.000000Z", 3600),
            None
        );
        assert_eq!(rfc3339_at_offset("not a timestamp", 3600), None);
        assert_eq!(rfc3339_at_offset("2026-02-18T12:00:00Z", 86_400), None);
    }
}
