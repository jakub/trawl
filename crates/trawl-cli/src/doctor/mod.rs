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

/// Every request `trawl doctor` may send, as method and path below the
/// target's base URL. None of them queries, ingests, or signs in with a key.
pub const REQUESTS: &[(&str, &str)] = &[
    ("GET", "/api/v1/health"),
    ("GET", "/api/v1/whoami"),
    ("GET", "/healthz"),
    ("POST", "/api/auth/login"),
];

/// The named checks `trawl doctor` runs, in order. `api.health.<key>` rows
/// are part of [`ClientCheck::ApiHealth`] and share its prerequisite.
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
}

impl ClientCheck {
    /// Every check, in the order the runner visits them.
    pub const ALL: [Self; 5] = [
        Self::ConnectionConfig,
        Self::ApiTransport,
        Self::ApiTls,
        Self::ApiHealth,
        Self::ApiIdentity,
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
        }
    }

    /// The check that must be `complete` before this one looks.
    ///
    /// Health and identity both wait for `api.tls`: nothing is read from, or
    /// sent to, a server whose certificate did not verify.
    #[must_use]
    pub const fn prerequisite(self) -> Option<Self> {
        match self {
            Self::ConnectionConfig => None,
            Self::ApiTransport => Some(Self::ConnectionConfig),
            Self::ApiTls => Some(Self::ApiTransport),
            Self::ApiHealth | Self::ApiIdentity => Some(Self::ApiTls),
        }
    }
}

/// Strip control and bidirectional-formatting characters from a string a
/// remote party sent, and cap its length, before the report shows it.
#[must_use]
pub fn display_safe(raw: &str) -> String {
    /// The most characters of one remote string the report shows.
    const MAX_CHARS: usize = 64;
    let mut clean: String = raw
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    c,
                    '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
        .collect();
    if let Some((cut, _)) = clean.char_indices().nth(MAX_CHARS) {
        clean.truncate(cut);
        clean.push('…');
    }
    clean
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
const NAME: &str = "doctor";

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
    Ok(report.verdict.exit_code())
}

/// Run every check the selection allows and build the report.
///
/// The whole report is built before anything is rendered.
pub async fn check(selection: &resolve::Selection) -> Report {
    let resolution = resolve::resolve(&selection.target);
    let mut config = Some(resolution.check);
    let mut api = resolution.connection.as_ref().map(api::ApiRun::new);
    let mut checks: Vec<Check> = Vec::new();
    let mut outcomes: Vec<(ClientCheck, Outcome)> = Vec::new();
    for step in ClientCheck::ALL {
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
                    ClientCheck::ConnectionConfig => unreachable!("handled above"),
                }
            }
        };
        outcomes.push((step, rows[0].outcome));
        checks.extend(rows);
    }
    let notes = api.map(api::ApiRun::into_notes).unwrap_or_default();
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
            let origin = report.target.origin.as_deref().unwrap_or("(unresolved)");
            writeln!(out, "target: {origin} ({})", report.target.source)?;
            let width = report
                .checks
                .iter()
                .map(|check| check.id.len())
                .max()
                .unwrap_or(0);
            for check in &report.checks {
                writeln!(out, "{}", check_line(check, width))?;
            }
            for note in &report.notes {
                writeln!(out, "note: {note}")?;
            }
            let verdict = verdict_name(report.verdict);
            writeln!(
                out,
                "verdict: {verdict} (exit {})",
                report.verdict.exit_code()
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
    /// one, ids are unique, and health and identity wait on api.tls.
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
                "api.identity"
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
                assert_eq!(at, 0);
            }
        }
        assert_eq!(
            ClientCheck::ApiHealth.prerequisite(),
            Some(ClientCheck::ApiTls)
        );
        assert_eq!(
            ClientCheck::ApiIdentity.prerequisite(),
            Some(ClientCheck::ApiTls)
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
            .checks
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
                    Some("api.tls")
                ),
            ]
        );
        assert_eq!(report.verdict, Verdict::Fail);
    }

    #[test]
    fn display_safe_strips_controls_and_caps() {
        assert_eq!(display_safe("ops-key"), "ops-key");
        assert_eq!(display_safe("a\u{1b}[31mb\r\nc"), "a[31mbc");
        assert_eq!(display_safe("evil\u{202e}txt\u{200b}"), "eviltxt");
        let long = "x".repeat(200);
        let shown = display_safe(&long);
        assert_eq!(shown.chars().count(), 65);
        assert!(shown.ends_with('…'));
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
