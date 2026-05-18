// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fleet-admin keys ...` — lifecycle for API keys in the Postgres keystore.
//!
//! Ports the trawl-admin shape onto fleet-auth's async [`KeyStore`].
//! `create` writes the plaintext token to stdout once; everything else
//! (metadata, prompts, summaries) goes to stderr so the output of
//! `keys create` is shell-pipeable.

use std::io::{BufRead, Write};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fleet_auth::{ApiKeyInfo, KeyStore, PrincipalKind, RoleAssignment};

use crate::error::AdminError;

/// Create a new API key.
///
/// Emits the plaintext token to stdout exactly once. All metadata
/// (name, kind, grants, prefix, expiry) goes to stderr.
pub async fn create(
    store: &KeyStore,
    name: &str,
    kind: PrincipalKind,
    assignments: &[RoleAssignment],
    expires: Option<&str>,
) -> Result<(), AdminError> {
    let expires_in = expires.map(parse_duration).transpose()?;

    let created = store
        .create_key(name, kind, assignments, expires_in)
        .await?;

    eprintln!("created API key:\n");
    eprintln!("  name:    {}", created.info.name);
    eprintln!("  kind:    {}", created.info.kind);
    eprintln!(
        "  grants:  {}",
        format_assignments(&created.info.assignments)
    );
    eprintln!("  prefix:  {}", created.info.prefix);
    if let Some(exp) = created.info.expires_at {
        eprintln!("  expires: {}", format_timestamp(&exp));
    } else {
        eprintln!("  expires: never");
    }
    eprintln!();
    // write_all avoids the println! formatter's non-zeroized String
    // intermediate, and the explicit flush ensures the token reaches the
    // descriptor even on broken-pipe or pre-exit teardown.
    let mut out = std::io::stdout().lock();
    out.write_all(created.plaintext_token.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    drop(out);
    eprintln!();
    eprintln!("WARNING: this token will not be shown again. store it securely.");

    Ok(())
}

/// List API keys.
pub async fn list(store: &KeyStore, all: bool) -> Result<(), AdminError> {
    let keys = store.list_keys(!all).await?;

    if keys.is_empty() {
        eprintln!("no keys found");
        return Ok(());
    }

    let table = format_keys_table(&keys);
    println!("{table}");
    eprintln!("{} key(s)", keys.len());

    Ok(())
}

/// Revoke a key by prefix, with `[y/N]` confirmation unless `--yes`.
///
/// Refuses to revoke when stdin is not a TTY and `--yes` was not passed —
/// scripts must opt in explicitly so an accidental `keys revoke <prefix>`
/// in a pipeline never silently nukes a key.
pub async fn revoke(store: &KeyStore, prefix: &str, yes: bool) -> Result<(), AdminError> {
    let info = store.get_key_by_prefix(prefix).await?;

    if !info.active {
        return Err(AdminError::AlreadyRevoked {
            prefix: info.prefix.clone(),
            name: info.name.clone(),
        });
    }

    eprintln!("  prefix:  {}", info.prefix);
    eprintln!("  name:    {}", info.name);
    eprintln!("  kind:    {}", info.kind);
    eprintln!("  grants:  {}", format_assignments(&info.assignments));
    eprintln!("  created: {}", format_timestamp(&info.created_at));

    if !yes {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err(AdminError::NonInteractive);
        }

        eprint!("\nrevoke this key? [y/N] ");
        std::io::stderr().flush()?;

        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;

        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            eprintln!("aborted");
            return Ok(());
        }
    }

    let revoked = store.revoke_key(prefix).await?;
    eprintln!("revoked key: {} ({})", revoked.prefix, revoked.name);
    Ok(())
}

/// Add a grant to an existing key.
pub async fn grant(
    store: &KeyStore,
    prefix: &str,
    assignment: &RoleAssignment,
) -> Result<(), AdminError> {
    store.grant_assignment(prefix, assignment).await?;
    eprintln!(
        "granted {}:{} to key {}",
        assignment.app, assignment.role, prefix
    );
    Ok(())
}

/// Parse a `--grant app:role` value into a [`RoleAssignment`].
///
/// Multiple colons are kept in the role half (`foo:super:admin` →
/// `(foo, super:admin)`) so role names can themselves be namespaced.
pub fn parse_grant(s: &str) -> Result<RoleAssignment, AdminError> {
    let (app, role) = s.split_once(':').ok_or_else(|| AdminError::InvalidGrant {
        input: s.to_owned(),
        reason: "expected APP:ROLE",
    })?;
    if app.is_empty() || role.is_empty() {
        return Err(AdminError::InvalidGrant {
            input: s.to_owned(),
            reason: "empty app or role",
        });
    }
    Ok(RoleAssignment {
        app: app.to_owned(),
        role: role.to_owned(),
    })
}

/// Render assignments as `"app1:role1, app2:role2"`, sorted by app, or
/// `"(none)"` if empty.
///
/// CLI-output flavour of [`fleet_auth::format_assignments`] — that one uses
/// `"none"` and comma-without-space for log fields, this one uses
/// `"(none)"` and comma-space for human reading. Sort matches fleet-auth's
/// for deterministic table output even when callers hand us an unsorted
/// slice (fleet-auth's `load_assignments` already orders by app, but
/// belt-and-braces keeps snapshot tests stable).
fn format_assignments(assignments: &[RoleAssignment]) -> String {
    if assignments.is_empty() {
        return "(none)".to_owned();
    }
    let mut sorted: Vec<&RoleAssignment> = assignments.iter().collect();
    sorted.sort_by(|a, b| a.app.cmp(&b.app));
    sorted
        .iter()
        .map(|a| format!("{}:{}", a.app, a.role))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render an [`ApiKeyInfo`] slice as a `comfy_table` block.
fn format_keys_table(keys: &[ApiKeyInfo]) -> comfy_table::Table {
    let mut table = comfy_table::Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    table.set_header(vec![
        "prefix",
        "name",
        "kind",
        "grants",
        "active",
        "created",
        "last used",
    ]);

    for key in keys {
        table.add_row(vec![
            key.prefix.clone(),
            key.name.clone(),
            key.kind.to_string(),
            format_assignments(&key.assignments),
            if key.active {
                "yes".to_owned()
            } else {
                "no".to_owned()
            },
            format_timestamp(&key.created_at),
            key.last_used
                .as_ref()
                .map_or_else(|| "never".to_owned(), format_timestamp),
        ]);
    }

    table
}

/// Format a `DateTime<Utc>` as `YYYY-MM-DD HH:MM:SS`.
fn format_timestamp(ts: &DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Parse a human-readable duration string (`"90d"`, `"24h"`, `"52w"`).
///
/// Units: `s`, `m`, `h`, `d`, `w`. Numeric overflow against `u64::MAX`
/// seconds is rejected with `duration too large`.
pub fn parse_duration(s: &str) -> Result<Duration, AdminError> {
    let original = s;
    let bad = |reason: &'static str| AdminError::InvalidDuration {
        input: original.to_owned(),
        reason,
    };

    if s == "never" {
        return Err(bad("omit --expires instead of 'never'"));
    }

    let s = s.trim();
    // `split_at` takes a BYTE index — slicing at `s.len() - 1` on a
    // multi-byte trailing char would panic on a non-char-boundary. Walk
    // the chars and slice on the codepoint start instead.
    let last_char_start = s.char_indices().next_back().ok_or_else(|| bad("empty"))?.0;
    let (num_str, unit) = s.split_at(last_char_start);
    if num_str.is_empty() {
        return Err(bad("missing numeric value"));
    }
    let num: u64 = num_str.parse().map_err(|_| bad("invalid numeric value"))?;

    let seconds = match unit {
        "s" => Some(num),
        "m" => num.checked_mul(60),
        "h" => num.checked_mul(3600),
        "d" => num.checked_mul(86_400),
        "w" => num.checked_mul(604_800),
        _ => return Err(bad("unknown duration unit (use s/m/h/d/w)")),
    }
    .ok_or_else(|| bad("duration too large"))?;

    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grant_valid() {
        let g = parse_grant("trawl:admin").unwrap();
        assert_eq!(g.app, "trawl");
        assert_eq!(g.role, "admin");
    }

    #[test]
    fn parse_grant_missing_colon() {
        assert!(parse_grant("trawladmin").is_err());
    }

    #[test]
    fn parse_grant_empty_app() {
        assert!(parse_grant(":admin").is_err());
    }

    #[test]
    fn parse_grant_empty_role() {
        assert!(parse_grant("trawl:").is_err());
    }

    #[test]
    fn parse_grant_multiple_colons_keeps_role_intact() {
        let g = parse_grant("trawl:super:admin").unwrap();
        assert_eq!(g.app, "trawl");
        assert_eq!(g.role, "super:admin");
    }

    #[test]
    fn parse_duration_days() {
        assert_eq!(
            parse_duration("90d").unwrap(),
            Duration::from_secs(90 * 86_400)
        );
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(
            parse_duration("24h").unwrap(),
            Duration::from_secs(24 * 3600)
        );
    }

    #[test]
    fn parse_duration_weeks() {
        assert_eq!(
            parse_duration("52w").unwrap(),
            Duration::from_secs(52 * 604_800)
        );
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
        let err = parse_duration("d").unwrap_err().to_string();
        assert!(err.contains("missing numeric"), "got: {err}");
    }

    #[test]
    fn parse_duration_rejects_overflow() {
        let err = parse_duration("999999999999999999w")
            .unwrap_err()
            .to_string();
        assert!(err.contains("too large"), "got: {err}");
    }

    #[test]
    fn parse_duration_multibyte_trailing_char_doesnt_panic() {
        // `日` is 3 bytes — naive `split_at(len-1)` would panic on a
        // non-char-boundary. Must come back as a clean error instead.
        let err = parse_duration("90日").unwrap_err().to_string();
        assert!(err.contains("unknown duration unit"), "got: {err}");
    }

    #[test]
    fn format_assignments_sorts_by_app() {
        let unsorted = vec![
            grant_for("zebra", "ro"),
            grant_for("alpha", "rw"),
            grant_for("mango", "admin"),
        ];
        assert_eq!(
            format_assignments(&unsorted),
            "alpha:rw, mango:admin, zebra:ro"
        );
    }

    #[test]
    fn parse_duration_rejects_never_literal() {
        let err = parse_duration("never").unwrap_err().to_string();
        assert!(err.contains("omit --expires"), "got: {err}");
    }

    fn grant_for(app: &str, role: &str) -> RoleAssignment {
        RoleAssignment {
            app: app.into(),
            role: role.into(),
        }
    }

    fn fixed_ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn test_keys() -> Vec<ApiKeyInfo> {
        vec![
            ApiKeyInfo {
                id: 1,
                prefix: "dGhpcyBp".into(),
                name: "web-frontend".into(),
                kind: PrincipalKind::Human,
                assignments: vec![grant_for("trawl", "analyst")],
                active: true,
                created_at: fixed_ts("2026-02-10T12:00:00+00:00"),
                expires_at: Some(fixed_ts("2026-05-11T12:00:00+00:00")),
                last_used: Some(fixed_ts("2026-02-10T14:30:00+00:00")),
                revoked_at: None,
            },
            ApiKeyInfo {
                id: 2,
                prefix: "YW5vdGhl".into(),
                name: "cli-readonly".into(),
                kind: PrincipalKind::Service,
                assignments: vec![grant_for("trawl", "reader")],
                active: true,
                created_at: fixed_ts("2026-02-09T08:00:00+00:00"),
                expires_at: None,
                last_used: None,
                revoked_at: None,
            },
            ApiKeyInfo {
                id: 3,
                prefix: "cmV2b2tl".into(),
                name: "old-key".into(),
                kind: PrincipalKind::Human,
                assignments: vec![
                    grant_for("trawl", "admin"),
                    grant_for("coastwatch", "siem_consumer"),
                ],
                active: false,
                created_at: fixed_ts("2026-01-01T00:00:00+00:00"),
                expires_at: None,
                last_used: Some(fixed_ts("2026-02-01T10:00:00+00:00")),
                revoked_at: Some(fixed_ts("2026-02-05T09:00:00+00:00")),
            },
        ]
    }

    #[test]
    fn snapshot_keys_table_all() {
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

    #[test]
    fn format_timestamp_strips_tz_to_seconds() {
        let ts = fixed_ts("2026-02-10T12:00:00+00:00");
        assert_eq!(format_timestamp(&ts), "2026-02-10 12:00:00");
    }

    #[test]
    fn format_assignments_renders_or_falls_back() {
        assert_eq!(format_assignments(&[]), "(none)");
        let a = vec![grant_for("trawl", "admin"), grant_for("cw", "ro")];
        // Sorted by app — see [`format_assignments`].
        assert_eq!(format_assignments(&a), "cw:ro, trawl:admin");
    }
}
