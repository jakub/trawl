// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --check-config`: the local configuration checks.

use std::path::{Path, PathBuf};

use trawl_config::STORAGE_MARKERS;

use crate::config::{Config, ConfigError};

pub fn check_config(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(path)?;
    check_loaded(&config)?;
    Ok(())
}

#[derive(Debug)]
pub enum LoadedFault {
    LogFile(std::io::Error),
    FleetUrl(ConfigError),
    AppUrl(ConfigError),
    Ingest,
    Syslog(crate::syslog::SyslogPeerFault),
}

impl std::fmt::Display for LoadedFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LogFile(error) => error.fmt(f),
            Self::FleetUrl(error) | Self::AppUrl(error) => error.fmt(f),
            Self::Ingest => {
                f.write_str("invalid setting at ingest: check severity_from and time_from")
            }
            Self::Syslog(fault) => fault.fmt(f),
        }
    }
}

impl std::error::Error for LoadedFault {}

pub fn check_loaded(config: &Config) -> Result<(), LoadedFault> {
    validate_file_log_config(config).map_err(LoadedFault::LogFile)?;
    config
        .auth
        .resolve_database_url()
        .map_err(LoadedFault::FleetUrl)?;
    config
        .storage
        .resolve_database_url()
        .map_err(LoadedFault::AppUrl)?;
    crate::ingest::producer::Derivation::resolve(&config.ingest)
        .map_err(|_| LoadedFault::Ingest)?;
    crate::syslog::SyslogPeers::resolve(&config.syslog).map_err(LoadedFault::Syslog)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFileRefusal {
    MarkerOverlap,
    Invalid,
    PermissionDenied,
    Unobserved,
}

impl LogFileRefusal {
    #[must_use]
    pub fn of(error: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        if let Some(inner) = error.get_ref()
            && inner.is::<MarkerOverlap>()
        {
            return Self::MarkerOverlap;
        }
        match error.kind() {
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            // `Other` is the resolver's own symlink limit: no OS error
            // decodes to it.
            ErrorKind::NotADirectory | ErrorKind::InvalidFilename | ErrorKind::Other => {
                Self::Invalid
            }
            kind if is_symlink_loop(kind) => Self::Invalid,
            _ => Self::Unobserved,
        }
    }
}

/// Whether `kind` is what `ELOOP` decodes to. That kind has no stable name
/// to match on, so the errno is decoded for comparison.
fn is_symlink_loop(kind: std::io::ErrorKind) -> bool {
    #[cfg(unix)]
    {
        kind == std::io::Error::from(rustix::io::Errno::LOOP).kind()
    }
    #[cfg(not(unix))]
    {
        let _ = kind;
        false
    }
}

struct MarkerOverlap(String);

impl std::fmt::Display for MarkerOverlap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Debug for MarkerOverlap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::error::Error for MarkerOverlap {}

pub fn validate_file_log_config(config: &Config) -> std::io::Result<()> {
    if !config.internal_telemetry_enabled()
        && let Some(path) = &config.server.log_file
    {
        validate_log_destination(path, &config.data.base_dir())?;
    }
    Ok(())
}

/// Resolve existing aliases and missing suffixes without creating anything.
/// Resolve symlinks before `..`, including dangling links to future markers.
fn resolve_log_destination(path: &Path) -> std::io::Result<PathBuf> {
    fn resolve(path: &Path, links: u8) -> std::io::Result<PathBuf> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_symlink() => {
                if links == 40 {
                    return Err(std::io::Error::other(
                        "too many symlinks in log or storage path",
                    ));
                }
                let target = std::fs::read_link(path)?;
                resolve(&path.parent().unwrap_or(path).join(target), links + 1)
            }
            Ok(_) => std::fs::canonicalize(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = path.parent() else {
                    return Err(error);
                };
                let mut resolved = resolve(parent, links)?;
                match path.components().next_back() {
                    Some(std::path::Component::Normal(name)) => resolved.push(name),
                    Some(std::path::Component::ParentDir) => {
                        resolved.pop();
                    }
                    Some(std::path::Component::CurDir) => {}
                    _ => return Err(error),
                }
                // Collapsing a missing `child/..` can reveal an existing
                // symlink at the resulting path. Resolve that alias too.
                if resolved == path {
                    Ok(resolved)
                } else {
                    resolve(&resolved, links)
                }
            }
            Err(error) => Err(error),
        }
    }
    resolve(&std::env::current_dir()?.join(path), 0)
}

fn marker_log_error(marker: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        MarkerOverlap(format!(
            "server.log_file overlaps reserved storage marker {}; select a separate log file",
            marker.display()
        )),
    )
}

pub fn validate_log_destination(path: &Path, data_root: &Path) -> std::io::Result<PathBuf> {
    let with_context = |error: std::io::Error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to validate server.log_file {}: {error}",
                path.display()
            ),
        )
    };
    let resolved = resolve_log_destination(path).map_err(with_context)?;
    for name in STORAGE_MARKERS {
        let marker = data_root.join(name);
        let resolved_marker = resolve_log_destination(&marker).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "failed to inspect reserved storage marker {} while validating server.log_file {}: {error}",
                    marker.display(),
                    path.display()
                ),
            )
        })?;
        if resolved == resolved_marker {
            return Err(marker_log_error(&marker));
        }
    }
    #[cfg(unix)]
    match std::fs::metadata(&resolved) {
        Ok(metadata) => validate_log_identity(&metadata, data_root)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(with_context(error)),
    }
    Ok(resolved)
}

#[cfg(unix)]
pub fn validate_log_identity(
    metadata: &std::fs::Metadata,
    data_root: &Path,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    for name in STORAGE_MARKERS {
        let marker = data_root.join(name);
        match std::fs::metadata(&marker) {
            Ok(other) if metadata.dev() == other.dev() && metadata.ino() == other.ino() => {
                return Err(marker_log_error(&marker));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!(
                        "failed to inspect reserved storage marker {}: {error}",
                        marker.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn log_destination_rejects_marker_aliases_and_keeps_normal_paths() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("nested")).unwrap();
        let alias = tmp.path().join("alias");
        symlink("data/nested", &alias).unwrap();
        let log_link = tmp.path().join("server.log");
        for marker in STORAGE_MARKERS {
            symlink(format!("data/{marker}"), &log_link).unwrap();
            for path in [
                log_link.clone(),
                tmp.path().join("missing/../server.log"),
                alias.join(format!("../{marker}")),
                data.join(format!("missing/../{marker}")),
            ] {
                let error = validate_log_destination(&path, &data).unwrap_err();
                assert!(
                    error.to_string().contains("reserved storage marker"),
                    "{error}"
                );
                assert!(!data.join(marker).exists());
                assert!(!data.join("missing").exists());
            }
            std::fs::remove_file(&log_link).unwrap();
            std::fs::write(data.join(marker), b"marker bytes").unwrap();
            std::fs::hard_link(data.join(marker), &log_link).unwrap();
            assert!(validate_log_destination(&log_link, &data).is_err());
            let opened = std::fs::File::open(&log_link).unwrap();
            assert!(validate_log_identity(&opened.metadata().unwrap(), &data).is_err());
            assert_eq!(std::fs::read(&log_link).unwrap(), b"marker bytes");
            std::fs::remove_file(log_link.clone()).unwrap();
            std::fs::remove_file(data.join(marker)).unwrap();
        }
        for path in [
            data.join("server.log"),
            data.join("logs/EPOCH"),
            tmp.path().join("EPOCH"),
        ] {
            assert!(validate_log_destination(&path, &data).is_ok());
            assert!(!path.exists());
        }
        symlink("data", tmp.path().join("data-alias")).unwrap();
        assert!(
            validate_log_destination(&data.join("EPOCH"), &tmp.path().join("data-alias")).is_err()
        );
    }

    #[test]
    fn marker_refusals_print_as_before_and_classify_apart() {
        let typed = marker_log_error(Path::new("/srv/trawl/data/EPOCH"));
        let plain = std::io::Error::new(std::io::ErrorKind::InvalidInput, typed.to_string());
        assert_eq!(format!("{typed:?}"), format!("{plain:?}"));
        let boxed: Box<dyn std::error::Error> = Box::new(marker_log_error(Path::new("/x/EPOCH")));
        let boxed_plain: Box<dyn std::error::Error> = Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            marker_log_error(Path::new("/x/EPOCH")).to_string(),
        ));
        assert_eq!(format!("{boxed:?}"), format!("{boxed_plain:?}"));
        assert_eq!(format!("{boxed}"), format!("{boxed_plain}"));
        assert_eq!(LogFileRefusal::of(&typed), LogFileRefusal::MarkerOverlap);
        assert_eq!(LogFileRefusal::of(&plain), LogFileRefusal::Unobserved);
    }

    #[cfg(unix)]
    #[test]
    fn log_file_refusals_classify_by_what_they_prove() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let of = |path: &Path, data: &Path| {
            LogFileRefusal::of(&validate_log_destination(path, data).unwrap_err())
        };
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        assert_eq!(
            of(&data.join("missing/../EPOCH"), &data),
            LogFileRefusal::MarkerOverlap
        );
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(of(&file.join("log"), &data), LogFileRefusal::Invalid);
        let cycle = tmp.path().join("cycle");
        symlink("cycle", &cycle).unwrap();
        assert_eq!(of(&cycle, &data), LogFileRefusal::Invalid);
        assert_eq!(
            of(&cycle.join("server.log"), &data),
            LogFileRefusal::Invalid
        );
        assert_eq!(
            of(&tmp.path().join("n".repeat(300)), &data),
            LogFileRefusal::Invalid
        );
        let blocked = tmp.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let traversal = std::fs::read_dir(&blocked);
        let denied = validate_log_destination(&blocked.join("server.log"), &data);
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Privileged runners traverse mode 000; the kind is asserted where
        // the filesystem denies it.
        if traversal.is_err() {
            assert_eq!(
                LogFileRefusal::of(&denied.unwrap_err()),
                LogFileRefusal::PermissionDenied
            );
        }
        for errno in [rustix::io::Errno::IO, rustix::io::Errno::STALE] {
            let error = std::io::Error::new(
                std::io::Error::from(errno).kind(),
                "failed to validate server.log_file",
            );
            assert_eq!(LogFileRefusal::of(&error), LogFileRefusal::Unobserved);
        }
    }

    #[cfg(unix)]
    #[test]
    fn log_destination_resolution_errors_do_not_change_storage() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let cycle = tmp.path().join("cycle");
        symlink("cycle", &cycle).unwrap();
        assert!(validate_log_destination(&cycle, &data).is_err());
        assert!(!data.exists());
        let file = tmp.path().join("file");
        std::fs::write(&file, b"preserve").unwrap();
        let path = file.join("log");
        let error = validate_log_destination(&path, &data).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        assert!(error.to_string().contains("server.log_file"));
        assert!(error.to_string().contains(&path.display().to_string()));
        assert_eq!(std::fs::read(&file).unwrap(), b"preserve");
        assert!(!data.exists());
        let blocked = tmp.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let traversal = std::fs::read_dir(&blocked);
        let result = validate_log_destination(&blocked.join("server.log"), &data);
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Privileged runners can traverse mode 000. Where the filesystem
        // denies traversal, resolution must preserve that error.
        if traversal.is_err() {
            assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        assert!(!data.exists());
    }
}
