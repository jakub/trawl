// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every way a trial verb or the `-p trial` profile can fail.
//!
//! No variant carries a token, a key prefix, or the contents of a state
//! file: a parse failure reports its position only, because the offending
//! value could be a secret.

use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum TrialError {
    #[error(
        "cannot place the trial state: XDG_STATE_HOME is unset, empty, or relative, \
         and HOME is not an absolute path"
    )]
    NoStateHome,

    #[error("refusing {}: it is a symbolic link, and trial state lives only in a real directory", path.display())]
    Symlink { path: PathBuf },

    #[error("refusing {}: it is not a directory", path.display())]
    NotADirectory { path: PathBuf },

    #[error("refusing {}: it is owned by uid {owner}, not by uid {me}", path.display())]
    ForeignOwner { path: PathBuf, owner: u32, me: u32 },

    #[error(
        "refusing {}: its mode is {mode:04o}, and group and other must have no access \
         (chmod go= {})",
        path.display(),
        path.display()
    )]
    LooseMode { path: PathBuf, mode: u32 },

    #[error("refusing {}: {reason}", path.display())]
    NotPrivateFile { path: PathBuf, reason: &'static str },

    #[error("failed to {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },

    #[error(
        "{} holds trial state schema {found}, and this trawl reads schema {expected}. \
         `trawl trial down` deletes that trial; `trawl trial up` then starts a new one",
        path.display()
    )]
    StateSchema {
        path: PathBuf,
        found: u32,
        expected: u32,
    },

    #[error("{} is not valid trial state ({detail})", path.display())]
    StateInvalid { path: PathBuf, detail: String },

    #[error(
        "-p trial reads its URL and token from the trial, but {what}. \
         Unset it, or drop -p trial"
    )]
    ProfileOverride { what: &'static str },

    #[error(
        "{config} defines [profiles.trial], and the profile name `trial` is reserved \
         for `trawl trial`. Rename that profile"
    )]
    ProfileInConfig { config: String },

    #[error("-p trial: no trial in {}. Start one with `trawl trial up`", dir.display())]
    NoTrial { dir: PathBuf },

    #[error(
        "-p trial: the trial in {} has no {missing} yet. Finish it with `trawl trial up`",
        dir.display()
    )]
    TrialNotReady { dir: PathBuf, missing: &'static str },

    #[error("no trial in {}. Start one with `trawl trial up`", dir.display())]
    NotCreated { dir: PathBuf },

    #[error(
        "the trial in {} has no operator token yet. Finish it with `trawl trial up`",
        dir.display()
    )]
    NoOperatorToken { dir: PathBuf },

    #[error(
        "Docker resources carry the trial's names or labels, but {} holds no trial \
         state:\n{listing}\nAnother user or state directory made them, or this trial's \
         state was deleted. trawl acts only on a trial it has state for: if they are \
         leftovers, remove them yourself (docker rm, docker volume rm, docker network rm)",
        dir.display()
    )]
    Orphaned { dir: PathBuf, listing: String },

    #[error(
        "port {port} on 127.0.0.1 is not free ({reason}). Stop what holds it, or \
         choose another port with {flag}"
    )]
    PortTaken {
        port: u16,
        flag: &'static str,
        reason: String,
    },

    #[error("--api-port and --web-port are both {port}; give them different ports")]
    SamePort { port: u16 },

    #[error(
        "the trial was created with {flag} {recorded}, and {flag} is fixed when the trial \
         is created. Omit {flag} to resume, or delete the trial with `trawl trial down` \
         and create it again"
    )]
    FixedAtCreation {
        flag: &'static str,
        recorded: String,
    },

    #[error(
        "the trial was created on Docker engine {recorded}, and this is engine {found}. \
         A trial resumes only on its own engine: switch back to it, or delete the trial \
         with `trawl trial down` on that engine"
    )]
    EngineChanged { recorded: String, found: String },

    #[error(
        "{reference} is now image {found}, but the trial recorded {recorded}. The trial \
         resumes only on the images it was created with: restore that image, or delete \
         the trial with `trawl trial down` and create it again"
    )]
    ImageChanged {
        reference: String,
        recorded: String,
        found: String,
    },

    #[error(
        "the trial's container {container} runs image {found}, not the recorded {recorded}. \
         Delete the trial with `trawl trial down` and create it again"
    )]
    ContainerImageChanged {
        container: String,
        recorded: String,
        found: String,
    },

    #[error(
        "the image {reference} the trial recorded is no longer on this engine. Restore it, \
         or delete the trial with `trawl trial down` and create it again"
    )]
    ImageGone { reference: String },

    #[error("`docker image inspect {reference}` printed output trawl cannot read")]
    ImageUnreadable { reference: String },

    #[error(
        "trial one-off containers are still running: {names}. They belong to an \
         interrupted `trawl trial` command. Wait for them to finish, or stop them with \
         `docker stop`, then run the command again"
    )]
    OneoffsRunning { names: String },

    #[error("fleet-admin: {what}")]
    Fleet { what: String },

    #[error(transparent)]
    KeysTable(#[from] super::keys::KeysTableError),

    #[error("the trial certificate is not usable: {reason}")]
    Certificate { reason: &'static str },

    #[error("the trial API at {url} did not answer: {source}")]
    Api {
        url: String,
        source: trawl_client::ClientError,
    },

    #[error("the {key} key is not what the trial minted: {problem}")]
    Identity { key: &'static str, problem: String },

    #[error(
        "{source}. If Docker reported a port in use, another program holds 127.0.0.1:{api} \
         or 127.0.0.1:{web}: stop it, or delete the trial with `trawl trial down` and \
         create it again with --api-port or --web-port"
    )]
    ServicesFailed {
        source: super::docker::DockerError,
        api: u16,
        web: u16,
    },

    #[error(
        "`trawl trial down` deletes only after confirmation, and stdin is not a terminal. \
         Run `trawl trial down --yes` to delete without asking; nothing was deleted"
    )]
    ConfirmationRequired,

    #[error("not deleted: the answer was not yes")]
    Declined,

    #[error(transparent)]
    Preflight(#[from] super::preflight::PreflightError),

    #[error(transparent)]
    Docker(#[from] super::docker::DockerError),

    #[error(transparent)]
    Ownership(#[from] super::ownership::OwnershipError),
}

impl TrialError {
    pub(crate) fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}
