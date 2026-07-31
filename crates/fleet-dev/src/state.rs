// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::config::validate_state_scope;
use crate::error::{Error, Result};

const METADATA_FILE: &str = "provider.json";
pub const API_KEY_FILE: &str = "dev-api-key";
pub const SESSION_KEY_FILE: &str = "session-aead-key";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderIdentity {
    pub schema: u32,
    pub provider: String,
    pub host: String,
    pub port: u16,
    pub database: String,
}

impl ProviderIdentity {
    #[must_use]
    pub fn new(provider: &str, host: &str, port: u16) -> Self {
        Self {
            schema: 1,
            provider: provider.to_owned(),
            host: host.to_ascii_lowercase(),
            port,
            database: "fleet_dev".to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct GlobalLock {
    file: File,
}

impl Drop for GlobalLock {
    fn drop(&mut self) {
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

impl GlobalLock {
    pub fn acquire(state_root: &Path) -> Result<Self> {
        let fleet_root = state_root.join("fleet");
        secure_directory(&fleet_root)?;
        let path = fleet_root.join("dev.lock");
        reject_symlink_if_present(&path)?;
        let file = secure_open(&path)?;
        fs4::FileExt::try_lock(&file).map_err(|source| match source {
            fs4::TryLockError::WouldBlock => Error::InvalidArgument(
                "another fleet-dev stack is active; stop it before starting a second".to_owned(),
            ),
            fs4::TryLockError::Error(source) => Error::Io(source),
        })?;
        Ok(Self { file })
    }
}

#[derive(Debug, Clone)]
pub struct ScopedState {
    directory: PathBuf,
}

impl ScopedState {
    pub fn open(state_root: &Path, scope: &str, identity: &ProviderIdentity) -> Result<Self> {
        validate_state_scope(scope).map_err(Error::InvalidArgument)?;
        let fleet_root = state_root.join("fleet");
        secure_directory(&fleet_root)?;
        let directory = fleet_root.join(scope);
        secure_directory(&directory)?;
        let state = Self { directory };
        state.bind_identity(identity)?;
        Ok(state)
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub fn api_key_path(&self) -> PathBuf {
        self.directory.join(API_KEY_FILE)
    }

    #[must_use]
    pub fn session_key_path(&self) -> PathBuf {
        self.directory.join(SESSION_KEY_FILE)
    }

    pub fn read_api_key(&self) -> Result<Option<String>> {
        read_secret_file(&self.api_key_path(), "development API key")
    }

    pub fn write_api_key(&self, value: &str) -> Result<()> {
        atomic_write(&self.api_key_path(), value.as_bytes())
    }

    pub fn read_session_key(&self) -> Result<Option<String>> {
        read_secret_file(&self.session_key_path(), "session AEAD key")
    }

    pub fn write_session_key(&self, value: &str) -> Result<()> {
        atomic_write(&self.session_key_path(), value.as_bytes())
    }

    fn bind_identity(&self, expected: &ProviderIdentity) -> Result<()> {
        let path = self.directory.join(METADATA_FILE);
        reject_symlink_if_present(&path)?;
        match open_read_nofollow(&path) {
            Ok(mut file) => {
                check_private_file(&file, &path)?;
                let mut raw = Vec::new();
                Read::by_ref(&mut file)
                    .take(64 * 1024 + 1)
                    .read_to_end(&mut raw)
                    .map_err(Error::Io)?;
                if raw.len() > 64 * 1024 {
                    return Err(Error::InvalidConfig {
                        kind: "provider identity",
                        path,
                        message: "file exceeds 65536 bytes".to_owned(),
                    });
                }
                let actual: ProviderIdentity =
                    serde_json::from_slice(&raw).map_err(|source| Error::ParseFile {
                        kind: "provider identity",
                        path: path.clone(),
                        message: source.to_string(),
                    })?;
                if actual != *expected {
                    return Err(Error::InvalidConfig {
                        kind: "provider identity",
                        path,
                        message: format!(
                            "state scope is already bound to {actual:?}, not {expected:?}"
                        ),
                    });
                }
                Ok(())
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                let body = serde_json::to_vec_pretty(expected).expect("serializable identity");
                atomic_write(&path, &body)
            }
            Err(source) => Err(Error::ReadFile {
                kind: "provider identity",
                path,
                source,
            }),
        }
    }
}

pub fn state_root(
    xdg_state_home: Option<&Path>,
    home: Option<&Path>,
) -> std::result::Result<PathBuf, String> {
    if let Some(xdg) = xdg_state_home.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(xdg.to_owned());
    }
    home.map(|path| path.join(".local/state"))
        .ok_or_else(|| "HOME is not set and XDG_STATE_HOME is unavailable".to_owned())
}

pub fn process_state_root() -> Result<PathBuf> {
    let xdg = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    state_root(xdg.as_deref(), home.as_deref()).map_err(Error::InvalidArgument)
}

fn secure_directory(path: &Path) -> Result<()> {
    reject_symlink_if_present(path)?;
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::InvalidArgument(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    set_mode(path, 0o700)?;
    Ok(())
}

fn secure_open(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    nofollow(&mut options);
    let file = options.open(path)?;
    set_file_mode(&file, 0o600)?;
    Ok(file)
}

fn read_secret_file(path: &Path, kind: &'static str) -> Result<Option<String>> {
    let mut file = match open_read_nofollow(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::ReadFile {
                kind,
                path: path.to_owned(),
                source,
            });
        }
    };
    check_private_file(&file, path)?;
    let mut value = String::new();
    Read::by_ref(&mut file)
        .take(64 * 1024 + 1)
        .read_to_string(&mut value)
        .map_err(|source| Error::ReadFile {
            kind,
            path: path.to_owned(),
            source,
        })?;
    if value.len() > 64 * 1024 {
        return Err(Error::InvalidConfig {
            kind,
            path: path.to_owned(),
            message: "file exceeds 65536 bytes".to_owned(),
        });
    }
    if value.is_empty() || value.contains(['\n', '\r']) {
        return Err(Error::InvalidConfig {
            kind,
            path: path.to_owned(),
            message: "expected exactly one non-empty line without a trailing newline".to_owned(),
        });
    }
    Ok(Some(value))
}

fn open_read_nofollow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    nofollow(&mut options);
    options.open(path)
}

#[cfg(unix)]
fn nofollow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(nix::libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn nofollow(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_file_mode(file: &File, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn check_private_file(file: &File, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::InvalidArgument(format!(
            "{} must be a regular owner-only file (mode 0600)",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_file(file: &File, path: &Path) -> Result<()> {
    if !file.metadata()?.is_file() {
        return Err(Error::InvalidArgument(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidArgument(format!("{} has no parent", path.display())))?;
    reject_symlink_if_present(path)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    set_mode(temporary.path(), 0o600)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn reject_symlink_if_present(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::InvalidArgument(format!(
            "refusing symlink at {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io(source)),
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(host: &str) -> ProviderIdentity {
        ProviderIdentity::new("docker", host, 5435)
    }

    #[test]
    fn xdg_precedes_home() {
        assert_eq!(
            state_root(Some(Path::new("/xdg")), Some(Path::new("/home/dev"))).unwrap(),
            Path::new("/xdg")
        );
        assert_eq!(
            state_root(None, Some(Path::new("/home/dev"))).unwrap(),
            Path::new("/home/dev/.local/state")
        );
    }

    #[test]
    fn scoped_state_binds_identity_and_reuses_secrets() {
        let root = tempfile::tempdir().unwrap();
        let state = ScopedState::open(root.path(), "docker", &identity("127.0.0.1")).unwrap();
        assert_eq!(state.read_api_key().unwrap(), None);
        state.write_api_key("flt_test").unwrap();
        assert_eq!(state.read_api_key().unwrap().as_deref(), Some("flt_test"));

        ScopedState::open(root.path(), "docker", &identity("127.0.0.1")).unwrap();
        let error = ScopedState::open(root.path(), "docker", &identity("other")).unwrap_err();
        assert!(error.to_string().contains("already bound"));
    }

    #[cfg(unix)]
    #[test]
    fn state_modes_are_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let state = ScopedState::open(root.path(), "docker", &identity("127.0.0.1")).unwrap();
        state.write_session_key("base64value").unwrap();
        assert_eq!(
            std::fs::metadata(state.directory()).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(state.session_key_path()).unwrap().mode() & 0o777,
            0o600
        );

        let target = root.path().join("target");
        std::fs::write(&target, "secret").unwrap();
        let scope = root.path().join("fleet/evil");
        symlink(&target, &scope).unwrap();
        assert!(ScopedState::open(root.path(), "evil", &identity("x")).is_err());
    }

    #[test]
    fn global_lock_rejects_a_concurrent_stack_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let first = GlobalLock::acquire(root.path()).unwrap();
        assert!(GlobalLock::acquire(root.path()).is_err());
        drop(first);
        GlobalLock::acquire(root.path()).unwrap();
    }
}
