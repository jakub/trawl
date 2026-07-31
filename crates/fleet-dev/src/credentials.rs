// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};

use zeroize::{Zeroize, Zeroizing};

use crate::command::{CommandRunner, CommandSpec, require_success};
use crate::config::{CnpgProfile, OpProfile, expand_tilde};
use crate::environment;
use crate::error::{Error, Result};
use crate::resolver::{SecretValue, parse_output};

pub const OP_TOKEN_ENV: &str = "OP_SERVICE_ACCOUNT_TOKEN";

pub struct ServiceToken(Zeroizing<String>);

impl ServiceToken {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ServiceToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted service token>")
    }
}

pub fn load_service_token(profile: &OpProfile) -> Result<ServiceToken> {
    let configured = profile.token_file.as_deref().ok_or_else(|| {
        Error::InvalidArgument(
            "the selected plan requires 1Password, but [op].token_file is not configured"
                .to_owned(),
        )
    })?;
    let path = expand_tilde(configured);
    let mut file = open_token_file(&path)?;
    validate_token_file(&file, &path)?;
    let mut token = String::new();
    file.read_to_string(&mut token)
        .map_err(|source| Error::ReadFile {
            kind: "1Password service-account token",
            path: path.clone(),
            source,
        })?;
    while token.ends_with(['\n', '\r']) {
        token.pop();
    }
    if token.is_empty() || token.contains(['\n', '\r']) {
        token.zeroize();
        return Err(Error::InvalidConfig {
            kind: "1Password service-account token",
            path,
            message: "expected exactly one non-empty token line".to_owned(),
        });
    }
    Ok(ServiceToken(Zeroizing::new(token)))
}

fn open_token_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|source| Error::ReadFile {
        kind: "1Password service-account token",
        path: path.to_owned(),
        source,
    })
}

fn validate_token_file(file: &std::fs::File, path: &Path) -> Result<()> {
    let metadata = file.metadata().map_err(|source| Error::ReadFile {
        kind: "1Password service-account token",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(Error::InvalidConfig {
            kind: "1Password service-account token",
            path: path.to_owned(),
            message: "token path must be a regular file".to_owned(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode() & 0o777;
        if mode != 0o600 {
            return Err(Error::InvalidConfig {
                kind: "1Password service-account token",
                path: path.to_owned(),
                message: format!(
                    "token file mode {mode:04o} is too broad; run `chmod 600 {}`",
                    path.display()
                ),
            });
        }
        let owner = metadata.uid();
        let effective = nix::unistd::geteuid().as_raw();
        if owner != effective {
            return Err(Error::InvalidConfig {
                kind: "1Password service-account token",
                path: path.to_owned(),
                message: format!(
                    "token file is owned by uid {owner}, expected effective uid {effective}"
                ),
            });
        }
    }
    Ok(())
}

pub fn validate_service_token(runner: &dyn CommandRunner, token: &ServiceToken) -> Result<()> {
    let spec = op_spec(token).args(["whoami", "--format", "json"]);
    let output = runner.output(&spec)?;
    require_success(
        &spec,
        &output,
        "service-account validation failed; verify the token and its vault access",
    )
}

pub fn read_reference(
    runner: &dyn CommandRunner,
    token: &ServiceToken,
    reference: &str,
) -> Result<SecretValue> {
    let spec = op_spec(token).args(["read", reference]);
    let output = runner.output(&spec)?;
    require_success(
        &spec,
        &output,
        "1Password reference resolution failed; verify the configured reference and vault access",
    )?;
    let mut value = String::from_utf8(output.stdout)
        .map_err(|_| Error::InvalidArgument("1Password returned a non-UTF-8 value".to_owned()))?;
    while value.ends_with(['\n', '\r']) {
        value.pop();
    }
    if value.is_empty() || value.contains(['\n', '\r']) {
        value.zeroize();
        return Err(Error::InvalidArgument(
            "1Password returned an empty or multi-line value".to_owned(),
        ));
    }
    Ok(SecretValue::new(value))
}

pub fn database_url(
    runner: &dyn CommandRunner,
    token: &ServiceToken,
    profile: &CnpgProfile,
    database: &str,
) -> Result<SecretValue> {
    let username_ref =
        expand_credential_reference(&profile.credential_ref_template, database, "username");
    let password_ref =
        expand_credential_reference(&profile.credential_ref_template, database, "password");
    let username = read_reference(runner, token, &username_ref)?;
    let password = read_reference(runner, token, &password_ref)?;
    let mut url = url::Url::parse("postgres://localhost").expect("static URL is valid");
    url.set_host(Some(&profile.host))
        .map_err(|_| Error::InvalidArgument("cnpg.host is not a valid URL host".to_owned()))?;
    url.set_port(Some(profile.port))
        .map_err(|()| Error::InvalidArgument("cnpg.port is not valid".to_owned()))?;
    url.set_username(username.expose())
        .map_err(|()| Error::InvalidArgument("CNPG username cannot be URL-encoded".to_owned()))?;
    url.set_password(Some(password.expose()))
        .map_err(|()| Error::InvalidArgument("CNPG password cannot be URL-encoded".to_owned()))?;
    url.set_path(database);
    // Do not allow SQLx's opportunistic TLS default to downgrade remote CNPG
    // connections to plaintext.
    url.query_pairs_mut().append_pair("sslmode", "require");
    Ok(SecretValue::new(url.into()))
}

#[derive(Debug)]
pub struct ResolverInvocation<'a> {
    pub app: &'a str,
    pub root: &'a Path,
    pub command: &'a [String],
    pub cwd: Option<&'a Path>,
    pub static_environment: &'a BTreeMap<String, String>,
    pub pre_resolved: &'a BTreeMap<String, &'a SecretValue>,
    pub token: &'a ServiceToken,
}

pub fn run_app_resolver(
    runner: &dyn CommandRunner,
    invocation: &ResolverInvocation<'_>,
) -> Result<BTreeMap<String, SecretValue>> {
    let (program, args) = invocation.command.split_first().ok_or_else(|| {
        Error::InvalidArgument(format!("resolver for {} has no command", invocation.app))
    })?;
    let mut environment = environment::sanitized_base();
    environment.insert(
        OsString::from(OP_TOKEN_ENV),
        OsString::from(invocation.token.expose()),
    );
    environment.extend(
        invocation
            .static_environment
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    environment.extend(
        invocation
            .pre_resolved
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value.expose()))),
    );
    let working_directory = invocation.cwd.map_or_else(
        || invocation.root.to_owned(),
        |path| invocation.root.join(path),
    );
    let spec = CommandSpec::new(program)
        .args(args)
        .cwd(working_directory)
        .environment(environment)
        .timeout(std::time::Duration::from_mins(1))
        .output_limits(crate::resolver::MAX_RESOLVER_OUTPUT_BYTES, 256 * 1024);
    let mut output = runner.output(&spec)?;
    let status = require_success(
        &spec,
        &output,
        format!("resolver for {} failed", invocation.app),
    );
    let parsed = status.and_then(|()| parse_output(invocation.app, &output.stdout));
    output.stdout.zeroize();
    output.stderr.zeroize();
    parsed
}

fn op_spec(token: &ServiceToken) -> CommandSpec {
    CommandSpec::new("op")
        .environment(environment::sanitized_base())
        .env(OP_TOKEN_ENV, token.expose())
        .timeout(std::time::Duration::from_secs(30))
}

fn expand_credential_reference(template: &str, database: &str, field: &str) -> String {
    template
        .replace("{database}", database)
        .replace("{field}", field)
}

#[must_use]
pub fn token_path(profile: &OpProfile) -> Option<PathBuf> {
    profile.token_file.as_deref().map(expand_tilde)
}

#[must_use]
pub fn has_token(environment: &BTreeMap<OsString, OsString>) -> bool {
    environment.contains_key(OsStr::new(OP_TOKEN_ENV))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs::OpenOptions;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug)]
    struct FakeRunner {
        outputs: Mutex<VecDeque<std::process::Output>>,
        seen: Mutex<Vec<CommandSpec>>,
    }

    impl FakeRunner {
        fn success(stdout: &[u8]) -> Self {
            Self {
                outputs: Mutex::new(VecDeque::from([std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: stdout.to_vec(),
                    stderr: Vec::new(),
                }])),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn successes(outputs: impl IntoIterator<Item = &'static [u8]>) -> Self {
            Self {
                outputs: Mutex::new(
                    outputs
                        .into_iter()
                        .map(|stdout| std::process::Output {
                            status: std::process::ExitStatus::from_raw(0),
                            stdout: stdout.to_vec(),
                            stderr: Vec::new(),
                        })
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn output(&self, spec: &CommandSpec) -> Result<std::process::Output> {
            self.seen.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    #[test]
    fn op_receives_token_but_no_ambient_secret() {
        let runner = FakeRunner::success(br#"{"account":{"url":"example"}}"#);
        let token = ServiceToken(Zeroizing::new("token".to_owned()));
        validate_service_token(&runner, &token).unwrap();
        let seen = runner.seen.lock().unwrap();
        assert!(has_token(&seen[0].environment));
        assert!(!seen[0].environment.contains_key(OsStr::new("DATABASE_URL")));
    }

    #[test]
    fn resolver_receives_only_declared_values() {
        let runner = FakeRunner::success(br#"{"schema":1,"values":{"api_key":"answer"}}"#);
        let token = ServiceToken(Zeroizing::new("token".to_owned()));
        let built_in = SecretValue::new("postgres://secret".to_owned());
        let command = ["resolver".to_owned()];
        let static_environment = BTreeMap::from([("COASTWATCH_ENV".to_owned(), "dev".to_owned())]);
        let pre_resolved = BTreeMap::from([("DATABASE_URL".to_owned(), &built_in)]);
        let values = run_app_resolver(
            &runner,
            &ResolverInvocation {
                app: "coastwatch",
                root: Path::new("/repo"),
                command: &command,
                cwd: None,
                static_environment: &static_environment,
                pre_resolved: &pre_resolved,
                token: &token,
            },
        )
        .unwrap();
        assert_eq!(values["app.api_key"].expose(), "answer");
        let seen = runner.seen.lock().unwrap();
        assert_eq!(
            seen[0].environment.get(OsStr::new("DATABASE_URL")),
            Some(&OsString::from("postgres://secret"))
        );
        assert!(
            !seen[0]
                .environment
                .contains_key(OsStr::new("UNDECLARED_SECRET"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn token_loader_requires_a_regular_owner_only_file_and_rejects_symlinks() {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("token");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"service-token\n").unwrap();
        drop(file);
        let profile = OpProfile {
            token_file: Some(path.clone()),
        };
        assert_eq!(
            load_service_token(&profile).unwrap().expose(),
            "service-token"
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(load_service_token(&profile).is_err());

        let link = temporary.path().join("token-link");
        symlink(&path, &link).unwrap();
        assert!(
            load_service_token(&OpProfile {
                token_file: Some(link)
            })
            .is_err()
        );
    }

    #[test]
    fn cnpg_template_expansion_and_url_encoding_are_exact() {
        let runner = FakeRunner::successes([b"user@name\n".as_slice(), b"p:/?#[]@\n".as_slice()]);
        let token = ServiceToken(Zeroizing::new("token".to_owned()));
        let profile = CnpgProfile {
            host: "10.0.100.89".to_owned(),
            port: 5432,
            state_scope: "cnpg".to_owned(),
            credential_ref_template: "op://Homelab/CNPG {database}/{field}".to_owned(),
            session_aead_key_ref: "op://Homelab/session/credential".to_owned(),
        };
        let result = database_url(&runner, &token, &profile, "trawl_dev").unwrap();
        let parsed = url::Url::parse(result.expose()).unwrap();
        assert_eq!(parsed.username(), "user%40name");
        assert_eq!(parsed.password(), Some("p%3A%2F%3F%23%5B%5D%40"));
        assert_eq!(parsed.path(), "/trawl_dev");
        assert_eq!(
            parsed.query_pairs().collect::<Vec<_>>(),
            [("sslmode".into(), "require".into())]
        );
        let seen = runner.seen.lock().unwrap();
        assert_eq!(
            seen.iter()
                .map(|spec| spec.args[1].to_string_lossy())
                .collect::<Vec<_>>(),
            [
                "op://Homelab/CNPG trawl_dev/username",
                "op://Homelab/CNPG trawl_dev/password"
            ]
        );
        assert!(seen.iter().all(|spec| has_token(&spec.environment)));
    }
}
