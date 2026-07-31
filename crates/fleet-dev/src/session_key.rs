// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ffi::OsString;
use std::path::Path;

use fleet_auth::SessionKey;
use zeroize::Zeroize;

use crate::cli::DatabaseMode;
use crate::command::{CommandRunner, CommandSpec, require_success};
use crate::config::MachineProfile;
use crate::credentials::{ServiceToken, read_reference};
use crate::environment;
use crate::error::{Error, Result};
use crate::resolver::SecretValue;
use crate::state::ScopedState;

pub fn resolve(
    runner: &dyn CommandRunner,
    trawl_root: &Path,
    profile: &MachineProfile,
    state: &ScopedState,
    token: Option<&ServiceToken>,
) -> Result<SecretValue> {
    match profile.database {
        DatabaseMode::Docker => resolve_docker(runner, trawl_root, state),
        DatabaseMode::Cnpg => {
            let token = token.ok_or_else(|| {
                Error::InvalidArgument(
                    "CNPG session-key resolution requires a validated service token".to_owned(),
                )
            })?;
            let reference = &profile
                .cnpg
                .as_ref()
                .expect("validated CNPG profile")
                .session_aead_key_ref;
            let secret = read_reference(runner, token, reference)?;
            SessionKey::from_base64(secret.expose()).map_err(|_| {
                Error::InvalidArgument(
                    "CNPG session key reference did not resolve to a valid 32-byte base64 key"
                        .to_owned(),
                )
            })?;
            Ok(secret)
        }
    }
}

fn resolve_docker(
    runner: &dyn CommandRunner,
    trawl_root: &Path,
    state: &ScopedState,
) -> Result<SecretValue> {
    if let Some(existing) = state.read_session_key()? {
        validate_canonical(&existing)?;
        return Ok(SecretValue::new(existing));
    }
    let spec = CommandSpec::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "fleet-admin",
            "--",
            "generate-session-key",
        ])
        .cwd(trawl_root)
        .environment(environment::sanitized_base())
        // The key arrives on stdout, so this one cannot stream; give it a
        // build-shaped deadline because `cargo run` may compile from cold.
        .timeout(crate::command::BUILD_TIMEOUT)
        .report_stderr();
    let output = runner.output(&spec)?;
    require_success(&spec, &output, "fleet-admin generate-session-key failed")?;
    let mut encoded = String::from_utf8(output.stdout).map_err(|_| {
        Error::InvalidArgument("fleet-admin returned a non-UTF-8 session key".to_owned())
    })?;
    if encoded.ends_with('\n') {
        encoded.pop();
        if encoded.ends_with('\r') {
            encoded.pop();
        }
    }
    if encoded.contains(['\n', '\r']) {
        encoded.zeroize();
        return Err(Error::InvalidArgument(
            "fleet-admin returned extra output with the session key".to_owned(),
        ));
    }
    validate_canonical(&encoded)?;
    state.write_session_key(&encoded)?;
    Ok(SecretValue::new(encoded))
}

fn validate_canonical(encoded: &str) -> Result<()> {
    if encoded.len() != 43 {
        return Err(Error::InvalidArgument(format!(
            "session key must be exactly 43 base64url-no-pad characters, got {}",
            encoded.len()
        )));
    }
    let key = SessionKey::from_base64(encoded)
        .map_err(|_| Error::InvalidArgument("session key is not valid base64".to_owned()))?;
    if key.to_base64url().as_str() != encoded {
        return Err(Error::InvalidArgument(
            "session key is not in canonical base64url-no-pad form".to_owned(),
        ));
    }
    Ok(())
}

#[must_use]
pub fn runtime_key_environment(value: &SecretValue) -> (OsString, OsString) {
    (
        OsString::from(fleet_auth::ENV_SESSION_AEAD_KEY),
        OsString::from(value.expose()),
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Mutex;

    use super::*;
    use crate::state::ProviderIdentity;

    #[derive(Debug)]
    struct KeyRunner {
        output: String,
        calls: Mutex<usize>,
    }

    impl CommandRunner for KeyRunner {
        fn output(&self, spec: &CommandSpec) -> Result<std::process::Output> {
            assert_eq!(spec.program, "cargo");
            assert!(
                !spec
                    .environment
                    .contains_key(std::ffi::OsStr::new("OP_SERVICE_ACCOUNT_TOKEN"))
            );
            *self.calls.lock().unwrap() += 1;
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: format!("{}\n", self.output).into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn accepts_only_canonical_base64url() {
        let canonical = SessionKey::from_bytes([0x42; 32]).to_base64url();
        assert_eq!(canonical.len(), 43);
        assert!(validate_canonical(&canonical).is_ok());
        assert!(validate_canonical("short").is_err());
        assert!(validate_canonical(&format!("{}=", canonical.as_str())).is_err());
    }

    #[test]
    fn docker_key_is_generated_once_and_persisted_canonically() {
        let root = tempfile::tempdir().unwrap();
        let state = ScopedState::open(
            root.path(),
            "docker",
            &ProviderIdentity::new("docker", "127.0.0.1", 5435),
        )
        .unwrap();
        let canonical = SessionKey::from_bytes([0x42; 32]).to_base64url();
        let runner = KeyRunner {
            output: canonical.to_string(),
            calls: Mutex::new(0),
        };
        let first = resolve_docker(&runner, Path::new("/trawl"), &state).unwrap();
        let second = resolve_docker(&runner, Path::new("/trawl"), &state).unwrap();
        assert_eq!(first.expose(), canonical.as_str());
        assert_eq!(second.expose(), canonical.as_str());
        assert_eq!(*runner.calls.lock().unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(state.session_key_path()).unwrap(),
            canonical.as_str()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                std::fs::metadata(state.session_key_path()).unwrap().mode() & 0o777,
                0o600
            );
        }
    }
}
