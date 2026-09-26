// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl trial`: a disposable installation the CLI owns (ADR-0045).
//!
//! The trial keeps its host state in `$XDG_STATE_HOME/trawl/trial`
//! ([`paths`]), serializes its lifecycle verbs through a lock file beside
//! that directory ([`lock`]), records what it has done in `state.json`
//! ([`state`]), and serves the reserved `-p trial` profile ([`profile`]).
//!
//! The verbs dispatch before `config.toml` is read, so a broken or absent
//! client config never blocks `up` or `down`.

mod error;
mod lock;
pub mod paths;
pub mod profile;
mod state;

pub use error::TrialError;

/// Compose project name. One trial per Docker engine.
#[cfg_attr(not(test), expect(dead_code, reason = "the Compose renderer uses it"))]
pub const PROJECT: &str = "trawl-trial";

/// Label every trial container, network, and volume carries, valued with
/// the trial id.
#[expect(dead_code, reason = "the Compose renderer and ownership scan use it")]
pub const LABEL_ID: &str = "sh.trawl.trial.id";

/// The reserved profile name that reads the trial directory.
pub const PROFILE: &str = "trial";

/// Repository of the trawl image; the tag is the CLI version.
#[expect(dead_code, reason = "up resolves the image from it")]
pub const IMAGE_REPO: &str = "ghcr.io/jakub/trawl";

/// PostgreSQL image the trial runs.
#[cfg_attr(not(test), expect(dead_code, reason = "the Compose renderer uses it"))]
pub const POSTGRES_IMAGE: &str = "postgres:18";

/// Loopback port for the HTTPS API when `--api-port` is omitted.
pub const DEFAULT_API_PORT: u16 = 15514;

/// Loopback port for the browser UI when `--web-port` is omitted.
pub const DEFAULT_WEB_PORT: u16 = 18090;

/// Name of the never-started container that claims the engine for one
/// trial. A second trial's create fails on the name conflict.
#[expect(dead_code, reason = "up creates the claim and down removes it last")]
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
#[derive(Debug, clap::Args)]
pub struct UpArgs {
    #[arg(
        long,
        value_name = "PORT",
        help = format!("Loopback port for the HTTPS API [default: {DEFAULT_API_PORT}]")
    )]
    pub api_port: Option<u16>,

    #[arg(
        long,
        value_name = "PORT",
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

impl TrialCommand {
    fn verb(&self) -> &'static str {
        match self {
            Self::Up(_) => "up",
            Self::Status => "status",
            Self::Key => "key",
            Self::Stop => "stop",
            Self::Down { .. } => "down",
        }
    }
}

/// Run one trial verb.
///
/// The lifecycle behind the verbs lands in a later commit; until then every
/// verb refuses with a plain error instead of doing part of its job.
pub fn run(cmd: &TrialCommand) -> Result<(), TrialError> {
    Err(TrialError::NotImplemented { verb: cmd.verb() })
}
