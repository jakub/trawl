// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The reserved `-p trial` profile.
//!
//! It overlays the trial's URL, operator token, and certificate onto the
//! in-memory [`Config`], in place of a named profile, so every command and
//! the TUI connect through the same path as any other profile. It never
//! writes `config.toml`.
//!
//! Anything else that could point the command somewhere else is refused
//! rather than silently losing or winning: the variables clap binds to
//! `--url` and `--token` (even empty), `--url`, `--token` or `--token-file`,
//! and a `[profiles.trial]` of the user's own. The overlay turns `insecure`
//! off and pins the trial's `ca.pem`, so `--insecure` or its variable then
//! fails through the `ca_cert`-with-`insecure` rule.
//!
//! Variable names come from clap's metadata, through [`env_rules`]: each
//! argument clap binds to a variable has an [`EnvRule`], and one without a
//! rule is refused when set.
//!
//! The certificate is read once, here, through the same checked read as
//! the token, and the connection pins those bytes. No pathname is handed
//! on for a later step to open again.

use clap::CommandFactory as _;

use crate::config::Config;

use super::TrialError;
use super::paths::{TrialPaths, read_private, read_public};
use super::state::TrialState;

/// A token file holds the token and one newline.
const MAX_TOKEN_BYTES: u64 = 4096;

/// `ca.pem` holds one self-signed certificate: a few kilobytes.
const MAX_CA_BYTES: u64 = 64 * 1024;

/// What `-p trial`, and `trawl trial status`'s warnings, do about one
/// argument clap binds to an environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvRule {
    /// The variable moves the connection when set at all, even empty:
    /// `-p trial` refuses it.
    RefuseWhenSet,
    /// The variable is a switch, and moves the connection when the argument
    /// parser reads it as on: `-p trial` then refuses through the
    /// `ca_cert`-with-`insecure` rule.
    RefuseWhenOn,
    /// The variable selects the profile: one other than `trial` sends
    /// commands without -p elsewhere.
    SelectsProfile,
}

/// The rule for the clap argument `id`, or `None` when it has none.
pub fn env_rule(id: &str) -> Option<EnvRule> {
    match id {
        "url" | "token" => Some(EnvRule::RefuseWhenSet),
        "insecure" => Some(EnvRule::RefuseWhenOn),
        "profile" => Some(EnvRule::SelectsProfile),
        _ => None,
    }
}

/// `(argument id, variable name)` for every argument clap binds to an
/// environment variable, anywhere in the command tree, once each, in
/// clap's order.
pub fn bound_env() -> Vec<(String, String)> {
    fn walk(command: &clap::Command, out: &mut Vec<(String, String)>) {
        for arg in command.get_arguments() {
            if let Some(name) = arg.get_env() {
                let pair = (
                    arg.get_id().as_str().to_owned(),
                    name.to_string_lossy().into_owned(),
                );
                if !out.contains(&pair) {
                    out.push(pair);
                }
            }
        }
        for subcommand in command.get_subcommands() {
            walk(subcommand, out);
        }
    }
    let mut command = crate::Cli::command();
    command.build();
    let mut out = Vec::new();
    walk(&command, &mut out);
    out
}

/// `(variable name, rule)` for every variable clap binds. A variable whose
/// argument has no rule is refused when set: a new binding fails closed.
pub fn env_rules() -> Vec<(String, EnvRule)> {
    bound_env()
        .into_iter()
        .map(|(id, name)| (name, env_rule(&id).unwrap_or(EnvRule::RefuseWhenSet)))
        .collect()
}

/// Connection settings the invocation supplied besides the profile.
///
/// clap merges a flag and its environment variable into one value, so the
/// environment is read separately. clap passes a variable set to the empty
/// string through as a value, and here it counts as set too.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// Each variable `-p trial` refuses that is set, in clap's order.
    pub env_set: Vec<String>,
    /// `--url`, or clap's merge of its variable.
    pub url_arg: bool,
    /// `--token`, or clap's merge of its variable.
    pub token_arg: bool,
    /// `--token-file`, which has no variable.
    pub token_file_arg: bool,
}

impl Overrides {
    /// Observe this process: the `_arg`s come from the parsed command
    /// line, the rest from the environment. A variable that is set to the
    /// empty string counts as set.
    pub fn observe(url_arg: bool, token_arg: bool, token_file_arg: bool) -> Self {
        Self::observe_with(url_arg, token_arg, token_file_arg, |name| {
            std::env::var_os(name).is_some()
        })
    }

    /// [`Overrides::observe`], with `set` answering whether a variable is
    /// present.
    fn observe_with(
        url_arg: bool,
        token_arg: bool,
        token_file_arg: bool,
        set: impl Fn(&str) -> bool,
    ) -> Self {
        Self {
            env_set: env_rules()
                .into_iter()
                .filter(|(name, rule)| *rule == EnvRule::RefuseWhenSet && set(name))
                .map(|(name, _)| name)
                .collect(),
            url_arg,
            token_arg,
            token_file_arg,
        }
    }

    fn refusal(self) -> Option<String> {
        if let Some(name) = self.env_set.into_iter().next() {
            Some(format!("{name} is set"))
        } else if self.url_arg {
            Some("--url was given".to_owned())
        } else if self.token_arg {
            Some("--token was given".to_owned())
        } else if self.token_file_arg {
            Some("--token-file was given".to_owned())
        } else {
            None
        }
    }
}

/// Overlay the trial onto `cfg`.
///
/// `config_path` names the config file in the `[profiles.trial]` refusal.
pub fn apply(
    cfg: &mut Config,
    overrides: Overrides,
    config_path: &str,
    paths: &TrialPaths,
) -> Result<(), TrialError> {
    if let Some(what) = overrides.refusal() {
        return Err(TrialError::ProfileOverride { what });
    }
    if cfg.profiles.contains_key(super::PROFILE) {
        return Err(TrialError::ProfileInConfig {
            config: config_path.to_owned(),
        });
    }

    let no_trial = || TrialError::NoTrial {
        dir: paths.dir.clone(),
    };
    if !paths.check_dir()? {
        return Err(no_trial());
    }
    let state = TrialState::load(&paths.state_file())?.ok_or_else(no_trial)?;

    let not_ready = |missing| TrialError::TrialNotReady {
        dir: paths.dir.clone(),
        missing,
    };
    let ca = read_public(&paths.ca_file(), MAX_CA_BYTES)?
        .filter(|pem| !pem.iter().all(u8::is_ascii_whitespace))
        .ok_or_else(|| not_ready("certificate"))?;
    let token_path = paths.operator_token_file();
    let token = read_private(&token_path, MAX_TOKEN_BYTES)?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| not_ready("operator token"))?;

    // 127.0.0.1, not localhost: localhost can resolve to ::1 first, and
    // Docker publishes the trial's port on IPv4 loopback only.
    cfg.server.url = format!("https://127.0.0.1:{}", state.ports.api);
    cfg.server.token = Some(token);
    cfg.server.insecure = false;
    cfg.server.ca_cert = None;
    cfg.server.pinned_ca = Some(ca);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use trawl_client::TlsTrust;

    use super::*;
    use crate::trial::paths::write_private;
    use crate::trial::state::tests::fixture;

    const TOKEN: &str = "flt_trialtokenvalue";
    const PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";

    /// A finished trial directory under a temp `XDG_STATE_HOME`.
    fn ready_trial(api_port: u16) -> (tempfile::TempDir, TrialPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = TrialPaths::resolve(Some(&tmp.path().join("state")), None).unwrap();
        paths.ensure_dir().unwrap();
        fixture(api_port).save(&paths.state_file()).unwrap();
        write_private(&paths.ca_file(), PEM, 0o644).unwrap();
        write_private(
            &paths.operator_token_file(),
            format!("{TOKEN}\n").as_bytes(),
            0o600,
        )
        .unwrap();
        (tmp, paths)
    }

    fn apply_default(cfg: &mut Config, paths: &TrialPaths) -> Result<(), TrialError> {
        apply(cfg, Overrides::default(), "config.toml", paths)
    }

    /// URL, token, and CA all come from the trial directory, and they reach
    /// the connection every command and the TUI build.
    #[test]
    fn resolves_url_token_and_ca_from_the_trial_directory() {
        let (_tmp, paths) = ready_trial(15999);
        let mut cfg: Config = toml::from_str(
            r#"
[server]
url = "https://prod.example:5514"
token = "flt_prod"
insecure = true

[profiles.lab]
url = "https://lab.example:5514"
"#,
        )
        .unwrap();
        apply_default(&mut cfg, &paths).unwrap();

        assert_eq!(cfg.server.url, "https://127.0.0.1:15999");
        assert!(!cfg.server.insecure, "the overlay turns insecure off");
        assert_eq!(cfg.server.pinned_ca.as_deref(), Some(PEM));

        let conn = crate::connection::trial(&cfg).unwrap();
        assert_eq!(conn.url, "https://127.0.0.1:15999");
        assert_eq!(conn.token, TOKEN);
        assert_eq!(conn.trust, TlsTrust::PinnedCa(PEM.to_vec()));
    }

    /// The certificate is read once, at resolution. Whatever happens to
    /// `ca.pem` afterwards, the connection pins the bytes read then, and no
    /// pathname is left for a later step to open.
    #[test]
    fn the_connection_pins_the_bytes_read_at_resolution() {
        let (_tmp, paths) = ready_trial(15999);
        let mut cfg: Config =
            toml::from_str("[server]\nca_cert = \"/etc/trawl/prod-ca.pem\"\n").unwrap();
        apply_default(&mut cfg, &paths).unwrap();
        assert_eq!(
            cfg.server.ca_cert, None,
            "no pathname survives the overlay, not even an inherited one"
        );

        let substitute = b"-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----\n";
        write_private(&paths.ca_file(), substitute, 0o644).unwrap();
        let conn = crate::connection::trial(&cfg).unwrap();
        assert_eq!(conn.trust, TlsTrust::PinnedCa(PEM.to_vec()));

        std::fs::remove_dir_all(&paths.dir).unwrap();
        let conn = crate::connection::trial(&cfg).unwrap();
        assert_eq!(
            conn.trust,
            TlsTrust::PinnedCa(PEM.to_vec()),
            "the trial directory is not read again"
        );
    }

    /// `ca.pem` goes through the checked read: a link or a file others can
    /// write is refused, and an empty one is an unfinished trial.
    #[test]
    fn an_untrusted_or_empty_certificate_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let (tmp, paths) = ready_trial(15999);
        std::fs::set_permissions(paths.ca_file(), std::fs::Permissions::from_mode(0o666)).unwrap();
        let err = apply_default(&mut Config::default(), &paths).unwrap_err();
        assert!(matches!(err, TrialError::NotPrivateFile { .. }), "{err:?}");

        let elsewhere = tmp.path().join("elsewhere.pem");
        std::fs::write(&elsewhere, PEM).unwrap();
        std::fs::remove_file(paths.ca_file()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, paths.ca_file()).unwrap();
        assert!(apply_default(&mut Config::default(), &paths).is_err());

        std::fs::remove_file(paths.ca_file()).unwrap();
        write_private(&paths.ca_file(), b" \n", 0o644).unwrap();
        let err = apply_default(&mut Config::default(), &paths).unwrap_err();
        assert!(
            matches!(
                err,
                TrialError::TrialNotReady {
                    missing: "certificate",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// The ancestry rule reaches the `-p trial` read path.
    #[test]
    fn a_loose_ancestor_refuses_the_profile() {
        use std::os::unix::fs::PermissionsExt as _;

        let (tmp, paths) = ready_trial(15999);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let mut cfg = Config::default();
        let err = apply_default(&mut cfg, &paths).unwrap_err();
        assert!(
            matches!(&err, TrialError::NotPrivateFile { path, .. } if path == tmp.path()),
            "{err:?}"
        );
        assert_eq!(cfg.server.token, None, "nothing applied");
    }

    /// `--insecure` / `TRAWL_INSECURE` land after the overlay and must not
    /// void the pin.
    #[test]
    fn insecure_after_the_overlay_fails_through_the_ca_cert_rule() {
        let (_tmp, paths) = ready_trial(15999);
        let mut cfg = Config::default();
        apply_default(&mut cfg, &paths).unwrap();
        cfg.apply_overrides(true);
        assert!(matches!(
            cfg.tls_trust(),
            Err(crate::config::ConfigError::CaCertWithInsecure)
        ));
    }

    #[test]
    fn every_connection_override_is_refused() {
        let (_tmp, paths) = ready_trial(15999);
        let env = |names: &[&str]| Overrides {
            env_set: names.iter().map(|&name| name.to_owned()).collect(),
            ..Overrides::default()
        };
        let cases = [
            (env(&["TRAWL_URL"]), "TRAWL_URL is set"),
            (env(&["TRAWL_TOKEN"]), "TRAWL_TOKEN is set"),
            (env(&["TRAWL_URL", "TRAWL_TOKEN"]), "TRAWL_URL is set"),
            (
                Overrides {
                    url_arg: true,
                    ..Overrides::default()
                },
                "--url was given",
            ),
            (
                Overrides {
                    token_arg: true,
                    ..Overrides::default()
                },
                "--token was given",
            ),
            (
                Overrides {
                    token_file_arg: true,
                    ..Overrides::default()
                },
                "--token-file was given",
            ),
            (
                Overrides {
                    env_set: vec!["TRAWL_TOKEN".to_owned()],
                    url_arg: true,
                    ..Overrides::default()
                },
                "TRAWL_TOKEN is set",
            ),
        ];
        for (overrides, expected) in cases {
            let label = format!("{overrides:?}");
            let mut cfg = Config::default();
            let err = apply(&mut cfg, overrides, "config.toml", &paths).unwrap_err();
            assert!(
                matches!(&err, TrialError::ProfileOverride { what } if what == expected),
                "{label}: {err:?}"
            );
            assert_eq!(
                cfg.server.url,
                crate::config::DEFAULT_URL,
                "nothing applied"
            );
        }
    }

    #[test]
    fn a_config_defined_trial_profile_is_refused() {
        let (_tmp, paths) = ready_trial(15999);
        let mut cfg: Config =
            toml::from_str("[profiles.trial]\nurl = \"https://elsewhere:5514\"\n").unwrap();
        let err = apply(
            &mut cfg,
            Overrides::default(),
            "/home/u/.config/trawl/config.toml",
            &paths,
        )
        .unwrap_err();
        assert!(matches!(err, TrialError::ProfileInConfig { .. }), "{err:?}");
        assert!(err.to_string().contains("[profiles.trial]"), "{err}");
        assert!(
            err.to_string()
                .contains("/home/u/.config/trawl/config.toml")
        );
    }

    #[test]
    fn a_missing_trial_names_trial_up() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = TrialPaths::resolve(Some(tmp.path()), None).unwrap();
        let err = apply_default(&mut Config::default(), &paths).unwrap_err();
        assert!(matches!(err, TrialError::NoTrial { .. }), "{err:?}");
        assert!(err.to_string().contains("trawl trial up"), "{err}");
        assert!(!paths.root.exists(), "-p trial creates nothing");

        // A directory without state is no trial either.
        paths.ensure_dir().unwrap();
        let err = apply_default(&mut Config::default(), &paths).unwrap_err();
        assert!(matches!(err, TrialError::NoTrial { .. }), "{err:?}");
    }

    #[test]
    fn an_unfinished_trial_names_trial_up() {
        let (_tmp, paths) = ready_trial(15999);
        std::fs::remove_file(paths.operator_token_file()).unwrap();
        let err = apply_default(&mut Config::default(), &paths).unwrap_err();
        assert!(
            matches!(
                err,
                TrialError::TrialNotReady {
                    missing: "operator token",
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("trawl trial up"), "{err}");

        std::fs::remove_file(paths.ca_file()).unwrap();
        let err = apply_default(&mut Config::default(), &paths).unwrap_err();
        assert!(
            matches!(
                err,
                TrialError::TrialNotReady {
                    missing: "certificate",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn errors_never_carry_the_token() {
        let (_tmp, paths) = ready_trial(15999);
        std::fs::remove_file(paths.ca_file()).unwrap();
        let err = apply_default(&mut Config::default(), &paths).unwrap_err();
        assert!(!err.to_string().contains(TOKEN));
        assert!(!format!("{err:?}").contains(TOKEN));
    }

    /// Every variable clap binds anywhere in the command tree has a rule,
    /// and between them `-p trial`'s refusals and `trawl trial status`'s
    /// warnings cover all of them. Every global flag that moves the
    /// connection is refused as a flag.
    #[test]
    fn clap_binding_policies_are_complete() {
        let bound = bound_env();
        let all: BTreeSet<String> = bound.iter().map(|(_, name)| name.clone()).collect();
        for (id, name) in &bound {
            assert!(
                env_rule(id).is_some(),
                "{name} (argument {id}) is bound by clap and has no -p trial rule"
            );
        }

        let refused: BTreeSet<String> = Overrides::observe_with(false, false, false, |_| true)
            .env_set
            .into_iter()
            .collect();
        assert!(
            refused.is_superset(&["TRAWL_URL", "TRAWL_TOKEN"].map(str::to_owned).into()),
            "{refused:?}"
        );
        let warned: BTreeSet<String> =
            super::super::render::moving_variables(&env_rules(), |_| true, true, Some("elsewhere"))
                .into_iter()
                .map(|(name, _)| name)
                .collect();
        assert_eq!(&refused | &warned, all);

        // Globals that move the connection without a variable of their own
        // are refused as flags; the rest are named here so a new global
        // has to be placed.
        let mut command = crate::Cli::command();
        command.build();
        let (_tmp, paths) = ready_trial(15999);
        let mut flags = BTreeSet::new();
        for arg in command.get_arguments().filter(|arg| arg.is_global_set()) {
            let id = arg.get_id().as_str();
            let overrides = match id {
                "url" => Overrides {
                    url_arg: true,
                    ..Overrides::default()
                },
                "token" => Overrides {
                    token_arg: true,
                    ..Overrides::default()
                },
                "token_file" => Overrides {
                    token_file_arg: true,
                    ..Overrides::default()
                },
                // --insecure's variable has a rule above, and the flag fails
                // through the ca_cert-with-insecure rule after the overlay.
                // --profile is -p trial itself, and --config the file it
                // reads.
                "insecure" | "profile" | "config" => continue,
                other => panic!("global --{other} has no -p trial policy"),
            };
            let err = apply(&mut Config::default(), overrides, "config.toml", &paths).unwrap_err();
            assert!(
                matches!(err, TrialError::ProfileOverride { .. }),
                "--{id}: {err:?}"
            );
            flags.insert(id.to_owned());
        }
        assert_eq!(
            flags,
            ["url", "token", "token_file"].map(str::to_owned).into()
        );
        assert_eq!(crate::trial::PROFILE, "trial");
    }
}
