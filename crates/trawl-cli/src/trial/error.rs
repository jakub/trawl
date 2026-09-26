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
    #[error("`trawl trial {verb}` is not implemented in this build yet")]
    NotImplemented { verb: &'static str },

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
