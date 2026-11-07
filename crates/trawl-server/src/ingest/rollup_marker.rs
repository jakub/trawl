// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rollup markers: the record that lets a daily rollup be finished or
//! abandoned after a crash from file identities alone (ADR-0041, #357).
//!
//! A rollup merges the hourly files of one service-day, and the prior daily
//! file when there is one, into `{service}.parquet.tmp`, then renames it
//! onto `{service}.parquet` and retires the hourly files it merged. Before
//! the rename it writes `data/{env}/{date}/.rollup-{service}` durably. The
//! marker records the size and BLAKE3 digest of the replacement, of the
//! prior daily file it merged (if any) and of each hourly input, which it
//! names relative to the day directory as `HH/{service}.parquet`.
//!
//! Recovery decides from what is at those paths now, with [`classify`]:
//!
//! - **a, [`Decision::Published`]**: the canonical file is the replacement.
//!   Whatever the tmp is, the rename happened.
//! - **b, [`Decision::Promote`]**: the tmp is the replacement, and the
//!   canonical file is absent with no prior recorded, or is the prior. The
//!   rename did not happen.
//! - **c, [`Decision::Abort`]**: every input is at its path with its
//!   recorded identity, and the canonical is absent with no prior recorded,
//!   or is the prior. Nothing was retired, so the merge can be abandoned.
//! - **d, [`Decision::Unresolved`]**: none of these is proven. Recovery
//!   keeps every file and the marker, and names the [`Proof`] that failed.
//!
//! In a and b, every input that is still at its path must carry its
//! recorded identity; an absent one was already retired. A present input
//! that differs, or one that cannot be read, makes the marker unresolved,
//! because retiring it would delete rows the replacement may not hold.
//!
//! [`classify`] reads only through the observer its caller passes, and
//! never changes a file, so recovery and `trawld --doctor` share it.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::no_follow;
use super::publication_marker::{OutputIdentity, parse_hex_digest};

/// Marker file name prefix inside a day directory.
pub const MARKER_PREFIX: &str = ".rollup-";

/// The value of the record's `format` field.
const FORMAT: &str = "trawl-rollup-marker";

/// The record version this release writes and reads.
const VERSION: u64 = 1;

/// Most inputs one marker lists. A rollup merges at most one hourly file
/// per hour directory, and the rollup scan takes only two-digit hour
/// directory names, `00` to `99`.
pub const MAX_INPUTS: usize = 100;

/// Largest marker the reader reads: 26 KiB.
///
/// [`RollupMarker::encode`] writes compact JSON, and validation bounds each
/// part of it:
///
/// - The `format` and `version` fields, the replacement and prior
///   identities, and the keys and punctuation around the input list take
///   about 300 bytes. Each identity is `{"size":U,"blake3":"H"}`, with a
///   `u64` of at most 20 digits and 64 hex digits: 105 bytes. 1 KiB covers
///   this part.
/// - Each input is `{"name":"HH/S.parquet","size":U,"blake3":"H"},`. Its
///   keys and punctuation take 31 bytes, its comma 1, the `HH/` and
///   `.parquet` around the service name 11, the size 20 and the digest 64:
///   127 bytes plus the service name. 128 bytes plus the longest service
///   name ([`trawl_config::MAX_SERVICE_NAME_LEN`]) covers one input.
///
/// So every marker a rollup writes fits, with [`MAX_INPUTS`] inputs.
pub const MAX_ROLLUP_MARKER_BYTES: u64 =
    (1024 + MAX_INPUTS * (trawl_config::MAX_SERVICE_NAME_LEN + 128)) as u64;

/// A size and digest, as the record spells them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityRecord {
    size: u64,
    blake3: String,
}

impl From<OutputIdentity> for IdentityRecord {
    fn from(identity: OutputIdentity) -> Self {
        Self {
            size: identity.size,
            blake3: identity.hash.to_hex().to_string(),
        }
    }
}

/// One merged hourly input, as the record spells it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRecord {
    /// `HH/{service}.parquet`, relative to the day directory.
    name: String,
    size: u64,
    blake3: String,
}

/// The on-disk marker body. Every field is required, `prior` included,
/// whose `null` records that no prior daily file was merged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: String,
    version: u64,
    replacement: IdentityRecord,
    #[serde(deserialize_with = "required_nullable")]
    prior: Option<IdentityRecord>,
    inputs: Vec<InputRecord>,
}

/// Deserialize an `Option` that must be present, as `null` or a value.
/// Serde treats a field with its own deserializer as required.
fn required_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<IdentityRecord>, D::Error> {
    Option::deserialize(deserializer)
}

/// One hourly input of a validated marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    hour: [u8; 2],
    identity: OutputIdentity,
}

impl Input {
    /// The hour directory's name: two ASCII digits.
    pub fn hour(&self) -> &str {
        std::str::from_utf8(&self.hour).expect("validated as ASCII digits")
    }

    /// The identity the input had when the rollup merged it.
    pub fn identity(&self) -> OutputIdentity {
        self.identity
    }
}

/// A rollup marker whose every part passed validation. Paths are built
/// only from its parts and the day directory the caller found it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupMarker {
    service: String,
    replacement: OutputIdentity,
    prior: Option<OutputIdentity>,
    inputs: Vec<Input>,
}

impl RollupMarker {
    /// Validate the parts of a marker about to be written. Each input is
    /// named `HH/{service}.parquet`, relative to the day directory.
    pub fn new(
        service: &str,
        replacement: OutputIdentity,
        prior: Option<OutputIdentity>,
        inputs: Vec<(String, OutputIdentity)>,
    ) -> Result<Self, String> {
        validate_service(service)?;
        if inputs.is_empty() {
            return Err("input list is empty".to_owned());
        }
        if inputs.len() > MAX_INPUTS {
            return Err(format!(
                "input list has {} entries, over the {MAX_INPUTS}-entry limit",
                inputs.len()
            ));
        }
        let mut seen = BTreeSet::new();
        let mut validated = Vec::with_capacity(inputs.len());
        for (name, identity) in inputs {
            let hour = parse_input_name(service, &name)
                .ok_or_else(|| format!("input {name:?} is not HH/{service}.parquet"))?;
            if !seen.insert(hour) {
                return Err(format!("input {name:?} is listed twice"));
            }
            validated.push(Input { hour, identity });
        }
        Ok(Self {
            service: service.to_owned(),
            replacement,
            prior,
            inputs: validated,
        })
    }

    fn from_record(service: &str, record: Record) -> Result<Self, String> {
        let identity = |what: &str, size: u64, blake3: &str| {
            parse_hex_digest(blake3)
                .map(|hash| OutputIdentity { size, hash })
                .ok_or_else(|| format!("{what} digest is not 64 lowercase hex digits"))
        };
        let replacement = identity(
            "replacement",
            record.replacement.size,
            &record.replacement.blake3,
        )?;
        let prior = record
            .prior
            .map(|prior| identity("prior", prior.size, &prior.blake3))
            .transpose()?;
        let inputs = record
            .inputs
            .into_iter()
            .map(|input| Ok((input.name, identity("input", input.size, &input.blake3)?)))
            .collect::<Result<_, String>>()?;
        Self::new(service, replacement, prior, inputs)
    }

    /// The service the marker's file name names.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// The identity of the file the rollup wrote.
    pub fn replacement(&self) -> OutputIdentity {
        self.replacement
    }

    /// The identity of the prior daily file the rollup merged, if any.
    pub fn prior(&self) -> Option<OutputIdentity> {
        self.prior
    }

    /// The hourly inputs, in the order the rollup listed them.
    pub fn inputs(&self) -> &[Input] {
        &self.inputs
    }

    /// `day_dir/{service}.parquet`.
    pub fn canonical(&self, day_dir: &Path) -> PathBuf {
        day_dir.join(format!("{}.parquet", self.service))
    }

    /// `day_dir/{service}.parquet.tmp`.
    pub fn tmp(&self, day_dir: &Path) -> PathBuf {
        day_dir.join(format!("{}.parquet.tmp", self.service))
    }

    /// `day_dir/HH/{service}.parquet` for `input`.
    pub fn input_path(&self, day_dir: &Path, input: &Input) -> PathBuf {
        day_dir
            .join(input.hour())
            .join(format!("{}.parquet", self.service))
    }

    /// `day_dir/.rollup-{service}`.
    pub fn marker_path(&self, day_dir: &Path) -> PathBuf {
        day_dir.join(marker_file_name(&self.service))
    }

    /// The marker body: compact JSON that [`read`] reads back.
    pub fn encode(&self) -> String {
        let record = Record {
            format: FORMAT.to_owned(),
            version: VERSION,
            replacement: self.replacement.into(),
            prior: self.prior.map(Into::into),
            inputs: self
                .inputs
                .iter()
                .map(|input| InputRecord {
                    name: format!("{}/{}.parquet", input.hour(), self.service),
                    size: input.identity.size,
                    blake3: input.identity.hash.to_hex().to_string(),
                })
                .collect(),
        };
        serde_json::to_string(&record).expect("a marker record always serializes")
    }
}

/// `.rollup-{service}`.
pub fn marker_file_name(service: &str) -> String {
    format!("{MARKER_PREFIX}{service}")
}

fn validate_service(service: &str) -> Result<(), String> {
    if trawl_config::is_valid_service_name(service) {
        Ok(())
    } else {
        Err(format!("invalid service name {service:?}"))
    }
}

/// The hour of `HH/{service}.parquet`, in exactly that spelling. Anything
/// else, an absolute path or one with `..` included, is `None`.
fn parse_input_name(service: &str, name: &str) -> Option<[u8; 2]> {
    let (hour, file) = name.split_once('/')?;
    let hour: [u8; 2] = hour.as_bytes().try_into().ok()?;
    (hour.iter().all(u8::is_ascii_digit) && file.strip_suffix(".parquet")? == service)
        .then_some(hour)
}

/// Why [`read`] returned no marker.
#[derive(Debug)]
pub enum ReadError {
    /// Nothing is at the path.
    Missing,
    /// It could not be opened or read, or is not a regular file.
    Unreadable(io::Error),
    /// It is over [`MAX_ROLLUP_MARKER_BYTES`].
    Oversized,
    /// It is not a JSON object carrying this format's discriminator: a
    /// marker from a release before #357, which lists bare paths.
    OldFormat,
    /// It carries the discriminator with a version this release does not
    /// read.
    UnsupportedVersion,
    /// It carries the discriminator and version but fails parsing or
    /// validation.
    Malformed(String),
}

impl ReadError {
    /// The proof that failed, for a marker that is there. `None` when the
    /// marker is gone.
    pub fn proof(&self) -> Option<Proof> {
        match self {
            Self::Missing => None,
            Self::Unreadable(_) => Some(Proof::MarkerUnreadable),
            Self::Oversized => Some(Proof::MarkerOversized),
            Self::OldFormat => Some(Proof::MarkerOldFormat),
            Self::UnsupportedVersion => Some(Proof::MarkerUnsupportedVersion),
            Self::Malformed(_) => Some(Proof::MarkerMalformed),
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("the rollup marker is gone"),
            Self::Unreadable(e) => write!(f, "failed to read rollup marker: {e}"),
            Self::Oversized => write!(
                f,
                "rollup marker is over the {MAX_ROLLUP_MARKER_BYTES}-byte limit"
            ),
            Self::OldFormat => f.write_str("rollup marker is in a format from an older release"),
            Self::UnsupportedVersion => f.write_str("rollup marker version is not supported"),
            Self::Malformed(reason) => write!(f, "invalid rollup marker: {reason}"),
        }
    }
}

/// Read and validate the rollup marker at `path`, opened with
/// [`no_follow::open`]. The service comes from the file name.
pub fn read(path: &Path) -> Result<RollupMarker, ReadError> {
    read_with(path, no_follow::open)
}

/// [`read`], opening the marker with `open` in place of
/// [`no_follow::open`]. `trawld --doctor` passes its own opener.
///
/// Whatever `open` returns is judged by `fstat` on that descriptor:
/// anything but a regular file is [`ReadError::Unreadable`], so nothing
/// swapped in at the path after a check is read, and the opener must not
/// wait on a FIFO. A marker `fstat` reports over
/// [`MAX_ROLLUP_MARKER_BYTES`] is never read, and one that grows after the
/// `fstat` is read only up to one byte past the bound.
pub fn read_with(
    path: &Path,
    open: impl FnOnce(&Path) -> io::Result<std::fs::File>,
) -> Result<RollupMarker, ReadError> {
    let service = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(MARKER_PREFIX))
        .ok_or_else(|| ReadError::Malformed("the file name is not a rollup marker's".to_owned()))?;
    validate_service(service).map_err(ReadError::Malformed)?;
    let file = open(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => ReadError::Missing,
        _ => ReadError::Unreadable(e),
    })?;
    let metadata = file.metadata().map_err(ReadError::Unreadable)?;
    if !metadata.file_type().is_file() {
        return Err(ReadError::Unreadable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        )));
    }
    if metadata.len() > MAX_ROLLUP_MARKER_BYTES {
        return Err(ReadError::Oversized);
    }
    let mut body = Vec::new();
    file.take(MAX_ROLLUP_MARKER_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(ReadError::Unreadable)?;
    if body.len() as u64 > MAX_ROLLUP_MARKER_BYTES {
        return Err(ReadError::Oversized);
    }
    decode(service, &body)
}

/// Parse and validate a marker body for `service`.
pub fn decode(service: &str, body: &[u8]) -> Result<RollupMarker, ReadError> {
    // An older release wrote one absolute path per line, never JSON. So a
    // body that is not a JSON object carrying the discriminator is old.
    let value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) if !body.trim_ascii_start().starts_with(b"{") => return Err(ReadError::OldFormat),
        Err(e) => return Err(ReadError::Malformed(format!("unparseable marker: {e}"))),
    };
    if value.get("format").and_then(serde_json::Value::as_str) != Some(FORMAT) {
        return Err(ReadError::OldFormat);
    }
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(VERSION) {
        return Err(ReadError::UnsupportedVersion);
    }
    let record: Record = serde_json::from_value(value)
        .map_err(|e| ReadError::Malformed(format!("unparseable marker: {e}")))?;
    RollupMarker::from_record(service, record).map_err(ReadError::Malformed)
}

/// What is at one path the marker names, as recovery's observer saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// Nothing is there.
    Absent,
    /// A regular file with this identity.
    Identity(OutputIdentity),
    /// Something is there that could not be opened as a regular file and
    /// hashed.
    Unreadable,
}

impl Observation {
    fn is(self, identity: OutputIdentity) -> bool {
        self == Self::Identity(identity)
    }
}

/// The proof whose failure leaves a marker unresolved (case d). Each has a
/// fixed label that logs and the doctor report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proof {
    /// The marker could not be opened or read, or is not a regular file.
    MarkerUnreadable,
    /// The marker is over its size bound.
    MarkerOversized,
    /// The marker is from a release that recorded no identities.
    MarkerOldFormat,
    /// The marker's version is not one this release reads.
    MarkerUnsupportedVersion,
    /// The marker fails parsing or validation.
    MarkerMalformed,
    /// The canonical daily file could not be read.
    CanonicalUnreadable,
    /// The canonical daily file is neither the replacement nor the prior
    /// the marker records, nor absent with no prior recorded.
    PriorMismatch,
    /// A listed input is not at its path.
    InputMissing,
    /// A listed input is at its path with another identity.
    InputMismatch,
    /// A listed input could not be read.
    InputUnreadable,
}

impl Proof {
    /// The fixed `snake_case` label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::MarkerUnreadable => "marker_unreadable",
            Self::MarkerOversized => "marker_oversized",
            Self::MarkerOldFormat => "marker_old_format",
            Self::MarkerUnsupportedVersion => "marker_unsupported_version",
            Self::MarkerMalformed => "marker_malformed",
            Self::CanonicalUnreadable => "canonical_unreadable",
            Self::PriorMismatch => "prior_mismatch",
            Self::InputMissing => "input_missing",
            Self::InputMismatch => "input_mismatch",
            Self::InputUnreadable => "input_unreadable",
        }
    }
}

impl fmt::Display for Proof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What recovery does with one marker. See the
/// [module documentation](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Case a: the canonical file is the replacement.
    Published,
    /// Case b: the tmp is the replacement and the rename did not happen.
    Promote,
    /// Case c: nothing was retired, and the merge can be abandoned.
    Abort,
    /// Case d: keep everything, because this proof failed.
    Unresolved(Proof),
}

/// Decide what recovery does with `marker`, found in `day_dir`, from what
/// `observe` reports at the paths it names. The canonical file is observed
/// before the tmp, and the tmp only when the canonical is not the
/// replacement. Never changes a file.
pub fn classify(
    marker: &RollupMarker,
    day_dir: &Path,
    mut observe: impl FnMut(&Path) -> Observation,
) -> Decision {
    let canonical = observe(&marker.canonical(day_dir));
    if canonical.is(marker.replacement) {
        return present_inputs_match(marker, day_dir, &mut observe)
            .map_or(Decision::Published, Decision::Unresolved);
    }
    // The rollup leaves the canonical file absent with no prior, or as the
    // prior, until its rename.
    let before_rename = match (canonical, marker.prior) {
        (Observation::Absent, None) => true,
        (canonical, Some(prior)) => canonical.is(prior),
        (Observation::Identity(_) | Observation::Unreadable, None) => false,
    };
    if before_rename && observe(&marker.tmp(day_dir)).is(marker.replacement) {
        return present_inputs_match(marker, day_dir, &mut observe)
            .map_or(Decision::Promote, Decision::Unresolved);
    }
    if !before_rename {
        return Decision::Unresolved(if canonical == Observation::Unreadable {
            Proof::CanonicalUnreadable
        } else {
            Proof::PriorMismatch
        });
    }
    for input in &marker.inputs {
        match observe(&marker.input_path(day_dir, input)) {
            Observation::Identity(identity) if identity == input.identity => {}
            Observation::Identity(_) => return Decision::Unresolved(Proof::InputMismatch),
            Observation::Absent => return Decision::Unresolved(Proof::InputMissing),
            Observation::Unreadable => return Decision::Unresolved(Proof::InputUnreadable),
        }
    }
    Decision::Abort
}

/// The proof that fails when an input still at its path does not carry its
/// recorded identity. Every input is observed.
fn present_inputs_match(
    marker: &RollupMarker,
    day_dir: &Path,
    observe: &mut impl FnMut(&Path) -> Observation,
) -> Option<Proof> {
    let mut failed = None;
    for input in &marker.inputs {
        let proof = match observe(&marker.input_path(day_dir, input)) {
            Observation::Absent => None,
            Observation::Identity(identity) if identity == input.identity => None,
            Observation::Identity(_) => Some(Proof::InputMismatch),
            Observation::Unreadable => Some(Proof::InputUnreadable),
        };
        failed = failed.or(proof);
    }
    failed
}

/// A marker body built part by part with no validation, so a test can
/// write one that recovery must refuse.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct RawMarker {
    pub format: String,
    pub version: u64,
    pub replacement: OutputIdentity,
    pub prior: Option<OutputIdentity>,
    /// Names as written, then identities.
    pub inputs: Vec<(String, OutputIdentity)>,
}

#[cfg(any(test, feature = "test-support"))]
impl RawMarker {
    /// This release's format and version, with these parts.
    pub fn new(
        replacement: OutputIdentity,
        prior: Option<OutputIdentity>,
        inputs: Vec<(String, OutputIdentity)>,
    ) -> Self {
        Self {
            format: FORMAT.to_owned(),
            version: VERSION,
            replacement,
            prior,
            inputs,
        }
    }

    /// The body, in the encoding [`RollupMarker::encode`] writes.
    pub fn encode(&self) -> String {
        let record = Record {
            format: self.format.clone(),
            version: self.version,
            replacement: self.replacement.into(),
            prior: self.prior.map(Into::into),
            inputs: self
                .inputs
                .iter()
                .map(|(name, identity)| InputRecord {
                    name: name.clone(),
                    size: identity.size,
                    blake3: identity.hash.to_hex().to_string(),
                })
                .collect(),
        };
        serde_json::to_string(&record).expect("a marker record always serializes")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const SERVICE: &str = "api";

    fn id(n: u8) -> OutputIdentity {
        OutputIdentity {
            size: u64::from(n),
            hash: blake3::hash(&[n]),
        }
    }

    fn input(hour: &str, n: u8) -> (String, OutputIdentity) {
        (format!("{hour}/{SERVICE}.parquet"), id(n))
    }

    fn day() -> PathBuf {
        PathBuf::from("/data/prod/2026-11-06")
    }

    fn decode_raw(raw: &RawMarker) -> Result<RollupMarker, ReadError> {
        decode(SERVICE, raw.encode().as_bytes())
    }

    fn assert_malformed(result: Result<RollupMarker, ReadError>, needle: &str) {
        match result {
            Err(ReadError::Malformed(reason)) => {
                assert!(reason.contains(needle), "{reason:?} lacks {needle:?}");
            }
            other => panic!("expected a malformed marker, got {other:?}"),
        }
    }

    // ---- encoding and validation ----

    #[test]
    fn a_marker_round_trips_through_its_encoding() {
        let marker = RollupMarker::new(
            SERVICE,
            id(1),
            Some(id(2)),
            vec![input("03", 3), input("17", 4)],
        )
        .unwrap();
        let decoded = decode(SERVICE, marker.encode().as_bytes()).unwrap();
        assert_eq!(decoded, marker);
        assert_eq!(decoded.inputs()[1].hour(), "17");
        assert_eq!(
            decoded.input_path(&day(), &decoded.inputs()[1]),
            day().join("17/api.parquet")
        );
        assert_eq!(decoded.canonical(&day()), day().join("api.parquet"));
        assert_eq!(decoded.tmp(&day()), day().join("api.parquet.tmp"));
        assert_eq!(decoded.marker_path(&day()), day().join(".rollup-api"));
        let none = RollupMarker::new(SERVICE, id(1), None, vec![input("00", 3)]).unwrap();
        assert!(none.encode().contains(r#""prior":null"#));
        assert_eq!(decode(SERVICE, none.encode().as_bytes()).unwrap(), none);
    }

    #[test]
    fn the_raw_encoder_writes_what_the_marker_encoder_writes() {
        let inputs = vec![input("03", 3)];
        let marker = RollupMarker::new(SERVICE, id(1), None, inputs.clone()).unwrap();
        assert_eq!(
            RawMarker::new(id(1), None, inputs).encode(),
            marker.encode()
        );
    }

    #[test]
    fn validation_refuses_names_outside_the_day_directory() {
        for name in [
            "/data/prod/2026-11-06/03/api.parquet",
            "../03/api.parquet",
            "03/../api.parquet",
            "3/api.parquet",
            "003/api.parquet",
            "ab/api.parquet",
            "03/api.parquet.tmp",
            "03/sub/api.parquet",
            "03/api",
            "api.parquet",
            "",
        ] {
            let raw = RawMarker::new(id(1), None, vec![(name.to_owned(), id(2))]);
            assert_malformed(decode_raw(&raw), "is not HH/api.parquet");
        }
    }

    #[test]
    fn validation_refuses_another_services_input() {
        let raw = RawMarker::new(id(1), None, vec![("03/web.parquet".to_owned(), id(2))]);
        assert_malformed(decode_raw(&raw), "is not HH/api.parquet");
        // The same body read as the other service's marker is valid.
        assert!(decode("web", raw.encode().as_bytes()).is_ok());
    }

    #[test]
    fn validation_refuses_a_duplicate_input() {
        let raw = RawMarker::new(id(1), None, vec![input("03", 2), input("03", 3)]);
        assert_malformed(decode_raw(&raw), "listed twice");
    }

    #[test]
    fn validation_bounds_the_input_count() {
        let raw = RawMarker::new(id(1), None, Vec::new());
        assert_malformed(decode_raw(&raw), "empty");
        let hundred: Vec<_> = (0..100).map(|h| input(&format!("{h:02}"), 2)).collect();
        assert!(decode_raw(&RawMarker::new(id(1), None, hundred.clone())).is_ok());
        let mut hundred_and_one = hundred;
        hundred_and_one.push(input("00", 3));
        assert_malformed(
            decode_raw(&RawMarker::new(id(1), None, hundred_and_one)),
            "over the 100-entry limit",
        );
    }

    #[test]
    fn validation_refuses_a_bad_digest() {
        let body = RawMarker::new(id(1), None, vec![input("03", 2)]).encode();
        let hex = id(1).hash.to_hex().to_string();
        for bad in [hex.to_uppercase(), hex[..63].to_owned(), format!("{hex}0")] {
            assert_malformed(
                decode(SERVICE, body.replace(&hex, &bad).as_bytes()),
                "replacement digest",
            );
        }
    }

    #[test]
    fn validation_refuses_unknown_and_missing_fields() {
        let body = RawMarker::new(id(1), None, vec![input("03", 2)]).encode();
        let extra = body.replacen('{', r#"{"extra":1,"#, 1);
        assert_malformed(decode(SERVICE, extra.as_bytes()), "unknown field");
        let nested = body.replacen(r#""name""#, r#""mode":1,"name""#, 1);
        assert_malformed(decode(SERVICE, nested.as_bytes()), "unknown field");
        let no_prior = body.replacen(r#""prior":null,"#, "", 1);
        assert_malformed(
            decode(SERVICE, no_prior.as_bytes()),
            "missing field `prior`",
        );
    }

    #[test]
    fn an_old_or_foreign_body_is_the_old_format() {
        for body in [
            "/data/prod/2026-11-06/03/api.parquet\n/data/prod/2026-11-06/04/api.parquet",
            "",
            "[]",
            "17",
            r#"{"inputs":[]}"#,
            r#"{"format":"something-else","version":1}"#,
        ] {
            assert!(
                matches!(decode(SERVICE, body.as_bytes()), Err(ReadError::OldFormat)),
                "{body:?}"
            );
        }
    }

    #[test]
    fn another_version_is_unsupported_and_broken_json_is_malformed() {
        let mut raw = RawMarker::new(id(1), None, vec![input("03", 2)]);
        raw.version = 2;
        assert!(matches!(
            decode_raw(&raw),
            Err(ReadError::UnsupportedVersion)
        ));
        let body = raw.encode();
        assert!(matches!(
            decode(SERVICE, &body.as_bytes()[..body.len() / 2]),
            Err(ReadError::Malformed(_))
        ));
    }

    #[test]
    fn every_read_error_but_missing_names_a_proof() {
        assert_eq!(ReadError::Missing.proof(), None);
        assert_eq!(ReadError::OldFormat.proof(), Some(Proof::MarkerOldFormat));
        assert_eq!(ReadError::Oversized.proof(), Some(Proof::MarkerOversized));
    }

    #[test]
    fn the_largest_marker_fits_the_size_bound() {
        let service = "s".repeat(trawl_config::MAX_SERVICE_NAME_LEN);
        let max = OutputIdentity {
            size: u64::MAX,
            hash: blake3::hash(b"x"),
        };
        let inputs = (0..MAX_INPUTS)
            .map(|h| (format!("{h:02}/{service}.parquet"), max))
            .collect();
        let marker = RollupMarker::new(&service, max, Some(max), inputs).unwrap();
        let len = marker.encode().len() as u64;
        assert!(len <= MAX_ROLLUP_MARKER_BYTES, "{len}");
        // The bound is tight enough to mean something: within 2 KiB.
        assert!(MAX_ROLLUP_MARKER_BYTES - len < 2048, "{len}");
    }

    // ---- the bounded reader ----

    fn write_marker(dir: &Path, body: &[u8]) -> PathBuf {
        let path = dir.join(marker_file_name(SERVICE));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn the_reader_reads_a_written_marker_and_reports_a_missing_one() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = RollupMarker::new(SERVICE, id(1), None, vec![input("03", 2)]).unwrap();
        let path = write_marker(tmp.path(), marker.encode().as_bytes());
        assert_eq!(read(&path).unwrap(), marker);
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(read(&path), Err(ReadError::Missing)));
    }

    #[test]
    fn the_reader_refuses_a_file_name_that_is_not_a_marker() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["api.parquet", ".rollup-", ".rollup-.hidden", ".rollup-a/b"] {
            let path = tmp.path().join(name);
            assert!(
                matches!(read(&path), Err(ReadError::Malformed(_))),
                "{name}"
            );
        }
    }

    #[test]
    fn the_reader_refuses_an_oversized_marker_without_reading_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(marker_file_name(SERVICE));
        std::fs::File::create(&path)
            .unwrap()
            .set_len(1 << 40)
            .unwrap();
        assert!(matches!(read(&path), Err(ReadError::Oversized)));
        // At the bound it is read; one byte more is refused.
        let body = RawMarker::new(id(1), None, vec![input("03", 2)]).encode();
        let at_bound = format!(
            "{body:<width$}",
            width = usize::try_from(MAX_ROLLUP_MARKER_BYTES).unwrap()
        );
        let path = write_marker(tmp.path(), at_bound.as_bytes());
        assert!(read(&path).is_ok());
        let path = write_marker(tmp.path(), format!("{at_bound} ").as_bytes());
        assert!(matches!(read(&path), Err(ReadError::Oversized)));
    }

    #[cfg(unix)]
    #[test]
    fn the_reader_refuses_a_symlinked_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("elsewhere");
        std::fs::write(
            &target,
            RawMarker::new(id(1), None, vec![input("03", 2)]).encode(),
        )
        .unwrap();
        let link = tmp.path().join(marker_file_name(SERVICE));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = read(&link).unwrap_err();
        assert!(matches!(err, ReadError::Unreadable(_)), "{err:?}");
        assert_eq!(err.proof(), Some(Proof::MarkerUnreadable));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_reader_refuses_a_fifo_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(marker_file_name(SERVICE));
        crate::ingest::no_follow::test_support::make_fifo(&path);
        let result = crate::ingest::no_follow::test_support::returns_promptly(move || read(&path));
        assert!(matches!(result, Err(ReadError::Unreadable(_))));
    }

    // ---- classify ----

    /// The states a test puts each path in. An input's state is relative
    /// to its recorded identity.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum File {
        Absent,
        Replacement,
        Prior,
        Other,
        Unreadable,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum In {
        Matches,
        Absent,
        Differs,
        Unreadable,
    }

    const REPLACEMENT: u8 = 1;
    const PRIOR: u8 = 2;
    const OTHER: u8 = 9;

    fn marker(prior: bool) -> RollupMarker {
        RollupMarker::new(
            SERVICE,
            id(REPLACEMENT),
            prior.then(|| id(PRIOR)),
            vec![input("03", 3), input("04", 4)],
        )
        .unwrap()
    }

    fn file_observation(file: File) -> Observation {
        match file {
            File::Absent => Observation::Absent,
            File::Replacement => Observation::Identity(id(REPLACEMENT)),
            File::Prior => Observation::Identity(id(PRIOR)),
            File::Other => Observation::Identity(id(OTHER)),
            File::Unreadable => Observation::Unreadable,
        }
    }

    fn input_observation(state: In, recorded: OutputIdentity) -> Observation {
        match state {
            In::Matches => Observation::Identity(recorded),
            In::Absent => Observation::Absent,
            In::Differs => Observation::Identity(id(OTHER)),
            In::Unreadable => Observation::Unreadable,
        }
    }

    /// Classify with the canonical, the tmp and the two inputs in these
    /// states, and return the decision and the paths observed in order.
    fn run(prior: bool, canonical: File, tmp: File, inputs: [In; 2]) -> (Decision, Vec<PathBuf>) {
        let marker = marker(prior);
        let day = day();
        let mut world = HashMap::from([
            (marker.canonical(&day), file_observation(canonical)),
            (marker.tmp(&day), file_observation(tmp)),
        ]);
        for (input, state) in marker.inputs().iter().zip(inputs) {
            world.insert(
                marker.input_path(&day, input),
                input_observation(state, input.identity()),
            );
        }
        let mut seen = Vec::new();
        let decision = classify(&marker, &day, |path| {
            seen.push(path.to_path_buf());
            world[path]
        });
        (decision, seen)
    }

    /// The rules of the module documentation, restated case by case.
    fn expected(prior: bool, canonical: File, tmp: File, inputs: [In; 2]) -> Decision {
        // In a and b, the first present input that differs or cannot be
        // read is reported; an absent one was retired.
        let present_inputs_ok = || {
            inputs.iter().find_map(|state| match state {
                In::Differs => Some(Proof::InputMismatch),
                In::Unreadable => Some(Proof::InputUnreadable),
                In::Matches | In::Absent => None,
            })
        };
        let before_rename = if prior {
            canonical == File::Prior
        } else {
            canonical == File::Absent
        };
        if canonical == File::Replacement {
            return present_inputs_ok().map_or(Decision::Published, Decision::Unresolved);
        }
        if tmp == File::Replacement && before_rename {
            return present_inputs_ok().map_or(Decision::Promote, Decision::Unresolved);
        }
        if !before_rename {
            return Decision::Unresolved(if canonical == File::Unreadable {
                Proof::CanonicalUnreadable
            } else {
                Proof::PriorMismatch
            });
        }
        // Only the first input that fails is reported.
        match inputs.iter().find(|state| **state != In::Matches) {
            None => Decision::Abort,
            Some(In::Absent) => Decision::Unresolved(Proof::InputMissing),
            Some(In::Differs) => Decision::Unresolved(Proof::InputMismatch),
            Some(_) => Decision::Unresolved(Proof::InputUnreadable),
        }
    }

    #[test]
    fn classify_matches_the_truth_table_for_every_state() {
        let files = [
            File::Absent,
            File::Replacement,
            File::Prior,
            File::Other,
            File::Unreadable,
        ];
        let ins = [In::Matches, In::Absent, In::Differs, In::Unreadable];
        let mut cases = 0;
        for prior in [false, true] {
            for canonical in files {
                for tmp in files {
                    for first in ins {
                        for second in ins {
                            let inputs = [first, second];
                            let (decision, seen) = run(prior, canonical, tmp, inputs);
                            assert_eq!(
                                decision,
                                expected(prior, canonical, tmp, inputs),
                                "prior {prior}, canonical {canonical:?}, tmp {tmp:?}, \
                                 inputs {inputs:?}"
                            );
                            // The canonical is always looked at first, and
                            // the tmp never once the canonical is the
                            // replacement.
                            assert_eq!(seen[0], day().join("api.parquet"));
                            if canonical == File::Replacement {
                                assert!(!seen.contains(&day().join("api.parquet.tmp")));
                            }
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 2 * 5 * 5 * 4 * 4);
    }

    /// A named row: prior recorded, canonical, tmp, inputs, decision.
    type Row = (&'static str, bool, File, File, [In; 2], Decision);

    /// The rows the acceptance criteria name, spelled out.
    #[test]
    #[allow(clippy::too_many_lines)] // one table, a row per named case
    fn classify_named_rows() {
        use File as F;
        use In as I;
        let all = [I::Matches, I::Matches];
        let rows: &[Row] = &[
            (
                "a, tmp present and matching",
                false,
                F::Replacement,
                F::Replacement,
                all,
                Decision::Published,
            ),
            (
                "a, tmp present and not matching",
                false,
                F::Replacement,
                F::Other,
                all,
                Decision::Published,
            ),
            (
                "a, tmp unreadable",
                false,
                F::Replacement,
                F::Unreadable,
                all,
                Decision::Published,
            ),
            (
                "a, tmp absent, inputs retired",
                true,
                F::Replacement,
                F::Absent,
                [I::Absent, I::Absent],
                Decision::Published,
            ),
            (
                "a, one input retired",
                false,
                F::Replacement,
                F::Absent,
                [I::Absent, I::Matches],
                Decision::Published,
            ),
            (
                "d, a with a present input that differs",
                false,
                F::Replacement,
                F::Absent,
                [I::Absent, I::Differs],
                Decision::Unresolved(Proof::InputMismatch),
            ),
            (
                "b, no prior",
                false,
                F::Absent,
                F::Replacement,
                all,
                Decision::Promote,
            ),
            (
                "b, canonical is the prior",
                true,
                F::Prior,
                F::Replacement,
                all,
                Decision::Promote,
            ),
            (
                "d, b with an unexpected canonical",
                false,
                F::Other,
                F::Replacement,
                all,
                Decision::Unresolved(Proof::PriorMismatch),
            ),
            (
                "d, b with the prior gone",
                true,
                F::Absent,
                F::Replacement,
                all,
                Decision::Unresolved(Proof::PriorMismatch),
            ),
            (
                "c, no prior",
                false,
                F::Absent,
                F::Absent,
                all,
                Decision::Abort,
            ),
            (
                "c, no prior, invalid tmp",
                false,
                F::Absent,
                F::Other,
                all,
                Decision::Abort,
            ),
            (
                "c, matching prior",
                true,
                F::Prior,
                F::Other,
                all,
                Decision::Abort,
            ),
            (
                "d, prior mismatch",
                true,
                F::Other,
                F::Absent,
                all,
                Decision::Unresolved(Proof::PriorMismatch),
            ),
            (
                "d, missing input",
                false,
                F::Absent,
                F::Absent,
                [I::Matches, I::Absent],
                Decision::Unresolved(Proof::InputMissing),
            ),
            (
                "d, input set aside as .merged",
                true,
                F::Prior,
                F::Other,
                [I::Absent, I::Matches],
                Decision::Unresolved(Proof::InputMissing),
            ),
            (
                "d, canonical unreadable",
                false,
                F::Unreadable,
                F::Replacement,
                all,
                Decision::Unresolved(Proof::CanonicalUnreadable),
            ),
        ];
        for &(name, prior, canonical, tmp, inputs, want) in rows {
            assert_eq!(run(prior, canonical, tmp, inputs).0, want, "{name}");
        }
    }

    #[test]
    fn a_merged_sibling_is_not_an_input_at_its_path() {
        // Recovery observes only `HH/{service}.parquet`. A `.merged` file
        // beside it is never looked at, so it cannot make case c hold.
        let marker = marker(false);
        let day = day();
        let mut seen = Vec::new();
        let decision = classify(&marker, &day, |path| {
            seen.push(path.to_path_buf());
            Observation::Absent
        });
        assert_eq!(decision, Decision::Unresolved(Proof::InputMissing));
        assert!(
            seen.iter()
                .all(|p| !p.to_string_lossy().ends_with(".merged"))
        );
    }

    #[test]
    fn proof_labels_are_stable() {
        let labels: Vec<_> = [
            Proof::MarkerUnreadable,
            Proof::MarkerOversized,
            Proof::MarkerOldFormat,
            Proof::MarkerUnsupportedVersion,
            Proof::MarkerMalformed,
            Proof::CanonicalUnreadable,
            Proof::PriorMismatch,
            Proof::InputMissing,
            Proof::InputMismatch,
            Proof::InputUnreadable,
        ]
        .iter()
        .map(|p| p.label())
        .collect();
        assert_eq!(
            labels,
            [
                "marker_unreadable",
                "marker_oversized",
                "marker_old_format",
                "marker_unsupported_version",
                "marker_malformed",
                "canonical_unreadable",
                "prior_mismatch",
                "input_missing",
                "input_mismatch",
                "input_unreadable",
            ]
        );
    }
}
