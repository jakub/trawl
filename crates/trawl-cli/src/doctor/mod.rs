// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl doctor`: check one client connection from where the CLI runs
//! (ADR-0047).
//!
//! The run is dispatched before `config.toml` is loaded, because it selects
//! its own target and key ([`resolve`]). It builds one
//! [`trawl_api::doctor::Report`], renders it once, and exits with the
//! verdict's status: 0 pass, 1 fail, 3 incomplete. A command line it refuses
//! is a clap usage error, exit 2.
//!
//! The checks and their prerequisites are data ([`ClientCheck`]); one
//! runner walks them in order and turns a check whose prerequisite did not
//! complete into `not_sampled`, reason `blocked`, naming that prerequisite.
//! Every request the doctor can send is listed in [`REQUESTS`].

use std::io::{self, IsTerminal as _, Write};
use std::path::PathBuf;

use clap::CommandFactory as _;
use trawl_api::doctor::{Check, Outcome, Report, Vantage, Verdict, reason};

use crate::CliError;

pub mod api;
pub mod resolve;
pub mod web;

/// Every request `trawl doctor` may send, as method and path below the
/// target's base URL. None of them queries, ingests, or signs in with a key.
pub const REQUESTS: &[(&str, &str)] = &[
    ("GET", "/api/v1/health"),
    ("GET", "/api/v1/whoami"),
    ("GET", "/healthz"),
    ("POST", "/api/auth/login"),
];

/// The named checks `trawl doctor` runs, in order. `api.health.<key>` rows
/// are part of [`ClientCheck::ApiHealth`] and share its prerequisite. The
/// `web.*` checks run only when `--web-url` names an origin; without it they
/// produce no rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCheck {
    /// The selected profile or flags resolve.
    ConnectionConfig,
    /// A TCP and TLS connection to the API origin opens.
    ApiTransport,
    /// The certificate verifies under the named trust mode.
    ApiTls,
    /// The health endpoint answers and parses, with one row per check.
    ApiHealth,
    /// `whoami` accepts the selected key.
    ApiIdentity,
    /// `--web-url` answers `trawl-web`'s health endpoint.
    WebTransport,
    /// `trawl-web` accepts `--web-url` as a browser origin.
    WebOrigin,
}

impl ClientCheck {
    /// Every check, in the order the runner visits them.
    pub const ALL: [Self; 7] = [
        Self::ConnectionConfig,
        Self::ApiTransport,
        Self::ApiTls,
        Self::ApiHealth,
        Self::ApiIdentity,
        Self::WebTransport,
        Self::WebOrigin,
    ];

    /// The stable id the report carries.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::ConnectionConfig => resolve::CONNECTION_CONFIG,
            Self::ApiTransport => api::API_TRANSPORT,
            Self::ApiTls => api::API_TLS,
            Self::ApiHealth => api::API_HEALTH,
            Self::ApiIdentity => api::API_IDENTITY,
            Self::WebTransport => web::WEB_TRANSPORT,
            Self::WebOrigin => web::WEB_ORIGIN,
        }
    }

    /// The check that must be `complete` before this one looks.
    ///
    /// Health waits for `api.tls`: nothing is read from a server whose
    /// certificate did not verify. Identity waits for `api.health`: the key
    /// goes only to a server that answered with trawl's health body under
    /// verified TLS, never to a redirect, a foreign service, or a server
    /// whose answer could not be read. The `web.*`
    /// checks do not depend on the API: `--web-url` was checked when the
    /// command line was, and the probe sends no key.
    #[must_use]
    pub const fn prerequisite(self) -> Option<Self> {
        match self {
            Self::ConnectionConfig | Self::WebTransport => None,
            Self::ApiTransport => Some(Self::ConnectionConfig),
            Self::ApiTls => Some(Self::ApiTransport),
            Self::ApiHealth => Some(Self::ApiTls),
            Self::ApiIdentity => Some(Self::ApiHealth),
            Self::WebOrigin => Some(Self::WebTransport),
        }
    }

    /// Whether the check belongs to `--web-url`.
    #[must_use]
    pub const fn is_web(self) -> bool {
        matches!(self, Self::WebTransport | Self::WebOrigin)
    }
}

/// What a run of the key shows as in the report.
pub const REDACTED: &str = "[redacted]";

/// The shortest run of the key's characters that is redacted. It is the
/// length of the prefix `fleet-auth` derives from a key (the first 8
/// characters after `flt_`), so that prefix, the whole key, and any longer
/// piece of it are all caught; a shorter key is redacted whole.
const KEY_RUN: usize = 8;

/// Strip control and bidirectional-formatting characters from a string a
/// remote party sent, redact every run of `key` in what is left, and cap
/// the length, before the report shows it.
///
/// A run is any stretch of at least [`KEY_RUN`] characters that also
/// appears in the key. Redaction runs after stripping, so control
/// characters spliced into the key do not hide it, and before the cap, so
/// the cap never leaves a piece of the key behind.
#[must_use]
pub fn display_safe(raw: &str, key: Option<&str>) -> String {
    /// The most characters of one remote string the report shows.
    const MAX_CHARS: usize = 64;
    display_safe_within(raw, key, MAX_CHARS)
}

/// [`display_safe`] with a cap of `max_chars` characters instead of the
/// default.
#[must_use]
pub fn display_safe_within(raw: &str, key: Option<&str>, max_chars: usize) -> String {
    let clean: Vec<char> = raw.chars().filter(|c| is_shown(*c)).collect();
    let key: Vec<char> = key.map(|key| key.chars().collect()).unwrap_or_default();
    let mut shown = String::new();
    let mut count = 0usize;
    let mut at = 0usize;
    while at < clean.len() {
        if count >= max_chars {
            shown.push('…');
            break;
        }
        let run = key_run(&clean[at..], &key);
        if run > 0 {
            shown.push_str(REDACTED);
            count += REDACTED.len();
            at += run;
        } else {
            shown.push(clean[at]);
            count += 1;
            at += 1;
        }
    }
    shown
}

/// Whether `raw`, with control and formatting characters stripped, holds a
/// run of `key` that [`display_safe`] would redact.
#[must_use]
pub fn holds_key(raw: &str, key: Option<&str>) -> bool {
    let Some(key) = key else { return false };
    let clean: Vec<char> = raw.chars().filter(|c| is_shown(*c)).collect();
    let key: Vec<char> = key.chars().collect();
    (0..clean.len()).any(|at| key_run(&clean[at..], &key) > 0)
}

/// Characters a remote string may not bring into the report: every
/// character whose Unicode bidirectional class is an explicit formatting
/// class (LRE, RLE, LRO, RLO, PDF, LRI, RLI, FSI, PDI) or one of the
/// implicit marks (ALM, LRM, RLM), which can reorder what a terminal
/// shows, and the invisible characters that can hide or split text.
const HIDDEN: [char; 21] = [
    // Bidirectional classes ALM, LRM, and RLM.
    '\u{061c}', // ARABIC LETTER MARK
    '\u{200e}', // LEFT-TO-RIGHT MARK
    '\u{200f}', // RIGHT-TO-LEFT MARK
    // Bidirectional classes LRE, RLE, PDF, LRO, and RLO.
    '\u{202a}', // LEFT-TO-RIGHT EMBEDDING
    '\u{202b}', // RIGHT-TO-LEFT EMBEDDING
    '\u{202c}', // POP DIRECTIONAL FORMATTING
    '\u{202d}', // LEFT-TO-RIGHT OVERRIDE
    '\u{202e}', // RIGHT-TO-LEFT OVERRIDE
    // Bidirectional classes LRI, RLI, FSI, and PDI.
    '\u{2066}', // LEFT-TO-RIGHT ISOLATE
    '\u{2067}', // RIGHT-TO-LEFT ISOLATE
    '\u{2068}', // FIRST STRONG ISOLATE
    '\u{2069}', // POP DIRECTIONAL ISOLATE
    // Zero-width and invisible characters.
    '\u{200b}', // ZERO WIDTH SPACE
    '\u{200c}', // ZERO WIDTH NON-JOINER
    '\u{200d}', // ZERO WIDTH JOINER
    '\u{2060}', // WORD JOINER
    '\u{2061}', // FUNCTION APPLICATION
    '\u{2062}', // INVISIBLE TIMES
    '\u{2063}', // INVISIBLE SEPARATOR
    '\u{2064}', // INVISIBLE PLUS
    '\u{feff}', // ZERO WIDTH NO-BREAK SPACE
];

/// Whether the report may show `c`: not a control character and not one
/// of [`HIDDEN`].
fn is_shown(c: char) -> bool {
    !c.is_control() && !HIDDEN.contains(&c)
}

/// The length of the longest start of `text` that appears somewhere in
/// `key`, when that is a redactable run, else 0.
fn key_run(text: &[char], key: &[char]) -> usize {
    let shortest = key.len().min(KEY_RUN);
    if shortest == 0 {
        return 0;
    }
    let mut longest = 0;
    // A piece of a piece of the key is a piece of the key, so the first
    // length that is not one ends the search.
    for len in 1..=text.len().min(key.len()) {
        if key.windows(len).any(|piece| piece == &text[..len]) {
            longest = len;
        } else {
            break;
        }
    }
    if longest >= shortest { longest } else { 0 }
}

/// Output format for the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// One line per check.
    Table,
    /// The versioned JSON report.
    Json,
}

/// `trawl doctor` arguments. The target comes from the global `--url` or
/// `--profile`.
#[derive(Debug, clap::Args)]
pub(crate) struct DoctorArgs {
    /// With --url: read the API key from the environment variable NAME.
    #[arg(long, value_name = "NAME")]
    token_env: Option<String>,

    /// With --url: read the API key from the file PATH.
    #[arg(long, value_name = "PATH")]
    token_file: Option<PathBuf>,

    /// Also check this browser origin (https, or http on a loopback host).
    #[arg(long, value_name = "ORIGIN")]
    web_url: Option<String>,

    /// Output format (auto-detected if omitted: table for TTY, json for pipes).
    #[arg(long, short, value_enum)]
    format: Option<Format>,
}

/// The global flags `trawl doctor` reads, as clap parsed them.
#[derive(Debug)]
pub(crate) struct Globals {
    pub url: Option<String>,
    pub token: bool,
    pub insecure: bool,
    pub profile: Option<String>,
    pub config: Option<String>,
}

/// The name clap gives the subcommand.
pub(crate) const NAME: &str = "doctor";

/// Every environment variable clap binds to an argument of `trawl doctor`,
/// global ones included. All of them are refused when present.
pub(crate) fn refused_env_names() -> Vec<String> {
    let mut cli = crate::Cli::command();
    cli.build();
    let doctor = cli
        .find_subcommand(NAME)
        .expect("the doctor subcommand is registered");
    doctor
        .get_arguments()
        .filter_map(|arg| arg.get_env())
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

/// A refusal as the clap usage error it is reported as (exit 2), with the
/// subcommand's usage line.
fn usage_error(refusal: resolve::Refusal) -> clap::Error {
    let mut cli = crate::Cli::command();
    cli.build();
    cli.find_subcommand_mut(NAME)
        .expect("the doctor subcommand is registered")
        .error(refusal.kind, refusal.message)
}

/// Run `trawl doctor` and return its exit status.
pub(crate) async fn run(globals: Globals, args: DoctorArgs) -> Result<u8, CliError> {
    let invocation = resolve::Invocation {
        url: globals.url,
        token: globals.token,
        insecure: globals.insecure,
        profile: globals.profile,
        config: globals.config,
        token_env: args.token_env,
        token_file: args.token_file,
        web_url: args.web_url,
    };
    let selection = resolve::select(&invocation, &refused_env_names(), |name| {
        std::env::var_os(name).is_some()
    })
    .map_err(|refusal| CliError::Arg(Box::new(usage_error(refusal))))?;

    let report = check(&selection).await;

    let format = args.format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            Format::Table
        } else {
            Format::Json
        }
    });
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render(&report, format, &mut out)?;
    out.flush()?;
    Ok(report.verdict().exit_code())
}

/// Run every check the selection allows and build the report.
///
/// The whole report is built before anything is rendered.
pub async fn check(selection: &resolve::Selection) -> Report {
    let resolution = resolve::resolve(&selection.target);
    let mut config = Some(resolution.check);
    let mut api = resolution.connection.as_ref().map(api::ApiRun::new);
    let mut web = selection.web.as_ref().map(web::WebRun::new);
    let mut checks: Vec<Check> = Vec::new();
    let mut outcomes: Vec<(ClientCheck, Outcome)> = Vec::new();
    for step in ClientCheck::ALL {
        if step.is_web() && web.is_none() {
            continue;
        }
        if let Some(prerequisite) = step.prerequisite() {
            let done = outcomes
                .iter()
                .find(|(check, _)| *check == prerequisite)
                .map(|(_, outcome)| *outcome);
            if done != Some(Outcome::Complete) {
                checks.push(blocked(step, prerequisite));
                outcomes.push((step, Outcome::NotSampled));
                continue;
            }
        }
        let rows = match step {
            ClientCheck::ConnectionConfig => {
                vec![config.take().expect("connection.config runs once")]
            }
            // Skipped above when --web-url is absent.
            ClientCheck::WebTransport => {
                vec![web.as_mut().expect("--web-url").transport().await]
            }
            ClientCheck::WebOrigin => vec![web.as_mut().expect("--web-url").origin().await],
            other => {
                // connection.config completed, so the connection resolved.
                let api = api
                    .as_mut()
                    .expect("a complete connection.config resolves a connection");
                match other {
                    ClientCheck::ApiTransport => vec![api.transport().await],
                    ClientCheck::ApiTls => vec![api.tls()],
                    ClientCheck::ApiHealth => api.health(),
                    ClientCheck::ApiIdentity => vec![api.identity().await],
                    ClientCheck::ConnectionConfig
                    | ClientCheck::WebTransport
                    | ClientCheck::WebOrigin => unreachable!("handled above"),
                }
            }
        };
        outcomes.push((step, rows[0].outcome));
        checks.extend(rows);
    }
    let mut notes = api.map(api::ApiRun::into_notes).unwrap_or_default();
    notes.extend(web.map(web::WebRun::into_notes).unwrap_or_default());
    Report::new(Vantage::Client, resolution.target, checks, notes)
}

/// The row for a check whose prerequisite did not complete.
fn blocked(step: ClientCheck, prerequisite: ClientCheck) -> Check {
    Check {
        id: step.id().to_owned(),
        outcome: Outcome::NotSampled,
        reason: Some(reason::BLOCKED.to_owned()),
        detail: None,
        source: None,
        blocked_by: Some(prerequisite.id().to_owned()),
        next_action: None,
    }
}

/// Render the report once: JSON is the report serialized, text is one line
/// per check, then the notes and the verdict.
pub fn render(report: &Report, format: Format, out: &mut impl Write) -> io::Result<()> {
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut *out, report).map_err(io::Error::other)?;
            writeln!(out)
        }
        Format::Table => {
            let origin = report.target().origin.as_deref().unwrap_or("(unresolved)");
            writeln!(out, "target: {origin} ({})", report.target().source)?;
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
            let verdict = verdict_name(report.verdict());
            writeln!(
                out,
                "verdict: {verdict} (exit {})",
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

    /// The variables the doctor refuses are exactly the ones clap binds for
    /// it, and those are the four ADR-0047 names. A new env-bound flag
    /// fails here until the ADR and the refusal agree about it.
    #[test]
    fn refused_env_names_match_what_clap_binds() {
        let mut names = refused_env_names();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "TRAWL_INSECURE",
                "TRAWL_PROFILE",
                "TRAWL_TOKEN",
                "TRAWL_URL"
            ]
        );

        // Walk the whole tree: an env binding anywhere clap would apply to
        // `trawl doctor` is in the list.
        let mut cli = crate::Cli::command();
        cli.build();
        let mut bound: Vec<String> = cli
            .get_arguments()
            .filter(|arg| arg.is_global_set())
            .filter_map(|arg| arg.get_env())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        bound.sort_unstable();
        assert_eq!(bound, names, "every global env binding reaches doctor");
    }

    /// The graph is data: every check but the first waits on an earlier
    /// one, ids are unique, health waits on api.tls, and identity on
    /// api.health.
    #[test]
    fn client_checks_form_an_ordered_graph() {
        let mut ids: Vec<&str> = ClientCheck::ALL.iter().map(|c| c.id()).collect();
        assert_eq!(
            ids,
            [
                "connection.config",
                "api.transport",
                "api.tls",
                "api.health",
                "api.identity",
                "web.transport",
                "web.origin"
            ]
        );
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ClientCheck::ALL.len());
        for (at, check) in ClientCheck::ALL.iter().enumerate() {
            if let Some(pre) = check.prerequisite() {
                let before = ClientCheck::ALL[..at].contains(&pre);
                assert!(before, "{check:?} waits on a later check");
            } else {
                assert!(
                    matches!(
                        check,
                        ClientCheck::ConnectionConfig | ClientCheck::WebTransport
                    ),
                    "{check:?} has no prerequisite"
                );
            }
        }
        assert_eq!(
            ClientCheck::ApiHealth.prerequisite(),
            Some(ClientCheck::ApiTls)
        );
        assert_eq!(
            ClientCheck::ApiIdentity.prerequisite(),
            Some(ClientCheck::ApiHealth)
        );
        assert_eq!(
            ClientCheck::WebOrigin.prerequisite(),
            Some(ClientCheck::WebTransport)
        );
    }

    /// A failed connection.config blocks every later check, each naming
    /// its own prerequisite, and nothing is contacted.
    #[tokio::test]
    async fn a_failed_prerequisite_blocks_the_rest() {
        let selection = resolve::Selection {
            target: resolve::TargetSelection::Url {
                url: resolve::check_url("https://127.0.0.1:1").unwrap(),
                insecure: false,
                key: resolve::KeySource::File(PathBuf::from("/nonexistent/doctor/key")),
            },
            web: None,
        };
        let report = check(&selection).await;
        let rows: Vec<(&str, Outcome, Option<&str>, Option<&str>)> = report
            .checks()
            .iter()
            .map(|c| {
                (
                    c.id.as_str(),
                    c.outcome,
                    c.reason.as_deref(),
                    c.blocked_by.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "connection.config",
                    Outcome::Failed,
                    Some("the file --token-file names does not exist"),
                    None
                ),
                (
                    "api.transport",
                    Outcome::NotSampled,
                    Some("blocked"),
                    Some("connection.config")
                ),
                (
                    "api.tls",
                    Outcome::NotSampled,
                    Some("blocked"),
                    Some("api.transport")
                ),
                (
                    "api.health",
                    Outcome::NotSampled,
                    Some("blocked"),
                    Some("api.tls")
                ),
                (
                    "api.identity",
                    Outcome::NotSampled,
                    Some("blocked"),
                    Some("api.health")
                ),
            ]
        );
        assert_eq!(report.verdict(), Verdict::Fail);
    }

    #[test]
    fn display_safe_strips_controls_and_caps() {
        assert_eq!(display_safe("ops-key", None), "ops-key");
        assert_eq!(display_safe("a\u{1b}[31mb\r\nc", None), "a[31mbc");
        assert_eq!(display_safe("evil\u{202e}txt\u{200b}", None), "eviltxt");
        let long = "x".repeat(200);
        let shown = display_safe(&long, None);
        assert_eq!(shown.chars().count(), 65);
        assert!(shown.ends_with('…'));
        assert_eq!(display_safe(&"x".repeat(64), None), "x".repeat(64));
    }

    /// Every bidirectional control and invisible character is dropped,
    /// wherever it sits, and the text around it stays.
    #[test]
    fn display_safe_strips_every_bidi_control() {
        let bidi = [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ];
        let invisible = [
            '\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{2061}', '\u{2062}', '\u{2063}',
            '\u{2064}', '\u{feff}',
        ];
        for c in bidi.into_iter().chain(invisible) {
            let raw = format!("{c}ab{c}c{c}");
            assert_eq!(display_safe(&raw, None), "abc", "U+{:04X}", u32::from(c));
        }
        assert_eq!(HIDDEN.len(), bidi.len() + invisible.len());
        // Letters of right-to-left scripts are text, not controls.
        assert_eq!(display_safe("\u{05d0}\u{0627}", None), "\u{05d0}\u{0627}");
    }

    /// The key, its `fleet-auth` prefix, and any piece of 8 or more of its
    /// characters are redacted, also when control characters are spliced
    /// into them or the piece straddles the length cap. Shorter pieces
    /// stay, and so does everything without a key.
    #[test]
    fn display_safe_redacts_the_key() {
        const KEY: &str = "flt_Ab3dEf9hIjKlMnOpQrStUvWxYz0123456789-_abcde";
        let key = Some(KEY);
        assert_eq!(display_safe(KEY, key), REDACTED);
        assert_eq!(
            display_safe("owner Ab3dEf9h (prefix)", key),
            "owner [redacted] (prefix)"
        );
        assert_eq!(
            display_safe("x flt_Ab3d\u{1b}Ef9hIjKl\u{200b}Mn y", key),
            "x [redacted] y"
        );
        assert_eq!(display_safe("Ab3dEf9", key), "Ab3dEf9");
        assert_eq!(display_safe("ops-key", key), "ops-key");
        let straddle = format!("{}{KEY}", "x".repeat(60));
        let shown = display_safe(&straddle, key);
        assert_eq!(shown, format!("{}{REDACTED}", "x".repeat(60)));
        let beyond = format!("{}{KEY}", "x".repeat(64));
        let shown = display_safe(&beyond, key);
        assert!(
            !shown.contains("flt_Ab3d") && shown.ends_with('…'),
            "{shown}"
        );
        // A key shorter than the run is redacted whole.
        assert_eq!(display_safe("a tiny b", Some("tiny")), "a [redacted] b");

        assert!(holds_key("check_Ab3dEf9h", key));
        assert!(holds_key("Ab3d\u{1b}Ef9h", key));
        assert!(!holds_key("duckdb", key));
        assert!(!holds_key("Ab3dEf9h", None));
    }

    #[test]
    fn a_refusal_is_a_usage_error() {
        let err = usage_error(resolve::Refusal {
            kind: clap::error::ErrorKind::ArgumentConflict,
            message: "TRAWL_URL is set".to_owned(),
        });
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("trawl doctor"), "{err}");
    }
}
