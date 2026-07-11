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

/// API key prefix as shown in `keys list` — 8 base64url chars.
///
/// Constructed via [`KeyPrefix::parse`] (clap `value_parser`) so malformed
/// values are rejected at parse time, not at the SQL boundary.
#[derive(Debug, Clone)]
pub struct KeyPrefix(String);

impl KeyPrefix {
    pub fn parse(s: &str) -> Result<Self, AdminError> {
        let bad = |reason: &'static str| AdminError::InvalidKeyPrefix {
            input: s.to_owned(),
            reason,
        };
        if s.len() != 8 {
            return Err(bad("key prefix must be 8 characters"));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(bad("key prefix must be base64url (A-Z, a-z, 0-9, _, -)"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for KeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Create a new API key.
///
/// Emits the plaintext token to stdout exactly once. All metadata
/// (name, kind, grants, prefix, expiry) goes to stderr.
pub async fn create(
    store: &KeyStore,
    name: &str,
    kind: PrincipalKind,
    assignments: &[RoleAssignment],
    expires: Option<Duration>,
) -> Result<(), AdminError> {
    let created = store.create_key(name, kind, assignments, expires).await?;

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
/// Refuses to revoke unless both stdin and stderr are TTYs or `--yes` was
/// passed — scripts must opt in explicitly so an accidental
/// `keys revoke <prefix>` in a pipeline never silently nukes a key.
pub async fn revoke(store: &KeyStore, prefix: &KeyPrefix, yes: bool) -> Result<(), AdminError> {
    let info = store.get_key_by_prefix(prefix.as_str()).await?;

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

    if !yes && !confirm_or_refuse("revoke this key?")? {
        return Ok(());
    }

    let revoked = store.revoke_key(prefix.as_str()).await?;
    eprintln!("revoked key: {} ({})", revoked.prefix, revoked.name);
    Ok(())
}

/// Pure `[y/N]` prompt loop, factored out for unit testing.
///
/// Writes `"\n{question} [y/N] "` and reads one line. Returns `Ok(true)`
/// only for `y`/`yes` (case-insensitive). EOF (a zero-byte read) is reported
/// to the operator before falling through to `Ok(false)` — without that,
/// "stdin closed mid-prompt" looks identical to "user typed n" in logs.
pub fn confirm_prompt<R: BufRead, W: Write>(
    question: &str,
    reader: &mut R,
    writer: &mut W,
) -> std::io::Result<bool> {
    write!(writer, "\n{question} [y/N] ")?;
    writer.flush()?;

    let mut answer = String::new();
    let n = reader.read_line(&mut answer)?;
    if n == 0 {
        writeln!(writer, "aborted: stdin closed before answer")?;
        return Ok(false);
    }

    let answer = answer.trim();
    if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
        Ok(true)
    } else {
        writeln!(writer, "aborted")?;
        Ok(false)
    }
}

/// TTY-gated `[y/N]` confirmation shared by confirmable subcommands.
///
/// Refuses with [`AdminError::NonInteractive`] unless both stdin and stderr
/// are TTYs, so a piped `keys revoke`/`revoke-grant` never proceeds without an
/// explicit `--yes`. Otherwise locks the descriptors and defers to
/// [`confirm_prompt`], returning whether the operator confirmed. Centralizes
/// the non-interactive-refusal invariant so new confirmable subcommands don't
/// hand-roll their own copy.
fn confirm_or_refuse(question: &str) -> Result<bool, AdminError> {
    use std::io::IsTerminal as _;
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(AdminError::NonInteractive);
    }

    let mut writer = stderr.lock();
    let mut reader = stdin.lock();
    confirm_prompt(question, &mut reader, &mut writer).map_err(Into::into)
}

/// Revoke a key's grant on an app, with `[y/N]` confirmation unless `--yes`.
///
/// Same interactivity contract as [`revoke`]: refuses to proceed unless both
/// stdin and stderr are TTYs or `--yes` was passed. No preflight read — the prompt
/// is built from the arguments, and a missing grant surfaces as the store's
/// `GrantNotFound` after confirmation (no read-then-delete race).
pub async fn revoke_grant(
    store: &KeyStore,
    prefix: &KeyPrefix,
    app: &str,
    yes: bool,
) -> Result<(), AdminError> {
    if !yes {
        let question = format!("revoke grant for app {app} on key {prefix}?");
        if !confirm_or_refuse(&question)? {
            return Ok(());
        }
    }

    store.revoke_assignment(prefix.as_str(), app).await?;
    eprintln!("revoked grant for app {app} on key {prefix}");
    Ok(())
}

/// Change a key's kind (human <-> service).
///
/// Non-interactive — the change is reversible and the result is
/// self-reported. Revoked keys are refused store-side
/// (`AuthError::KeyRevoked`).
pub async fn retype(
    store: &KeyStore,
    prefix: &KeyPrefix,
    kind: PrincipalKind,
) -> Result<(), AdminError> {
    let info = store.retype_key(prefix.as_str(), kind).await?;
    eprintln!(
        "retyped key {} ({}) as {}",
        info.prefix, info.name, info.kind
    );
    Ok(())
}

/// Parse a `revoke-grant` app argument, extracting the app half.
///
/// Accepts either a bare `app` or the `app:role` form `keys grant` takes —
/// the role half is ignored, since grants are keyed by `(key, app)`. First
/// colon wins, mirroring trawl-admin (`foo:super:admin` → `foo`). An empty
/// app (from `""` or `:role`) is rejected at the clap boundary rather than
/// surviving to the store and garbling the confirmation prompt.
pub fn parse_revoke_grant_app(s: &str) -> Result<String, AdminError> {
    let app = s.split_once(':').map_or(s, |(app, _)| app);
    if app.is_empty() {
        return Err(AdminError::InvalidGrant {
            input: s.to_owned(),
            reason: "empty app",
        });
    }
    Ok(app.to_owned())
}

/// Add a grant to an existing key.
pub async fn grant(
    store: &KeyStore,
    prefix: &KeyPrefix,
    assignment: &RoleAssignment,
) -> Result<(), AdminError> {
    store.grant_assignment(prefix.as_str(), assignment).await?;
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
    let bad = |reason: &'static str| AdminError::InvalidGrant {
        input: s.to_owned(),
        reason,
    };
    if s.chars().any(char::is_whitespace) {
        return Err(bad("whitespace not allowed in APP:ROLE"));
    }
    let (app, role) = s.split_once(':').ok_or_else(|| bad("expected APP:ROLE"))?;
    if app.is_empty() || role.is_empty() {
        return Err(bad("empty app or role"));
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
/// `"none"` and comma-without-space for log fields; this one uses
/// `"(none)"` and comma-space for human reading. Sorted independently of
/// the input so callers can hand us an arbitrarily-ordered slice and still
/// get deterministic output.
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
    fn parse_revoke_grant_app_bare_app() {
        assert_eq!(parse_revoke_grant_app("trawl").unwrap(), "trawl");
    }

    #[test]
    fn parse_revoke_grant_app_strips_role() {
        assert_eq!(parse_revoke_grant_app("trawl:admin").unwrap(), "trawl");
    }

    #[test]
    fn parse_revoke_grant_app_first_colon_semantics() {
        assert_eq!(
            parse_revoke_grant_app("trawl:super:admin").unwrap(),
            "trawl"
        );
    }

    #[test]
    fn parse_revoke_grant_app_rejects_empty_input() {
        assert!(parse_revoke_grant_app("").is_err());
    }

    #[test]
    fn parse_revoke_grant_app_rejects_empty_app_before_colon() {
        assert!(parse_revoke_grant_app(":admin").is_err());
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

    fn run_prompt(question: &str, input: &str) -> (bool, String) {
        let mut reader = std::io::BufReader::new(input.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let result = confirm_prompt(question, &mut reader, &mut writer).expect("prompt io");
        (result, String::from_utf8(writer).expect("utf8"))
    }

    #[test]
    fn confirm_prompt_accepts_y_variants() {
        for ans in ["y\n", "Y\n", "yes\n", "YES\n", "Yes\n", "  y  \n"] {
            let (accepted, _) = run_prompt("revoke this key?", ans);
            assert!(accepted, "{ans:?} should accept");
        }
    }

    #[test]
    fn confirm_prompt_rejects_n_and_blank() {
        for ans in ["n\n", "N\n", "no\n", "\n", "  \n", "maybe\n"] {
            let (accepted, out) = run_prompt("revoke this key?", ans);
            assert!(!accepted, "{ans:?} should reject");
            assert!(out.contains("aborted"), "expected 'aborted' in {out:?}");
            assert!(
                !out.contains("stdin closed"),
                "non-EOF should not mention stdin closure"
            );
        }
    }

    #[test]
    fn confirm_prompt_eof_is_distinguishable_from_no() {
        let (accepted, out) = run_prompt("revoke this key?", "");
        assert!(!accepted);
        assert!(
            out.contains("stdin closed before answer"),
            "expected EOF marker in {out:?}"
        );
    }

    #[test]
    fn confirm_prompt_writes_caller_question_before_reading() {
        let (_, out) = run_prompt("revoke this key?", "n\n");
        assert!(
            out.contains("revoke this key? [y/N]"),
            "missing prompt text in {out:?}"
        );
        let (_, out) = run_prompt("revoke grant for app trawl on key aaaabbbb?", "n\n");
        assert!(
            out.contains("revoke grant for app trawl on key aaaabbbb? [y/N]"),
            "question must come from the caller, got {out:?}"
        );
    }

    #[test]
    fn format_assignments_renders_or_falls_back() {
        assert_eq!(format_assignments(&[]), "(none)");
        let a = vec![grant_for("trawl", "admin"), grant_for("cw", "ro")];
        // Sorted by app — see [`format_assignments`].
        assert_eq!(format_assignments(&a), "cw:ro, trawl:admin");
    }
}
