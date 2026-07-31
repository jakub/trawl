// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use clap::Parser;
use fleet_dev::cli::Cli;
use fleet_dev::command::SystemCommandRunner;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("fleet_dev=info")),
        )
        .init();
    let cli = Cli::parse();
    if let Err(error) = cli.validate_usage() {
        error.exit();
    }
    match fleet_dev::controller::run(cli, &SystemCommandRunner::default()).await {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("fleet-dev: {error}");
            std::process::exit(1);
        }
    }
}
