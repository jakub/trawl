// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl mcp`: the stdio MCP server, bound to one named profile
//! (ADR-0058).
//!
//! The command selects its target the way `trawl doctor` does (ADR-0047),
//! and is dispatched before `config.toml` is loaded the way other commands
//! load it:
//!
//! - The ambient environment never selects anything: `TRAWL_URL`,
//!   `TRAWL_TOKEN`, `TRAWL_PROFILE` and `TRAWL_INSECURE` end the process
//!   with exit 2 when present, even empty, named and never echoed. clap
//!   never reads them for this command (see `parse_cli`).
//! - `--url`, `--token` and `--insecure` are refused: the profile binds the
//!   URL, the key and the trust.
//! - The URL and trust come from the profile through the doctor's
//!   [`resolve`](crate::doctor::resolve) rules. The key comes from that
//!   profile's own table or from `--token-file`, never from
//!   `[server].token`; both at once is refused, and so is neither.
//! - `-p trial` resolves through the trial's own rules (ADR-0045).
//!
//! Nothing contacts a server at startup: each tool reports its own refusal
//! when it is called. stdout carries protocol frames only, so every log
//! line goes to stderr and no TUI starts. Exit status: 0 when the client
//! closes the session, 1 when the transport fails, 2 for every refusal
//! before the server starts. Each failure is one line on stderr.

use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use trawl_client::{HttpClient, TlsTrust};
use trawl_core::sanitize::sanitize_display_text;
use trawl_mcp::TrawlServer;

use crate::config::{self, Config};
use crate::doctor::resolve::{self, Failure, Key, TargetSelection};
use crate::doctor::{Globals, env_bound_names};
use crate::trial;

/// The name clap gives the subcommand.
pub(crate) const NAME: &str = "mcp";

/// The exit status of every refusal before the server starts.
const REFUSED: u8 = 2;

/// The exit status when the transport fails.
const TRANSPORT_FAILED: u8 = 1;

/// `trawl mcp` arguments. The profile is the global `-p/--profile`.
#[derive(Debug, clap::Args)]
pub(crate) struct McpArgs {
    /// Read the API key from the file PATH, for a profile that holds no
    /// token.
    #[arg(long, value_name = "PATH")]
    token_file: Option<PathBuf>,
}

/// Why `trawl mcp` will not start: one line, naming variables, flags and
/// files, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refusal(String);

/// Run `trawl mcp` and return its exit status.
pub(crate) async fn run(globals: Globals, args: McpArgs) -> u8 {
    let started = match start(&globals, &args) {
        Ok(started) => started,
        Err(Refusal(message)) => {
            report(&message);
            return REFUSED;
        }
    };
    if started.insecure {
        report_line(crate::INSECURE_WARNING);
    }
    init_tracing();
    match trawl_mcp::serve_stdio(started.server).await {
        Ok(()) => 0,
        Err(error) => {
            report(&error.to_string());
            TRANSPORT_FAILED
        }
    }
}

/// A server ready to serve, and whether its trust skips verification.
struct Started {
    server: TrawlServer<HttpClient>,
    insecure: bool,
}

/// Select, resolve and build the server, with no network call.
fn start(globals: &Globals, args: &McpArgs) -> Result<Started, Refusal> {
    let selection = select(globals, &env_bound_names(NAME), |name| {
        std::env::var_os(name).is_some()
    })?;
    let resolution = resolve::resolve(&selection);
    let Some(connection) = resolution.connection else {
        let check = resolution.check;
        return Err(Refusal(format!(
            "{}: {}; {}",
            check.source.unwrap_or_default(),
            check.reason.unwrap_or_default(),
            check.next_action.unwrap_or_default(),
        )));
    };
    let key = choose_key(&selection, connection.key, args.token_file.as_deref())?;
    let display_zone = display_zone(globals.config.as_deref())?;
    let client = HttpClient::with_trust(connection.url.base(), key.secret(), &connection.trust)
        .map_err(|_| {
            Refusal(
                "the API client cannot be built from the profile's trust: check that ca_cert \
                 names a PEM file holding the server's CA certificate"
                    .to_owned(),
            )
        })?;
    Ok(Started {
        server: TrawlServer::new(client).with_display_zone(display_zone),
        insecure: matches!(connection.trust, TlsTrust::AcceptInvalid),
    })
}

/// Turn the command line into the profile to resolve, or refuse it.
///
/// `refused_env` is every variable clap binds for `trawl mcp`, and
/// `env_present` answers whether one is set in this process, empty or not.
/// Every refused variable that is present is named.
fn select(
    globals: &Globals,
    refused_env: &[String],
    env_present: impl Fn(&str) -> bool,
) -> Result<TargetSelection, Refusal> {
    let present: Vec<&str> = refused_env
        .iter()
        .map(String::as_str)
        .filter(|name| env_present(name))
        .collect();
    if !present.is_empty() {
        let (names, verb, them) = match present.as_slice() {
            [one] => ((*one).to_owned(), "is", "it"),
            [init @ .., last] => (format!("{} and {last}", init.join(", ")), "are", "them"),
            [] => unreachable!("present is not empty"),
        };
        return Err(Refusal(format!(
            "{names} {verb} set in the environment, and trawl mcp takes its target, key, and \
             trust only from --profile and --token-file. Unset {them}, then name the profile \
             with --profile"
        )));
    }
    let bound = |flag: &str| {
        Refusal(format!(
            "{flag} cannot be used with trawl mcp: the profile binds the connection's URL, key, \
             and trust. Change the profile, or name a key file with --token-file"
        ))
    };
    if globals.url.is_some() {
        return Err(bound("--url"));
    }
    if globals.token {
        return Err(bound("--token"));
    }
    if globals.insecure {
        return Err(bound("--insecure"));
    }
    let Some(ref profile) = globals.profile else {
        return Err(Refusal(
            "trawl mcp needs a profile: give --profile NAME".to_owned(),
        ));
    };
    let config = globals.config.clone();
    Ok(if profile == trial::PROFILE {
        TargetSelection::Trial { config }
    } else {
        TargetSelection::Profile {
            name: profile.clone(),
            config,
        }
    })
}

/// The one key the server sends: the profile's own, or the file's.
fn choose_key(
    selection: &TargetSelection,
    profile_key: Option<Key>,
    token_file: Option<&Path>,
) -> Result<Key, Refusal> {
    match (profile_key, token_file) {
        (Some(key), None) => Ok(key),
        (Some(key), Some(_)) => Err(Refusal(format!(
            "two key sources: {} and --token-file. Give one: remove --token-file, or \
             remove the token from the profile",
            key.source()
        ))),
        (None, Some(path)) => resolve::key_from_file(path).map_err(|failure| {
            let Failure {
                source,
                reason,
                next_action,
            } = failure;
            Refusal(format!("{source}: {reason}; {next_action}"))
        }),
        (None, None) => {
            let table = match selection {
                TargetSelection::Profile { name, .. } => format!("[profiles.{name}]"),
                _ => "the profile".to_owned(),
            };
            Err(Refusal(format!(
                "no key: {table} holds no token and no --token-file was given. Set the key as \
                 token in {table}, or name a file that holds it with --token-file"
            )))
        }
    }
}

/// The CLI config's `ui.timezone`, checked, for `orient` to display times
/// in. The doctor's resolution read the same file a moment ago.
fn display_zone(config: Option<&str>) -> Result<String, Refusal> {
    let path = config.unwrap_or(config::DEFAULT_CONFIG_PATH);
    let cfg = Config::load(config).map_err(|_| {
        Refusal(format!(
            "config file {path}: the config file cannot be read; check that it exists and is \
             valid TOML for trawl"
        ))
    })?;
    let zone = cfg.ui.timezone;
    trawl_engine::timezone::resolve_utc_offset(&zone).map_err(|_| {
        Refusal(format!(
            "ui.timezone in {path} is not \"local\", \"UTC\", or a fixed offset such as \
             \"+05:30\"; fix it, or remove it for \"local\""
        ))
    })?;
    Ok(zone)
}

/// Send every log line to stderr, at the level `RUST_LOG` names (default
/// `warn`): rmcp logs through tracing, and a line on stdout would corrupt
/// the protocol.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_ansi(io::stderr().is_terminal())
        .init();
}

/// Write `message` as one `trawl:` line on stderr, with control and
/// formatting characters replaced.
fn report(message: &str) {
    report_line(&format!("trawl: {}", sanitize_display_text(message)));
}

/// Write `line` to stderr. A closed stderr leaves nowhere to report to.
fn report_line(line: &str) {
    let _ = writeln!(io::stderr().lock(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: [&str; 4] = [
        "TRAWL_URL",
        "TRAWL_TOKEN",
        "TRAWL_INSECURE",
        "TRAWL_PROFILE",
    ];

    fn env_names() -> Vec<String> {
        ENV.iter().map(|&n| n.to_owned()).collect()
    }

    fn globals(profile: Option<&str>) -> Globals {
        Globals {
            url: None,
            token: false,
            insecure: false,
            profile: profile.map(str::to_owned),
            config: None,
        }
    }

    fn refused(globals: &Globals, present: &[&str]) -> String {
        select(globals, &env_names(), |name| present.contains(&name))
            .expect_err("refused")
            .0
    }

    #[test]
    fn the_refused_variables_are_the_four_globals() {
        let mut names = env_bound_names(NAME);
        names.sort_unstable();
        let mut expected = env_names();
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    #[test]
    fn every_present_variable_is_named() {
        for name in ENV {
            let message = refused(&globals(Some("home")), &[name]);
            assert!(message.starts_with(&format!("{name} is set")), "{message}");
        }
        let message = refused(&globals(Some("home")), &ENV);
        assert!(
            message.starts_with("TRAWL_URL, TRAWL_TOKEN, TRAWL_INSECURE and TRAWL_PROFILE are set"),
            "{message}"
        );
    }

    #[test]
    fn flags_that_would_move_the_profile_are_refused() {
        let mut url = globals(Some("home"));
        url.url = Some("https://elsewhere:5514".to_owned());
        assert!(refused(&url, &[]).starts_with("--url cannot"));
        let mut token = globals(Some("home"));
        token.token = true;
        assert!(refused(&token, &[]).starts_with("--token cannot"));
        let mut insecure = globals(Some("home"));
        insecure.insecure = true;
        assert!(refused(&insecure, &[]).starts_with("--insecure cannot"));
    }

    #[test]
    fn a_profile_is_required_and_trial_is_its_own_selection() {
        assert!(refused(&globals(None), &[]).contains("needs a profile"));
        let select = |profile| select(&globals(Some(profile)), &env_names(), |_| false);
        assert_eq!(select("trial"), Ok(TargetSelection::Trial { config: None }));
        assert_eq!(
            select("home"),
            Ok(TargetSelection::Profile {
                name: "home".to_owned(),
                config: None
            })
        );
    }
}
