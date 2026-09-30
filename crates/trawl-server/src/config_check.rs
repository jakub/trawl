// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --check-config`: the local configuration checks.
//!
//! These checks read the configuration and path metadata only. They connect
//! to nothing and create nothing. `trawld --check-config` runs them on a
//! file it loads, the normal boot runs the file-log validators before it
//! opens the log, and `trawld --doctor` runs [`check_loaded`] on a file it
//! read through its own bounded reader, so the three agree on what a valid
//! configuration is.

use std::path::{Path, PathBuf};

use crate::config::{Config, ConfigError};

/// Validate local configuration only. Database URLs are required, but no
/// network connectivity, credentials, or stored data contents are inspected.
/// File-log validation reads path metadata to detect marker aliases.
///
/// # Errors
/// The first check that refuses the file, with the text `--check-config`
/// prints.
pub fn check_config(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(path)?;
    check_loaded(&config)?;
    Ok(())
}

/// A loaded configuration that the checks after parsing refuse.
///
/// Its `Display` is the text `--check-config` prints for that refusal. The
/// doctor matches on the variant and never shows the text.
#[derive(Debug)]
pub enum LoadedFault {
    /// `server.log_file` aliases a reserved storage marker, or could not be
    /// resolved.
    LogFile(std::io::Error),
    /// No Fleet database URL is set.
    FleetUrl(ConfigError),
    /// No app-state database URL is set.
    AppUrl(ConfigError),
    /// The ingest derivation settings do not resolve.
    Ingest,
}

impl std::fmt::Display for LoadedFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LogFile(error) => error.fmt(f),
            Self::FleetUrl(error) | Self::AppUrl(error) => error.fmt(f),
            Self::Ingest => {
                f.write_str("invalid setting at ingest: check severity_from and time_from")
            }
        }
    }
}

impl std::error::Error for LoadedFault {}

/// The checks `--check-config` runs after the file parses, in its order.
///
/// # Errors
/// The first check that refuses the configuration.
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
    Ok(())
}

/// What a [`LoadedFault::LogFile`] error proves about the destination, for
/// a caller that reports the refusal without its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFileRefusal {
    /// It is, or resolves to, a reserved storage marker.
    MarkerOverlap,
    /// Its path can name no file: a component is not a directory, a name is
    /// too long, or the symlinks loop.
    Invalid,
    /// The running user may not inspect a path on the way.
    PermissionDenied,
    /// Inspecting it failed for another reason, which proves nothing about
    /// the destination.
    Unobserved,
}

impl LogFileRefusal {
    /// Classify an error from [`validate_file_log_config`].
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

/// The refusal of a log path that is a reserved storage marker, carried in
/// the `io::Error` so [`LogFileRefusal::of`] can tell it from a failure to
/// inspect. `Display` and `Debug` are the message's own, as a `String`
/// payload's are, so what boot and `--check-config` print is unchanged.
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

const STORAGE_MARKERS: [&str; 3] = ["EPOCH", "CATALOG", "REPIN"];

/// Refuse a `server.log_file` that aliases a reserved storage marker, when
/// the file logger is the one in use (internal telemetry off).
///
/// # Errors
/// The first alias or resolution failure found.
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

/// Resolve `path` without creating anything and refuse it when it aliases
/// a reserved storage marker under `data_root`.
///
/// # Errors
/// An alias, or a resolution failure naming the path.
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

/// Refuse an opened or existing file that is one of the reserved storage
/// markers under `data_root`, by device and inode.
///
/// # Errors
/// The marker it aliases, or a marker that could not be inspected.
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

    /// A marker refusal prints exactly as the `String` error it replaced,
    /// through `Display` and through `Debug` (what `main`'s `Termination`
    /// prints), and only it classifies as a marker overlap.
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
        // An `EINVAL`, such as `readlink` on a link replaced after `lstat`,
        // proves nothing.
        assert_eq!(LogFileRefusal::of(&plain), LogFileRefusal::Unobserved);
    }

    /// Each refusal the validator produces classifies by what it proves:
    /// an overlap and a path that can name no file are invalid, a denied
    /// traversal and any other failure to inspect are not.
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
        // The resolver's own link limit, and the kernel's ELOOP from a loop
        // on the way to the last component.
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
