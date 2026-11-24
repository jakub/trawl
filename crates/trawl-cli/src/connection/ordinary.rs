// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ordinary policy: where the key goes for every command but `trawl
//! doctor`, `trawl mcp`, and `-p trial` (ADR-0065).

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

const SERVER_URL: &str = "url in [server]";
const SERVER_TOKEN: &str = "token in [server]";
const URL_FLAG: &str = "--url";
const TOKEN_FLAG: &str = "--token";

pub(crate) struct Inputs {
    url: Option<(String, Origin)>,
    /// `TRAWL_URL`'s value: clap's when the URL came from it, read
    /// separately when `--url` was typed beside it. clap reports only the
    /// winner, and an exported key stays bound to the exported URL.
    env_url: Option<OsString>,
    token: Option<(Zeroizing<String>, Origin)>,
    token_file: Option<PathBuf>,
    url_env: String,
    token_env: String,
}

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

    /// `--token` typed beside `--token-file` is a usage error. `run`
    /// refuses it before every command but `doctor` and `mcp`; the policy
    /// checks again so it never depends on its caller.
    pub(crate) fn key_flags(&self) -> Result<(), ConnectionError> {
        if matches!(self.token, Some((_, Origin::CommandLine))) && self.token_file.is_some() {
            return Err(ConnectionError::BothKeyFlags);
        }
        Ok(())
    }

    pub(crate) fn no_credential(&self) -> ConnectionError {
        ConnectionError::NoCredential {
            token_env: self.token_env.clone(),
        }
    }
}

fn bound_env(id: &str) -> String {
    crate::Cli::command()
        .get_arguments()
        .find(|arg| arg.get_id() == id)
        .and_then(clap::Arg::get_env)
        .map(|name| name.to_string_lossy().into_owned())
        .expect("clap binds a variable to --url and to --token")
}

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

#[derive(Debug)]
struct Bases {
    server: Located,
    profile: Option<Located>,
    env: Option<Located>,
    typed: Option<Located>,
}

impl Bases {
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

    fn saved(&self) -> &Located {
        self.profile.as_ref().unwrap_or(&self.server)
    }

    fn target(&self, inputs: &Inputs) -> &Located {
        let exported = matches!(inputs.url, Some((_, Origin::Environment)));
        self.typed
            .as_ref()
            .or(self.env.as_ref().filter(|_| exported))
            .unwrap_or_else(|| self.saved())
    }
}

enum Candidate<'a> {
    Typed(&'a str),
    File(&'a Path),
    Bound {
        text: &'a str,
        source: String,
        binding: &'a Located,
        empty: &'static str,
        next: String,
    },
}

impl<'a> Candidate<'a> {
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

/// `cfg` is the config as loaded, with the profile's trust applied and its
/// `url` and `token` left in their tables. `profile` is the selected
/// profile, never `trial`. `Ok(None)` means no key exists anywhere.
pub(crate) fn ordinary(
    cfg: &Config,
    profile: Option<&str>,
    inputs: &Inputs,
) -> Result<Option<ConnectionParams>, ConnectionError> {
    inputs.key_flags()?;
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

        let dev = resolve(&cfg, Some("dev"), &inputs()).unwrap().unwrap();
        assert_eq!(
            (dev.url.clone(), dev.token.clone()),
            (a.clone(), T.to_owned())
        );
        assert_eq!(dev.trust, TlsTrust::AcceptInvalid);

        assert_eq!(
            refused(resolve(&cfg, Some("lab"), &inputs())),
            (
                "token in [server]".to_owned(),
                "url in [profiles.lab]".to_owned()
            )
        );

        assert_eq!(
            sent(resolve(&cfg, Some("same"), &inputs())),
            (a.clone(), T.to_owned())
        );

        assert_eq!(
            sent(resolve(&cfg, Some("own"), &inputs())),
            (a.clone(), "flt_own".to_owned())
        );
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

        assert_eq!(
            refused(resolve(
                &cfg,
                Some("dev"),
                &typed_url("https://lab.example:5514")
            )),
            ("token in [server]".to_owned(), "--url".to_owned())
        );

        assert!(matches!(
            ordinary(&cfg, Some("absent"), &inputs()),
            Err(ConnectionError::Config(_))
        ));
    }

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
        assert_eq!(
            sent(resolve(&cfg, None, &typed_url("https://A.EXAMPLE:5514/"))),
            (a.clone(), T.to_owned())
        );

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
        let mut shadowed = with_token(typed_url(&a), K, Origin::Environment);
        shadowed.env_url = Some(b.into());
        assert_eq!(
            refused(resolve(&cfg, None, &shadowed)),
            ("TRAWL_TOKEN".to_owned(), "--url".to_owned())
        );

        let typed = with_token(typed_url(b), K2, Origin::CommandLine);
        assert_eq!(
            sent(resolve(&cfg, None, &typed)),
            (b.to_owned(), K2.to_owned())
        );

        let keyless = config("[server]\nurl = \"https://a.example:5514\"\n");
        assert!(resolve(&keyless, None, &typed_url(b)).unwrap().is_none());
        let default = with_token(inputs(), K, Origin::Environment);
        assert_eq!(
            sent(resolve(&Config::default(), None, &default)),
            ("https://localhost:5514".to_owned(), K.to_owned())
        );
    }

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
        std::fs::write(dir.path().join("key"), "flt_file\n").unwrap();
        file.token_file = Some(dir.path().join("key"));
        assert_eq!(sent(resolve(&cfg, None, &file)).1, "flt_file");
    }

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
