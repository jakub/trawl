// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a `trawld --doctor` row may say, and the two renderings of the
//! report (#269, D18).
//!
//! A row is a [`Row`], and every string in it is built here from parts
//! that cannot carry a value the report must not show: `&'static str`
//! literals, integers, a [`SelectedPath`] the operator chose, the running
//! user's [`UserName`], a [`HealthKey`], and a [`QuotedValue`] from the
//! closed health vocabulary. No constructor takes a `String`, a `Display`,
//! or an error, so the `Display` or `Debug` of a database, OS, TLS or HTTP
//! error, a database URL, a certificate name, a catalog identifier, or a
//! listener address has no way into a row. The runner in [`super`] turns
//! rows into [`trawl_api::doctor::Check`]s; nothing else builds one.

use std::io::{self, Write};
use std::path::Path;

use trawl_api::doctor::health::{is_health_key, is_quotable_value};
use trawl_api::doctor::{Check, Outcome, Report, Verdict};

use super::ServerCheck;

/// Output format for the report, named as `trawl doctor` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// One line per check.
    Table,
    /// The versioned JSON report.
    Json,
}

impl Format {
    /// The format when `--format` is absent: table on a terminal, JSON
    /// otherwise, as `trawl doctor` picks.
    #[must_use]
    pub fn detect() -> Self {
        if io::IsTerminal::is_terminal(&io::stdout()) {
            Self::Table
        } else {
            Self::Json
        }
    }
}

/// Which operator selection a [`SelectedPath`] is. ADR-0047 lets a report
/// name the configuration and credential files the operator selected, and
/// no other path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// The file `--config` names.
    ConfigFlag,
    /// `[server] tls_cert_path`.
    TlsCertPath,
    /// `[server] tls_key_path`.
    TlsKeyPath,
    /// The file `PGPASSFILE` names.
    PgPassFile,
}

/// A path the operator selected, as the report may show it: control and
/// bidirectional-formatting characters are replaced by `?`.
#[derive(Clone, PartialEq, Eq)]
pub struct SelectedPath {
    selection: Selection,
    shown: String,
}

impl SelectedPath {
    /// The path `selection` names.
    #[must_use]
    pub fn new(selection: Selection, path: &Path) -> Self {
        let shown = path
            .to_string_lossy()
            .chars()
            .map(|c| if is_hidden(c) { '?' } else { c })
            .collect();
        Self { selection, shown }
    }

    /// Which selection this is.
    #[must_use]
    pub const fn selection(&self) -> Selection {
        self.selection
    }

    /// The path as the report shows it.
    #[must_use]
    pub fn as_shown(&self) -> &str {
        &self.shown
    }
}

impl std::fmt::Debug for SelectedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectedPath")
            .field("selection", &self.selection)
            .field("shown", &self.shown)
            .finish()
    }
}

/// Characters a path or name may not bring into the report: control
/// characters, and the bidirectional and invisible formatting characters
/// that can reorder or hide what a terminal shows.
fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// The running user's name, as the password database returned it, when it
/// is a plain account name: `[A-Za-z0-9._-]`, then up to 31 more of those
/// or `$`. Any other name is not shown; the uid still is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserName(String);

impl UserName {
    /// `name`, when it is a plain account name.
    #[must_use]
    pub fn new(name: &str) -> Option<Self> {
        let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
        let bytes = name.as_bytes();
        let ok = (1..=32).contains(&bytes.len())
            && plain(bytes[0])
            && bytes[1..].iter().all(|&b| plain(b) || b == b'$');
        ok.then(|| Self(name.to_owned()))
    }
}

/// A health check name the report may show ([`is_health_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthKey(String);

impl HealthKey {
    /// The last part of the one row that stands for every check name that
    /// is not an identifier. It starts with `_`, which no check name may,
    /// so it cannot collide with a server's check.
    pub const INVALID: &'static str = "_invalid";

    /// `name`, when the report may show it.
    #[must_use]
    pub fn new(name: &str) -> Option<Self> {
        is_health_key(name).then(|| Self(name.to_owned()))
    }

    /// The key of the row for names that are not identifiers.
    #[must_use]
    pub fn invalid() -> Self {
        Self(Self::INVALID.to_owned())
    }
}

/// A health value the report may quote ([`is_quotable_value`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotedValue(String);

impl QuotedValue {
    /// `value`, when the report may quote it.
    #[must_use]
    pub fn new(value: &str) -> Option<Self> {
        is_quotable_value(value).then(|| Self(value.to_owned()))
    }
}

/// Text for a row's detail, source or next action, built only from parts
/// the report may show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text(String);

impl Text {
    /// Start with a literal.
    #[must_use]
    pub fn new(literal: &'static str) -> Self {
        Self(literal.to_owned())
    }

    /// Append a literal.
    #[must_use]
    pub fn lit(mut self, literal: &'static str) -> Self {
        self.0.push_str(literal);
        self
    }

    /// Append an integer.
    #[must_use]
    pub fn int(mut self, n: impl Into<i128>) -> Self {
        self.0.push_str(&n.into().to_string());
        self
    }

    /// Append a path the operator selected.
    #[must_use]
    pub fn path(mut self, path: &SelectedPath) -> Self {
        self.0.push_str(path.as_shown());
        self
    }

    /// Append the running user's name.
    #[must_use]
    pub fn user(mut self, name: &UserName) -> Self {
        self.0.push_str(&name.0);
        self
    }

    /// Append a health check name.
    #[must_use]
    pub fn key(mut self, key: &HealthKey) -> Self {
        self.0.push_str(&key.0);
        self
    }

    /// Append a health value from the closed vocabulary.
    #[must_use]
    pub fn quoted(mut self, value: &QuotedValue) -> Self {
        self.0.push_str(&value.0);
        self
    }

    /// The text as the report shows it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The text as the report shows it, owned.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// One row of the report, before the runner places it.
///
/// A row names its [`ServerCheck`]; a per-key health row also names its
/// [`HealthKey`]. `blocked_by` is not settable: only the runner writes a
/// row for a check whose prerequisite did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    check: ServerCheck,
    key: Option<HealthKey>,
    outcome: Outcome,
    reason: Option<&'static str>,
    detail: Option<Text>,
    source: Option<Text>,
    next: Option<Text>,
    blocked_by: Option<ServerCheck>,
}

impl Row {
    fn new(check: ServerCheck, outcome: Outcome, reason: Option<&'static str>) -> Self {
        Self {
            check,
            key: None,
            outcome,
            reason,
            detail: None,
            source: None,
            next: None,
            blocked_by: None,
        }
    }

    /// The assertion held.
    #[must_use]
    pub fn complete(check: ServerCheck) -> Self {
        Self::new(check, Outcome::Complete, None)
    }

    /// The assertion held, for a reason that is part of the contract, such
    /// as [`trawl_api::doctor::reason::WILL_INITIALIZE`].
    #[must_use]
    pub fn complete_because(check: ServerCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::Complete, Some(reason))
    }

    /// Evidence against the assertion, said as a fixed sentence or code.
    #[must_use]
    pub fn failed(check: ServerCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::Failed, Some(reason))
    }

    /// The doctor could not look; `reason` is a code from
    /// [`trawl_api::doctor::reason`].
    #[must_use]
    pub fn not_sampled(check: ServerCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::NotSampled, Some(reason))
    }

    /// The operator did not select what the check needs.
    #[must_use]
    pub fn not_configured(check: ServerCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::NotConfigured, Some(reason))
    }

    /// A per-key row of `check`, such as `server.listener.health.duckdb`.
    #[must_use]
    pub fn for_key(
        check: ServerCheck,
        key: HealthKey,
        outcome: Outcome,
        reason: Option<&'static str>,
    ) -> Self {
        Self {
            key: Some(key),
            ..Self::new(check, outcome, reason)
        }
    }

    /// The row for a check whose prerequisite `by` did not complete. Only
    /// the runner writes one.
    pub(super) fn blocked(check: ServerCheck, by: ServerCheck) -> Self {
        Self {
            blocked_by: Some(by),
            ..Self::new(
                check,
                Outcome::NotSampled,
                Some(trawl_api::doctor::reason::BLOCKED),
            )
        }
    }

    /// This row as a root run reports it, when the check asserts access:
    /// a `complete` becomes `not_sampled`, reason `ran_as_root`, keeping
    /// what it observed. Only the runner applies it.
    pub(super) fn into_root_run(self) -> Self {
        if self.outcome != Outcome::Complete {
            return self;
        }
        Self {
            outcome: Outcome::NotSampled,
            reason: Some(trawl_api::doctor::reason::RAN_AS_ROOT),
            next: Some(Text::new(
                "rerun as the service user to check what it can read and write",
            )),
            ..self
        }
    }

    /// Whether this is a per-key row.
    #[must_use]
    pub const fn is_keyed(&self) -> bool {
        self.key.is_some()
    }

    /// Set the observed, sanitized facts.
    #[must_use]
    pub fn detail(mut self, detail: Text) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Name the setting or input the check read.
    #[must_use]
    pub fn source(mut self, source: Text) -> Self {
        self.source = Some(source);
        self
    }

    /// Say what the operator should do next.
    #[must_use]
    pub fn next(mut self, next: Text) -> Self {
        self.next = Some(next);
        self
    }

    /// The check this row reports.
    #[must_use]
    pub const fn check(&self) -> ServerCheck {
        self.check
    }

    /// The row's outcome.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// The row's reason.
    #[must_use]
    pub const fn reason(&self) -> Option<&'static str> {
        self.reason
    }

    /// The report's form of this row.
    pub(super) fn into_check(self) -> Check {
        let id = match &self.key {
            Some(key) => format!("{}.{}", self.check.id(), key.0),
            None => self.check.id().to_owned(),
        };
        Check {
            id,
            outcome: self.outcome,
            reason: self.reason.map(str::to_owned),
            detail: self.detail.map(|text| text.0),
            source: self.source.map(|text| text.0),
            blocked_by: self.blocked_by.map(|by| by.id().to_owned()),
            next_action: self.next.map(|text| text.0),
        }
    }
}

/// Render the report once: JSON is the report serialized, text is one line
/// per check, then the notes and the verdict, as `trawl doctor` prints them.
pub fn render(report: &Report, format: Format, out: &mut impl Write) -> io::Result<()> {
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut *out, report).map_err(io::Error::other)?;
            writeln!(out)
        }
        Format::Table => {
            writeln!(out, "target: this host ({})", report.target().source)?;
            let width = report
                .checks()
                .iter()
                .map(|check| check.id.len())
                .max()
                .unwrap_or(0);
            for check in report.checks() {
                writeln!(out, "{}", check_line(check, width))?;
            }
            for note in report.notes() {
                writeln!(out, "note: {note}")?;
            }
            writeln!(
                out,
                "verdict: {} (exit {})",
                verdict_name(report.verdict()),
                report.verdict().exit_code()
            )
        }
    }
}

/// One text line: id, outcome, then reason, detail, source, what blocked
/// it, and the next action, each when present.
fn check_line(check: &Check, width: usize) -> String {
    let outcome = match check.outcome {
        Outcome::Complete => "complete",
        Outcome::Failed => "failed",
        Outcome::NotConfigured => "not_configured",
        Outcome::NotSampled => "not_sampled",
    };
    let mut parts: Vec<String> = Vec::new();
    parts.extend(check.reason.clone());
    parts.extend(check.detail.clone());
    parts.extend(
        check
            .source
            .as_ref()
            .map(|source| format!("source: {source}")),
    );
    parts.extend(
        check
            .blocked_by
            .as_ref()
            .map(|by| format!("blocked by {by}")),
    );
    parts.extend(
        check
            .next_action
            .as_ref()
            .map(|next| format!("next: {next}")),
    );
    let line = format!("{:<width$}  {outcome:<14}  {}", check.id, parts.join("; "));
    line.trim_end().to_owned()
}

const fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
        Verdict::Incomplete => "incomplete",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selected_path_shows_no_control_characters() {
        let path = SelectedPath::new(
            Selection::ConfigFlag,
            Path::new("/etc/trawl/\u{1b}[2Jx\u{202e}.toml"),
        );
        assert_eq!(path.as_shown(), "/etc/trawl/?[2Jx?.toml");
        assert_eq!(path.selection(), Selection::ConfigFlag);
    }

    #[test]
    fn only_plain_user_names_are_shown() {
        for name in ["trawl", "systemd-network", "a.b_c", "machine$", "0"] {
            assert!(UserName::new(name).is_some(), "{name}");
        }
        for name in ["", "$x", "a b", "evil\u{1b}[2J", &"a".repeat(33), "ünï"] {
            assert!(UserName::new(name).is_none(), "{name:?}");
        }
    }

    #[test]
    fn text_joins_only_what_it_is_given() {
        let path = SelectedPath::new(Selection::TlsCertPath, Path::new("/tls/cert.pem"));
        let user = UserName::new("trawl").unwrap();
        let text = Text::new("uid ")
            .int(1000_u32)
            .lit(" (")
            .user(&user)
            .lit("); ")
            .path(&path);
        assert_eq!(text.as_str(), "uid 1000 (trawl); /tls/cert.pem");
        assert!(HealthKey::new("Bad-Key").is_none());
        assert!(QuotedValue::new("Weird Value").is_none());
        let key = HealthKey::new("duckdb").unwrap();
        let value = QuotedValue::new("recovering").unwrap();
        assert_eq!(
            Text::new("").key(&key).lit("=").quoted(&value).as_str(),
            "duckdb=recovering"
        );
    }

    #[test]
    fn a_row_becomes_a_check_with_its_id() {
        let row = Row::for_key(
            ServerCheck::ListenerHealth,
            HealthKey::new("duckdb").unwrap(),
            Outcome::Failed,
            Some("reported error"),
        )
        .next(Text::new("read trawld's log"));
        let check = row.into_check();
        assert_eq!(check.id, "server.listener.health.duckdb");
        assert_eq!(check.outcome, Outcome::Failed);
        assert_eq!(check.reason.as_deref(), Some("reported error"));
        assert_eq!(check.next_action.as_deref(), Some("read trawld's log"));
        assert_eq!(check.blocked_by, None);

        let blocked = Row::blocked(ServerCheck::AppSchema, ServerCheck::AppConnect).into_check();
        assert_eq!(blocked.id, "server.app.schema");
        assert_eq!(blocked.reason.as_deref(), Some("blocked"));
        assert_eq!(blocked.blocked_by.as_deref(), Some("server.app.connect"));
        assert_eq!(
            Row::for_key(
                ServerCheck::ListenerHealth,
                HealthKey::invalid(),
                Outcome::Failed,
                None
            )
            .into_check()
            .id,
            "server.listener.health._invalid"
        );
    }
}
