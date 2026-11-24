// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use fleet_auth::{
    ENV_SESSION_AEAD_KEY, ENV_SESSION_COOKIE_DOMAIN, ENV_SESSION_COOKIE_PATH,
    ENV_SESSION_COOKIE_SECURE, ENV_SESSION_PUBLIC_ORIGINS, KEY_LEN, PublicOriginsError,
    SessionRuntimeError,
};
use trawl_api::doctor::reason;

use super::output::{EnvName, Row, SelectedPath, Selection, Text};
use super::read::{self, ReadFault};
use super::{Ctx, Runner, WebCheck};
use crate::config::{
    ConfigError, CookieSettings, KeySource, Origins, OriginsFrom, SettingSource, key_from_env,
};

/// The checks this group runs, in [`WebCheck::ALL`] order.
pub(super) const CHECKS: [WebCheck; 3] = [
    WebCheck::PublicOrigins,
    WebCheck::CookieSettings,
    WebCheck::CookieKey,
];

pub(super) async fn run(runner: &mut Runner, ctx: &mut Ctx) {
    for check in CHECKS {
        let Some(gate) = runner.gate(check) else {
            continue;
        };
        let row = match gate.check() {
            WebCheck::PublicOrigins => public_origins(ctx),
            WebCheck::CookieSettings => cookie_settings(ctx),
            WebCheck::CookieKey => cookie_key(ctx).await,
            other => unreachable!("{other:?} is not a settings check"),
        };
        runner.record(gate, row);
    }
}

const fn origins_word(n: usize) -> &'static str {
    if n == 1 { " origin" } else { " origins" }
}

fn public_origins(ctx: &Ctx) -> Row {
    let check = WebCheck::PublicOrigins;
    let from_file = || Text::new("[web] public_origins in ").path(&ctx.config_path);
    let from_env = || Text::new(ENV_SESSION_PUBLIC_ORIGINS);
    let fix_file = || {
        Text::new("list the browser-visible origins in [web] public_origins in ")
            .path(&ctx.config_path)
            .lit(", or in ")
            .lit(ENV_SESSION_PUBLIC_ORIGINS)
    };
    let fix_env = || {
        Text::new("correct ")
            .lit(ENV_SESSION_PUBLIC_ORIGINS)
            .lit(": a comma-separated list of browser-visible origins, such as https://trawl.example.com")
    };

    let error = match &ctx.public_origins {
        Ok(Origins { origins, from }) => {
            let count = origins.iter().count();
            let detail = Text::new("").int(count as u64).lit(origins_word(count));
            return match from {
                OriginsFrom::File => Row::complete(check)
                    .detail(detail.lit(", from the config file"))
                    .source(from_file()),
                OriginsFrom::Environment {
                    replaced_file_entries,
                } => {
                    let detail = detail.lit(", from ").lit(ENV_SESSION_PUBLIC_ORIGINS);
                    let detail = if *replaced_file_entries == 0 {
                        detail.lit("; the config file lists none")
                    } else {
                        detail
                            .lit(", which replaces the config file's list of ")
                            .int(*replaced_file_entries as u64)
                    };
                    Row::complete(check).detail(detail).source(from_env())
                }
            };
        }
        Err(error) => error,
    };

    let invalid = "an origin in the list is not valid";
    let duplicate = "two entries in the list are the same origin";
    let entry = |index: usize| {
        Text::new("entry ")
            .int(index as u64)
            .lit(" (counting from 0) is not a valid origin")
    };
    let pair = |first: usize, second: usize| {
        Text::new("entries ")
            .int(first as u64)
            .lit(" and ")
            .int(second as u64)
            .lit(" (counting from 0) are the same origin after normalization")
    };
    match error {
        ConfigError::PublicOrigins { source } => {
            let row = match source {
                PublicOriginsError::Empty => Row::failed(check, "the origin list is empty"),
                PublicOriginsError::Entry { index, .. } => {
                    Row::failed(check, invalid).detail(entry(*index))
                }
                PublicOriginsError::Duplicate { first, second } => {
                    Row::failed(check, duplicate).detail(pair(*first, *second))
                }
            };
            row.source(from_file()).next(fix_file())
        }
        ConfigError::SessionEnvOrigins(source) => {
            let row = match source {
                SessionRuntimeError::InvalidOrigin { index, .. } => {
                    Row::failed(check, invalid).detail(entry(*index))
                }
                SessionRuntimeError::DuplicateOrigin { first, second, .. } => {
                    Row::failed(check, duplicate).detail(pair(*first, *second))
                }
                SessionRuntimeError::NotUnicode { .. }
                | SessionRuntimeError::InvalidKey { .. }
                | SessionRuntimeError::InvalidValue { .. } => {
                    Row::failed(check, "the origin list does not resolve")
                }
            };
            row.source(from_env()).next(fix_env())
        }
        ConfigError::SessionEnvValue { name, reason } => {
            Row::failed(check, "the origin list does not resolve")
                .detail(Text::new(name).lit(": ").lit(reason))
                .source(from_env())
                .next(fix_env())
        }
        ConfigError::EnvUtf8 { .. } => Row::failed(check, "the origin list is not UTF-8 text")
            .source(from_env())
            .next(fix_env()),
        _ => Row::failed(check, "the origin list does not resolve").source(from_file()),
    }
}

const COOKIE_VARIABLES: [&str; 3] = [
    ENV_SESSION_COOKIE_PATH,
    ENV_SESSION_COOKIE_SECURE,
    ENV_SESSION_COOKIE_DOMAIN,
];

/// Secure off while any effective public origin is not loopback is
/// `failed`: a browser on another host would send the session cookie
/// in the clear.
fn cookie_settings(ctx: &Ctx) -> Row {
    let check = WebCheck::CookieSettings;
    let source = || {
        Text::new("[web] in ")
            .path(&ctx.config_path)
            .lit(", and the FLEET_SESSION_COOKIE_* variables")
    };
    let settings = match &ctx.cookie_settings {
        Ok(settings) => settings,
        Err(error) => return cookie_settings_fault(error),
    };
    let detail = settings_detail(settings);
    let Ok(Origins { origins, .. }) = &ctx.public_origins else {
        unreachable!("proxy.cookie_settings runs only after proxy.public_origins completed")
    };
    let remote = origins
        .iter()
        .filter(|origin| !origin.is_loopback())
        .count();
    if settings.secure || remote == 0 {
        return Row::complete(check).detail(detail).source(source());
    }
    let next = match settings.secure_from {
        SettingSource::Environment(name) => Text::new("unset ")
            .lit(name)
            .lit(" or set it to true, so the cookie carries Secure"),
        _ => Text::new("remove allow_insecure_cookies = true from ")
            .path(&ctx.config_path)
            .lit(", or serve only loopback origins"),
    };
    Row::failed(check, "Secure is off for an origin that is not loopback")
        .detail(
            detail
                .lit("; ")
                .int(remote as u64)
                .lit(origins_word(remote))
                .lit(" not loopback"),
        )
        .source(source())
        .next(next)
}

fn settings_detail(settings: &CookieSettings) -> Text {
    let from = |text: Text, source: SettingSource| text.lit(" (").setting(source).lit(")");
    let text = Text::new("Secure ").lit(if settings.secure { "on" } else { "off" });
    let text =
        from(text, settings.secure_from)
            .lit("; ")
            .lit(if settings.shared_domain.is_some() {
                "a shared domain"
            } else {
                "host-only"
            });
    let text = from(text, settings.domain_from).lit("; path /");
    let text = from(text, settings.path_from)
        .lit("; lifetime ")
        .int(settings.ttl_secs)
        .lit(" s");
    from(text, settings.ttl_from)
}

fn cookie_settings_fault(error: &ConfigError) -> Row {
    let check = WebCheck::CookieSettings;
    let named = |name: &'static str| {
        Text::new("correct or unset ")
            .lit(name)
            .lit(" in the service's environment")
    };
    match error {
        ConfigError::SessionEnvValue { name, reason } => {
            Row::failed(check, "a session cookie variable is not valid")
                .detail(Text::new(name).lit(": ").lit(reason))
                .source(Text::new(name))
                .next(named(name))
        }
        ConfigError::EnvUtf8 { name } => {
            let row = Row::failed(check, "a session cookie variable is not UTF-8 text");
            match COOKIE_VARIABLES.into_iter().find(|known| known == name) {
                Some(known) => row.source(Text::new(known)).next(named(known)),
                None => row,
            }
        }
        _ => Row::failed(check, "the session cookie settings do not resolve"),
    }
}

const PERSIST_KEY: &str = "set `cookie_secret_path` so sessions survive restarts";

async fn cookie_key(ctx: &Ctx) -> Row {
    let check = WebCheck::CookieKey;
    match &ctx.cookie_key {
        Ok(KeySource::FleetEnv(_)) => Row::complete(check)
            .detail(Text::new("a 32-byte key; sessions survive restarts"))
            .source(Text::new(ENV_SESSION_AEAD_KEY)),
        Ok(KeySource::ConfigEnv { name }) => config_env_key(ctx, name),
        Ok(KeySource::File { path }) => {
            // Shown as written; `~` expands on the reading thread.
            let shown = SelectedPath::new(Selection::CookieSecretPath, path);
            let len = read::secret_len(path.clone(), read::cap::KEY).await;
            key_file_row(ctx, &shown, len)
        }
        Ok(KeySource::None) => Row::not_configured(check, reason::EPHEMERAL_EACH_START)
            .detail(Text::new(
                "no key source is set: each start makes a new key, so every session ends when trawl-web restarts",
            ))
            .next(Text::new(PERSIST_KEY)),
        Err(error) => fleet_env_key_fault(error),
    }
}

fn fleet_env_key_fault(error: &ConfigError) -> Row {
    let check = WebCheck::CookieKey;
    let fix = Text::new("set ")
        .lit(ENV_SESSION_AEAD_KEY)
        .lit(" to the base64 of 32 random bytes, or unset it");
    let row = match error {
        ConfigError::EnvUtf8 { .. } => Row::failed(check, "the session key is not UTF-8 text"),
        ConfigError::EnvKey { .. } => Row::failed(check, "the session key is not valid")
            .detail(Text::new("expected the base64 of exactly 32 bytes")),
        _ => Row::failed(check, "the session key does not resolve"),
    };
    row.source(Text::new(ENV_SESSION_AEAD_KEY)).next(fix)
}

fn config_env_key(ctx: &Ctx, name: &str) -> Row {
    let check = WebCheck::CookieKey;
    let shown = EnvName::new(name);
    let source = if shown.is_shown() {
        Text::new("")
            .env(&shown)
            .lit(", named by [web] cookie_secret_env")
    } else {
        Text::new("").env(&shown)
    };
    let fix = || {
        Text::new("set ")
            .env(&shown)
            .lit(" to the base64 of 32 random bytes in the service's environment, or correct cookie_secret_env in ")
            .path(&ctx.config_path)
    };
    let row = match key_from_env(name) {
        Ok(_key) => {
            return Row::complete(check)
                .detail(Text::new("a 32-byte key; sessions survive restarts"))
                .source(source);
        }
        Err(ConfigError::EnvMissing { .. }) => {
            Row::failed(check, "the variable cookie_secret_env names is not set")
        }
        Err(ConfigError::EnvUtf8 { .. }) => Row::failed(
            check,
            "the variable cookie_secret_env names is not UTF-8 text",
        ),
        Err(ConfigError::EnvKey { .. }) => Row::failed(
            check,
            "the variable cookie_secret_env names does not hold a key",
        )
        .detail(Text::new("expected the base64 of exactly 32 bytes")),
        Err(_) => Row::failed(check, "the session key does not resolve"),
    };
    row.source(source).next(fix())
}

fn key_file_row(ctx: &Ctx, shown: &SelectedPath, read: Result<usize, ReadFault>) -> Row {
    let check = WebCheck::CookieKey;
    let not_a_key = "the key file is not 32 bytes";
    let write_key = || {
        Text::new("write exactly 32 random bytes to ")
            .path(shown)
            .lit(", such as with head -c 32 /dev/urandom")
    };
    let row = match read {
        Ok(KEY_LEN) => Row::complete(check).detail(Text::new(
            "32 bytes, readable by this user; sessions survive restarts",
        )),
        Ok(len) => Row::failed(check, not_a_key)
            .detail(Text::new("it holds ").int(len as u64).lit(" bytes"))
            .next(write_key()),
        Err(ReadFault::TooLarge) => Row::failed(check, not_a_key)
            .detail(
                Text::new("it holds more than ")
                    .int(read::cap::KEY)
                    .lit(" bytes"),
            )
            .next(write_key()),
        Err(ReadFault::Missing) => Row::failed(check, "the key file does not exist").next(
            write_key()
                .lit(", or correct cookie_secret_path in ")
                .path(&ctx.config_path),
        ),
        Err(ReadFault::NotRegular) => {
            Row::failed(check, "the key file is not a regular file").next(write_key())
        }
        Err(ReadFault::SymlinkLoop) => {
            Row::failed(check, "the key file path is a symlink loop").next(write_key())
        }
        Err(ReadFault::PermissionDenied) => Row::not_sampled(check, reason::PERMISSION_DENIED)
            .next(
                Text::new("rerun as the service user; if it cannot read ")
                    .path(shown)
                    .lit(" either, let it read the file"),
            ),
        Err(ReadFault::TimedOut) => Row::not_sampled(check, reason::TIMED_OUT).detail(
            Text::new("the read did not finish in ")
                .int(read::READ_DEADLINE.as_secs())
                .lit(" s"),
        ),
        Err(ReadFault::Io) => Row::not_sampled(check, reason::UNREADABLE),
    };
    row.source(Text::new("cookie_secret_path ").path(shown))
        .access()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use fleet_auth::{PublicOrigins, SessionKey};
    use trawl_api::doctor::{Outcome, Target, Verdict};

    use super::*;
    use crate::doctor::RunAs;

    fn ctx(origins: &[&str], secure: bool, key: KeySource) -> Ctx {
        Ctx {
            config_path: SelectedPath::new(Selection::ConfigFlag, Path::new("/etc/trawld.toml")),
            run_as: RunAs {
                uid: Some(1000),
                root: false,
            },
            public_origins: Ok(Origins {
                origins: PublicOrigins::parse(origins).expect("valid origins"),
                from: OriginsFrom::File,
            }),
            cookie_settings: Ok(CookieSettings {
                secure,
                secure_from: if secure {
                    SettingSource::Default
                } else {
                    SettingSource::File
                },
                shared_domain: Some("private-secret.example".to_owned()),
                domain_from: SettingSource::File,
                path_from: SettingSource::Default,
                ttl_secs: 3600,
                ttl_from: SettingSource::File,
            }),
            cookie_key: Ok(key),
            upstream: Err(ConfigError::EnvMissing {
                name: "UNUSED".to_owned(),
            }),
            pinned_roots: None,
        }
    }

    async fn settings_rows(ctx: &mut Ctx, root: bool) -> Runner {
        let mut runner = Runner::new(RunAs {
            uid: Some(if root { 0 } else { 1000 }),
            root,
        });
        for check in [WebCheck::Config, WebCheck::Identity] {
            let gate = runner.gate(check).expect("no prerequisite");
            runner.record(gate, Row::complete(check));
        }
        run(&mut runner, ctx).await;
        runner
    }

    #[tokio::test]
    async fn no_key_source_can_still_pass() {
        let mut ctx = ctx(&["https://trawl.example.com"], true, KeySource::None);
        let mut runner = settings_rows(&mut ctx, false).await;
        assert_eq!(
            runner.outcome(WebCheck::CookieKey),
            Some((Outcome::NotConfigured, Some(reason::EPHEMERAL_EACH_START)))
        );
        for check in [WebCheck::UpstreamTrust, WebCheck::UpstreamHealth] {
            let gate = runner.gate(check).expect("trust completed");
            runner.record(gate, Row::complete(check));
        }
        let report = runner.finish(Target {
            origin: None,
            source: "--config /etc/trawld.toml".to_owned(),
        });
        assert_eq!(report.verdict(), Verdict::Pass);
        assert_eq!(report.verdict().exit_code(), 0);
        let key = report
            .checks()
            .iter()
            .find(|c| c.id == "proxy.cookie_key")
            .unwrap();
        assert_eq!(key.next_action.as_deref(), Some(PERSIST_KEY));
    }

    #[tokio::test]
    async fn insecure_cookies_fail_only_off_loopback() {
        let cases: [(&[&str], bool, Outcome); 5] = [
            (&["http://localhost:8090"], false, Outcome::Complete),
            (
                &["http://127.0.0.2:8090", "http://[::1]:8090"],
                false,
                Outcome::Complete,
            ),
            (
                &["http://localhost:8090", "https://trawl.example.com"],
                false,
                Outcome::Failed,
            ),
            (&["http://app.localhost:8090"], false, Outcome::Failed),
            (&["https://trawl.example.com"], true, Outcome::Complete),
        ];
        for (origins, secure, outcome) in cases {
            let mut ctx = ctx(origins, secure, KeySource::None);
            let runner = settings_rows(&mut ctx, false).await;
            let (got, _) = runner.outcome(WebCheck::CookieSettings).unwrap();
            assert_eq!(got, outcome, "{origins:?} secure {secure}");
        }
    }

    #[tokio::test]
    async fn only_a_key_file_is_access() {
        let key = SessionKey::from_bytes([7; KEY_LEN]);
        let mut env = ctx(
            &["https://trawl.example.com"],
            true,
            KeySource::FleetEnv(key),
        );
        let runner = settings_rows(&mut env, true).await;
        assert_eq!(
            runner.outcome(WebCheck::CookieKey),
            Some((Outcome::Complete, None))
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.cookie");
        std::fs::write(&path, [7; KEY_LEN]).unwrap();
        for (root, outcome) in [
            (false, (Outcome::Complete, None)),
            (true, (Outcome::NotSampled, Some(reason::RAN_AS_ROOT))),
        ] {
            let mut file = ctx(
                &["https://trawl.example.com"],
                true,
                KeySource::File { path: path.clone() },
            );
            let runner = settings_rows(&mut file, root).await;
            assert_eq!(
                runner.outcome(WebCheck::CookieKey),
                Some(outcome),
                "root {root}"
            );
        }
    }
}
