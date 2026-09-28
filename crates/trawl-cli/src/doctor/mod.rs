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

use std::io::{self, IsTerminal as _, Write};
use std::path::PathBuf;

use clap::CommandFactory as _;
use trawl_api::doctor::{Check, Outcome, Report, Vantage};

use crate::CliError;

pub mod resolve;

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
pub(crate) fn run(globals: Globals, args: DoctorArgs) -> Result<u8, CliError> {
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

    let report = check(&selection);

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
pub fn check(selection: &resolve::Selection) -> Report {
    let resolution = resolve::resolve(&selection.target);
    let checks = vec![resolution.check];
    Report::new(Vantage::Client, resolution.target, checks, Vec::new())
}

/// Render the report once, as text or JSON.
pub fn render(report: &Report, format: Format, out: &mut impl Write) -> io::Result<()> {
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut *out, report).map_err(io::Error::other)?;
            writeln!(out)
        }
        Format::Table => {
            let origin = report.target.origin.as_deref().unwrap_or("(unresolved)");
            writeln!(out, "target: {origin} ({})", report.target.source)?;
            for check in &report.checks {
                render_check(check, out)?;
            }
            for note in &report.notes {
                writeln!(out, "note: {note}")?;
            }
            writeln!(out, "verdict: {}", verdict_name(report))
        }
    }
}

fn render_check(check: &Check, out: &mut impl Write) -> io::Result<()> {
    let outcome = match check.outcome {
        Outcome::Complete => "complete",
        Outcome::Failed => "failed",
        Outcome::NotConfigured => "not_configured",
        Outcome::NotSampled => "not_sampled",
    };
    let mut line = format!("{:<24} {outcome}", check.id);
    for part in [&check.reason, &check.detail, &check.source]
        .into_iter()
        .flatten()
    {
        line.push_str("  ");
        line.push_str(part);
    }
    if let Some(blocked_by) = &check.blocked_by {
        line.push_str("  blocked by ");
        line.push_str(blocked_by);
    }
    writeln!(out, "{line}")?;
    if let Some(next) = &check.next_action {
        writeln!(out, "{:<24} next: {next}", "")?;
    }
    Ok(())
}

const fn verdict_name(report: &Report) -> &'static str {
    match report.verdict {
        trawl_api::doctor::Verdict::Pass => "pass",
        trawl_api::doctor::Verdict::Fail => "fail",
        trawl_api::doctor::Verdict::Incomplete => "incomplete",
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
