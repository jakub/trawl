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
}
