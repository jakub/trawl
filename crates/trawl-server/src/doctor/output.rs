// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a `trawld --doctor` row may say, and the two renderings of the report.
//! No constructor takes a `String`, a `Display`, or an error, so the `Display`
//! or `Debug` of an error has no way into a row.

use std::io::{self, Write};
use std::path::Path;

use trawl_api::doctor::{Check, Outcome, Report, Verdict};

use super::ServerCheck;
use crate::env_dirs;
use crate::handlers::HEALTH_CHECK_NAMES;

/// Output format for the report, named as `trawl doctor` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// One line per check.
    Table,
    /// The versioned JSON report.
    Json,
}

impl Format {
    #[must_use]
    pub fn detect() -> Self {
        if io::IsTerminal::is_terminal(&io::stdout()) {
            Self::Table
        } else {
            Self::Json
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    ConfigFlag,
    TlsCertPath,
    TlsKeyPath,
    PgPassFile,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SelectedPath {
    selection: Selection,
    shown: String,
}

impl SelectedPath {
    #[must_use]
    pub fn new(selection: Selection, path: &Path) -> Self {
        let shown = path
            .to_string_lossy()
            .chars()
            .map(|c| if is_hidden(c) { '?' } else { c })
            .collect();
        Self { selection, shown }
    }

    #[must_use]
    pub const fn selection(&self) -> Selection {
        self.selection
    }

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserName(String);

impl UserName {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthKey(&'static str);

impl HealthKey {
    /// The last part of the one row that stands for every check name that
    /// trawld does not report. It starts with `_`, which no check name
    /// does, so it cannot collide with a server's check.
    pub const INVALID: &'static str = "_invalid";

    #[must_use]
    pub fn new(name: &str) -> Option<Self> {
        HEALTH_CHECK_NAMES
            .into_iter()
            .find(|known| *known == name)
            .map(Self)
    }

    #[must_use]
    pub const fn invalid() -> Self {
        Self(Self::INVALID)
    }
}

pub const INVALID_SERVICE: &str = "<invalid service>";
pub const INVALID_ENV: &str = "<invalid env>";
pub const INVALID_DATE: &str = "<invalid date>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text(String);

impl Text {
    #[must_use]
    pub fn new(literal: &'static str) -> Self {
        Self(literal.to_owned())
    }

    #[must_use]
    pub fn lit(mut self, literal: &'static str) -> Self {
        self.0.push_str(literal);
        self
    }

    #[must_use]
    pub fn int(mut self, n: impl Into<i128>) -> Self {
        self.0.push_str(&n.into().to_string());
        self
    }

    #[must_use]
    pub fn path(mut self, path: &SelectedPath) -> Self {
        self.0.push_str(path.as_shown());
        self
    }

    #[must_use]
    pub fn user(mut self, name: &UserName) -> Self {
        self.0.push_str(&name.0);
        self
    }

    #[must_use]
    pub fn key(mut self, key: &HealthKey) -> Self {
        self.0.push_str(key.0);
        self
    }

    #[must_use]
    pub fn service_name(mut self, name: &str) -> Self {
        self.0
            .push_str(if trawl_config::is_valid_service_name(name) {
                name
            } else {
                INVALID_SERVICE
            });
        self
    }

    #[must_use]
    pub fn env_name(mut self, name: &str) -> Self {
        self.0.push_str(if trawl_config::is_valid_env_name(name) {
            name
        } else {
            INVALID_ENV
        });
        self
    }

    #[must_use]
    pub fn date(mut self, day: &str) -> Self {
        match env_dirs::parse_date(day) {
            Some(date) => self.0.push_str(&env_dirs::date_name(date)),
            None => self.0.push_str(INVALID_DATE),
        }
        self
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

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

    #[must_use]
    pub fn complete(check: ServerCheck) -> Self {
        Self::new(check, Outcome::Complete, None)
    }

    #[must_use]
    pub fn complete_because(check: ServerCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::Complete, Some(reason))
    }

    #[must_use]
    pub fn failed(check: ServerCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::Failed, Some(reason))
    }

    #[must_use]
    pub fn not_sampled(check: ServerCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::NotSampled, Some(reason))
    }

    #[must_use]
    pub fn not_configured(check: ServerCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::NotConfigured, Some(reason))
    }

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

    #[must_use]
    pub const fn is_keyed(&self) -> bool {
        self.key.is_some()
    }

    #[must_use]
    pub fn detail(mut self, detail: Text) -> Self {
        self.detail = Some(detail);
        self
    }

    #[must_use]
    pub fn source(mut self, source: Text) -> Self {
        self.source = Some(source);
        self
    }

    #[must_use]
    pub fn next(mut self, next: Text) -> Self {
        self.next = Some(next);
        self
    }

    #[must_use]
    pub const fn check(&self) -> ServerCheck {
        self.check
    }

    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        self.outcome
    }

    #[must_use]
    pub const fn reason(&self) -> Option<&'static str> {
        self.reason
    }

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
    fn names_and_dates_are_shown_only_once_valid() {
        let text = Text::new("")
            .env_name("prod")
            .lit(" ")
            .service_name("api.v2")
            .lit(" ")
            .date("2026-09-23");
        assert_eq!(text.as_str(), "prod api.v2 2026-09-23");
        for env in ["", "Prod", "a/b", "evil\u{1b}[2J", &"a".repeat(33)] {
            assert_eq!(Text::new("").env_name(env).as_str(), INVALID_ENV, "{env:?}");
        }
        for service in ["", ".hidden", "a b", "a/b", "x\u{202e}y", &"s".repeat(129)] {
            assert_eq!(
                Text::new("").service_name(service).as_str(),
                INVALID_SERVICE,
                "{service:?}"
            );
        }
        for day in [
            "",
            "2026-13-01",
            "2026-02-30",
            "2026-09-23\u{1b}",
            "../x",
            "2026-9-3x",
        ] {
            assert_eq!(Text::new("").date(day).as_str(), INVALID_DATE, "{day:?}");
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
        assert!(HealthKey::new("private_secret").is_none());
        let key = HealthKey::new("duckdb").unwrap();
        assert_eq!(Text::new("").key(&key).lit(" ok").as_str(), "duckdb ok");
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
