// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The trial's two keys, minted by `fleet-admin` inside the image.
//!
//! The CLI does not link fleet-auth and never reaches PostgreSQL from the
//! host, so every key and role operation is a `compose run` of the
//! `fleet-admin` service. This module builds those argument vectors and
//! parses what `fleet-admin` prints.
//!
//! The permission lists are literals (ADR-0045): a permission trawl adds
//! later never widens a trial silently. A test reads trawl-server's policy
//! and fails when the two lists stop covering every permission, so the
//! author of a new permission decides where it belongs.
//!
//! A key prefix identifies a key for revocation. It is never printed: it
//! has no `Display`, its `Debug` is redacted, and it enters an argument
//! vector only as a withheld argument.

use std::fmt;

use zeroize::Zeroizing;

use super::compose::Service;
use super::docker::Args;

/// The app namespace of every trawl permission in a Fleet role.
pub const APP: &str = "trawl";

/// `trial-operator`: every permission except `ingest`, as ADR-0045 lists
/// them.
pub const OPERATOR_PERMISSIONS: [&str; 9] = [
    "query",
    "schema_read",
    "validate",
    "saved_query",
    "export",
    "stream",
    "query_cancel",
    "server_manage",
    "schema_write",
];

/// `trial-ingest`: `ingest` only.
pub const INGEST_PERMISSIONS: [&str; 1] = ["ingest"];

/// The two trial keys. Each key holds one role of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialKey {
    Operator,
    Ingest,
}

impl TrialKey {
    pub const ALL: [Self; 2] = [Self::Operator, Self::Ingest];

    /// The key's name, and its role's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Operator => "trial-operator",
            Self::Ingest => "trial-ingest",
        }
    }

    /// The `fleet-admin keys create --kind` value.
    pub fn kind(self) -> &'static str {
        match self {
            Self::Operator => "human",
            Self::Ingest => "service",
        }
    }

    pub fn permissions(self) -> &'static [&'static str] {
        match self {
            Self::Operator => &OPERATOR_PERMISSIONS,
            Self::Ingest => &INGEST_PERMISSIONS,
        }
    }
}

/// `compose run` of the `fleet-admin` service with `rest` as its
/// arguments. `--no-deps`: PostgreSQL is already up when a key step runs,
/// and a one-shot step must not start or recreate anything else.
fn fleet_admin(rest: Args) -> Args {
    Args::new()
        .args(["run", "--rm", "--no-deps", "-T", Service::FleetAdmin.name()])
        .then(rest)
}

/// `fleet-admin migrate`: the Fleet schema, before any key exists.
pub fn migrate_args() -> Args {
    fleet_admin(Args::new().arg("migrate"))
}

/// `fleet-admin roles show <role>`: exits 0 when the role exists.
pub fn role_show_args(key: TrialKey) -> Args {
    fleet_admin(Args::new().args(["roles", "show", key.name()]))
}

/// `fleet-admin roles create` with the key's literal permissions.
pub fn role_create_args(key: TrialKey) -> Args {
    let mut args = Args::new().args(["roles", "create", "--name", key.name()]);
    for permission in key.permissions() {
        args = args.arg("--perm").arg(format!("{APP}:{permission}"));
    }
    fleet_admin(args)
}

/// `fleet-admin keys create`, with no `--expires`, so the key never
/// expires. Run it sealed: stdout is the token.
pub fn key_create_args(key: TrialKey) -> Args {
    fleet_admin(Args::new().args([
        "keys",
        "create",
        "--name",
        key.name(),
        "--kind",
        key.kind(),
        "--role",
        key.name(),
    ]))
}

/// `fleet-admin keys list`: the active keys, as [`parse_keys_table`]
/// reads them. The table holds prefixes, so never show its stdout.
pub fn keys_list_args() -> Args {
    fleet_admin(Args::new().args(["keys", "list"]))
}

/// `fleet-admin keys revoke <prefix> --yes`. The prefix is withheld from
/// every error, and `fleet-admin` echoes it on stderr, so run it sealed.
pub fn key_revoke_args(prefix: &KeyPrefix) -> Args {
    fleet_admin(
        Args::new()
            .args(["keys", "revoke"])
            .withheld(&prefix.0)
            .arg("--yes"),
    )
}

/// A key's operational prefix: 8 base64url characters.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyPrefix(String);

impl KeyPrefix {
    pub fn parse(text: &str) -> Option<Self> {
        (text.len() == 8
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .then(|| Self(text.to_owned()))
    }

    /// For comparison with the recorded prefix and for the state file.
    /// Never print it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for KeyPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyPrefix(<redacted>)")
    }
}

/// A token `keys create` printed: the exact stdout bytes, token and one
/// newline, which is what `operator.token` and `ingest.token` hold.
pub struct MintedToken {
    pub file_bytes: Zeroizing<Vec<u8>>,
    pub prefix: KeyPrefix,
}

impl fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MintedToken(<redacted>)")
    }
}

/// The Fleet token shape: `flt_` and 43 base64url characters.
const TOKEN_PREFIX: &str = "flt_";
const TOKEN_BODY_LEN: usize = 43;

/// The prefix of a token (`flt_` plus 43 base64url characters): the first
/// 8 characters of its body. `None` for anything else.
pub fn token_prefix(token: &str) -> Option<KeyPrefix> {
    let body = token.strip_prefix(TOKEN_PREFIX)?;
    let valid = body.len() == TOKEN_BODY_LEN
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !valid {
        return None;
    }
    KeyPrefix::parse(&body[..8])
}

impl MintedToken {
    /// Read `keys create` stdout, which must be one token and one newline.
    /// `None` otherwise; the caller reports that without the bytes.
    pub fn parse(stdout: &[u8]) -> Option<Self> {
        let line = stdout.strip_suffix(b"\n")?;
        let token = std::str::from_utf8(line).ok()?;
        let prefix = token_prefix(token)?;
        Some(Self {
            file_bytes: Zeroizing::new(stdout.to_vec()),
            prefix,
        })
    }
}

/// One row of `fleet-admin keys list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub prefix: KeyPrefix,
    pub name: String,
    pub kind: String,
    pub roles: Vec<String>,
    pub active: bool,
}

/// Why a `keys list` table was refused. No variant quotes the table: its
/// cells are prefixes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeysTableError {
    #[error(
        "`fleet-admin keys list` printed a table trawl does not recognize ({reason}, line {line})"
    )]
    Unrecognized { line: usize, reason: &'static str },
}

/// The header `fleet-admin keys list` prints, in order.
const HEADER: [&str; 7] = [
    "prefix",
    "name",
    "kind",
    "roles",
    "active",
    "created",
    "last used",
];

/// Parse the table `fleet-admin keys list` prints (comfy-table,
/// `UTF8_FULL` with round corners).
///
/// Strict: the frame, the header, and every cell are checked, and a row
/// that wrapped onto a second line is refused rather than guessed at.
/// Empty output is no keys (`fleet-admin` says so on stderr).
pub fn parse_keys_table(text: &str) -> Result<Vec<KeyRow>, KeysTableError> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.iter().all(|line| line.trim().is_empty()) {
        return Ok(Vec::new());
    }
    let refuse = |line: usize, reason| KeysTableError::Unrecognized {
        line: line + 1,
        reason,
    };
    let frame = |i: usize, first: char, last: char, reason| {
        let line = lines.get(i).copied().unwrap_or_default();
        if line.starts_with(first) && line.ends_with(last) {
            Ok(())
        } else {
            Err(refuse(i, reason))
        }
    };

    frame(0, '╭', '╮', "no top border")?;
    let header = cells(lines.get(1).copied().unwrap_or_default())
        .ok_or_else(|| refuse(1, "no header row"))?;
    if header != HEADER {
        return Err(refuse(1, "unexpected columns"));
    }
    frame(2, '╞', '╡', "no header separator")?;

    let mut rows = Vec::new();
    let mut i = 3;
    loop {
        let row = cells(lines.get(i).copied().unwrap_or_default())
            .ok_or_else(|| refuse(i, "not a table row"))?;
        rows.push(key_row(&row).map_err(|reason| refuse(i, reason))?);
        i += 1;
        match lines.get(i).and_then(|line| line.chars().next()) {
            Some('├') => {
                frame(i, '├', '┤', "broken row separator")?;
                i += 1;
            }
            Some('╰') => {
                frame(i, '╰', '╯', "broken bottom border")?;
                break;
            }
            _ => return Err(refuse(i, "a row continues onto another line")),
        }
    }
    if lines[i + 1..].iter().any(|line| !line.trim().is_empty()) {
        return Err(refuse(i + 1, "text after the table"));
    }
    Ok(rows)
}

/// The trimmed cells of `│ a ┆ b │`, or `None` when the line is not a row.
fn cells(line: &str) -> Option<Vec<&str>> {
    let inner = line.strip_prefix('│')?.strip_suffix('│')?;
    Some(inner.split('┆').map(str::trim).collect())
}

fn key_row(cells: &[&str]) -> Result<KeyRow, &'static str> {
    let [prefix, name, kind, roles, active, created, last_used] = cells else {
        return Err("wrong number of cells");
    };
    if prefix.is_empty() {
        return Err("a row continues onto another line");
    }
    let prefix = KeyPrefix::parse(prefix).ok_or("a prefix is not 8 base64url characters")?;
    if name.is_empty() {
        return Err("a key has no name");
    }
    if !matches!(*kind, "human" | "service") {
        return Err("a kind is neither human nor service");
    }
    let active = match *active {
        "yes" => true,
        "no" => false,
        _ => return Err("active is neither yes nor no"),
    };
    let timestamp = |s: &str| {
        s.len() == 19 && chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").is_ok()
    };
    if !timestamp(created) || !(timestamp(last_used) || *last_used == "never") {
        return Err("a timestamp is not YYYY-MM-DD HH:MM:SS");
    }
    let roles = if *roles == "(none)" {
        Vec::new()
    } else {
        roles.split(", ").map(str::to_owned).collect()
    };
    if roles.iter().any(String::is_empty) {
        return Err("an empty role name");
    }
    Ok(KeyRow {
        prefix,
        name: (*name).to_owned(),
        kind: (*kind).to_owned(),
        roles,
        active,
    })
}

/// The active keys named after `key`: the ones a rerun that lost the
/// token's plaintext revokes before minting again.
pub fn active_prefixes(rows: &[KeyRow], key: TrialKey) -> Vec<KeyPrefix> {
    rows.iter()
        .filter(|row| row.active && row.name == key.name())
        .map(|row| row.prefix.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// fleet-admin's own snapshot of `keys list --all`: the format this
    /// parser must keep reading.
    const KEYS_TABLE_SNAPSHOT: &str = include_str!(
        "../../../fleet-admin/src/commands/snapshots/fleet_admin__commands__keys__tests__snapshot_keys_table_all.snap"
    );

    /// The snapshot's body, after insta's front matter.
    fn snapshot_table() -> &'static str {
        let mut parts = KEYS_TABLE_SNAPSHOT.splitn(3, "---\n");
        parts.next();
        parts.next();
        parts.next().expect("an insta snapshot body")
    }

    #[test]
    fn parses_fleet_admins_own_table() {
        let rows = parse_keys_table(snapshot_table()).unwrap();
        let summary: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.prefix.as_str(),
                    r.name.as_str(),
                    r.kind.as_str(),
                    r.roles.clone(),
                    r.active,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "dGhpcyBp",
                    "web-frontend",
                    "human",
                    vec!["trawl-analyst".to_owned()],
                    true
                ),
                (
                    "YW5vdGhl",
                    "cli-readonly",
                    "service",
                    vec!["trawl-reader".to_owned()],
                    true
                ),
                (
                    "cmV2b2tl",
                    "old-key",
                    "human",
                    vec![
                        "coastwatch-siem_consumer".to_owned(),
                        "trawl-admin".to_owned()
                    ],
                    false
                ),
            ]
        );
    }

    #[test]
    fn empty_output_is_no_keys() {
        assert_eq!(parse_keys_table(""), Ok(Vec::new()));
        assert_eq!(parse_keys_table("\n"), Ok(Vec::new()));
    }

    #[test]
    fn a_wrapped_row_is_refused() {
        let mut lines: Vec<&str> = snapshot_table().lines().collect();
        let row = lines
            .iter()
            .position(|line| line.starts_with("│ YW5vdGhl"))
            .unwrap();
        lines.insert(
            row + 1,
            "│          ┆ -wrapped     ┆         ┆                                       ┆        ┆                     ┆                     │",
        );
        let err = parse_keys_table(&lines.join("\n")).unwrap_err();
        assert!(
            err.to_string().contains("continues onto another line"),
            "{err}"
        );
    }

    #[test]
    fn malformed_tables_are_refused_without_quoting_cells() {
        let table = snapshot_table();
        let cases = [
            ("no keys found\n".to_owned(), "no top border"),
            (
                table.replacen("last used", "expires  ", 1),
                "unexpected columns",
            ),
            (
                table.replacen("┆ human   ┆", "┆ robot   ┆", 1),
                "neither human nor service",
            ),
            (
                table.replacen("┆ yes    ┆", "┆ maybe  ┆", 1),
                "neither yes nor no",
            ),
            (table.replacen("dGhpcyBp", "dGhpcy/p", 1), "8 base64url"),
            (
                table.replacen("2026-02-10 12:00:00", "2026-02-10 12:00:0x", 1),
                "timestamp",
            ),
            (table.replace('╯', "x"), "broken bottom border"),
            (format!("{table}extra\n"), "text after the table"),
            (
                table.lines().take(4).collect::<Vec<_>>().join("\n"),
                "continues onto another line",
            ),
        ];
        for (text, reason) in cases {
            let err = parse_keys_table(&text).unwrap_err();
            let message = err.to_string();
            assert!(message.contains(reason), "{reason}: {message}");
            for prefix in ["dGhpcyBp", "YW5vdGhl", "cmV2b2tl"] {
                assert!(!message.contains(prefix), "{message}");
            }
        }
    }

    #[test]
    fn active_prefixes_select_by_exact_name_and_activity() {
        let rows = vec![
            KeyRow {
                prefix: KeyPrefix::parse("AAAAAAAA").unwrap(),
                name: "trial-operator".into(),
                kind: "human".into(),
                roles: vec!["trial-operator".into()],
                active: true,
            },
            KeyRow {
                prefix: KeyPrefix::parse("BBBBBBBB").unwrap(),
                name: "trial-operator".into(),
                kind: "human".into(),
                roles: vec![],
                active: false,
            },
            KeyRow {
                prefix: KeyPrefix::parse("CCCCCCCC").unwrap(),
                name: "trial-operator-2".into(),
                kind: "human".into(),
                roles: vec![],
                active: true,
            },
            KeyRow {
                prefix: KeyPrefix::parse("DDDDDDDD").unwrap(),
                name: "trial-ingest".into(),
                kind: "service".into(),
                roles: vec![],
                active: true,
            },
        ];
        let got: Vec<_> = active_prefixes(&rows, TrialKey::Operator)
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect();
        assert_eq!(got, ["AAAAAAAA"]);
        let got: Vec<_> = active_prefixes(&rows, TrialKey::Ingest)
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect();
        assert_eq!(got, ["DDDDDDDD"]);
    }

    #[test]
    fn a_minted_token_is_exactly_one_token_and_a_newline() {
        let token = "flt_dGhpcyBpcyBhIHRlc3QgdG9rZW4gYm9keSAxMjM0NTY";
        assert_eq!(token.len(), 4 + TOKEN_BODY_LEN);
        let stdout = format!("{token}\n");
        let minted = MintedToken::parse(stdout.as_bytes()).unwrap();
        assert_eq!(minted.prefix.as_str(), "dGhpcyBp");
        assert_eq!(&minted.file_bytes[..], stdout.as_bytes());
        assert!(!format!("{minted:?}").contains("dGhpcyBp"));

        for bad in [
            token.to_owned(),
            format!("{token}\n\n"),
            format!(" {token}\n"),
            format!("{}\n", &token[..token.len() - 1]),
            format!("flt-{}\n", &token[4..]),
            format!("{}!\n", &token[..token.len() - 1]),
            String::new(),
        ] {
            assert!(MintedToken::parse(bad.as_bytes()).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_prefix_is_never_shown() {
        let prefix = KeyPrefix::parse("pfx12345").unwrap();
        assert!(!format!("{prefix:?}").contains("pfx12345"));
        let args = key_revoke_args(&prefix);
        assert!(!args.display().contains("pfx12345"), "{}", args.display());
        assert!(!format!("{args:?}").contains("pfx12345"));
        assert!(args.argv().iter().any(|a| a == "pfx12345"));
    }

    #[test]
    fn argv_builders_match_fleet_admins_interface() {
        let shown = |args: Args| args.display();
        assert_eq!(
            shown(migrate_args()),
            "docker run --rm --no-deps -T fleet-admin migrate"
        );
        assert_eq!(
            shown(role_show_args(TrialKey::Ingest)),
            "docker run --rm --no-deps -T fleet-admin roles show trial-ingest"
        );
        assert_eq!(
            shown(role_create_args(TrialKey::Operator)),
            "docker run --rm --no-deps -T fleet-admin roles create --name trial-operator \
             --perm trawl:query --perm trawl:schema_read --perm trawl:validate \
             --perm trawl:saved_query --perm trawl:export --perm trawl:stream \
             --perm trawl:query_cancel --perm trawl:server_manage --perm trawl:schema_write"
        );
        assert_eq!(
            shown(role_create_args(TrialKey::Ingest)),
            "docker run --rm --no-deps -T fleet-admin roles create --name trial-ingest \
             --perm trawl:ingest"
        );
        assert_eq!(
            shown(key_create_args(TrialKey::Operator)),
            "docker run --rm --no-deps -T fleet-admin keys create --name trial-operator \
             --kind human --role trial-operator"
        );
        assert_eq!(
            shown(key_create_args(TrialKey::Ingest)),
            "docker run --rm --no-deps -T fleet-admin keys create --name trial-ingest \
             --kind service --role trial-ingest"
        );
        assert!(!shown(key_create_args(TrialKey::Operator)).contains("--expires"));
        assert_eq!(
            shown(keys_list_args()),
            "docker run --rm --no-deps -T fleet-admin keys list"
        );
    }

    /// Every permission literal in `Permission::as_str`, read from
    /// trawl-server's policy source.
    fn server_permissions() -> BTreeSet<&'static str> {
        const POLICY: &str = include_str!("../../../trawl-server/src/policy.rs");
        let start = POLICY
            .find("pub fn as_str(self) -> &'static str")
            .expect("Permission::as_str in policy.rs");
        let body = &POLICY[start..];
        let body = &body[..body.find("\n    }\n").expect("the end of as_str")];
        let found: BTreeSet<&str> = body
            .lines()
            .filter_map(|line| line.split_once("=> \"")?.1.strip_suffix("\","))
            .collect();
        assert!(found.len() >= 10, "read too few permissions: {found:?}");
        found
    }

    #[test]
    fn every_operator_literal_is_a_real_permission_and_ingest_is_not_one() {
        let server = server_permissions();
        for permission in OPERATOR_PERMISSIONS.iter().chain(&INGEST_PERMISSIONS) {
            assert!(
                server.contains(permission),
                "{permission} is not a trawl permission"
            );
        }
        assert!(!OPERATOR_PERMISSIONS.contains(&"ingest"));
        let unique: BTreeSet<_> = OPERATOR_PERMISSIONS.iter().collect();
        assert_eq!(unique.len(), OPERATOR_PERMISSIONS.len());
    }

    #[test]
    fn the_trial_keys_cover_every_permission() {
        let server = server_permissions();
        let trial: BTreeSet<&str> = OPERATOR_PERMISSIONS
            .iter()
            .chain(&INGEST_PERMISSIONS)
            .copied()
            .collect();
        let uncovered: Vec<_> = server.difference(&trial).collect();
        assert!(
            uncovered.is_empty(),
            "trawl-server has permissions no trial key holds: {uncovered:?}. The trial's \
             lists are literal on purpose, so a new permission never widens a trial \
             silently: decide under ADR-0045 whether `trial-operator` or `trial-ingest` \
             holds it, then add it to OPERATOR_PERMISSIONS or INGEST_PERMISSIONS"
        );
    }

    #[test]
    fn names_and_kinds_are_the_adr_s() {
        assert_eq!(TrialKey::Operator.name(), "trial-operator");
        assert_eq!(TrialKey::Operator.kind(), "human");
        assert_eq!(TrialKey::Ingest.name(), "trial-ingest");
        assert_eq!(TrialKey::Ingest.kind(), "service");
        assert_eq!(TrialKey::ALL.len(), 2);
    }
}
