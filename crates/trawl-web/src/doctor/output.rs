// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every string in a [`Row`] is built here from parts that cannot carry a
//! value the report must not show. No constructor takes a `String`, a
//! `Display`, or an error.

use std::io::{self, Write};
use std::path::Path;

use fleet_auth::SessionKey;
use trawl_api::doctor::health::HEALTH_CHECK_NAMES;
use trawl_api::doctor::{Check, Outcome, Report, Verdict};

use super::WebCheck;
use crate::config::SettingSource;

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

/// ADR-0047 lets a report name the configuration, credential and CA files
/// the operator selected, and no other path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    ConfigFlag,
    CookieSecretPath,
    UpstreamCaPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedPath {
    selection: Selection,
    shown: String,
}

impl SelectedPath {
    #[must_use]
    pub fn new(selection: Selection, path: &Path) -> Self {
        Self {
            selection,
            shown: without_hidden(&path.to_string_lossy()),
        }
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

fn without_hidden(text: &str) -> String {
    text.chars()
        .map(|c| if is_hidden(c) { '?' } else { c })
        .collect()
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
    /// It starts with `_`, which no check name does, so it cannot collide
    /// with a server's check.
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

/// An operator may have pasted the key itself where its variable's name
/// belongs. So the name is shown only when it does not decode as a session
/// key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvName(Option<String>);

impl EnvName {
    pub const WITHHELD: &'static str = "the variable named by `cookie_secret_env`";

    #[must_use]
    pub fn new(name: &str) -> Self {
        let bytes = name.as_bytes();
        let shaped = (1..=64).contains(&bytes.len())
            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            && bytes[1..]
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'_');
        let shown = shaped && SessionKey::from_base64(name).is_err();
        Self(shown.then(|| name.to_owned()))
    }

    #[must_use]
    pub const fn is_shown(&self) -> bool {
        self.0.is_some()
    }

    fn as_shown(&self) -> &str {
        self.0.as_deref().unwrap_or(Self::WITHHELD)
    }
}

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
    pub fn env(mut self, name: &EnvName) -> Self {
        self.0.push_str(name.as_shown());
        self
    }

    #[must_use]
    pub fn setting(self, source: SettingSource) -> Self {
        match source {
            SettingSource::File => self.lit("the config file"),
            SettingSource::Environment(name) => self.lit(name),
            SettingSource::DerivedFromFile => {
                self.lit("derived from [server] http_addr in the config file")
            }
            SettingSource::DerivedFromEnvironment(name) => self.lit("derived from ").lit(name),
            SettingSource::Default => self.lit("the default"),
        }
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
    check: WebCheck,
    key: Option<HealthKey>,
    outcome: Outcome,
    reason: Option<&'static str>,
    detail: Option<Text>,
    source: Option<Text>,
    next: Option<Text>,
    blocked_by: Option<WebCheck>,
    access: bool,
}

impl Row {
    fn new(check: WebCheck, outcome: Outcome, reason: Option<&'static str>) -> Self {
        Self {
            check,
            key: None,
            outcome,
            reason,
            detail: None,
            source: None,
            next: None,
            blocked_by: None,
            access: false,
        }
    }

    #[must_use]
    pub fn complete(check: WebCheck) -> Self {
        Self::new(check, Outcome::Complete, None)
    }

    #[must_use]
    pub fn complete_because(check: WebCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::Complete, Some(reason))
    }

    #[must_use]
    pub fn failed(check: WebCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::Failed, Some(reason))
    }

    #[must_use]
    pub fn not_sampled(check: WebCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::NotSampled, Some(reason))
    }

    #[must_use]
    pub fn not_configured(check: WebCheck, reason: &'static str) -> Self {
        Self::new(check, Outcome::NotConfigured, Some(reason))
    }

    #[must_use]
    pub fn for_key(
        check: WebCheck,
        key: HealthKey,
        outcome: Outcome,
        reason: Option<&'static str>,
    ) -> Self {
        Self {
            key: Some(key),
            ..Self::new(check, outcome, reason)
        }
    }

    /// Only an access check's own row may carry the mark
    /// ([`WebCheck::is_access`]).
    #[must_use]
    pub const fn access(mut self) -> Self {
        self.access = true;
        self
    }

    pub(super) fn blocked(check: WebCheck, by: WebCheck) -> Self {
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
        if !self.access || self.outcome != Outcome::Complete {
            return self;
        }
        Self {
            outcome: Outcome::NotSampled,
            reason: Some(trawl_api::doctor::reason::RAN_AS_ROOT),
            next: Some(Text::new(
                "rerun as the service user to check what it can read",
            )),
            ..self
        }
    }

    #[must_use]
    pub const fn is_keyed(&self) -> bool {
        self.key.is_some()
    }

    #[must_use]
    pub const fn is_access(&self) -> bool {
        self.access
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
    pub const fn check(&self) -> WebCheck {
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
        for name in ["trawl", "trawl-web", "a.b_c", "machine$", "0"] {
            assert!(UserName::new(name).is_some(), "{name}");
        }
        for name in ["", "$x", "a b", "evil\u{1b}[2J", &"a".repeat(33), "ünï"] {
            assert!(UserName::new(name).is_none(), "{name:?}");
        }
    }

    #[test]
    fn only_variable_shaped_env_names_are_shown() {
        for name in ["TRAWL_WEB_COOKIE_KEY", "_KEY", "k", &"A".repeat(64)] {
            let shown = EnvName::new(name);
            assert!(shown.is_shown(), "{name}");
            assert_eq!(Text::new("").env(&shown).as_str(), name);
        }
        // 32 bytes of key as unpadded base64 is 43 variable-shaped
        // characters; the key decodes, so it is withheld.
        let key = "cHJpdmF0ZS1zZWNyZXQtcHJpdmF0ZS1zZWNyZXQtYWI";
        assert!(SessionKey::from_base64(key).is_ok(), "the fixture is a key");
        for name in [
            key,
            "cHJpdmF0ZS1zZWNyZXQtcHJpdmF0ZS1zZWNyZXQtYWI=",
            "",
            "1KEY",
            "A KEY",
            "KEY\u{1b}[2J",
            "privat\u{e9}",
            &"A".repeat(65),
        ] {
            let withheld = EnvName::new(name);
            assert!(!withheld.is_shown(), "{name:?}");
            assert_eq!(
                Text::new("").env(&withheld).as_str(),
                EnvName::WITHHELD,
                "{name:?}"
            );
        }
    }

    #[test]
    fn text_joins_only_what_it_is_given() {
        let path = SelectedPath::new(Selection::UpstreamCaPath, Path::new("/tls/cert.pem"));
        let user = UserName::new("trawl-web").unwrap();
        let text = Text::new("uid ")
            .int(1000_u32)
            .lit(" (")
            .user(&user)
            .lit("); ")
            .path(&path);
        assert_eq!(text.as_str(), "uid 1000 (trawl-web); /tls/cert.pem");
        assert!(HealthKey::new("Bad-Key").is_none());
        assert!(HealthKey::new("private_secret").is_none());
        let key = HealthKey::new("duckdb").unwrap();
        assert_eq!(Text::new("").key(&key).lit(" ok").as_str(), "duckdb ok");
        let sources = [
            (SettingSource::File, "the config file"),
            (
                SettingSource::Environment("TRAWL_WEB_BIND_ADDR"),
                "TRAWL_WEB_BIND_ADDR",
            ),
            (
                SettingSource::DerivedFromFile,
                "derived from [server] http_addr in the config file",
            ),
            (
                SettingSource::DerivedFromEnvironment("TRAWL_HTTP_ADDR"),
                "derived from TRAWL_HTTP_ADDR",
            ),
            (SettingSource::Default, "the default"),
        ];
        for (source, shown) in sources {
            assert_eq!(Text::new("").setting(source).as_str(), shown);
        }
    }

    #[test]
    fn a_row_becomes_a_check_with_its_id() {
        let row = Row::for_key(
            WebCheck::UpstreamHealth,
            HealthKey::new("duckdb").unwrap(),
            Outcome::Failed,
            Some("reported error"),
        )
        .next(Text::new("read trawld's log"));
        let check = row.into_check();
        assert_eq!(check.id, "proxy.upstream.health.duckdb");
        assert_eq!(check.outcome, Outcome::Failed);
        assert_eq!(check.reason.as_deref(), Some("reported error"));
        assert_eq!(check.next_action.as_deref(), Some("read trawld's log"));
        assert_eq!(check.blocked_by, None);

        let blocked = Row::blocked(WebCheck::CookieSettings, WebCheck::PublicOrigins).into_check();
        assert_eq!(blocked.id, "proxy.cookie_settings");
        assert_eq!(blocked.reason.as_deref(), Some("blocked"));
        assert_eq!(blocked.blocked_by.as_deref(), Some("proxy.public_origins"));
        assert_eq!(
            Row::for_key(
                WebCheck::UpstreamHealth,
                HealthKey::invalid(),
                Outcome::Failed,
                None
            )
            .into_check()
            .id,
            "proxy.upstream.health._invalid"
        );
    }

    #[test]
    fn only_a_complete_access_row_becomes_ran_as_root() {
        let check = WebCheck::CookieKey;
        let converted = Row::complete(check).access().into_root_run();
        assert_eq!(
            (converted.outcome(), converted.reason()),
            (Outcome::NotSampled, Some("ran_as_root"))
        );
        for row in [
            Row::complete(check),
            Row::failed(check, "the key file is not 32 bytes").access(),
            Row::not_configured(check, "ephemeral_each_start").access(),
        ] {
            assert_eq!(row.clone().into_root_run(), row);
        }
    }
}
