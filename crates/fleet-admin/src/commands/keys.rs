//! Handlers for `fleet-admin keys` subcommands.

use std::time::Duration;

use fleet_auth::Role;
use fleet_auth::keys::ApiKeyInfo;
use fleet_auth::store::KeyStore;

/// Create a new API key.
pub fn create(
    store: &KeyStore,
    name: &str,
    role: Role,
    expires: Option<&str>,
) -> Result<(), String> {
    let expires_in = match expires {
        Some(s) => Some(parse_duration(s)?),
        None => None,
    };

    let created = store
        .create_key(name, role, expires_in)
        .map_err(|e| e.to_string())?;

    eprintln!("created API key:\n");
    eprintln!("  name:    {}", created.info.name);
    eprintln!("  role:    {}", created.info.role);
    eprintln!("  prefix:  {}", created.info.prefix);
    if let Some(ref exp) = created.info.expires_at {
        eprintln!("  expires: {exp}");
    } else {
        eprintln!("  expires: never");
    }
    eprintln!();
    println!("{}", &*created.plaintext_token);
    eprintln!();
    eprintln!("WARNING: this token will not be shown again. store it securely.");

    Ok(())
}

/// List API keys.
pub fn list(store: &KeyStore, all: bool) -> Result<(), String> {
    let keys = store.list_keys(!all).map_err(|e| e.to_string())?;

    if keys.is_empty() {
        eprintln!("no keys found");
        return Ok(());
    }

    let table = format_keys_table(&keys);
    println!("{table}");
    eprintln!("{} key(s)", keys.len());

    Ok(())
}

/// Revoke an API key by prefix.
pub fn revoke(store: &KeyStore, prefix: &str) -> Result<(), String> {
    let info = store.revoke_key(prefix).map_err(|e| e.to_string())?;
    eprintln!("revoked key: {} ({})", info.prefix, info.name);
    Ok(())
}

/// Format a list of keys as a table.
fn format_keys_table(keys: &[ApiKeyInfo]) -> comfy_table::Table {
    let mut table = comfy_table::Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    table.set_header(vec![
        "prefix",
        "name",
        "role",
        "active",
        "created",
        "last used",
    ]);

    for key in keys {
        table.add_row(vec![
            key.prefix.clone(),
            key.name.clone(),
            key.role.to_string(),
            if key.active {
                "yes".to_owned()
            } else {
                "no".to_owned()
            },
            format_timestamp(&key.created_at),
            key.last_used
                .as_deref()
                .map_or_else(|| "never".to_owned(), format_timestamp),
        ]);
    }

    table
}

/// Format an ISO 8601 timestamp for display (truncate to seconds, drop timezone).
fn format_timestamp(ts: &str) -> String {
    // RFC 3339 format: "2026-02-10T12:00:00.123456789+00:00"
    // We want: "2026-02-10 12:00:00"
    let s = ts.replace('T', " ");
    // Strip timezone offset or Z suffix.
    let s = s.split('+').next().unwrap_or(&s);
    let s = s.split('Z').next().unwrap_or(s);
    // Strip sub-second precision (everything after the seconds).
    match s.find('.') {
        Some(dot) => s[..dot].to_owned(),
        None => s.to_owned(),
    }
}

/// Parse a human-readable duration string (e.g., "90d", "24h", "52w").
fn parse_duration(s: &str) -> Result<Duration, String> {
    if s == "never" {
        return Err("use omitting --expires instead of 'never'".into());
    }

    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".into());
    }

    let (num_str, unit) = s.split_at(s.len() - 1);
    if num_str.is_empty() {
        return Err(format!("missing numeric value in duration: {s}"));
    }
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration number: {num_str}"))?;

    let seconds = match unit {
        "s" => Some(num),
        "m" => num.checked_mul(60),
        "h" => num.checked_mul(3600),
        "d" => num.checked_mul(86400),
        "w" => num.checked_mul(604_800),
        _ => return Err(format!("unknown duration unit: {unit} (use s/m/h/d/w)")),
    }
    .ok_or_else(|| format!("duration too large: {s}"))?;

    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_days() {
        let d = parse_duration("90d").unwrap();
        assert_eq!(d, Duration::from_secs(90 * 86400));
    }

    #[test]
    fn parse_duration_hours() {
        let d = parse_duration("24h").unwrap();
        assert_eq!(d, Duration::from_secs(24 * 3600));
    }

    #[test]
    fn parse_duration_weeks() {
        let d = parse_duration("52w").unwrap();
        assert_eq!(d, Duration::from_secs(52 * 604_800));
    }

    #[test]
    fn parse_duration_invalid_unit() {
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn parse_duration_invalid_number() {
        assert!(parse_duration("abcd").is_err());
    }

    #[test]
    fn parse_duration_unit_only() {
        let err = parse_duration("d").unwrap_err();
        assert!(
            err.contains("missing numeric"),
            "expected 'missing numeric', got: {err}"
        );
    }

    #[test]
    fn parse_duration_rejects_overflow() {
        let err = parse_duration("999999999999999999w").unwrap_err();
        assert!(
            err.contains("too large"),
            "expected 'too large', got: {err}"
        );
    }

    #[test]
    fn format_timestamp_strips_tz() {
        let ts = "2026-02-10T12:00:00+00:00";
        assert_eq!(format_timestamp(ts), "2026-02-10 12:00:00");
    }

    #[test]
    fn format_timestamp_strips_z() {
        let ts = "2026-02-10T12:00:00Z";
        assert_eq!(format_timestamp(ts), "2026-02-10 12:00:00");
    }

    fn test_keys() -> Vec<ApiKeyInfo> {
        vec![
            ApiKeyInfo {
                id: 1,
                prefix: "dGhpcyBp".into(),
                name: "web-frontend".into(),
                role: Role::Analyst,
                active: true,
                created_at: "2026-02-10T12:00:00+00:00".into(),
                expires_at: Some("2026-05-11T12:00:00+00:00".into()),
                last_used: Some("2026-02-10T14:30:00+00:00".into()),
                revoked_at: None,
            },
            ApiKeyInfo {
                id: 2,
                prefix: "YW5vdGhl".into(),
                name: "cli-readonly".into(),
                role: Role::Reader,
                active: true,
                created_at: "2026-02-09T08:00:00+00:00".into(),
                expires_at: None,
                last_used: None,
                revoked_at: None,
            },
            ApiKeyInfo {
                id: 3,
                prefix: "cmV2b2tl".into(),
                name: "old-key".into(),
                role: Role::Admin,
                active: false,
                created_at: "2026-01-01T00:00:00+00:00".into(),
                expires_at: None,
                last_used: Some("2026-02-01T10:00:00+00:00".into()),
                revoked_at: Some("2026-02-05T09:00:00+00:00".into()),
            },
        ]
    }

    #[test]
    fn snapshot_keys_table() {
        let keys = test_keys();
        let table = format_keys_table(&keys);
        insta::assert_snapshot!(table.to_string());
    }

    #[test]
    fn snapshot_keys_table_active_only() {
        let keys: Vec<_> = test_keys().into_iter().filter(|k| k.active).collect();
        let table = format_keys_table(&keys);
        insta::assert_snapshot!(table.to_string());
    }
}
