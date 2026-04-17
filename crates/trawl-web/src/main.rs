// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web`: browser-facing session proxy binary.
//!
//! Reads the same `~/.config/trawl/config.toml` as trawld, looking at its
//! `[web]` section for proxy-specific settings.

use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use trawl_web::config::ResolvedConfig;
use trawl_web::routes;
use trawl_web::state::AppState;

#[derive(Parser, Debug)]
#[command(name = "trawl-web", about = "trawl browser-facing session proxy")]
struct Cli {
    /// Config file path. Defaults to `~/.config/trawl/config.toml`.
    #[arg(short, long, env = "TRAWL_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(|| {
        let expanded = shellexpand::tilde("~/.config/trawl/config.toml").into_owned();
        PathBuf::from(expanded)
    });

    let resolved = ResolvedConfig::load(&config_path)?;
    tracing::info!(
        config = %config_path.display(),
        bind_addr = %resolved.bind_addr,
        upstream = %resolved.upstream_url,
        "loaded config"
    );

    let bind_addr = resolved.bind_addr.clone();
    let state = AppState::from_config(resolved)?;
    let router = routes::build(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "trawl-web listening");
    axum::serve(listener, router).await?;
    Ok(())
}
