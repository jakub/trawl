// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl trial`: a disposable installation the CLI owns (ADR-0045).

mod certificate;
mod compose;
mod docker;
mod error;
mod keys;
mod lifecycle;
mod lock;
mod ownership;
pub mod paths;
mod preflight;
pub mod profile;
mod render;
mod sample;
mod secrets;
mod state;

pub use error::TrialError;

pub const PROJECT: &str = "trawl-trial";

pub const LABEL_ID: &str = "sh.trawl.trial.id";

pub const PROFILE: &str = "trial";

pub const IMAGE_REPO: &str = "ghcr.io/jakub/trawl";

pub fn published_image_tag(version: &str) -> &str {
    version
        .split_once('+')
        .map_or(version, |(release, _build)| release)
}

pub fn default_image() -> String {
    format!(
        "{IMAGE_REPO}:{}",
        published_image_tag(env!("CARGO_PKG_VERSION"))
    )
}

pub const POSTGRES_IMAGE: &str = "postgres:18";

pub const DEFAULT_API_PORT: u16 = 15514;

pub const DEFAULT_WEB_PORT: u16 = 18090;

/// Name of the never-started container that claims the engine for one
/// trial. A second trial's create fails on the name conflict.
pub const CLAIM_NAME: &str = "trawl-trial-claim";

/// `trawl trial <verb>`.
#[derive(Debug, clap::Subcommand)]
pub enum TrialCommand {
    /// Create the trial, or resume it: PostgreSQL, trawld, and trawl-web on
    /// loopback, two keys, and sample data. Prints the addresses and token
    /// file paths, never a token.
    Up(UpArgs),

    /// Show the trial's state, addresses, image digests, certificate
    /// fingerprint, and sample range.
    Status,

    /// Print the operator token and one newline, nothing else.
    Key,

    /// Stop the trial's containers. The databases, keys, samples, and
    /// state stay, and a later `up` resumes.
    Stop,

    /// Delete every labelled trial container, volume, and network, then
    /// the trial directory.
    Down {
        /// Delete without asking (required when stdin is not a terminal).
        #[arg(long)]
        yes: bool,
    },
}

/// `trawl trial up` arguments. Ports and the image are fixed when the
/// trial is created: on resume an omitted flag means the recorded value.
/// A port is 1 to 65535: 0 would pass the bind test on an ephemeral port
/// and leave the trial on no port at all.
#[derive(Debug, clap::Args)]
pub struct UpArgs {
    #[arg(
        long,
        value_name = "PORT",
        value_parser = clap::value_parser!(u16).range(1..),
        help = format!("Loopback port for the HTTPS API [default: {DEFAULT_API_PORT}]")
    )]
    pub api_port: Option<u16>,

    #[arg(
        long,
        value_name = "PORT",
        value_parser = clap::value_parser!(u16).range(1..),
        help = format!("Loopback port for the browser UI [default: {DEFAULT_WEB_PORT}]")
    )]
    pub web_port: Option<u16>,

    /// Run this trawl image instead of the published one for this CLI
    /// version.
    #[arg(long, value_name = "REFERENCE")]
    pub image: Option<String>,

    /// Skip the sample events.
    #[arg(long)]
    pub no_sample_data: bool,
}

pub async fn run(cmd: &TrialCommand) -> Result<(), TrialError> {
    let paths = paths::TrialPaths::from_env()?;
    match cmd {
        TrialCommand::Up(args) => lifecycle::up(&paths, args).await,
        TrialCommand::Status => lifecycle::status(&paths).await,
        TrialCommand::Key => lifecycle::key(&paths),
        TrialCommand::Stop => lifecycle::stop(&paths).await,
        TrialCommand::Down { yes } => lifecycle::down(&paths, *yes).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `scripts/release/test_release_version.py` holds the release side's
    /// mapping to the same vectors, so the CLI's default image and the
    /// published tag cannot drift apart.
    #[test]
    fn the_published_tag_drops_build_metadata() {
        for (version, tag) in [
            ("1.1.0-rc.1+build.7", "1.1.0-rc.1"),
            ("0.9.0", "0.9.0"),
            ("1.0.0+abc", "1.0.0"),
            ("1.2.3-rc.1", "1.2.3-rc.1"),
        ] {
            assert_eq!(published_image_tag(version), tag, "{version}");
            // Docker's tag grammar: [A-Za-z0-9_][A-Za-z0-9_.-]{0,127}.
            assert!(
                tag.len() <= 128
                    && tag
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
                "{tag}"
            );
        }
    }
}
