// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

/// Trawl-owned local Fleet development controller.
#[derive(Debug, Clone, Parser)]
#[command(name = "fleet-dev", version, about)]
pub struct Cli {
    /// Machine profile. An absent default profile uses portable conventions.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Absolute path to the required Trawl checkout.
    #[arg(long, global = true)]
    pub trawl_root: Option<PathBuf>,

    /// Invocation-only exposure override.
    #[arg(long, global = true)]
    pub exposure: Option<Exposure>,

    /// Invocation-only database-provider override.
    #[arg(long, global = true)]
    pub database: Option<DatabaseMode>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Prepare persistent development prerequisites.
    Setup {
        /// App to prepare; defaults to every discovered app.
        target: Option<Target>,
        /// Replace conflicting Tailscale Serve mappings after printing them.
        #[arg(long)]
        force: bool,
    },

    /// Inspect prerequisites without mutating local or remote state.
    Doctor {
        /// App to inspect; defaults to every discovered app.
        target: Option<Target>,
    },

    /// Render the selected stack without resolving secret values.
    Plan {
        target: Target,
        #[arg(long, value_enum, default_value_t = PlanFormat::Human)]
        format: PlanFormat,
    },

    /// Prepare and run one application.
    Dev(DevArgs),

    /// Prepare and run every discovered application.
    All(AllArgs),
}

#[derive(Debug, Clone, Args)]
pub struct DevArgs {
    pub app: App,

    /// Absolute path to the selected application checkout.
    #[arg(long)]
    pub app_root: PathBuf,

    /// Build the Trawl SPA with Trunk's release profile.
    #[arg(long)]
    pub release_spa: bool,
}

#[derive(Debug, Clone, Args)]
pub struct AllArgs {
    /// Explicit Coastwatch checkout; profile then sibling discovery follow.
    #[arg(long)]
    pub coastwatch_root: Option<PathBuf>,

    /// Build the Trawl SPA with Trunk's release profile.
    #[arg(long)]
    pub release_spa: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum App {
    Trawl,
    Coastwatch,
}

impl App {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trawl => "trawl",
            Self::Coastwatch => "coastwatch",
        }
    }
}

impl Cli {
    pub fn validate_usage(&self) -> std::result::Result<(), clap::Error> {
        if matches!(
            &self.command,
            Command::Dev(DevArgs {
                app: App::Coastwatch,
                release_spa: true,
                ..
            })
        ) {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                "--release-spa is valid only when the selection includes Trawl",
            ));
        }
        Ok(())
    }
}

impl std::fmt::Display for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Target {
    Trawl,
    Coastwatch,
    All,
}

impl Target {
    #[must_use]
    pub const fn app(self) -> Option<App> {
        match self {
            Self::Trawl => Some(App::Trawl),
            Self::Coastwatch => Some(App::Coastwatch),
            Self::All => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PlanFormat {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Exposure {
    #[default]
    Localhost,
    Tailscale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseMode {
    #[default]
    Docker,
    Cnpg,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_overrides_work_before_subcommand() {
        let cli = Cli::try_parse_from([
            "fleet-dev",
            "--exposure",
            "tailscale",
            "--database",
            "cnpg",
            "plan",
            "trawl",
        ])
        .unwrap();
        assert_eq!(cli.exposure, Some(Exposure::Tailscale));
        assert_eq!(cli.database, Some(DatabaseMode::Cnpg));
    }

    #[test]
    fn global_overrides_work_after_subcommand() {
        let cli = Cli::try_parse_from([
            "fleet-dev",
            "dev",
            "trawl",
            "--app-root",
            "/src/trawl",
            "--exposure",
            "tailscale",
            "--database",
            "cnpg",
        ])
        .unwrap();
        assert_eq!(cli.exposure, Some(Exposure::Tailscale));
        assert_eq!(cli.database, Some(DatabaseMode::Cnpg));
    }

    #[test]
    fn coastwatch_release_spa_is_a_clap_usage_error() {
        let cli = Cli::try_parse_from([
            "fleet-dev",
            "dev",
            "coastwatch",
            "--app-root",
            "/src/coastwatch",
            "--release-spa",
        ])
        .unwrap();
        let error = cli.validate_usage().unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
