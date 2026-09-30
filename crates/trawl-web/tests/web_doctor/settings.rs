// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `proxy.public_origins`, `proxy.cookie_settings` and `proxy.cookie_key`
//! (#271 AC1, AC6, AC8, AC9; D7, D8, D12).
//!
//! Every run goes through [`doctor`] or [`run_web_doctor_in_userns`], so
//! the leak check sees every report, with the origins, the shared domain,
//! the key and its base64 planted. These tests assert only the settings
//! rows: the upstream rows belong to the upstream group. Where a test needs
//! the run not to fail on the upstream, it pins a CA file that does not
//! exist, so the upstream is incomplete (`ca_not_present`) and nothing is
//! dialled.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use trawl_api::doctor::{Outcome, Report};

use crate::support::{
    PLANTED_KEY, PLANTED_ORIGIN, Userns, WebDoctorConfig, assert_unchanged, doctor, doctor_args,
    fs_snapshot, home_env, report, row, run_web_doctor_in_userns, verdict, write_config, write_key,
};

/// A doctor run's environment.
type Env = Vec<(&'static str, OsString)>;

/// The spec's words for the next action when no key persists.
const PERSIST_KEY: &str = "set `cookie_secret_path` so sessions survive restarts";

/// The base64 of [`PLANTED_KEY`], as an operator would set it.
fn planted_key_base64() -> String {
    fleet_auth::SessionKey::from_bytes(*PLANTED_KEY)
        .to_base64url()
        .as_str()
        .to_owned()
}

/// `home_env(home)` plus `extra`.
fn env_with(home: &Path, extra: &[(&'static str, &str)]) -> Env {
    let mut env = home_env(home);
    env.extend(
        extra
            .iter()
            .map(|(name, value)| (*name, OsString::from(value))),
    );
    env
}

/// Run the doctor over `fixture`, written in `home`, with `env`, planting
/// the fixture's values, the key's base64, and `extra`.
fn run(home: &Path, fixture: &WebDoctorConfig, env: &Env, extra: &[&str]) -> (i32, Report) {
    let config = write_config(home, fixture);
    let mut planted = fixture.planted();
    planted.push(planted_key_base64());
    planted.extend(extra.iter().map(|value| (*value).to_owned()));
    doctor(&config, env, &planted)
}

/// A pinned CA path in `home` with nothing there: the upstream is
/// `not_sampled`, so a run whose other rows pass cannot fail on it.
fn absent_ca(home: &Path) -> PathBuf {
    home.join("tls-absent").join("cert.pem")
}

/// [`write_key`] of `len` bytes at `path`, then chmod it to `mode`.
fn write_key_with_mode(path: &Path, len: usize, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    write_key(path, len);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// One `proxy.cookie_key` case: the key source a test configures, and
/// what the row must say.
struct KeyCase {
    label: &'static str,
    /// `cookie_secret_path`, as a file of this many bytes and this mode.
    file: Option<(usize, u32)>,
    /// `cookie_secret_path` naming a path where nothing is.
    missing_file: bool,
    /// `cookie_secret_env`.
    secret_env: Option<String>,
    /// Variables for the run.
    env: Vec<(&'static str, String)>,
    expect: (Outcome, Option<&'static str>),
    /// Text the row's source must contain.
    source: Option<String>,
}

impl KeyCase {
    fn new(label: &'static str, expect: (Outcome, Option<&'static str>)) -> Self {
        Self {
            label,
            file: None,
            missing_file: false,
            secret_env: None,
            env: Vec::new(),
            expect,
            source: None,
        }
    }
}

/// Every cookie key source and what its row says (AC6, D7): a 32-byte key
/// from `FLEET_SESSION_AEAD_KEY`, from the variable `cookie_secret_env`
/// names, or in the `cookie_secret_path` file is `complete`, naming its
/// source; a file of another length, or none, is `failed`; a file the
/// running user may not read is `not_sampled`, `permission_denied`; an
/// invalid `FLEET_SESSION_AEAD_KEY` fails with no fallback to a good file;
/// and no source is `not_configured`, `ephemeral_each_start`, which does
/// not fail the run.
#[test]
fn web_doctor_cookie_key_sources() {
    for case in key_cases(&planted_key_base64()) {
        let home = tempfile::tempdir().unwrap();
        let key_path = home.path().join("lib").join("web.cookie");
        let mut fixture = WebDoctorConfig::in_dir(home.path());
        if let Some((len, mode)) = case.file {
            write_key_with_mode(&key_path, len, mode);
            fixture.cookie_secret_path = Some(key_path.clone());
        }
        if case.missing_file {
            fixture.cookie_secret_path = Some(key_path.clone());
        }
        fixture.cookie_secret_env.clone_from(&case.secret_env);
        let env: Vec<(&'static str, &str)> = case
            .env
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let (code, report) = run(home.path(), &fixture, &env_with(home.path(), &env), &[]);

        let key_row = row(&report, "proxy.cookie_key");
        assert_eq!(
            verdict(&report, "proxy.cookie_key"),
            case.expect,
            "{}: {key_row:?}",
            case.label
        );
        let source = key_row.source.as_deref().unwrap_or_default();
        if (case.file.is_some() || case.missing_file) && case.env.is_empty() {
            assert!(
                source.contains(&key_path.display().to_string()),
                "{}: {key_row:?}",
                case.label
            );
        }
        if let Some(expected) = &case.source {
            assert!(
                source.contains(expected.as_str()),
                "{}: {key_row:?}",
                case.label
            );
        }
        if case.expect.0 == Outcome::Failed {
            assert_eq!(code, 1, "{}", case.label);
            assert!(key_row.next_action.is_some(), "{}: {key_row:?}", case.label);
        }
    }

    // No source: `not_configured`, with the spec's next action; the row
    // does not fail the run, whose upstream here is incomplete.
    let home = tempfile::tempdir().unwrap();
    let mut fixture = WebDoctorConfig::in_dir(home.path());
    fixture.upstream_ca_path = Some(absent_ca(home.path()));
    let (code, report) = run(home.path(), &fixture, &home_env(home.path()), &[]);
    let key_row = row(&report, "proxy.cookie_key");
    assert_eq!(
        (key_row.outcome, key_row.reason.as_deref()),
        (Outcome::NotConfigured, Some("ephemeral_each_start")),
        "{key_row:?}"
    );
    assert_eq!(key_row.next_action.as_deref(), Some(PERSIST_KEY));
    for id in ["proxy.public_origins", "proxy.cookie_settings"] {
        assert_eq!(verdict(&report, id), (Outcome::Complete, None), "{id}");
    }
    assert_ne!(code, 1, "no key source failed the run: {report:?}");
}

/// The cases of [`web_doctor_cookie_key_sources`], whose good key is the
/// base64 `key`. The unreadable-file case is left out, saying so, when
/// this test runs as real root, which reads a mode-000 file.
fn key_cases(key: &str) -> Vec<KeyCase> {
    let good = |label, source: &str| {
        let mut case = KeyCase::new(label, (Outcome::Complete, None));
        case.source = Some(source.to_owned());
        case
    };
    let not_a_key = (Outcome::Failed, Some("the key file is not 32 bytes"));
    let mut cases = Vec::new();

    let mut case = good("FLEET_SESSION_AEAD_KEY", "FLEET_SESSION_AEAD_KEY");
    case.env = vec![("FLEET_SESSION_AEAD_KEY", key.to_owned())];
    // Configured sources lose to the Fleet variable, as at startup.
    case.file = Some((31, 0o600));
    cases.push(case);

    let mut case = good(
        "cookie_secret_env",
        "TRAWL_WEB_COOKIE_KEY, named by [web] cookie_secret_env",
    );
    case.secret_env = Some("TRAWL_WEB_COOKIE_KEY".to_owned());
    case.env = vec![("TRAWL_WEB_COOKIE_KEY", key.to_owned())];
    cases.push(case);

    let mut case = KeyCase::new(
        "cookie_secret_env names an unset variable",
        (
            Outcome::Failed,
            Some("the variable cookie_secret_env names is not set"),
        ),
    );
    case.secret_env = Some("TRAWL_WEB_COOKIE_KEY".to_owned());
    case.source = Some("TRAWL_WEB_COOKIE_KEY".to_owned());
    cases.push(case);

    // The key pasted where its variable's name belongs is never shown.
    let mut case = KeyCase::new(
        "cookie_secret_env holds a key",
        (
            Outcome::Failed,
            Some("the variable cookie_secret_env names is not set"),
        ),
    );
    case.secret_env = Some(key.to_owned());
    case.source = Some("the variable named by `cookie_secret_env`".to_owned());
    cases.push(case);

    let mut case = KeyCase::new(
        "cookie_secret_env names a variable that is not a key",
        (
            Outcome::Failed,
            Some("the variable cookie_secret_env names does not hold a key"),
        ),
    );
    case.secret_env = Some("TRAWL_WEB_COOKIE_KEY".to_owned());
    case.env = vec![("TRAWL_WEB_COOKIE_KEY", "private-secret".to_owned())];
    cases.push(case);

    let mut case = good("a 32-byte file", "cookie_secret_path ");
    case.file = Some((32, 0o600));
    cases.push(case);

    let mut case = KeyCase::new("a 31-byte file", not_a_key);
    case.file = Some((31, 0o600));
    cases.push(case);

    let mut case = KeyCase::new("a 33-byte file", not_a_key);
    case.file = Some((33, 0o600));
    cases.push(case);

    let mut case = KeyCase::new("a 64-byte file", not_a_key);
    case.file = Some((64, 0o600));
    cases.push(case);

    let mut case = KeyCase::new(
        "no file",
        (Outcome::Failed, Some("the key file does not exist")),
    );
    case.missing_file = true;
    cases.push(case);

    // An invalid Fleet key is selected first, and fails: no fallback.
    let mut case = KeyCase::new(
        "an invalid FLEET_SESSION_AEAD_KEY",
        (Outcome::Failed, Some("the session key is not valid")),
    );
    case.env = vec![("FLEET_SESSION_AEAD_KEY", "private-secret".to_owned())];
    case.file = Some((32, 0o600));
    case.source = Some("FLEET_SESSION_AEAD_KEY".to_owned());
    cases.push(case);

    // Root reads a mode-000 file, so only a run as another user can see
    // the refusal.
    if nix::unistd::geteuid().is_root() {
        eprintln!(
            "SKIPPED: web_doctor_cookie_key_sources: the unreadable-file case, because this \
             test runs as real root, which reads a mode-000 file"
        );
    } else {
        let mut case = KeyCase::new(
            "a file this user cannot read",
            (Outcome::NotSampled, Some("permission_denied")),
        );
        case.file = Some((32, 0o000));
        cases.push(case);
    }

    cases
}

/// One `proxy.public_origins` case: the file's list, the variable, and
/// what the row must say.
struct OriginCase<'a> {
    label: &'a str,
    file: Vec<String>,
    variable: Option<&'a str>,
    expect: (Outcome, Option<&'a str>),
    detail: Option<&'a str>,
    /// Text the row's source must contain.
    source: &'a str,
}

/// Run `case` with `planted` and check its rows: a complete list lets the
/// cookie settings look; a failed one blocks them and fails the run.
fn check_origin_case(case: OriginCase<'_>, planted: &[&str]) {
    let label = case.label;
    let home = tempfile::tempdir().unwrap();
    let mut fixture = WebDoctorConfig::in_dir(home.path());
    fixture.public_origins = case.file;
    let extra: Vec<(&'static str, &str)> = case
        .variable
        .map(|value| ("FLEET_SESSION_PUBLIC_ORIGINS", value))
        .into_iter()
        .collect();
    let mut planted = planted.to_vec();
    planted.extend(case.variable.filter(|value| !value.is_empty()));
    let (code, report) = run(
        home.path(),
        &fixture,
        &env_with(home.path(), &extra),
        &planted,
    );
    let origins = row(&report, "proxy.public_origins");
    assert_eq!(
        (origins.outcome, origins.reason.as_deref()),
        case.expect,
        "{label}: {origins:?}"
    );
    assert_eq!(origins.detail.as_deref(), case.detail, "{label}");
    assert!(
        origins
            .source
            .as_deref()
            .is_some_and(|shown| shown.contains(case.source)),
        "{label}: {origins:?}"
    );
    let settings = row(&report, "proxy.cookie_settings");
    if case.expect.0 == Outcome::Complete {
        assert_eq!(
            (settings.outcome, settings.reason.as_deref()),
            (Outcome::Complete, None),
            "{label}"
        );
    } else {
        assert!(origins.next_action.is_some(), "{label}: {origins:?}");
        assert_eq!(
            (settings.outcome, settings.blocked_by.as_deref()),
            (Outcome::NotSampled, Some("proxy.public_origins")),
            "{label}"
        );
        assert_eq!(code, 1, "{label}");
    }
}

/// Where the origin allowlist came from, never what it holds (AC8, D1):
/// the file's list; `FLEET_SESSION_PUBLIC_ORIGINS` replacing it, reported
/// as replaced even when the two lists match; the variable with no file
/// list. An empty or invalid list, in either place, is `failed`, naming
/// only the entry's index, and blocks the cookie settings.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one table of origin sources, clearer unsplit"
)]
fn web_doctor_origin_sources() {
    let second = "https://logs.private-secret.example:8443";
    let env_origin = "https://env.private-secret.example";
    let bad_entry = "ftp://bad.private-secret.example/path";
    let file_two = vec![PLANTED_ORIGIN.to_owned(), second.to_owned()];
    let both = format!("{PLANTED_ORIGIN},{second}");
    let from_file = "[web] public_origins in ";
    let from_env = "FLEET_SESSION_PUBLIC_ORIGINS";
    let complete = (Outcome::Complete, None);
    let invalid = (Outcome::Failed, Some("an origin in the list is not valid"));
    let duplicate = (
        Outcome::Failed,
        Some("two entries in the list are the same origin"),
    );
    let same_pair =
        Some("entries 0 and 1 (counting from 0) are the same origin after normalization");

    let cases = vec![
        OriginCase {
            label: "the file's list",
            file: file_two.clone(),
            variable: None,
            expect: complete,
            detail: Some("2 origins, from the config file"),
            source: from_file,
        },
        OriginCase {
            label: "the variable replaces the file's list",
            file: file_two.clone(),
            variable: Some(env_origin),
            expect: complete,
            detail: Some(
                "1 origin, from FLEET_SESSION_PUBLIC_ORIGINS, which replaces the config \
                 file's list of 2",
            ),
            source: from_env,
        },
        OriginCase {
            label: "the variable replaces the same list",
            file: file_two.clone(),
            variable: Some(both.as_str()),
            expect: complete,
            detail: Some(
                "2 origins, from FLEET_SESSION_PUBLIC_ORIGINS, which replaces the config \
                 file's list of 2",
            ),
            source: from_env,
        },
        OriginCase {
            label: "the variable with no file list",
            file: Vec::new(),
            variable: Some(env_origin),
            expect: complete,
            detail: Some("1 origin, from FLEET_SESSION_PUBLIC_ORIGINS; the config file lists none"),
            source: from_env,
        },
        OriginCase {
            label: "an empty file list",
            file: Vec::new(),
            variable: None,
            expect: (Outcome::Failed, Some("the origin list is empty")),
            detail: None,
            source: from_file,
        },
        OriginCase {
            label: "an invalid file entry",
            file: vec![PLANTED_ORIGIN.to_owned(), bad_entry.to_owned()],
            variable: None,
            expect: invalid,
            detail: Some("entry 1 (counting from 0) is not a valid origin"),
            source: from_file,
        },
        OriginCase {
            label: "a duplicate file entry",
            file: vec![PLANTED_ORIGIN.to_owned(), format!("{PLANTED_ORIGIN}:443")],
            variable: None,
            expect: duplicate,
            detail: same_pair,
            source: from_file,
        },
        OriginCase {
            label: "an empty variable",
            file: file_two.clone(),
            variable: Some(""),
            expect: invalid,
            detail: Some("entry 0 (counting from 0) is not a valid origin"),
            source: from_env,
        },
        OriginCase {
            label: "an invalid variable entry",
            file: file_two.clone(),
            variable: Some(
                "https://env.private-secret.example,https://ok.private-secret.example,\
                 private-secret",
            ),
            expect: invalid,
            detail: Some("entry 2 (counting from 0) is not a valid origin"),
            source: from_env,
        },
        OriginCase {
            label: "a duplicate variable entry",
            file: file_two.clone(),
            variable: Some(
                "https://env.private-secret.example,https://env.private-secret.example:443",
            ),
            expect: duplicate,
            detail: same_pair,
            source: from_env,
        },
    ];
    for case in cases {
        check_origin_case(case, &[env_origin, second, bad_entry]);
    }
}

/// With no key source the doctor generates no key and writes nothing
/// (AC1, D15): the row is `not_configured`, `ephemeral_each_start`, and
/// HOME, which holds the configuration, is unchanged. A key path with
/// nothing at it fails, and nothing is created there either.
#[test]
fn web_doctor_generates_no_key() {
    let home = tempfile::tempdir().unwrap();
    let mut fixture = WebDoctorConfig::in_dir(home.path());
    fixture.upstream_ca_path = Some(absent_ca(home.path()));
    let config = write_config(home.path(), &fixture);
    let mut planted = fixture.planted();
    planted.push(planted_key_base64());

    let before = fs_snapshot(home.path());
    let (code, report) = doctor(&config, &home_env(home.path()), &planted);
    assert_unchanged(
        &before,
        &fs_snapshot(home.path()),
        "a doctor run with no key",
    );
    assert_eq!(
        verdict(&report, "proxy.cookie_key"),
        (Outcome::NotConfigured, Some("ephemeral_each_start")),
        "{report:?}"
    );
    assert_ne!(code, 1, "{report:?}");

    let key_path = home.path().join("lib").join("web.cookie");
    fixture.cookie_secret_path = Some(key_path.clone());
    let config = write_config(home.path(), &fixture);
    let before = fs_snapshot(home.path());
    let (code, report) = doctor(&config, &home_env(home.path()), &planted);
    assert_unchanged(
        &before,
        &fs_snapshot(home.path()),
        "a doctor run with an absent key file",
    );
    assert_eq!(
        verdict(&report, "proxy.cookie_key"),
        (Outcome::Failed, Some("the key file does not exist"))
    );
    assert!(!key_path.exists());
    assert_eq!(code, 1);
}

/// One root-run case: the key file, or the Fleet variable, and the row.
struct RootCase {
    label: &'static str,
    /// `cookie_secret_path`, as a file of this many bytes and this mode.
    file: Option<(usize, u32)>,
    /// A good `FLEET_SESSION_AEAD_KEY` instead.
    fleet_env: bool,
    expect: (Outcome, Option<&'static str>),
}

/// Run as root, the key file's readability is `not_sampled`,
/// `ran_as_root`, and the run cannot exit 0 (AC9, D12), even for a file
/// no other user could open. A content failure still fails, and a key
/// from the environment is content, so it stays `complete`. HOME is
/// mounted read-only for each run.
#[test]
fn web_doctor_root_run_is_incomplete() {
    let Some(userns) = Userns::for_test("web_doctor_root_run_is_incomplete") else {
        return;
    };
    let ran_as_root = (Outcome::NotSampled, Some("ran_as_root"));
    let cases = [
        RootCase {
            label: "a 32-byte key file",
            file: Some((32, 0o600)),
            fleet_env: false,
            expect: ran_as_root,
        },
        RootCase {
            label: "a mode-000 key file",
            file: Some((32, 0o000)),
            fleet_env: false,
            expect: ran_as_root,
        },
        RootCase {
            label: "a 31-byte key file",
            file: Some((31, 0o600)),
            fleet_env: false,
            expect: (Outcome::Failed, Some("the key file is not 32 bytes")),
        },
        RootCase {
            label: "FLEET_SESSION_AEAD_KEY",
            file: None,
            fleet_env: true,
            expect: (Outcome::Complete, None),
        },
    ];
    for RootCase {
        label,
        file,
        fleet_env,
        expect,
    } in cases
    {
        let home = tempfile::tempdir().unwrap();
        let key_path = home.path().join("lib").join("web.cookie");
        let mut fixture = WebDoctorConfig::in_dir(home.path());
        if let Some((len, mode)) = file {
            write_key_with_mode(&key_path, len, mode);
            fixture.cookie_secret_path = Some(key_path.clone());
        }
        let config = write_config(home.path(), &fixture);
        let key = planted_key_base64();
        let mut env = home_env(home.path());
        if fleet_env {
            env.push(("FLEET_SESSION_AEAD_KEY", OsString::from(&key)));
        }
        let mut planted_owned = fixture.planted();
        planted_owned.push(key);
        let planted: Vec<&str> = planted_owned.iter().map(String::as_str).collect();

        let before = fs_snapshot(home.path());
        let run = run_web_doctor_in_userns(
            &userns,
            Some(home.path()),
            &doctor_args(&config, "json"),
            &env,
            &planted,
        );
        assert_unchanged(&before, &fs_snapshot(home.path()), label);
        assert!(run.stderr.is_empty(), "{label}: {}", run.stderr);
        let report = report(&run.stdout);
        let key_row = row(&report, "proxy.cookie_key");
        assert_eq!(
            (key_row.outcome, key_row.reason.as_deref()),
            expect,
            "{label}: {key_row:?}"
        );
        if file.is_some() {
            assert!(
                key_row
                    .source
                    .as_deref()
                    .is_some_and(|source| source.contains(&key_path.display().to_string())),
                "{label}: {key_row:?}"
            );
        }
        assert_ne!(run.code, 0, "{label}: {}", run.stdout);
        assert_eq!(i32::from(report.verdict().exit_code()), run.code, "{label}");
    }
}

/// One `proxy.cookie_settings` case.
struct CookieCase<'a> {
    label: &'a str,
    /// `[web] public_origins`.
    origins: Vec<String>,
    /// `allow_insecure_cookies = true` in the file.
    insecure: bool,
    variables: Vec<(&'static str, &'a str)>,
    expect: (Outcome, Option<&'a str>),
    /// Text the row's detail or next action must contain.
    text: &'a str,
}

/// Cookies without Secure on an origin that is not loopback fail (D8),
/// judged on the effective origins after the environment replaced the
/// file's list; on loopback origins they pass. A `FLEET_SESSION_COOKIE_*`
/// value that does not parse fails, naming the variable and the rule, not
/// the value. Each setting is reported with its source, and a shared
/// domain as such, never by value.
#[test]
fn web_doctor_insecure_cookie_off_loopback_fails() {
    for case in cookie_cases() {
        let label = case.label;
        let home = tempfile::tempdir().unwrap();
        let mut fixture = WebDoctorConfig::in_dir(home.path());
        fixture.public_origins = case.origins;
        fixture.allow_insecure_cookies = case.insecure;
        let planted: Vec<&str> = case
            .variables
            .iter()
            .map(|(_, value)| *value)
            .filter(|value| *value != "false")
            .collect();
        let (code, report) = run(
            home.path(),
            &fixture,
            &env_with(home.path(), &case.variables),
            &planted,
        );
        let settings = row(&report, "proxy.cookie_settings");
        assert_eq!(
            (settings.outcome, settings.reason.as_deref()),
            case.expect,
            "{label}: {settings:?}"
        );
        let shown = [&settings.detail, &settings.next_action];
        assert!(
            shown
                .iter()
                .any(|part| part.as_deref().is_some_and(|part| part.contains(case.text))),
            "{label}: {settings:?}"
        );
        if case.expect.0 == Outcome::Failed {
            assert_eq!(code, 1, "{label}");
        }
    }

    // A shared domain and the file's lifetime, each with its source; the
    // domain is planted by the fixture, so the leak check proves it is
    // not shown.
    let home = tempfile::tempdir().unwrap();
    let mut fixture = WebDoctorConfig::in_dir(home.path());
    fixture.shared_domain = Some("private-secret.example".to_owned());
    fixture.session_ttl_secs = Some(3600);
    let (_, report) = run(
        home.path(),
        &fixture,
        &env_with(home.path(), &[("FLEET_SESSION_COOKIE_PATH", "/")]),
        &[],
    );
    let settings = row(&report, "proxy.cookie_settings");
    assert_eq!(
        settings.detail.as_deref(),
        Some(
            "Secure on (the default); a shared domain (the config file); \
             path / (FLEET_SESSION_COOKIE_PATH); lifetime 3600 s (the config file)"
        ),
        "{settings:?}"
    );
}

/// The cases of [`web_doctor_insecure_cookie_off_loopback_fails`].
fn cookie_cases() -> Vec<CookieCase<'static>> {
    let off_loopback = (
        Outcome::Failed,
        Some("Secure is off for an origin that is not loopback"),
    );
    let not_valid = (
        Outcome::Failed,
        Some("a session cookie variable is not valid"),
    );
    let complete = (Outcome::Complete, None);
    let remote = || vec![PLANTED_ORIGIN.to_owned()];
    let loopback = || {
        vec![
            "http://localhost:8090".to_owned(),
            "http://[::1]:8090".to_owned(),
        ]
    };
    let case = |label, origins, insecure, variables, expect, text| CookieCase {
        label,
        origins,
        insecure,
        variables,
        expect,
        text,
    };
    vec![
        case(
            "Secure by default",
            remote(),
            false,
            vec![],
            complete,
            "Secure on (the default); host-only (the default); path / (the default); \
             lifetime 86400 s (the default)",
        ),
        case(
            "allow_insecure_cookies off loopback",
            remote(),
            true,
            vec![],
            off_loopback,
            "remove allow_insecure_cookies = true from ",
        ),
        case(
            "allow_insecure_cookies on loopback",
            loopback(),
            true,
            vec![],
            complete,
            "Secure off (the config file)",
        ),
        case(
            "FLEET_SESSION_COOKIE_SECURE=false off loopback",
            remote(),
            false,
            vec![("FLEET_SESSION_COOKIE_SECURE", "false")],
            off_loopback,
            "unset FLEET_SESSION_COOKIE_SECURE or set it to true",
        ),
        case(
            "the variable's origins are not loopback",
            loopback(),
            true,
            vec![("FLEET_SESSION_PUBLIC_ORIGINS", PLANTED_ORIGIN)],
            off_loopback,
            "1 origin not loopback",
        ),
        case(
            "the variable's origins are loopback",
            remote(),
            true,
            vec![("FLEET_SESSION_PUBLIC_ORIGINS", "http://localhost:8090")],
            complete,
            "Secure off (the config file)",
        ),
        case(
            "FLEET_SESSION_COOKIE_SECURE not a boolean",
            remote(),
            false,
            vec![("FLEET_SESSION_COOKIE_SECURE", "private-secret")],
            not_valid,
            "FLEET_SESSION_COOKIE_SECURE: expected exactly `true` or `false`",
        ),
        case(
            "FLEET_SESSION_COOKIE_DOMAIN not a domain",
            remote(),
            false,
            vec![("FLEET_SESSION_COOKIE_DOMAIN", "private-secret.example/x")],
            not_valid,
            "FLEET_SESSION_COOKIE_DOMAIN: ",
        ),
        case(
            "FLEET_SESSION_COOKIE_PATH not /",
            remote(),
            false,
            vec![("FLEET_SESSION_COOKIE_PATH", "/private-secret")],
            not_valid,
            "FLEET_SESSION_COOKIE_PATH: ",
        ),
    ]
}
