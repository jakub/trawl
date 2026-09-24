// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Publication markers: the record that makes one compaction publish
//! exactly-once across crashes (ADR-0041, slice 1).
//!
//! A publish turns consumed WAL files into one canonical parquet file. The
//! marker is written durably before the output is renamed into place, and it
//! is removed only after the consumed WAL files are retired. While it exists,
//! it claims the files it names: no compaction of its `(env, service)`, no
//! stale-tmp cleanup of its output, no rollup or retention of its partition.
//!
//! Location: `wal_dir/{env}/.publish-{service}.json`, one per
//! `(env, service)`. The env comes from the directory name and the service
//! from the file name. The `.json` extension keeps the marker out of the
//! compactor's `*.ndjson` WAL scan, and the staged writer's temp name
//! (`..publish-{service}.json.next.<pid>`) is inert for the same reason.
//!
//! Recovery decides publication from the canonical file's identity (size and
//! BLAKE3 digest) and nothing else. A missing temporary output never proves
//! publication, because recovery itself may have removed it before a crash.
//! Every recovery branch is idempotent, so an interrupted recovery can rerun.
//!
//! Recovery deletes files named by an on-disk record, so [`read_marker`]
//! confines every component before any path is built: paths are only ever
//! `data_dir/{env}/{date}/{HH}/{service}.parquet[.tmp]` and
//! `wal_dir/{env}/{name}`, from validated parts.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Marker file name prefix inside `wal_dir/{env}`.
const MARKER_PREFIX: &str = ".publish-";
/// Marker file name suffix. Anything but `.ndjson`, which the WAL scan claims.
const MARKER_SUFFIX: &str = ".json";

/// The on-disk marker body. Every component is relative; see the module
/// docs for how paths are rebuilt from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationMarker {
    /// `YYYY-MM-DD/HH`, relative to `data_dir/{env}`.
    pub partition: String,
    /// Bare file names of the consumed WAL files in `wal_dir/{env}`. Only
    /// surviving inputs; a quarantined `.corrupt` file is never listed.
    pub wal: Vec<String>,
    /// Size in bytes of the published output.
    pub size: u64,
    /// BLAKE3 digest of the published output, 64 lowercase hex digits.
    pub blake3: String,
}

/// The size and BLAKE3 digest of an output file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputIdentity {
    pub size: u64,
    pub hash: blake3::Hash,
}

/// Hash a file by streaming it. The size is the number of bytes hashed, so
/// the pair describes one read of the file rather than a stat and a read.
pub fn identity_of(path: &Path) -> io::Result<OutputIdentity> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let size = io::copy(&mut file, &mut hasher)?;
    Ok(OutputIdentity {
        size,
        hash: hasher.finalize(),
    })
}

/// A marker whose every component passed confinement. Paths are built only
/// from these components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedMarker {
    env: String,
    service: String,
    date: NaiveDate,
    hour: u8,
    wal: Vec<String>,
    identity: OutputIdentity,
}

impl ValidatedMarker {
    /// Validate the parts of a marker about to be written.
    pub fn new(
        env: &str,
        service: &str,
        date: NaiveDate,
        hour: u8,
        wal: Vec<String>,
        identity: OutputIdentity,
    ) -> Result<Self, String> {
        validate_env(env)?;
        validate_service(service)?;
        if hour > 23 {
            return Err(format!("hour {hour} is out of range"));
        }
        validate_wal_names(service, &wal)?;
        Ok(Self {
            env: env.to_owned(),
            service: service.to_owned(),
            date,
            hour,
            wal,
            identity,
        })
    }

    fn from_record(env: &str, service: &str, record: PublicationMarker) -> Result<Self, String> {
        let (date, hour) = parse_partition(&record.partition)
            .ok_or_else(|| format!("invalid partition {:?}", record.partition))?;
        let hash = parse_hex_digest(&record.blake3)
            .ok_or_else(|| "digest is not 64 lowercase hex digits".to_owned())?;
        Self::new(
            env,
            service,
            date,
            hour,
            record.wal,
            OutputIdentity {
                size: record.size,
                hash,
            },
        )
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn date(&self) -> NaiveDate {
        self.date
    }

    pub fn hour(&self) -> u8 {
        self.hour
    }

    pub fn identity(&self) -> OutputIdentity {
        self.identity
    }

    /// The bare WAL file names, in marker order.
    pub fn wal_names(&self) -> &[String] {
        &self.wal
    }

    /// `YYYY-MM-DD/HH`, the marker's partition field.
    pub fn partition(&self) -> String {
        format!("{}/{:02}", self.date.format("%Y-%m-%d"), self.hour)
    }

    /// `data_dir/{env}/{date}/{HH}`.
    pub fn output_dir(&self, data_dir: &Path) -> PathBuf {
        data_dir
            .join(&self.env)
            .join(self.date.format("%Y-%m-%d").to_string())
            .join(format!("{:02}", self.hour))
    }

    /// The published output path.
    pub fn canonical(&self, data_dir: &Path) -> PathBuf {
        self.output_dir(data_dir)
            .join(format!("{}.parquet", self.service))
    }

    /// The staged output path the publish renames from.
    pub fn tmp(&self, data_dir: &Path) -> PathBuf {
        self.output_dir(data_dir)
            .join(format!("{}.parquet.tmp", self.service))
    }

    /// `wal_dir/{env}`, the directory holding the WAL files and the marker.
    pub fn wal_env_dir(&self, wal_dir: &Path) -> PathBuf {
        wal_dir.join(&self.env)
    }

    /// The consumed WAL paths, in marker order.
    pub fn wal_paths(&self, wal_dir: &Path) -> Vec<PathBuf> {
        let env_dir = self.wal_env_dir(wal_dir);
        self.wal.iter().map(|name| env_dir.join(name)).collect()
    }

    /// The hot-buffer batch ids of the consumed WAL files: `{env}/{stem}`,
    /// as compaction derives them from WAL paths.
    pub fn batch_ids(&self) -> Vec<String> {
        self.wal
            .iter()
            .map(|name| {
                let stem = name.strip_suffix(".ndjson").unwrap_or(name);
                format!("{}/{stem}", self.env)
            })
            .collect()
    }

    /// `wal_dir/{env}/.publish-{service}.json`.
    pub fn marker_path(&self, wal_dir: &Path) -> PathBuf {
        self.wal_env_dir(wal_dir)
            .join(marker_file_name(&self.service))
    }

    /// The marker body as written to disk.
    pub fn encode(&self) -> String {
        let record = PublicationMarker {
            partition: self.partition(),
            wal: self.wal.clone(),
            size: self.identity.size,
            blake3: self.identity.hash.to_hex().to_string(),
        };
        serde_json::to_string(&record).expect("a marker record always serializes")
    }
}

fn marker_file_name(service: &str) -> String {
    format!("{MARKER_PREFIX}{service}{MARKER_SUFFIX}")
}

/// The service a marker file name names, before validation. `None` for any
/// file that is not a marker, including the staged writer's temp names.
fn marker_service(file_name: &str) -> Option<&str> {
    file_name
        .strip_prefix(MARKER_PREFIX)?
        .strip_suffix(MARKER_SUFFIX)
}

fn validate_env(env: &str) -> Result<(), String> {
    if !trawl_config::is_valid_env_name(env) {
        return Err(format!("invalid env name {env:?}"));
    }
    if trawl_config::RESERVED_ENV_NAMES.contains(&env) {
        return Err(format!("reserved env name {env:?}"));
    }
    Ok(())
}

fn validate_service(service: &str) -> Result<(), String> {
    if trawl_config::is_valid_service_name(service) {
        Ok(())
    } else {
        Err(format!("invalid service name {service:?}"))
    }
}

/// Each entry is one path component inside `wal_dir/{env}` that the WAL
/// scan would have picked up for `service`.
fn validate_wal_names(service: &str, wal: &[String]) -> Result<(), String> {
    if wal.is_empty() {
        return Err("WAL list is empty".to_owned());
    }
    let mut seen = BTreeSet::new();
    for name in wal {
        // The service charset excludes `/`, NUL and spaces; a name made of it
        // is a single component. `.` and `..` fail the suffix check.
        let Some(stem) = name.strip_suffix(".ndjson") else {
            return Err(format!("WAL entry {name:?} is not an .ndjson file"));
        };
        if stem.is_empty()
            || name.starts_with('.')
            || !name.bytes().all(trawl_config::is_valid_service_char)
        {
            return Err(format!("WAL entry {name:?} is not a single file name"));
        }
        if super::compaction::extract_service_from_filename(stem) != service {
            return Err(format!(
                "WAL entry {name:?} does not belong to service {service:?}"
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err(format!("WAL entry {name:?} is listed twice"));
        }
    }
    Ok(())
}

/// Parse `YYYY-MM-DD/HH` in exactly the spelling compaction writes.
fn parse_partition(partition: &str) -> Option<(NaiveDate, u8)> {
    let (date, hour) = partition.split_once('/')?;
    if hour.len() != 2 || !hour.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    if parsed.format("%Y-%m-%d").to_string() != date {
        return None;
    }
    let hour: u8 = hour.parse().ok()?;
    (hour <= 23).then_some((parsed, hour))
}

fn parse_hex_digest(hex: &str) -> Option<blake3::Hash> {
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    blake3::Hash::from_hex(hex).ok()
}

/// Why a marker could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerError {
    /// The marker is readable but fails confinement or parsing.
    Invalid(String),
    /// The marker could not be inspected or read.
    Io(String),
}

impl fmt::Display for MarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid publication marker: {reason}"),
            Self::Io(error) => f.write_str(error),
        }
    }
}

/// Read and confine the marker at `path`. The env is the parent directory's
/// name and the service comes from the file name.
pub fn read_marker(path: &Path) -> Result<ValidatedMarker, MarkerError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| MarkerError::Invalid("marker file name is not UTF-8".to_owned()))?;
    let service = marker_service(file_name)
        .ok_or_else(|| MarkerError::Invalid(format!("{file_name:?} is not a marker name")))?;
    let env = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(|| MarkerError::Invalid("marker env directory is not UTF-8".to_owned()))?;
    validate_env(env).map_err(MarkerError::Invalid)?;
    validate_service(service).map_err(MarkerError::Invalid)?;

    let kind = std::fs::symlink_metadata(path)
        .map_err(|e| MarkerError::Io(format!("failed to inspect {}: {e}", path.display())))?
        .file_type();
    if !kind.is_file() {
        return Err(MarkerError::Invalid(
            "marker is not a regular file".to_owned(),
        ));
    }
    let body = std::fs::read(path)
        .map_err(|e| MarkerError::Io(format!("failed to read {}: {e}", path.display())))?;
    let record: PublicationMarker = serde_json::from_slice(&body)
        .map_err(|e| MarkerError::Invalid(format!("unparseable marker: {e}")))?;
    ValidatedMarker::from_record(env, service, record).map_err(MarkerError::Invalid)
}

/// Write `marker` durably into `wal_dir/{env}`: staged write, fsync, rename,
/// directory fsync. A failed directory fsync is an error; the marker may then
/// be visible, and the caller must not rename the output into place.
pub fn write_marker(wal_dir: &Path, marker: &ValidatedMarker) -> Result<(), String> {
    crate::epoch::publish_marker_durable(
        &marker.wal_env_dir(wal_dir),
        &marker_file_name(&marker.service),
        &marker.encode(),
    )
}

/// Unlink a marker and fsync its directory. A missing marker is success, so
/// a rerun after a crash between the two steps still makes removal durable.
pub fn remove_marker_durably(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("failed to remove {}: {e}", path.display())),
    }
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    crate::epoch::fsync_dir(dir)
        .map_err(|e| format!("failed to fsync directory {}: {e}", dir.display()))
}

/// Marker files in one env's WAL directory, sorted by name.
fn list_markers(env_dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut markers = Vec::new();
    for entry in std::fs::read_dir(env_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(service) = name.to_str().and_then(marker_service) else {
            continue;
        };
        markers.push((service.to_owned(), entry.path()));
    }
    markers.sort();
    Ok(markers)
}

/// Env directories under the WAL root, and whether every root entry could be
/// inspected. An entry that cannot be inspected may be an env holding
/// markers, so an incomplete listing must block every env it did not list.
/// An unreadable root is an error.
fn list_wal_envs(wal_dir: &Path) -> Result<(Vec<(String, PathBuf)>, bool), String> {
    let mut skipped = false;
    let envs = crate::env_dirs::try_list_env_dirs_observed(wal_dir, || skipped = true)
        .map_err(|e| format!("failed to list WAL directory {}: {e}", wal_dir.display()))?;
    Ok((envs, !skipped))
}

/// What the pending markers claim, from one scan of the WAL root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublicationClaims {
    /// Some WAL root entry could not be inspected. It may be an env holding
    /// markers, so every env outside `scanned_envs` counts as claimed.
    root_incomplete: bool,
    /// Envs whose directory the scan reached.
    scanned_envs: BTreeSet<String>,
    /// `(env, service)` pairs with any marker, valid or not.
    services: BTreeSet<(String, String)>,
    /// `(env, service)` pairs whose marker could not be read or validated.
    /// Their partition is unknown, so they claim every output of the service.
    unknown_services: BTreeSet<(String, String)>,
    /// Envs whose WAL directory could not be listed: every service blocked.
    unlisted_envs: BTreeSet<String>,
    /// Envs with an unlisted directory or an unusable marker. Retention
    /// cannot tell which dates they claim, so they claim every date.
    opaque_envs: BTreeSet<String>,
    /// `(env, date, hour, service)` outputs named by valid markers.
    outputs: BTreeSet<(String, NaiveDate, u8, String)>,
}

impl PublicationClaims {
    /// Whether any marker, or any unknown, exists.
    pub fn any(&self) -> bool {
        self.root_incomplete || !self.services.is_empty() || !self.opaque_envs.is_empty()
    }

    /// Whether an env may hold markers this scan could not see.
    fn unseen(&self, env: &str) -> bool {
        self.root_incomplete && !self.scanned_envs.contains(env)
    }

    /// Whether compaction of `(env, service)` must wait for recovery.
    pub fn blocks_service(&self, env: &str, service: &str) -> bool {
        self.unseen(env)
            || self.unlisted_envs.contains(env)
            || self
                .services
                .contains(&(env.to_owned(), service.to_owned()))
    }

    /// Whether retention must keep the `data_dir/{env}/{date}` directory.
    pub fn claims_date(&self, env: &str, date: NaiveDate) -> bool {
        // An uninspectable root entry has no name, so it may be any env.
        self.root_incomplete
            || self.opaque_envs.contains(env)
            || self
                .outputs
                .iter()
                .any(|(e, d, _, _)| e == env && *d == date)
    }

    /// Whether the canonical or temporary output of `service` in
    /// `data_dir/{env}/{date}/{hour}` is claimed by a marker.
    pub fn claims_output(&self, env: &str, date: NaiveDate, hour: u8, service: &str) -> bool {
        self.unseen(env)
            || self.unlisted_envs.contains(env)
            || self
                .unknown_services
                .contains(&(env.to_owned(), service.to_owned()))
            || self
                .outputs
                .contains(&(env.to_owned(), date, hour, service.to_owned()))
    }
}

/// Collect the claims of every marker under `wal_dir`. A missing root has no
/// claims. An unreadable root is an error; callers must then assume
/// everything is claimed. A root entry that cannot be inspected makes every
/// env the scan did not reach claimed.
pub fn scan_claims(wal_dir: &Path) -> Result<PublicationClaims, String> {
    let mut claims = PublicationClaims::default();
    let (envs, complete) = list_wal_envs(wal_dir)?;
    claims.root_incomplete = !complete;
    for (env, env_dir) in envs {
        claims.scanned_envs.insert(env.clone());
        let Ok(markers) = list_markers(&env_dir) else {
            claims.unlisted_envs.insert(env.clone());
            claims.opaque_envs.insert(env);
            continue;
        };
        for (service, path) in markers {
            claims.services.insert((env.clone(), service.clone()));
            if let Ok(marker) = read_marker(&path) {
                claims
                    .outputs
                    .insert((env.clone(), marker.date, marker.hour, service));
            } else {
                claims.unknown_services.insert((env.clone(), service));
                claims.opaque_envs.insert(env.clone());
            }
        }
    }
    Ok(claims)
}

/// Why recovery refused to act on a marker. Recovery touches nothing and the
/// marker keeps blocking its `(env, service)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contradiction {
    /// The marker is unparseable or fails confinement.
    InvalidMarker(String),
    /// A path the marker names is a symlink, directory or other non-file.
    NotRegularFile(PathBuf),
    /// Neither the canonical output nor the temporary output exists.
    OutputMissing,
    /// The canonical output does not carry the recorded identity and no
    /// temporary output exists.
    OutputMismatch,
}

impl Contradiction {
    /// A stable name for logs.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidMarker(_) => "invalid_marker",
            Self::NotRegularFile(_) => "not_regular_file",
            Self::OutputMissing => "output_missing",
            Self::OutputMismatch => "output_mismatch",
        }
    }
}

impl fmt::Display for Contradiction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMarker(reason) => write!(f, "invalid marker: {reason}"),
            Self::NotRegularFile(path) => {
                write!(f, "{} is not a regular file", path.display())
            }
            Self::OutputMissing => {
                f.write_str("neither the canonical nor the temporary output exists")
            }
            Self::OutputMismatch => f.write_str(
                "the canonical output does not carry the recorded identity \
                 and no temporary output exists",
            ),
        }
    }
}

/// What recovery did with one marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// The canonical output carries the recorded identity. The WAL files
    /// were retired and the marker removed.
    Published,
    /// The output was never published. The marker and the temporary output
    /// were removed; the WAL files stay for the next compaction.
    Unpublished,
    /// Nothing was touched.
    Contradictory(Contradiction),
}

impl RecoveryOutcome {
    pub const fn kind(&self) -> RecoveryOutcomeKind {
        match self {
            Self::Published => RecoveryOutcomeKind::Published,
            Self::Unpublished => RecoveryOutcomeKind::Unpublished,
            Self::Contradictory(_) => RecoveryOutcomeKind::Contradictory,
        }
    }
}

/// [`RecoveryOutcome`] without its detail, for a metric label, plus
/// `Failed` for a recovery attempt that returned an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecoveryOutcomeKind {
    Published,
    Unpublished,
    Contradictory,
    /// A filesystem error or interruption stopped recovery of one marker,
    /// or recovery could not start. The next pass retries.
    Failed,
}

impl RecoveryOutcomeKind {
    pub const ALL: [Self; 4] = [
        Self::Published,
        Self::Unpublished,
        Self::Contradictory,
        Self::Failed,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Unpublished => "unpublished",
            Self::Contradictory => "contradictory",
            Self::Failed => "failed",
        }
    }

    /// Count one outcome on `trawl_publication_recovery_total`.
    pub fn record(self) {
        metrics::counter!(crate::metrics::PUBLICATION_RECOVERY_TOTAL, "outcome" => self.label())
            .increment(1);
    }
}

/// One marker's recovery. `Err` is a filesystem error or an interruption:
/// the marker may remain, keeps blocking, and the next pass retries it.
#[derive(Debug)]
pub struct RecoveryEntry {
    pub env: String,
    pub service: String,
    pub marker: PathBuf,
    pub result: Result<RecoveryOutcome, String>,
}

impl RecoveryEntry {
    pub fn kind(&self) -> RecoveryOutcomeKind {
        self.result
            .as_ref()
            .map_or(RecoveryOutcomeKind::Failed, RecoveryOutcome::kind)
    }
}

/// Every marker recovery looked at, plus env directories it could not list.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub entries: Vec<RecoveryEntry>,
    /// Env WAL directories that could not be listed. Their markers, if any,
    /// were not recovered.
    pub unlisted_envs: Vec<(String, String)>,
    /// Some WAL root entry could not be inspected, so recovery may have
    /// missed an env. The caller that lists the root counts that failure.
    pub root_incomplete: bool,
}

impl RecoveryReport {
    /// Count every marker's outcome on `trawl_publication_recovery_total`.
    /// Returns how many markers stay blocking their service: contradictions
    /// and failures.
    ///
    /// Unlisted envs and an incomplete root are not counted here. The caller
    /// that lists the same directories for its own work (the compaction
    /// tick's WAL scan, boot's WAL validation) owns that failure.
    pub fn record(&self) -> u64 {
        let mut blocked = 0;
        for entry in &self.entries {
            let kind = entry.kind();
            kind.record();
            if matches!(
                kind,
                RecoveryOutcomeKind::Contradictory | RecoveryOutcomeKind::Failed
            ) {
                blocked += 1;
            }
        }
        blocked
    }
}

/// Recover every pending marker under `wal_dir`.
///
/// `drain` runs for a published marker after the output directory fsync and
/// before WAL retirement; a live tick removes the marker's hot batches there
/// (under the publication write guard it holds), and boot passes a no-op.
/// It must be idempotent: an earlier attempt may have drained already.
///
/// Returns `Err` only when the WAL root cannot be listed. Each marker's
/// outcome is logged here; callers count the report with
/// [`RecoveryReport::record`].
pub fn recover(
    wal_dir: &Path,
    data_dir: &Path,
    mut drain: impl FnMut(&ValidatedMarker) -> Result<(), String>,
) -> Result<RecoveryReport, String> {
    let mut report = RecoveryReport::default();
    let (envs, complete) = list_wal_envs(wal_dir)?;
    report.root_incomplete = !complete;
    for (env, env_dir) in envs {
        let markers = match list_markers(&env_dir) {
            Ok(markers) => markers,
            Err(e) => {
                let error = format!("failed to list {}: {e}", env_dir.display());
                tracing::error!(
                    event_type = "publication_recovery_failed",
                    env = %env,
                    error = %error,
                    "cannot list publication markers; the env stays blocked"
                );
                report.unlisted_envs.push((env, error));
                continue;
            }
        };
        for (service, marker) in markers {
            let result = recover_one(&marker, wal_dir, data_dir, &mut drain);
            log_outcome(&env, &service, &marker, &result);
            report.entries.push(RecoveryEntry {
                env: env.clone(),
                service,
                marker,
                result,
            });
        }
    }
    Ok(report)
}

fn log_outcome(env: &str, service: &str, marker: &Path, result: &Result<RecoveryOutcome, String>) {
    match result {
        Ok(RecoveryOutcome::Published) => tracing::info!(
            event_type = "publication_recovered",
            env = %env,
            compact_service = %service,
            outcome = "published",
            "completed an interrupted publish"
        ),
        Ok(RecoveryOutcome::Unpublished) => tracing::info!(
            event_type = "publication_recovered",
            env = %env,
            compact_service = %service,
            outcome = "unpublished",
            "rolled back an unpublished output; its WAL stays for compaction"
        ),
        Ok(RecoveryOutcome::Contradictory(reason)) => tracing::error!(
            event_type = "publication_recovery_failed",
            env = %env,
            compact_service = %service,
            marker = %marker.display(),
            reason = reason.code(),
            detail = %reason,
            "contradictory publication marker; nothing touched and the service stays blocked"
        ),
        Err(error) => tracing::error!(
            event_type = "publication_recovery_failed",
            env = %env,
            compact_service = %service,
            marker = %marker.display(),
            error = %error,
            "publication recovery failed; the service stays blocked until a later pass"
        ),
    }
}

/// What a path the marker names currently is.
enum Entry {
    Absent,
    File(std::fs::Metadata),
    Other,
}

/// Inspect without following symlinks. Only `NotFound` means absent.
fn inspect(path: &Path) -> Result<Entry, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(Entry::File(meta)),
        Ok(_) => Ok(Entry::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Entry::Absent),
        Err(e) => Err(format!("failed to inspect {}: {e}", path.display())),
    }
}

/// Recover the marker at `marker_path` by the decision table in the module
/// docs. Every branch validates before its first mutation and is idempotent.
pub fn recover_one(
    marker_path: &Path,
    wal_dir: &Path,
    data_dir: &Path,
    drain: &mut impl FnMut(&ValidatedMarker) -> Result<(), String>,
) -> Result<RecoveryOutcome, String> {
    let marker = match read_marker(marker_path) {
        Ok(marker) => marker,
        Err(MarkerError::Invalid(reason)) => {
            return Ok(RecoveryOutcome::Contradictory(
                Contradiction::InvalidMarker(reason),
            ));
        }
        Err(MarkerError::Io(error)) => return Err(error),
    };
    let canonical = marker.canonical(data_dir);
    let published = match inspect(&canonical)? {
        Entry::Absent => None,
        Entry::Other => {
            return Ok(RecoveryOutcome::Contradictory(
                Contradiction::NotRegularFile(canonical),
            ));
        }
        Entry::File(meta) => Some(
            meta.len() == marker.identity.size
                && identity_of(&canonical)
                    .map_err(|e| format!("failed to hash {}: {e}", canonical.display()))?
                    == marker.identity,
        ),
    };
    if published == Some(true) {
        return recover_published(&marker, marker_path, wal_dir, data_dir, drain);
    }
    let tmp = marker.tmp(data_dir);
    match inspect(&tmp)? {
        Entry::File(_) => recover_unpublished(marker_path, &tmp),
        Entry::Other => Ok(RecoveryOutcome::Contradictory(
            Contradiction::NotRegularFile(tmp),
        )),
        Entry::Absent => Ok(RecoveryOutcome::Contradictory(match published {
            None => Contradiction::OutputMissing,
            Some(_) => Contradiction::OutputMismatch,
        })),
    }
}

fn recover_published(
    marker: &ValidatedMarker,
    marker_path: &Path,
    wal_dir: &Path,
    data_dir: &Path,
    drain: &mut impl FnMut(&ValidatedMarker) -> Result<(), String>,
) -> Result<RecoveryOutcome, String> {
    let wal_paths = marker.wal_paths(wal_dir);
    for path in &wal_paths {
        if let Entry::Other = inspect(path)? {
            return Ok(RecoveryOutcome::Contradictory(
                Contradiction::NotRegularFile(path.clone()),
            ));
        }
    }
    let output_dir = marker.output_dir(data_dir);
    crate::epoch::fsync_dir(&output_dir)
        .map_err(|e| format!("failed to fsync directory {}: {e}", output_dir.display()))?;
    step("recover:published:after_output_fsync")?;
    drain(marker)?;
    for (i, path) in wal_paths.iter().enumerate() {
        // Delete, else rename aside out of the WAL scan; already gone is done.
        super::compaction::retire_merged_input(path)?;
        step(&format!("recover:published:after_retire:{i}"))?;
    }
    let env_dir = marker.wal_env_dir(wal_dir);
    crate::epoch::fsync_dir(&env_dir)
        .map_err(|e| format!("failed to fsync directory {}: {e}", env_dir.display()))?;
    step("recover:published:after_wal_dir_fsync")?;
    remove_marker_durably(marker_path)?;
    step("recover:published:after_marker_unlink")?;
    Ok(RecoveryOutcome::Published)
}

/// Roll back a publish that never renamed its output: remove the marker
/// durably, then the tmp. The live publish uses this when a consumed WAL
/// file vanished before the rename; recovery's unpublished branch is the
/// same sequence.
pub fn roll_back_unpublished(marker_path: &Path, tmp: &Path) -> Result<(), String> {
    recover_unpublished(marker_path, tmp).map(|_| ())
}

/// Remove the marker durably before the tmp: removing the tmp first and
/// crashing would leave a marker whose output is gone, a contradiction.
fn recover_unpublished(marker_path: &Path, tmp: &Path) -> Result<RecoveryOutcome, String> {
    remove_marker_durably(marker_path)?;
    step("recover:unpublished:after_marker_remove")?;
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("failed to remove {}: {e}", tmp.display())),
    }
    step("recover:unpublished:after_tmp_remove")?;
    Ok(RecoveryOutcome::Unpublished)
}

fn step(name: &str) -> Result<(), String> {
    crash_point(name).map_err(|e| e.to_string())
}

/// A named point in a publish or recovery sequence where tests stop the
/// process.
///
/// In builds with `test-support` (and unit tests), `TRAWL_TEST_CRASH_AT`
/// names one point, read once. Reaching it logs
/// `event_type = "test_crash_point"` and parks the thread forever, so a
/// harness can wait for the line and SIGKILL the daemon there. Unit tests can
/// instead set [`interrupt`] to make the point return an error, stopping the
/// sequence in-process. Release builds compile this to `Ok(())`.
pub fn crash_point(name: &str) -> io::Result<()> {
    #[cfg(test)]
    if interrupt::matches(name) {
        return Err(io::Error::other(format!("test interruption at {name}")));
    }
    #[cfg(any(test, feature = "test-support"))]
    {
        static CRASH_AT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        let crash_at = CRASH_AT.get_or_init(|| std::env::var("TRAWL_TEST_CRASH_AT").ok());
        if crash_at.as_deref() == Some(name) {
            tracing::error!(
                event_type = "test_crash_point",
                point = name,
                "parked at test crash point"
            );
            loop {
                std::thread::park();
            }
        }
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = name;
    Ok(())
}

/// Unit-test interruption: make one crash point on the current thread
/// return an error, so a test can stop a sequence after any step and rerun.
#[cfg(test)]
pub(crate) mod interrupt {
    use std::cell::RefCell;

    thread_local! {
        static AT: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// Interrupt at `name` until the returned guard drops.
    pub(crate) fn at(name: &str) -> Guard {
        AT.with(|at| *at.borrow_mut() = Some(name.to_owned()));
        Guard
    }

    pub(super) fn matches(name: &str) -> bool {
        AT.with(|at| at.borrow().as_deref() == Some(name))
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            AT.with(|at| *at.borrow_mut() = None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const ENV: &str = "prod";
    const SERVICE: &str = "nginx";
    const WAL_A: &str = "nginx_1700000000000_ab12.ndjson";
    const WAL_B: &str = "nginx_1700000000001_cd34.ndjson";
    const OUTPUT: &[u8] = b"PAR1 new output bytes PAR1";
    const OLD_OUTPUT: &[u8] = b"PAR1 earlier canonical PAR1";

    struct Fixture {
        root: tempfile::TempDir,
        wal: PathBuf,
        data: PathBuf,
        marker: ValidatedMarker,
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 23).unwrap()
    }

    fn identity(bytes: &[u8]) -> OutputIdentity {
        OutputIdentity {
            size: bytes.len() as u64,
            hash: blake3::hash(bytes),
        }
    }

    /// WAL files present, tmp written, marker written: the state right
    /// after publish step 3.
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let marker = ValidatedMarker::new(
            ENV,
            SERVICE,
            date(),
            7,
            vec![WAL_A.to_owned(), WAL_B.to_owned()],
            identity(OUTPUT),
        )
        .unwrap();
        std::fs::create_dir_all(marker.wal_env_dir(&wal)).unwrap();
        std::fs::create_dir_all(marker.output_dir(&data)).unwrap();
        for path in marker.wal_paths(&wal) {
            std::fs::write(path, b"{\"message\":\"x\"}\n").unwrap();
        }
        std::fs::write(marker.tmp(&data), OUTPUT).unwrap();
        write_marker(&wal, &marker).unwrap();
        Fixture {
            root: tmp,
            wal,
            data,
            marker,
        }
    }

    /// The state right after publish step 4: output renamed into place.
    fn published_fixture() -> Fixture {
        let f = fixture();
        std::fs::rename(f.marker.tmp(&f.data), f.marker.canonical(&f.data)).unwrap();
        f
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                if meta.is_dir() {
                    out.insert(rel, b"<dir>".to_vec());
                    walk(root, &path, out);
                } else if meta.file_type().is_symlink() {
                    out.insert(rel, b"<symlink>".to_vec());
                } else {
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn recover_all(f: &Fixture, drained: &mut Vec<Vec<String>>) -> RecoveryReport {
        recover(&f.wal, &f.data, |m| {
            drained.push(m.batch_ids());
            Ok(())
        })
        .unwrap()
    }

    fn only_result(report: &RecoveryReport) -> &Result<RecoveryOutcome, String> {
        assert_eq!(report.entries.len(), 1, "{report:?}");
        assert!(report.unlisted_envs.is_empty());
        &report.entries[0].result
    }

    fn assert_published_end_state(f: &Fixture) {
        assert!(!f.marker.marker_path(&f.wal).exists(), "marker removed");
        for path in f.marker.wal_paths(&f.wal) {
            assert!(!path.exists(), "{} retired", path.display());
        }
        assert_eq!(
            identity_of(&f.marker.canonical(&f.data)).unwrap(),
            f.marker.identity(),
            "canonical untouched"
        );
        assert!(!scan_claims(&f.wal).unwrap().any());
    }

    fn assert_unpublished_end_state(f: &Fixture, canonical: Option<&[u8]>) {
        assert!(!f.marker.marker_path(&f.wal).exists(), "marker removed");
        for path in f.marker.wal_paths(&f.wal) {
            assert!(path.is_file(), "{} kept for compaction", path.display());
        }
        match canonical {
            Some(bytes) => {
                assert_eq!(std::fs::read(f.marker.canonical(&f.data)).unwrap(), bytes);
            }
            None => assert!(!f.marker.canonical(&f.data).exists()),
        }
        assert!(!scan_claims(&f.wal).unwrap().any());
    }

    // ---- codec ----

    #[test]
    fn golden_encoding() {
        let marker = ValidatedMarker::new(
            ENV,
            SERVICE,
            date(),
            7,
            vec![WAL_A.to_owned(), WAL_B.to_owned()],
            identity(b"abc"),
        )
        .unwrap();
        assert_eq!(
            marker.encode(),
            "{\"partition\":\"2026-09-23/07\",\
             \"wal\":[\"nginx_1700000000000_ab12.ndjson\",\"nginx_1700000000001_cd34.ndjson\"],\
             \"size\":3,\
             \"blake3\":\"6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85\"}"
        );
    }

    #[test]
    fn written_marker_reads_back_with_confined_paths() {
        let f = fixture();
        let path = f.marker.marker_path(&f.wal);
        assert_eq!(path, f.wal.join("prod/.publish-nginx.json"));
        let read = read_marker(&path).unwrap();
        assert_eq!(read, f.marker);
        assert_eq!(
            read.canonical(&f.data),
            f.data.join("prod/2026-09-23/07/nginx.parquet")
        );
        assert_eq!(
            read.tmp(&f.data),
            f.data.join("prod/2026-09-23/07/nginx.parquet.tmp")
        );
        assert_eq!(
            read.wal_paths(&f.wal),
            vec![
                f.wal.join("prod").join(WAL_A),
                f.wal.join("prod").join(WAL_B)
            ]
        );
        assert_eq!(
            read.batch_ids(),
            vec![
                "prod/nginx_1700000000000_ab12".to_owned(),
                "prod/nginx_1700000000001_cd34".to_owned()
            ]
        );
    }

    #[test]
    fn identity_streams_size_and_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out");
        let bytes = vec![7u8; 3 * 65536 + 11];
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(identity_of(&path).unwrap(), identity(&bytes));
        assert_eq!(
            identity_of(&tmp.path().join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    // ---- confinement ----

    fn read_body(env: &str, service: &str, body: &str) -> Result<ValidatedMarker, MarkerError> {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(env);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(marker_file_name(service));
        std::fs::write(&path, body).unwrap();
        read_marker(&path)
    }

    fn body(partition: &str, wal: &[&str], blake3: &str) -> String {
        serde_json::json!({ "partition": partition, "wal": wal, "size": 3, "blake3": blake3 })
            .to_string()
    }

    fn hex() -> String {
        blake3::hash(b"abc").to_hex().to_string()
    }

    fn assert_invalid(result: Result<ValidatedMarker, MarkerError>, needle: &str) {
        match result {
            Err(MarkerError::Invalid(reason)) => {
                assert!(reason.contains(needle), "{needle:?} not in {reason:?}");
            }
            other => panic!("expected Invalid({needle}), got {other:?}"),
        }
    }

    #[test]
    fn confinement_accepts_a_valid_body() {
        let marker = read_body(ENV, SERVICE, &body("2026-09-23/00", &[WAL_A], &hex())).unwrap();
        assert_eq!((marker.date(), marker.hour()), (date(), 0));
        let marker = read_body(ENV, SERVICE, &body("2026-09-23/23", &[WAL_A], &hex())).unwrap();
        assert_eq!(marker.hour(), 23);
    }

    #[test]
    fn confinement_rejects_wal_entries_outside_the_env_directory() {
        let cases: &[(&[&str], &str)] = &[
            (&[".."], "not an .ndjson file"),
            (&["."], "not an .ndjson file"),
            (&["../nginx_1_ab.ndjson"], "not a single file name"),
            (&["sub/nginx_1_ab.ndjson"], "not a single file name"),
            (&["/abs/nginx_1_ab.ndjson"], "not a single file name"),
            (&["/nginx_1_ab.ndjson"], "not a single file name"),
            (&[".ndjson"], "not a single file name"),
            (&["..ndjson"], "not a single file name"),
            (&["nginx_1_ab\u{0}.ndjson"], "not a single file name"),
            (&["postgres_1_ab.ndjson"], "does not belong to service"),
            (&["nginx-extra_1_ab.ndjson"], "does not belong to service"),
            (&["nginx_1_ab.json"], "not an .ndjson file"),
            (&["nginx_1_ab.ndjson.corrupt"], "not an .ndjson file"),
            (&[], "WAL list is empty"),
            (&[WAL_A, WAL_B, WAL_A], "listed twice"),
        ];
        for (wal, needle) in cases {
            assert_invalid(
                read_body(ENV, SERVICE, &body("2026-09-23/07", wal, &hex())),
                needle,
            );
        }
    }

    #[test]
    fn confinement_rejects_bad_partitions_and_digests() {
        for partition in [
            "2026-09-23",
            "2026-09-23/24",
            "2026-09-23/7",
            "2026-9-23/07",
            "2026-02-30/07",
            "2026-09-23/07/x",
            "../2026-09-23/07",
            "/2026-09-23/07",
            "2026-09-23/+7",
        ] {
            assert_invalid(
                read_body(ENV, SERVICE, &body(partition, &[WAL_A], &hex())),
                "invalid partition",
            );
        }
        for digest in [
            hex().to_uppercase(),
            hex()[..63].to_owned(),
            "zz".repeat(32),
        ] {
            assert_invalid(
                read_body(ENV, SERVICE, &body("2026-09-23/07", &[WAL_A], &digest)),
                "digest",
            );
        }
    }

    #[test]
    fn confinement_rejects_bad_env_service_and_shape() {
        let good = body("2026-09-23/07", &[WAL_A], &hex());
        assert_invalid(read_body("wal", SERVICE, &good), "reserved env");
        assert_invalid(read_body("scheduled", SERVICE, &good), "reserved env");
        assert_invalid(read_body("Prod", SERVICE, &good), "invalid env");
        assert_invalid(read_body(ENV, ".hidden", &good), "invalid service");
        assert_invalid(read_body(ENV, "", &good), "invalid service");
        assert_invalid(read_body(ENV, SERVICE, "{"), "unparseable");
        let extra = serde_json::json!({
            "partition": "2026-09-23/07", "wal": [WAL_A], "size": 3,
            "blake3": hex(), "canonical": "/etc/passwd"
        })
        .to_string();
        assert_invalid(read_body(ENV, SERVICE, &extra), "unknown field");
        let missing = serde_json::json!({ "partition": "2026-09-23/07", "wal": [WAL_A] });
        assert_invalid(
            read_body(ENV, SERVICE, &missing.to_string()),
            "missing field",
        );
    }

    #[cfg(unix)]
    #[test]
    fn confinement_refuses_a_symlinked_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(ENV);
        std::fs::create_dir_all(&dir).unwrap();
        let target = tmp.path().join("elsewhere.json");
        std::fs::write(&target, body("2026-09-23/07", &[WAL_A], &hex())).unwrap();
        let link = dir.join(marker_file_name(SERVICE));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_invalid(read_marker(&link), "not a regular file");
    }

    #[test]
    fn marker_files_are_invisible_to_the_wal_scan() {
        // A service named like a WAL file must not turn its marker, the
        // staged writer's temp name or a retired input into scan hits.
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let env_dir = wal.join(ENV);
        std::fs::create_dir_all(&env_dir).unwrap();
        let marker = ValidatedMarker::new(
            ENV,
            "x.ndjson",
            date(),
            7,
            vec!["x.ndjson_1_ab.ndjson".to_owned()],
            identity(b"abc"),
        )
        .unwrap();
        write_marker(&wal, &marker).unwrap();
        assert!(env_dir.join(".publish-x.ndjson.json").is_file());
        std::fs::write(
            env_dir.join(format!(
                "..publish-x.ndjson.json.next.{}",
                std::process::id()
            )),
            b"{",
        )
        .unwrap();
        std::fs::write(env_dir.join("x.ndjson_1_ab.ndjson.merged"), b"").unwrap();
        let hits =
            super::super::compaction::scan_wal_files(&env_dir, std::time::Duration::ZERO).unwrap();
        assert!(hits.is_empty(), "{hits:?}");
        assert_eq!(read_marker(&marker.marker_path(&wal)).unwrap(), marker);
    }

    // ---- durable write and remove ----

    #[test]
    fn write_marker_propagates_directory_fsync_failure() {
        let f = fixture();
        let env_dir = f.marker.wal_env_dir(&f.wal);
        let _fail = crate::epoch::fail_dir_fsync::set(&env_dir);
        let err = write_marker(&f.wal, &f.marker).unwrap_err();
        assert!(err.contains("failed to fsync directory"), "{err}");
        let err = remove_marker_durably(&f.marker.marker_path(&f.wal)).unwrap_err();
        assert!(err.contains("failed to fsync directory"), "{err}");
    }

    #[test]
    fn remove_marker_durably_accepts_a_missing_marker() {
        let f = fixture();
        let path = f.marker.marker_path(&f.wal);
        remove_marker_durably(&path).unwrap();
        assert!(!path.exists());
        remove_marker_durably(&path).unwrap();
    }

    // ---- claims ----

    #[test]
    fn claims_of_a_valid_marker() {
        let f = fixture();
        let claims = scan_claims(&f.wal).unwrap();
        assert!(claims.any());
        assert!(claims.blocks_service(ENV, SERVICE));
        assert!(!claims.blocks_service(ENV, "postgres"));
        assert!(!claims.blocks_service("lab", SERVICE));
        assert!(claims.claims_date(ENV, date()));
        assert!(!claims.claims_date(ENV, date().pred_opt().unwrap()));
        assert!(!claims.claims_date("lab", date()));
        assert!(claims.claims_output(ENV, date(), 7, SERVICE));
        assert!(!claims.claims_output(ENV, date(), 8, SERVICE));
        assert!(!claims.claims_output(ENV, date(), 7, "postgres"));
    }

    #[test]
    fn an_unparseable_marker_blocks_its_service_and_claims_every_date() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        std::fs::create_dir_all(wal.join(ENV)).unwrap();
        std::fs::create_dir_all(wal.join("lab")).unwrap();
        std::fs::write(wal.join(ENV).join(".publish-nginx.json"), b"{").unwrap();
        let claims = scan_claims(&wal).unwrap();
        assert!(claims.any());
        assert!(claims.blocks_service(ENV, SERVICE));
        assert!(!claims.blocks_service(ENV, "postgres"));
        assert!(claims.claims_date(ENV, date()));
        assert!(claims.claims_date(ENV, NaiveDate::from_ymd_opt(2001, 1, 1).unwrap()));
        assert!(!claims.claims_date("lab", date()));
        assert!(claims.claims_output(ENV, date(), 3, SERVICE));
        assert!(!claims.claims_output(ENV, date(), 3, "postgres"));
    }

    #[test]
    fn no_markers_no_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        assert!(!scan_claims(&wal).unwrap().any(), "missing root");
        std::fs::create_dir_all(wal.join(ENV)).unwrap();
        std::fs::write(wal.join(ENV).join(WAL_A), b"").unwrap();
        // Staged temp names and markers outside env directories are inert.
        std::fs::write(wal.join(ENV).join("..publish-nginx.json.next.1"), b"{").unwrap();
        std::fs::create_dir_all(wal.join("wal")).unwrap();
        std::fs::write(wal.join("wal").join(".publish-nginx.json"), b"{").unwrap();
        assert!(!scan_claims(&wal).unwrap().any());
    }

    #[test]
    fn an_unreadable_wal_root_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        std::fs::write(&wal, b"not a directory").unwrap();
        assert!(scan_claims(&wal).is_err());
        assert!(recover(&wal, tmp.path(), |_| Ok(())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_uninspectable_root_entry_claims_every_env_it_hides() {
        use std::os::unix::fs::PermissionsExt as _;
        let f = fixture();
        std::fs::create_dir_all(f.wal.join("lab")).unwrap();
        let mode = std::fs::metadata(&f.wal).unwrap().permissions();
        // Readable but not searchable: entries list, metadata fails.
        std::fs::set_permissions(&f.wal, std::fs::Permissions::from_mode(0o600)).unwrap();
        let probe = std::fs::metadata(f.wal.join("lab"));
        let claims = scan_claims(&f.wal);
        let report = recover(&f.wal, &f.data, |_| Ok(()));
        std::fs::set_permissions(&f.wal, mode).unwrap();
        if probe.is_ok() {
            eprintln!("skipped: privileges bypass directory search permissions");
            return;
        }
        let claims = claims.unwrap();
        assert!(claims.any());
        assert!(claims.blocks_service(ENV, SERVICE));
        assert!(claims.blocks_service("lab", "postgres"));
        assert!(claims.claims_date("lab", date()));
        assert!(claims.claims_output("lab", date(), 7, "postgres"));
        let report = report.unwrap();
        assert!(report.root_incomplete);
        assert!(report.entries.is_empty());
        assert!(f.marker.marker_path(&f.wal).is_file(), "nothing recovered");
    }

    #[test]
    fn a_complete_scan_does_not_claim_other_envs() {
        let f = fixture();
        let claims = scan_claims(&f.wal).unwrap();
        assert!(!claims.blocks_service("lab", SERVICE));
        assert!(!claims.claims_output("lab", date(), 7, SERVICE));
    }

    // ---- recovery decision table ----

    #[test]
    fn row_published_canonical_matches_retires_wal_and_removes_marker() {
        let f = published_fixture();
        let mut drained = Vec::new();
        let report = recover_all(&f, &mut drained);
        assert_eq!(only_result(&report), &Ok(RecoveryOutcome::Published));
        assert_eq!(drained, vec![f.marker.batch_ids()]);
        assert_published_end_state(&f);
    }

    #[test]
    fn row_published_with_a_tmp_present_leaves_the_tmp() {
        let f = published_fixture();
        std::fs::write(f.marker.tmp(&f.data), b"unrelated").unwrap();
        let report = recover_all(&f, &mut Vec::new());
        assert_eq!(only_result(&report), &Ok(RecoveryOutcome::Published));
        assert_published_end_state(&f);
        assert_eq!(std::fs::read(f.marker.tmp(&f.data)).unwrap(), b"unrelated");
    }

    #[test]
    fn row_unpublished_canonical_absent_removes_marker_and_tmp_keeps_wal() {
        let f = fixture();
        let mut drained = Vec::new();
        let report = recover_all(&f, &mut drained);
        assert_eq!(only_result(&report), &Ok(RecoveryOutcome::Unpublished));
        assert!(drained.is_empty(), "unpublished batches stay hot");
        assert!(!f.marker.tmp(&f.data).exists());
        assert_unpublished_end_state(&f, None);
    }

    #[test]
    fn row_unpublished_canonical_mismatch_keeps_the_earlier_canonical() {
        let f = fixture();
        std::fs::write(f.marker.canonical(&f.data), OLD_OUTPUT).unwrap();
        let report = recover_all(&f, &mut Vec::new());
        assert_eq!(only_result(&report), &Ok(RecoveryOutcome::Unpublished));
        assert!(!f.marker.tmp(&f.data).exists());
        assert_unpublished_end_state(&f, Some(OLD_OUTPUT));
    }

    #[test]
    fn row_same_size_different_bytes_is_not_published() {
        let f = fixture();
        let mut forged = OUTPUT.to_vec();
        forged[0] ^= 1;
        std::fs::write(f.marker.canonical(&f.data), &forged).unwrap();
        let report = recover_all(&f, &mut Vec::new());
        assert_eq!(only_result(&report), &Ok(RecoveryOutcome::Unpublished));
        assert_unpublished_end_state(&f, Some(&forged));
    }

    fn assert_contradiction_touches_nothing(f: &Fixture, expected: &Contradiction) {
        let before = snapshot(f.root.path());
        let mut drained = Vec::new();
        for _ in 0..2 {
            let report = recover_all(f, &mut drained);
            match only_result(&report) {
                Ok(RecoveryOutcome::Contradictory(reason)) => assert_eq!(reason, expected),
                other => panic!("expected {expected:?}, got {other:?}"),
            }
            let claims = scan_claims(&f.wal).unwrap();
            assert!(claims.blocks_service(ENV, SERVICE));
        }
        assert!(drained.is_empty());
        assert_eq!(snapshot(f.root.path()), before, "nothing touched");
    }

    #[test]
    fn row_contradictory_canonical_and_tmp_absent() {
        let f = fixture();
        std::fs::remove_file(f.marker.tmp(&f.data)).unwrap();
        assert_contradiction_touches_nothing(&f, &Contradiction::OutputMissing);
    }

    #[test]
    fn row_contradictory_canonical_mismatch_and_tmp_absent() {
        let f = fixture();
        std::fs::remove_file(f.marker.tmp(&f.data)).unwrap();
        std::fs::write(f.marker.canonical(&f.data), OLD_OUTPUT).unwrap();
        assert_contradiction_touches_nothing(&f, &Contradiction::OutputMismatch);
    }

    #[test]
    fn row_contradictory_invalid_marker() {
        let f = published_fixture();
        std::fs::write(
            f.marker.marker_path(&f.wal),
            body("2026-09-23/07", &["../../data/prod/x.ndjson"], &hex()),
        )
        .unwrap();
        let before = snapshot(f.root.path());
        let report = recover_all(&f, &mut Vec::new());
        match only_result(&report) {
            Ok(RecoveryOutcome::Contradictory(Contradiction::InvalidMarker(reason))) => {
                assert!(reason.contains("not a single file name"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(snapshot(f.root.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn row_contradictory_symlinked_canonical_tmp_or_wal() {
        use std::os::unix::fs::symlink;
        // Symlinked canonical pointing at bytes that DO match the identity.
        let f = fixture();
        let outside = f.root.path().join("outside.parquet");
        std::fs::write(&outside, OUTPUT).unwrap();
        symlink(&outside, f.marker.canonical(&f.data)).unwrap();
        assert_contradiction_touches_nothing(
            &f,
            &Contradiction::NotRegularFile(f.marker.canonical(&f.data)),
        );

        let f = fixture();
        let tmp = f.marker.tmp(&f.data);
        std::fs::remove_file(&tmp).unwrap();
        let outside = f.root.path().join("outside.tmp");
        std::fs::write(&outside, OUTPUT).unwrap();
        symlink(&outside, &tmp).unwrap();
        assert_contradiction_touches_nothing(&f, &Contradiction::NotRegularFile(tmp));

        let f = published_fixture();
        let wal_b = f.marker.wal_paths(&f.wal)[1].clone();
        std::fs::remove_file(&wal_b).unwrap();
        let outside = f.root.path().join("outside.ndjson");
        std::fs::write(&outside, b"keep me").unwrap();
        symlink(&outside, &wal_b).unwrap();
        assert_contradiction_touches_nothing(&f, &Contradiction::NotRegularFile(wal_b));
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");
    }

    #[test]
    fn a_directory_in_place_of_the_canonical_is_contradictory() {
        let f = fixture();
        std::fs::create_dir(f.marker.canonical(&f.data)).unwrap();
        assert_contradiction_touches_nothing(
            &f,
            &Contradiction::NotRegularFile(f.marker.canonical(&f.data)),
        );
    }

    #[test]
    fn a_failed_directory_fsync_is_an_error_not_a_decision() {
        let f = published_fixture();
        let output_dir = f.marker.output_dir(&f.data);
        {
            let _fail = crate::epoch::fail_dir_fsync::set(&output_dir);
            let report = recover_all(&f, &mut Vec::new());
            let err = only_result(&report).as_ref().unwrap_err();
            assert!(err.contains("failed to fsync directory"), "{err}");
        }
        assert!(f.marker.marker_path(&f.wal).is_file(), "marker kept");
        for path in f.marker.wal_paths(&f.wal) {
            assert!(path.is_file());
        }
        let report = recover_all(&f, &mut Vec::new());
        assert_eq!(only_result(&report), &Ok(RecoveryOutcome::Published));
        assert_published_end_state(&f);
    }

    #[test]
    fn a_failed_drain_keeps_the_marker_and_the_wal() {
        let f = published_fixture();
        let report = recover(&f.wal, &f.data, |_| Err("drain failed".to_owned())).unwrap();
        assert_eq!(only_result(&report), &Err("drain failed".to_owned()));
        assert!(f.marker.marker_path(&f.wal).is_file());
        for path in f.marker.wal_paths(&f.wal) {
            assert!(path.is_file());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_wal_that_can_be_neither_deleted_nor_renamed_keeps_the_marker() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let f = published_fixture();
        let env_dir = f.marker.wal_env_dir(&f.wal);
        if std::fs::metadata(&env_dir).unwrap().uid() == 0 {
            return; // root ignores directory permissions
        }
        std::fs::set_permissions(&env_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let report = recover_all(&f, &mut Vec::new());
        std::fs::set_permissions(&env_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = only_result(&report).as_ref().unwrap_err();
        assert!(err.contains("failed to retire"), "{err}");
        assert!(f.marker.marker_path(&f.wal).is_file(), "marker kept");
        assert!(scan_claims(&f.wal).unwrap().blocks_service(ENV, SERVICE));
        let report = recover_all(&f, &mut Vec::new());
        assert_eq!(only_result(&report), &Ok(RecoveryOutcome::Published));
        assert_published_end_state(&f);
    }

    #[test]
    fn markers_in_several_envs_each_recover() {
        let f = published_fixture();
        let other = ValidatedMarker::new(
            "lab",
            "postgres",
            date(),
            7,
            vec!["postgres_1_ab.ndjson".to_owned()],
            identity(OUTPUT),
        )
        .unwrap();
        std::fs::create_dir_all(other.wal_env_dir(&f.wal)).unwrap();
        std::fs::create_dir_all(other.output_dir(&f.data)).unwrap();
        std::fs::write(&other.wal_paths(&f.wal)[0], b"").unwrap();
        std::fs::write(other.tmp(&f.data), OUTPUT).unwrap();
        write_marker(&f.wal, &other).unwrap();
        let report = recover_all(&f, &mut Vec::new());
        let outcomes: Vec<_> = report
            .entries
            .iter()
            .map(|e| (e.env.as_str(), e.service.as_str(), e.result.clone()))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                ("lab", "postgres", Ok(RecoveryOutcome::Unpublished)),
                ("prod", "nginx", Ok(RecoveryOutcome::Published)),
            ]
        );
        assert!(!scan_claims(&f.wal).unwrap().any());
    }

    #[test]
    fn outcome_labels_are_stable() {
        let labels: Vec<_> = RecoveryOutcomeKind::ALL.iter().map(|k| k.label()).collect();
        assert_eq!(
            labels,
            ["published", "unpublished", "contradictory", "failed"]
        );
        assert_eq!(
            RecoveryOutcome::Contradictory(Contradiction::OutputMissing).kind(),
            RecoveryOutcomeKind::Contradictory
        );
    }

    // ---- AC2: interrupted recovery reruns to the same end state ----

    const PUBLISHED_STEPS: [&str; 5] = [
        "recover:published:after_output_fsync",
        "recover:published:after_retire:0",
        "recover:published:after_retire:1",
        "recover:published:after_wal_dir_fsync",
        "recover:published:after_marker_unlink",
    ];

    const UNPUBLISHED_STEPS: [&str; 2] = [
        "recover:unpublished:after_marker_remove",
        "recover:unpublished:after_tmp_remove",
    ];

    #[test]
    fn ac2_published_recovery_interrupted_at_every_step_reruns_to_the_same_end_state() {
        for point in PUBLISHED_STEPS {
            let f = published_fixture();
            let mut drained = Vec::new();
            {
                let _stop = interrupt::at(point);
                let report = recover_all(&f, &mut drained);
                let err = only_result(&report).as_ref().unwrap_err();
                assert!(err.contains(point), "{point}: {err}");
            }
            // Until the unlink, the marker still claims the service.
            let unlinked = point == "recover:published:after_marker_unlink";
            assert_eq!(
                scan_claims(&f.wal).unwrap().blocks_service(ENV, SERVICE),
                !unlinked,
                "{point}"
            );
            let report = recover_all(&f, &mut drained);
            if unlinked {
                assert!(report.entries.is_empty(), "{point}: {report:?}");
            } else {
                assert_eq!(
                    only_result(&report),
                    &Ok(RecoveryOutcome::Published),
                    "{point}"
                );
            }
            assert_published_end_state(&f);
            // The drain may repeat; it never sees a different batch set.
            assert!(!drained.is_empty(), "{point}");
            assert!(drained.iter().all(|ids| *ids == f.marker.batch_ids()));
        }
    }

    #[test]
    fn ac2_unpublished_recovery_interrupted_at_every_step_reruns_to_the_same_end_state() {
        for canonical in [None, Some(OLD_OUTPUT)] {
            for point in UNPUBLISHED_STEPS {
                let f = fixture();
                if let Some(bytes) = canonical {
                    std::fs::write(f.marker.canonical(&f.data), bytes).unwrap();
                }
                {
                    let _stop = interrupt::at(point);
                    let report = recover_all(&f, &mut Vec::new());
                    let err = only_result(&report).as_ref().unwrap_err();
                    assert!(err.contains(point), "{point}: {err}");
                }
                let mut drained = Vec::new();
                let report = recover_all(&f, &mut drained);
                // The marker went first in both cases, so the rerun finds none.
                assert!(report.entries.is_empty(), "{point}: {report:?}");
                assert!(drained.is_empty());
                assert_unpublished_end_state(&f, canonical);
                // Interrupted before the tmp removal, the tmp is an orphan with
                // no marker: stale-tmp cleanup or the next COPY replaces it.
                assert_eq!(
                    f.marker.tmp(&f.data).exists(),
                    point == "recover:unpublished:after_marker_remove",
                    "{point}"
                );
            }
        }
    }

    #[test]
    fn crash_point_is_a_no_op_unless_selected() {
        crash_point("publish:after_marker").unwrap();
        let _stop = interrupt::at("publish:after_marker");
        assert!(crash_point("publish:after_rename").is_ok());
        assert!(crash_point("publish:after_marker").is_err());
    }
}
