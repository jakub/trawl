// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ordinary policy: where the key goes for every command but `trawl
//! doctor`, `trawl mcp`, and `-p trial` (ADR-0065).
//!
//! A saved or exported key goes only to the API base it is bound to:
//!
//! - `[server].token` is bound to `[server].url`, whose default is
//!   `https://localhost:5514`.
//! - A profile's own `token` is bound to the profile's effective URL: its
//!   own `url`, else `[server].url`. A profile with no `token` leaves
//!   `[server].token` bound to `[server].url`.
//! - `TRAWL_TOKEN` is bound to `TRAWL_URL` when that is set, else to the
//!   saved base: the profile's effective URL, before `--url` or
//!   `TRAWL_URL` applies.
//!
//! The target is the first of a typed `--url`, `TRAWL_URL`, the profile's
//! `url`, and `[server].url`. A typed `--token` or `--token-file` goes to
//! the target, whatever it is. Any other key goes only to a target equal to
//! its binding ([`ApiBase::same_base`]).
//!
//! The first key source present is the one used: a typed `--token`, then
//! `--token-file`, then `TRAWL_TOKEN`, then the profile's own `token`, then
//! `[server].token`. One that is empty, unreadable, or bound elsewhere
//! fails the command; the next is never tried in its place, so a key the
//! user did not mean to send is never sent instead.
//!
//! Every URL in play is checked before any key is read or any file opened,
//! because a typed key goes to whatever the target turns out to be. Only
//! the selected profile's URL is in play; other profiles are not read.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use clap::CommandFactory as _;
use zeroize::Zeroizing;

use super::{
    ApiBase, ConnectionError, Key, Origin, Sources, UrlProblem, key_from_file, key_from_text,
};
use crate::cli::ConnectionParams;
use crate::config::{Config, ProfileConfig};

/// Labels for sources that clap does not name.
const SERVER_URL: &str = "url in [server]";
const SERVER_TOKEN: &str = "token in [server]";
const URL_FLAG: &str = "--url";
const TOKEN_FLAG: &str = "--token";

/// What the command line and the environment supplied, with where each
/// value came from.
pub(crate) struct Inputs {
    /// `--url` or `TRAWL_URL`, as clap merged them.
    url: Option<(String, Origin)>,
    /// `TRAWL_URL`'s value: clap's when the URL came from it, read
    /// separately when `--url` was typed beside it. clap reports only the
    /// winner, and an exported key stays bound to the exported URL.
    env_url: Option<OsString>,
    /// `--token` or `TRAWL_TOKEN`, as clap merged them.
    token: Option<(Zeroizing<String>, Origin)>,
    /// `--token-file`.
    token_file: Option<PathBuf>,
    /// The variable names clap binds to `--url` and `--token`.
    url_env: String,
    token_env: String,
}

/// Hand-written so the key never reaches a log line or a panic message.
impl fmt::Debug for Inputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inputs")
            .field("url", &self.url.as_ref().map(|(_, origin)| origin))
            .field("token", &self.token.as_ref().map(|(_, origin)| origin))
            .field("token_file", &self.token_file.is_some())
            .finish_non_exhaustive()
    }
}

impl Inputs {
    /// The global flags as clap parsed them, and where `--url` and
    /// `--token` came from. A value whose source is unknown counts as the
    /// less trusted one: a URL as typed, a key as exported.
    pub(crate) fn new(
        url: Option<String>,
        token: Option<String>,
        token_file: Option<PathBuf>,
        sources: Sources,
    ) -> Self {
        let url_env = bound_env("url");
        let token_env = bound_env("token");
        let url = url.map(|url| (url, sources.url.unwrap_or(Origin::CommandLine)));
        let env_url = match url {
            Some((ref value, Origin::Environment)) => Some(OsString::from(value)),
            _ => std::env::var_os(&url_env),
        };
        let token = token.map(|token| {
            (
                Zeroizing::new(token),
                sources.token.unwrap_or(Origin::Environment),
            )
        });
        Self {
            url,
            env_url,
            token,
            token_file,
            url_env,
            token_env,
        }
    }

    /// The error for a command that needs a key when none exists anywhere.
    pub(crate) fn no_credential(&self) -> ConnectionError {
        ConnectionError::NoCredential {
            token_env: self.token_env.clone(),
        }
    }
}

/// The environment variable clap binds to the global argument `id`.
fn bound_env(id: &str) -> String {
    crate::Cli::command()
        .get_arguments()
        .find(|arg| arg.get_id() == id)
        .and_then(clap::Arg::get_env)
        .map(|name| name.to_string_lossy().into_owned())
        .expect("clap binds a variable to --url and to --token")
}

/// A checked base and the source it is reported as.
#[derive(Debug)]
struct Located {
    base: ApiBase,
    label: String,
}

fn located(raw: &str, label: String) -> Result<Located, ConnectionError> {
    match ApiBase::parse(raw) {
        Ok(base) => Ok(Located { base, label }),
        Err(problem) => Err(ConnectionError::Url {
            input: label,
            problem,
        }),
    }
}

/// Every URL in play, each checked before any key is read.
#[derive(Debug)]
struct Bases {
    /// `[server].url`, or its default.
    server: Located,
    /// The selected profile's `url`.
    profile: Option<Located>,
    /// `TRAWL_URL`, whether or not it is the target.
    env: Option<Located>,
    /// A typed `--url`.
    typed: Option<Located>,
}

impl Bases {
    /// Check each URL in this order; the first that fails is reported.
    fn check(
        cfg: &Config,
        table: Option<(&str, &ProfileConfig)>,
        inputs: &Inputs,
    ) -> Result<Self, ConnectionError> {
        let server = located(&cfg.server.url, SERVER_URL.to_owned())?;
        let profile = match table {
            Some((name, ProfileConfig { url: Some(url), .. })) => {
                Some(located(url, format!("url in [profiles.{name}]"))?)
            }
            _ => None,
        };
        let env = match inputs.env_url {
            Some(ref raw) => {
                let label = inputs.url_env.clone();
                let Some(raw) = raw.to_str() else {
                    return Err(ConnectionError::Url {
                        input: label,
                        problem: UrlProblem::Malformed,
                    });
                };
                Some(located(raw, label)?)
            }
            None => None,
        };
        let typed = match inputs.url {
            Some((ref raw, Origin::CommandLine)) => Some(located(raw, URL_FLAG.to_owned())?),
            _ => None,
        };
        Ok(Self {
            server,
            profile,
            env,
            typed,
        })
    }

    /// The saved base: the profile's effective URL, before `--url` or
    /// `TRAWL_URL` applies.
    fn saved(&self) -> &Located {
        self.profile.as_ref().unwrap_or(&self.server)
    }

    /// Where the command goes: a typed `--url`, else `TRAWL_URL` when it
    /// gave the URL, else the saved base.
    fn target(&self, inputs: &Inputs) -> &Located {
        let exported = matches!(inputs.url, Some((_, Origin::Environment)));
        self.typed
            .as_ref()
            .or(self.env.as_ref().filter(|_| exported))
            .unwrap_or_else(|| self.saved())
    }
}

/// The key source the policy selected, before it is read.
enum Candidate<'a> {
    /// `--token`: goes to the target.
    Typed(&'a str),
    /// `--token-file`: goes to the target.
    File(&'a Path),
    /// A saved or exported key: goes only to its binding.
    Bound {
        text: &'a str,
        source: String,
        binding: &'a Located,
        empty: &'static str,
        next: String,
    },
}

impl<'a> Candidate<'a> {
    /// The first key source present, or `None` when there is none.
    fn select(
        cfg: &'a Config,
        table: Option<(&str, &'a ProfileConfig)>,
        inputs: &'a Inputs,
        bases: &'a Bases,
    ) -> Option<Self> {
        if let Some((ref token, Origin::CommandLine)) = inputs.token {
            return Some(Self::Typed(token));
        }
        if let Some(ref path) = inputs.token_file {
            return Some(Self::File(path));
        }
        if let Some((ref token, Origin::Environment)) = inputs.token {
            return Some(Self::Bound {
                text: token,
                source: inputs.token_env.clone(),
                binding: bases.env.as_ref().unwrap_or_else(|| bases.saved()),
                empty: "the variable is empty",
                next: format!("export the key in {}, or unset it", inputs.token_env),
            });
        }
        if let Some((name, table)) = table
            && let Some(ref token) = table.token
        {
            let source = format!("token in [profiles.{name}]");
            return Some(Self::Bound {
                text: token,
                next: format!("set the key as {source}, or remove the token line"),
                source,
                binding: bases.saved(),
                empty: "the profile's token is empty",
            });
        }
        let token = cfg.server.token.as_deref()?;
        Some(Self::Bound {
            text: token,
            source: SERVER_TOKEN.to_owned(),
            binding: &bases.server,
            empty: "the token is empty",
            next: format!("set the key as {SERVER_TOKEN}, or remove the token line"),
        })
    }

    /// Read the key, after checking that a bound one may go to `target`.
    fn read(self, target: &Located) -> Result<Key, ConnectionError> {
        match self {
            Self::Typed(text) => text_key(
                text,
                TOKEN_FLAG,
                "the key is empty",
                "give the key after --token, or name a file that holds it with --token-file",
            ),
            Self::File(path) => Ok(key_from_file(path)?),
            Self::Bound {
                text,
                source,
                binding,
                empty,
                next,
            } => {
                if !binding.base.same_base(&target.base) {
                    return Err(ConnectionError::Binding {
                        credential: source,
                        target: target.label.clone(),
                    });
                }
                text_key(text, &source, empty, &next)
            }
        }
    }
}

/// Resolve the connection for an ordinary command.
///
/// `cfg` is the config as loaded, with the profile's trust applied and its
/// `url` and `token` left in their tables. `profile` is the selected
/// profile, never `trial`. `Ok(None)` means no key exists anywhere.
pub(crate) fn ordinary(
    cfg: &Config,
    profile: Option<&str>,
    inputs: &Inputs,
) -> Result<Option<ConnectionParams>, ConnectionError> {
    if matches!(inputs.token, Some((_, Origin::CommandLine))) && inputs.token_file.is_some() {
        return Err(ConnectionError::BothKeyFlags);
    }
    let table = profile
        .map(|name| cfg.profile(name).map(|table| (name, table)))
        .transpose()?;
    let bases = Bases::check(cfg, table, inputs)?;
    let target = bases.target(inputs);
    let Some(candidate) = Candidate::select(cfg, table, inputs, &bases) else {
        return Ok(None);
    };
    let key = candidate.read(target)?;
    Ok(Some(ConnectionParams {
        url: target.base.request_base(),
        token: key.secret().to_owned(),
        trust: cfg.tls_trust()?,
    }))
}

/// A trimmed, non-empty key from `input`, or the failure that names it.
fn text_key(text: &str, input: &str, empty: &str, next: &str) -> Result<Key, ConnectionError> {
    key_from_text(text, input).ok_or_else(|| ConnectionError::Key {
        input: input.to_owned(),
        reason: empty.to_owned(),
        next: next.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use trawl_client::TlsTrust;

    use super::*;

    const T: &str = "flt_servertoken";
    const K: &str = "flt_envtoken";
    const K2: &str = "flt_typedtoken";

    fn inputs() -> Inputs {
        Inputs {
            url: None,
            env_url: None,
            token: None,
            token_file: None,
            url_env: "TRAWL_URL".to_owned(),
            token_env: "TRAWL_TOKEN".to_owned(),
        }
    }

    fn typed_url(url: &str) -> Inputs {
        Inputs {
            url: Some((url.to_owned(), Origin::CommandLine)),
            ..inputs()
        }
    }

    fn exported_url(url: &str) -> Inputs {
        Inputs {
            url: Some((url.to_owned(), Origin::Environment)),
            env_url: Some(url.into()),
            ..inputs()
        }
    }

    fn with_token(mut inputs: Inputs, token: &str, origin: Origin) -> Inputs {
        inputs.token = Some((Zeroizing::new(token.to_owned()), origin));
        inputs
    }

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    /// Run the policy as `run_configured` does: the profile's trust
    /// applied first.
    fn resolve(
        cfg: &Config,
        profile: Option<&str>,
        inputs: &Inputs,
    ) -> Result<Option<ConnectionParams>, ConnectionError> {
        let mut cfg = cfg.clone();
        if let Some(name) = profile {
            cfg.apply_profile(name).unwrap();
        }
        ordinary(&cfg, profile, inputs)
    }

    fn sent(result: Result<Option<ConnectionParams>, ConnectionError>) -> (String, String) {
        let conn = result.expect("resolves").expect("a key");
        (conn.url, conn.token)
    }

    fn refused(result: Result<Option<ConnectionParams>, ConnectionError>) -> (String, String) {
        match result {
            Err(ConnectionError::Binding { credential, target }) => (credential, target),
            other => panic!("expected a binding refusal, got {other:?}"),
        }
    }

    const SERVER: &str =
        "[server]\nurl = \"https://a.example:5514\"\ntoken = \"flt_servertoken\"\n";

    /// AC 3: a profile keeps `[server].token` only for a base equal to
    /// `[server].url`, and its own token goes to its own effective URL.
    #[test]
    fn ordinary_profile_credentials_keep_their_binding() {
        let cfg = config(&format!(
            "{SERVER}
[profiles.dev]
insecure = true

[profiles.lab]
url = \"https://lab.example:5514\"

[profiles.same]
url = \"HTTPS://A.Example:5514/\"

[profiles.own]
token = \"flt_own\"

[profiles.ownlab]
url = \"https://lab.example:5514/trawl\"
token = \"flt_ownlab\"
"
        ));
        let a = "https://a.example:5514".to_owned();

        // A trust-only profile sends T to A, under its own trust.
        let dev = resolve(&cfg, Some("dev"), &inputs()).unwrap().unwrap();
        assert_eq!(
            (dev.url.clone(), dev.token.clone()),
            (a.clone(), T.to_owned())
        );
        assert_eq!(dev.trust, TlsTrust::AcceptInvalid);

        // A profile pointing at another server does not take T there.
        assert_eq!(
            refused(resolve(&cfg, Some("lab"), &inputs())),
            (
                "token in [server]".to_owned(),
                "url in [profiles.lab]".to_owned()
            )
        );

        // The same base spelled differently is the same base.
        assert_eq!(
            sent(resolve(&cfg, Some("same"), &inputs())),
            (a.clone(), T.to_owned())
        );

        // A profile's own token with no url goes to [server].url.
        assert_eq!(
            sent(resolve(&cfg, Some("own"), &inputs())),
            (a.clone(), "flt_own".to_owned())
        );
        // ... and its own url when it has one, never falling back to T.
        assert_eq!(
            sent(resolve(&cfg, Some("ownlab"), &inputs())),
            (
                "https://lab.example:5514/trawl".to_owned(),
                "flt_ownlab".to_owned()
            )
        );
        assert_eq!(
            refused(resolve(
                &cfg,
                Some("ownlab"),
                &typed_url("https://a.example:5514")
            )),
            ("token in [profiles.ownlab]".to_owned(), "--url".to_owned())
        );

        // --url B moves even a trust-only profile's target; T stays.
        assert_eq!(
            refused(resolve(
                &cfg,
                Some("dev"),
                &typed_url("https://lab.example:5514")
            )),
            ("token in [server]".to_owned(), "--url".to_owned())
        );

        // An unknown profile is the config's own error.
        assert!(matches!(
            ordinary(&cfg, Some("absent"), &inputs()),
            Err(ConnectionError::Config(_))
        ));
    }

    /// The policy's rules, row by row, without a subprocess.
    #[test]
    fn ordinary_selection_and_binding() {
        let cfg = config(SERVER);
        let a = "https://a.example:5514".to_owned();
        let b = "https://b.example:5514";

        assert_eq!(
            sent(resolve(&cfg, None, &inputs())),
            (a.clone(), T.to_owned())
        );
        assert_eq!(
            refused(resolve(&cfg, None, &typed_url(b))),
            ("token in [server]".to_owned(), "--url".to_owned())
        );
        assert_eq!(
            refused(resolve(&cfg, None, &exported_url(b))),
            ("token in [server]".to_owned(), "TRAWL_URL".to_owned())
        );
        // A default port and a trailing / are the same base.
        assert_eq!(
            sent(resolve(&cfg, None, &typed_url("https://A.EXAMPLE:5514/"))),
            (a.clone(), T.to_owned())
        );

        // TRAWL_TOKEN binds to the saved base, or to TRAWL_URL.
        let env_k = with_token(typed_url(b), K, Origin::Environment);
        assert_eq!(
            refused(resolve(&cfg, None, &env_k)),
            ("TRAWL_TOKEN".to_owned(), "--url".to_owned())
        );
        let exported = with_token(exported_url(b), K, Origin::Environment);
        assert_eq!(
            sent(resolve(&cfg, None, &exported)),
            (b.to_owned(), K.to_owned())
        );
        // Bound to TRAWL_URL, K is refused at a typed --url A, and T under
        // it is not tried.
        let mut shadowed = with_token(typed_url(&a), K, Origin::Environment);
        shadowed.env_url = Some(b.into());
        assert_eq!(
            refused(resolve(&cfg, None, &shadowed)),
            ("TRAWL_TOKEN".to_owned(), "--url".to_owned())
        );

        // A typed key goes to the target, whatever binds the others.
        let typed = with_token(typed_url(b), K2, Origin::CommandLine);
        assert_eq!(
            sent(resolve(&cfg, None, &typed)),
            (b.to_owned(), K2.to_owned())
        );

        // No key anywhere is not a refusal.
        let keyless = config("[server]\nurl = \"https://a.example:5514\"\n");
        assert!(resolve(&keyless, None, &typed_url(b)).unwrap().is_none());
        // With nothing saved, TRAWL_TOKEN binds to the default base.
        let default = with_token(inputs(), K, Origin::Environment);
        assert_eq!(
            sent(resolve(&Config::default(), None, &default)),
            ("https://localhost:5514".to_owned(), K.to_owned())
        );
    }

    /// A key source that is selected is used or fails: an empty one never
    /// hands over to the next.
    #[test]
    fn ordinary_key_sources_never_fall_through() {
        let cfg = config(SERVER);
        let a = "https://a.example:5514";
        let empty = |result: Result<Option<ConnectionParams>, ConnectionError>| match result {
            Err(ConnectionError::Key { input, .. }) => input,
            other => panic!("expected a key failure, got {other:?}"),
        };
        assert_eq!(
            empty(resolve(
                &cfg,
                None,
                &with_token(inputs(), " ", Origin::CommandLine)
            )),
            "--token"
        );
        assert_eq!(
            empty(resolve(
                &cfg,
                None,
                &with_token(inputs(), "", Origin::Environment)
            )),
            "TRAWL_TOKEN"
        );
        let blank = config(&format!(
            "[server]\nurl = \"{a}\"\ntoken = \" \"\n\n[profiles.p]\ntoken = \"\"\n"
        ));
        assert_eq!(empty(resolve(&blank, None, &inputs())), "token in [server]");
        assert_eq!(
            empty(resolve(&blank, Some("p"), &inputs())),
            "token in [profiles.p]"
        );

        let dir = tempfile::tempdir().unwrap();
        let mut file = with_token(inputs(), K, Origin::Environment);
        file.token_file = Some(dir.path().join("absent"));
        assert_eq!(empty(resolve(&cfg, None, &file)), "key from --token-file");

        let mut both = with_token(inputs(), K2, Origin::CommandLine);
        both.token_file = Some(dir.path().join("absent"));
        assert!(matches!(
            resolve(&cfg, None, &both),
            Err(ConnectionError::BothKeyFlags)
        ));
        // An exported key beside --token-file is no conflict: the file wins.
        std::fs::write(dir.path().join("key"), "flt_file\n").unwrap();
        file.token_file = Some(dir.path().join("key"));
        assert_eq!(sent(resolve(&cfg, None, &file)).1, "flt_file");
    }

    /// AC 5: a URL with userinfo, a query, a fragment, an encoded `?` or
    /// `#`, a backslash, whitespace, or another scheme is refused from
    /// each source before any key file is opened, and the message names
    /// the source, never the URL.
    #[test]
    fn connection_url_rejections_name_each_source() {
        let bad = [
            "https://alice:s3cret@h.example:1",
            "https://h.example:1/x?s3cret=1",
            "https://h.example:1/x#s3cret",
            "https://h.example:1/x%3Fs3cret",
            "https://h.example:1/x%23s3cret",
            "https://h.example:1/s3cret\\x",
            "https://h.example:1/s3 cret",
            "ftp://h.example:1/s3cret",
        ];
        let good = "https://a.example:5514";
        // Opening this would fail the run as a key failure, so a URL
        // failure proves the URL was checked first.
        let mut keyed = with_token(inputs(), K, Origin::Environment);
        keyed.token_file = Some("/nonexistent/s3cret/key".into());
        for raw in bad {
            let cases: [(&str, Config, Option<&str>, Inputs); 4] = [
                (
                    "url in [server]",
                    config(&format!("[server]\nurl = {raw:?}\n")),
                    None,
                    clone_inputs(&keyed),
                ),
                (
                    "url in [profiles.p]",
                    config(&format!(
                        "[server]\nurl = {good:?}\n[profiles.p]\nurl = {raw:?}\n"
                    )),
                    Some("p"),
                    clone_inputs(&keyed),
                ),
                (
                    "TRAWL_URL",
                    config(&format!("[server]\nurl = {good:?}\n")),
                    None,
                    Inputs {
                        env_url: Some(raw.into()),
                        ..clone_inputs(&keyed)
                    },
                ),
                (
                    "--url",
                    config(&format!("[server]\nurl = {good:?}\n")),
                    None,
                    Inputs {
                        url: Some((raw.to_owned(), Origin::CommandLine)),
                        ..clone_inputs(&keyed)
                    },
                ),
            ];
            for (label, cfg, profile, inputs) in cases {
                let err = resolve(&cfg, profile, &inputs).expect_err(raw);
                assert!(
                    matches!(err, ConnectionError::Url { ref input, .. } if input == label),
                    "{raw} from {label}: {err:?}"
                );
                let message = err.to_string();
                assert!(message.starts_with(label), "{message}");
                for leak in ["s3cret", "alice", "h.example"] {
                    assert!(!message.contains(leak), "{raw}: {message}");
                }
            }
        }
    }

    fn clone_inputs(inputs: &Inputs) -> Inputs {
        Inputs {
            url: inputs.url.clone(),
            env_url: inputs.env_url.clone(),
            token: inputs
                .token
                .as_ref()
                .map(|(token, origin)| (token.clone(), *origin)),
            token_file: inputs.token_file.clone(),
            url_env: inputs.url_env.clone(),
            token_env: inputs.token_env.clone(),
        }
    }

    /// The refusal names both sources and both remedies, and none of the
    /// key, the URLs, or the path.
    #[test]
    fn a_binding_refusal_names_sources_and_remedies_only() {
        let message = ConnectionError::Binding {
            credential: "token in [server]".to_owned(),
            target: "--url".to_owned(),
        }
        .to_string();
        for needed in ["token in [server]", "--url", "--token ", "--token-file"] {
            assert!(message.contains(needed), "{message}");
        }
        let missing = inputs().no_credential().to_string();
        assert!(missing.contains("--token-file"), "{missing}");
        assert!(missing.contains("TRAWL_TOKEN"), "{missing}");
    }

    /// The labels for the URL and token variables are the names clap
    /// binds, and Debug never prints a key.
    #[test]
    fn inputs_take_their_names_from_clap() {
        let inputs = Inputs::new(
            None,
            Some(K.to_owned()),
            None,
            Sources {
                url: None,
                token: Some(Origin::Environment),
            },
        );
        assert_eq!(inputs.url_env, "TRAWL_URL");
        assert_eq!(inputs.token_env, "TRAWL_TOKEN");
        assert!(!format!("{inputs:?}").contains(K));
        // An unknown source is the less trusted one.
        let unknown = Inputs::new(
            Some("https://h".to_owned()),
            Some(K.to_owned()),
            None,
            Sources::default(),
        );
        assert!(matches!(unknown.url, Some((_, Origin::CommandLine))));
        assert!(matches!(unknown.token, Some((_, Origin::Environment))));
    }
}
