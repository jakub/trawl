// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web`: browser-facing session proxy binary.
//!
//! Reads the same `~/.trawl/trawld.toml` as trawld (matching trawld's
//! `--config` default), looking at its `[web]` section for
//! proxy-specific settings. The CLI config at
//! `~/.config/trawl/config.toml` has a different schema (with
//! `[server].url`, `[server].token`, and `[profiles.*]`) and is NOT
//! a valid input here.
//!
//! `--doctor` checks that configuration without starting the proxy
//! (ADR-0047, #271): `main` sends it to [`trawl_web::doctor`] before the
//! tracing subscriber, the configuration load, or any runtime exists.

use std::path::PathBuf;

use clap::{CommandFactory as _, FromArgMatches as _, Parser};
use tracing_subscriber::EnvFilter;
use trawl_web::config::ResolvedConfig;
use trawl_web::routes;
use trawl_web::state::AppState;
use trawl_web::upstream::CA_REREAD_INTERVAL;

/// Default config path — kept in sync with `trawl-server`'s default so a
/// single `trawld.toml` configures both daemons.
const DEFAULT_CONFIG_PATH: &str = "~/.trawl/trawld.toml";

#[derive(Parser, Debug)]
#[command(name = "trawl-web", about = "trawl browser-facing session proxy")]
struct Cli {
    /// Config file path. Same schema as trawld; reads the `[web]` block.
    #[arg(short, long, env = "TRAWL_CONFIG", default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    /// Check, without starting the proxy or changing anything, whether
    /// trawl-web can start, keep sessions, and reach trawld with the
    /// config that --config names on the command line, then exit: 0 pass,
    /// 1 fail, 3 incomplete.
    #[arg(long)]
    doctor: bool,

    /// With --doctor: report format (auto-detected if omitted: table for
    /// TTY, json for pipes).
    #[arg(long, short, value_enum, requires = "doctor")]
    format: Option<trawl_web::doctor::Format>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The doctor never reaches the proxy's startup: no subscriber, no
    // configuration load, no runtime but its own. `--doctor=` is caught as
    // well, so a spelling clap refuses is still refused by the doctor's
    // usage path, which quotes no value.
    if std::env::args_os()
        .any(|arg| arg == "--doctor" || arg.as_encoded_bytes().starts_with(b"--doctor="))
    {
        std::process::exit(i32::from(doctor_main()));
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed building the Runtime")
        .block_on(serve())
}

/// The usage line every `--doctor` usage error ends with.
const DOCTOR_USAGE: &str = "Usage: trawl-web --doctor --config <PATH> [--format <table|json>]";

/// `trawl-web --doctor`. Returns the exit status: the report's (0, 1, 3)
/// or 2 for a refused command line.
///
/// No tracing subscriber exists on this path, so a log line from shared
/// code goes nowhere.
fn doctor_main() -> u8 {
    // clap's help shows an env-bound argument's current value, as
    // `[env: NAME=value]`. The doctor's output names no value it did not
    // select (ADR-0047), so its help names each variable without one.
    let command = Cli::command().mut_args(|arg| arg.hide_env_values(true));
    let matches = match command.try_get_matches() {
        Ok(matches) => matches,
        Err(error) => return doctor_usage_error(&error),
    };
    // ADR-0047: the doctor checks the file the operator names on its
    // command line, never one TRAWL_CONFIG points at or the default.
    if matches.value_source("config") != Some(clap::parser::ValueSource::CommandLine) {
        eprintln!(
            "[trawl-web] --doctor needs --config on the command line; it does not read \
             TRAWL_CONFIG or the default path\n{DOCTOR_USAGE}"
        );
        return 2;
    }
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => return doctor_usage_error(&error),
    };
    // The argument as given: the doctor expands its `~` itself, under a
    // deadline, since with HOME unset or empty the expansion asks the
    // password database.
    trawl_web::doctor::run(&cli.config, cli.format)
}

/// Report a refused `--doctor` command line, exit status 2, without
/// echoing any argument or environment value: clap's own message quotes
/// the value it refused. Help prints as clap prints it; the help comes
/// from the command `doctor_main` built, which shows no environment value.
fn doctor_usage_error(error: &clap::Error) -> u8 {
    use clap::error::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        let _ = error.print();
        return 0;
    }
    let kind = error
        .kind()
        .as_str()
        .unwrap_or("the command line is not valid");
    eprintln!("[trawl-web] --doctor: {kind}\n{DOCTOR_USAGE}");
    2
}

/// The proxy: load the configuration, build the state, bind, and serve.
async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_ansi(trawl_config::color::stdout_ansi())
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(trawl_web::DEFAULT_LOG_FILTER)),
        )
        .init();

    let cli = Cli::parse();
    // clap doesn't expand ~ for us; do it here so `--config ~/foo.toml`
    // and the default both resolve.
    let config_path = PathBuf::from(shellexpand::tilde(&cli.config.to_string_lossy()).into_owned());

    let resolved = ResolvedConfig::load(&config_path)?;
    // The allowlist goes in the startup line because it is the first thing
    // to check when a browser gets a 403 from a page that looks right: the
    // list here is normalized, so `https://x:443` in the file shows as
    // `https://x`, which is what the browser will actually send. The count
    // is its own field and the sample is capped, since nothing bounds how
    // many origins an operator states.
    let (public_origins_count, public_origins) =
        trawl_web::config::summarize_origins(&resolved.public_origins);
    tracing::info!(
        config = %config_path.display(),
        bind_addr = %resolved.bind_addr,
        upstream = %resolved.upstream_url,
        public_origins_count,
        public_origins = %public_origins,
        "loaded config"
    );

    let bind_addr = resolved.bind_addr.clone();
    let state = AppState::from_config(resolved)?;
    // Held for the life of the process; `None` under the platform roots.
    let _ca_reread = state.spawn_upstream_ca_reread(CA_REREAD_INTERVAL);
    let router = routes::build(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "trawl-web listening");
    axum::serve(listener, router).await?;
    Ok(())
}
