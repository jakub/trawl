// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reads the same `~/.trawl/trawld.toml` as trawld, looking at its `[web]`
//! section. The CLI config at `~/.config/trawl/config.toml` has a different
//! schema and is NOT a valid input here.

use std::path::PathBuf;

use clap::{CommandFactory as _, FromArgMatches as _, Parser};
use tracing_subscriber::EnvFilter;
use trawl_web::config::ResolvedConfig;
use trawl_web::listener::{self, ConnectionTimeouts};
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
    // `--doctor=` is caught as well, so a spelling clap refuses is still
    // refused by the doctor's usage path, which quotes no value.
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

const DOCTOR_USAGE: &str = "Usage: trawl-web --doctor --config <PATH> [--format <table|json>]";

/// No tracing subscriber exists on this path, so a log line from shared
/// code goes nowhere.
fn doctor_main() -> u8 {
    let command = Cli::command().mut_args(|arg| arg.hide_env_values(true));
    let matches = match command.try_get_matches() {
        Ok(matches) => matches,
        Err(error) => return doctor_usage_error(&error),
    };
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

/// Report a refused `--doctor` command line without echoing any argument or
/// environment value: clap's own message quotes the value it refused.
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

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_ansi(trawl_config::color::stdout_ansi())
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(trawl_web::DEFAULT_LOG_FILTER)),
        )
        .init();

    let cli = Cli::parse();
    let config_path = PathBuf::from(shellexpand::tilde(&cli.config.to_string_lossy()).into_owned());

    let resolved = ResolvedConfig::load(&config_path)?;
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
    listener::serve(
        listener,
        router,
        ConnectionTimeouts::DEFAULT,
        shutdown_signal(),
    )
    .await;
    Ok(())
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %error, "cannot listen for SIGINT; shutdown waits for SIGTERM");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!(error = %error, "cannot listen for SIGTERM; shutdown waits for SIGINT");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => tracing::info!("SIGINT received; shutting down"),
        () = terminate => tracing::info!("SIGTERM received; shutting down"),
    }
}
