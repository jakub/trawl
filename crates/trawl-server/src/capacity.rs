// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Disk headroom for the filesystems trawl writes to, and each
//! environment's projected retention reach (ADR-0042).
//!
//! Pure logic plus injected IO seams. [`sample_headroom`] reads through a
//! `stat` seam, inside the repin [`fence`] both capacity samples take.
//! Nothing here reads the clock, so the projection takes `today` as an
//! argument. The measurement's status and
//! age belong to the ADR-0033 cache that holds the samples
//! ([`crate::metrics`]), not to this module.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{Days, NaiveDate};
use trawl_api::{
    Capacity, DeletionFloor, EnvironmentCapacity, FilesystemHeadroom, FilesystemRole,
    HeadroomReading, PressureEvidence, Reach, ReachEnd, StorageMeasurement,
    StorageMeasurementStatus, WithheldReason,
};
use trawl_config::RetentionConfig;

pub mod fence;

/// One role's reading: the device its path lives on and that device's
/// totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleReading {
    pub role: FilesystemRole,
    pub device: u64,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// One device's headroom, labelled with every role that lives on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceHeadroom {
    /// Non-empty, ordered data, wal, spill.
    pub roles: Vec<FilesystemRole>,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// One complete headroom attempt: every role's filesystem, grouped by
/// device, and whether the repin fence saw a repin overlap the stats.
///
/// The repin answer rides the same attempt so that a later projection
/// reads both facts from one moment, with one status and one age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadroomSample {
    pub filesystems: Vec<DeviceHeadroom>,
    pub repin_in_flight: bool,
}

/// Collapse role readings into one row per device.
///
/// Rows are ordered by their first role, and each row's roles are ordered
/// data, wal, spill. Roles on one device share a row, and that row's
/// totals are the first role's reading: available bytes are never summed,
/// because two paths on one device draw on the same free space.
#[must_use]
pub fn group_by_device(readings: &[RoleReading]) -> Vec<DeviceHeadroom> {
    let mut sorted = readings.to_vec();
    sorted.sort_by_key(|reading| reading.role);
    let mut rows: Vec<(u64, DeviceHeadroom)> = Vec::new();
    for reading in sorted {
        match rows
            .iter_mut()
            .find(|(device, _)| *device == reading.device)
        {
            Some((_, row)) => row.roles.push(reading.role),
            None => rows.push((
                reading.device,
                DeviceHeadroom {
                    roles: vec![reading.role],
                    total_bytes: reading.total_bytes,
                    available_bytes: reading.available_bytes,
                },
            )),
        }
    }
    rows.into_iter().map(|(_, row)| row).collect()
}

/// The data filesystem's floor against its available bytes.
///
/// Pressure deletion starts only while available bytes are strictly below
/// the floor, so equality is no deficit. A floor of 0 turns pressure
/// deletion off.
#[must_use]
pub fn assess_floor(floor_bytes: u64, available_bytes: u64) -> DeletionFloor {
    if floor_bytes == 0 {
        return DeletionFloor::Off;
    }
    DeletionFloor::Armed {
        floor_bytes,
        deficit_bytes: floor_bytes.saturating_sub(available_bytes),
    }
}

/// The wire rows for a sample. Only the row holding the data role carries
/// the floor: nothing reclaims the other filesystems.
#[must_use]
pub fn filesystem_rows(sample: &HeadroomSample, floor_bytes: u64) -> Vec<FilesystemHeadroom> {
    sample
        .filesystems
        .iter()
        .map(|device| FilesystemHeadroom {
            roles: device.roles.clone(),
            total_bytes: device.total_bytes,
            available_bytes: device.available_bytes,
            floor: device
                .roles
                .contains(&FilesystemRole::Data)
                .then(|| assess_floor(floor_bytes, device.available_bytes)),
        })
        .collect()
}

/// Take one headroom attempt.
///
/// `stat` answers `(device, total_bytes, available_bytes)` for a path
/// ([`stat_filesystem`] in production). The stats run inside a
/// [`fence::RepinFence`] on the data root and `jobs`, the same fence the
/// Parquet scan takes, so any repin job that overlaps the stats is
/// recorded in the sample. Any error fails the whole attempt, so the cache
/// keeps the last complete sample rather than publish a partial one that
/// would lose a device's row. `wal_dir` is `None` when ingest is off.
///
/// # Errors
/// The fence's open error, then the first `stat` error in role order,
/// then the fence's close error.
pub fn sample_headroom(
    data_root: &Path,
    wal_dir: Option<&Path>,
    spill_dir: &Path,
    jobs: &crate::repin::JobGeneration,
    stat: impl Fn(&Path) -> std::io::Result<(u64, u64, u64)>,
) -> std::io::Result<HeadroomSample> {
    let fence = fence::RepinFence::open(data_root, jobs)?;
    let roles = [
        (FilesystemRole::Data, Some(data_root)),
        (FilesystemRole::Wal, wal_dir),
        (FilesystemRole::Spill, Some(spill_dir)),
    ];
    let mut readings = Vec::with_capacity(roles.len());
    for (role, path) in roles {
        let Some(path) = path else { continue };
        let (device, total_bytes, available_bytes) = stat(path)?;
        readings.push(RoleReading {
            role,
            device,
            total_bytes,
            available_bytes,
        });
    }
    Ok(HeadroomSample {
        filesystems: group_by_device(&readings),
        repin_in_flight: fence.close()?,
    })
}

/// The production `stat` seam: the path's device, then the filesystem's
/// total and available bytes from one `statvfs`.
///
/// # Errors
/// The metadata or `statvfs` error for `path`.
pub fn stat_filesystem(path: &Path) -> std::io::Result<(u64, u64, u64)> {
    let device = crate::repin::marker::device_of_meta(&std::fs::metadata(path)?);
    let stats = fs4::statvfs(path)?;
    Ok((device, stats.total_space(), stats.available_space()))
}

/// Parquet bytes per `(env, date)` partition, from one storage walk.
pub type EnvDateBytes = BTreeMap<(String, NaiveDate), u64>;

/// An environment with fewer observed days than this has no rate.
pub const MIN_OBSERVED_DAYS: usize = 3;

/// The newest and oldest observed day, in days before today. Today and
/// yesterday are still settling under rollup and late arrivals.
const NEWEST_OBSERVED_DAYS_AGO: u64 = 2;
const OLDEST_OBSERVED_DAYS_AGO: u64 = 8;

/// The least common multiple of every rated observed-day count (3 to 7).
/// Scaling every rate by it makes each mean observed day an exact integer,
/// so the projection never rounds before its final floor.
const RATE_SCALE: u128 = 420;

/// An environment's observed days: each date from today−8 through today−2
/// with its stored bytes, oldest first.
///
/// The oldest date is the oldest one holding counted Parquet: `partitions`
/// is one environment's bytes by date from the storage scan, which buckets
/// only counted files, so an empty date directory never appears in it.
/// Retention always deletes an environment's oldest date first, so a
/// missing date newer than the oldest date was a quiet day and counts as
/// zero. A date older than it is absent, not zero. An empty date directory
/// does not anchor that zero-fill: it is no evidence of a quiet ingest day,
/// and anchoring on it would add zero days and overstate reach.
#[must_use]
pub fn observed_days(
    today: NaiveDate,
    partitions: &BTreeMap<NaiveDate, u64>,
) -> Vec<(NaiveDate, u64)> {
    let Some(&oldest) = partitions.keys().next() else {
        return Vec::new();
    };
    (NEWEST_OBSERVED_DAYS_AGO..=OLDEST_OBSERVED_DAYS_AGO)
        .rev()
        .filter_map(|ago| today.checked_sub_days(Days::new(ago)))
        .filter(|day| *day >= oldest)
        .map(|day| (day, partitions.get(&day).copied().unwrap_or(0)))
        .collect()
}

/// The data filesystem's available bytes, admitted for a projection.
///
/// Only [`projection_basis`] builds one, and only from complete
/// measurements, so a projection can never be computed from a retained,
/// failed sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionBasis {
    data_available_bytes: u64,
}

/// Admit the measurements behind a projection, or name why it is withheld.
///
/// `parquet` is the storage scan's measurement; the partition bytes handed
/// to [`project`] must be the sample read with it, and `parquet_saw_repin`
/// is that sample's repin evidence. `headroom` and `sample` are the
/// headroom measurement and its retained sample. Precedence: either
/// measurement not `complete` is [`WithheldReason::MeasurementUnavailable`];
/// a repin in flight at either sample's attempt is
/// [`WithheldReason::RetentionSuppressed`], since it holds two generations
/// of stored bytes. The two samples come from separate caches with
/// separate ages, so neither one's evidence speaks for the other.
///
/// # Errors
/// The reason every environment's reach is withheld.
pub fn projection_basis(
    parquet: &StorageMeasurement,
    parquet_saw_repin: bool,
    headroom: &StorageMeasurement,
    sample: Option<&HeadroomSample>,
) -> Result<ProjectionBasis, WithheldReason> {
    let complete =
        |measurement: &StorageMeasurement| measurement.status == StorageMeasurementStatus::Complete;
    if !complete(parquet) || !complete(headroom) {
        return Err(WithheldReason::MeasurementUnavailable);
    }
    let sample = sample.ok_or(WithheldReason::MeasurementUnavailable)?;
    let data = sample
        .filesystems
        .iter()
        .find(|device| device.roles.contains(&FilesystemRole::Data))
        .ok_or(WithheldReason::MeasurementUnavailable)?;
    if parquet_saw_repin || sample.repin_in_flight {
        return Err(WithheldReason::RetentionSuppressed);
    }
    Ok(ProjectionBasis {
        data_available_bytes: data.available_bytes,
    })
}

/// Every environment's capacity, and the finite environments whose growth
/// the projection left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// Environments with at least one stored partition, sorted by name.
    pub environments: Vec<EnvironmentCapacity>,
    /// Finite-retention environments with too few observed days, whose
    /// stored bytes were reserved instead. Sorted by name; empty when the
    /// projection is withheld, because nothing was projected.
    pub growth_excluded: Vec<String>,
}

/// One environment's partitions, summarised for the projection.
struct EnvRow<'a> {
    env: &'a str,
    max_age_days: u64,
    /// The oldest date holding counted Parquet. An empty date directory is
    /// not a partition here (see [`observed_days`]).
    oldest: NaiveDate,
    stored_bytes: u64,
    observed: Vec<(NaiveDate, u64)>,
}

impl EnvRow<'_> {
    fn keeps_forever(&self) -> bool {
        self.max_age_days == 0
    }

    fn has_rate(&self) -> bool {
        self.observed.len() >= MIN_OBSERVED_DAYS
    }

    /// A finite environment with a rate: its growth is projected.
    fn is_rated(&self) -> bool {
        !self.keeps_forever() && self.has_rate()
    }

    fn observed_bytes(&self) -> u128 {
        self.observed
            .iter()
            .map(|&(_, bytes)| u128::from(bytes))
            .fold(0, u128::saturating_add)
    }

    /// The mean observed day, times [`RATE_SCALE`]: exact, because the
    /// observed-day count divides the scale.
    fn scaled_mean_day(&self) -> u128 {
        let days = u128::try_from(self.observed.len()).unwrap_or(u128::MAX);
        self.observed_bytes()
            .saturating_mul(RATE_SCALE / days.max(1))
    }

    /// The largest observed day, times [`RATE_SCALE`].
    fn scaled_largest_day(&self) -> u128 {
        let largest = self.observed.iter().map(|&(_, bytes)| bytes).max();
        u128::from(largest.unwrap_or(0)).saturating_mul(RATE_SCALE)
    }
}

fn date_string(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// Project each environment's retention reach (ADR-0042).
///
/// `env_dates` is the storage scan's partition bytes; every environment
/// with a partition is listed, with its effective `max_age_days` from
/// `retention` and its oldest date (the oldest holding counted Parquet)
/// and stored bytes, whatever the basis.
/// `basis` comes from [`projection_basis`]; an `Err` withholds every
/// environment's reach with that reason.
///
/// The model: under sustained pressure the expiry-ratio order (ADR-0018)
/// drives every finite environment toward one fraction `f` of its
/// `max_age_days`. The budget is every partition's bytes plus the data
/// filesystem's available bytes, minus the deletion floor
/// (`retention.min_free_disk_bytes`, the one the retention loop enforces).
/// Keep-forever
/// environments, and finite ones with fewer than [`MIN_OBSERVED_DAYS`]
/// observed days, are reserved at their stored bytes. The rest solve
/// `Σ daily_bytes × f × max_age_days = budget − reserved`, once with each
/// environment's mean observed day (the `high` end) and once with its
/// largest (the `low` end). An end is the full policy when `f ≥ 1`,
/// decided before any rounding; otherwise `floor(f × max_age_days)` whole
/// days, or, with a floor of 0, the disk filling first.
///
/// Arithmetic is `u128` and saturating: exact while bytes × `max_age_days`
/// × 420 fits in 128 bits, and never a panic beyond it.
#[must_use]
pub fn project(
    today: NaiveDate,
    env_dates: &EnvDateBytes,
    basis: Result<ProjectionBasis, WithheldReason>,
    retention: &RetentionConfig,
) -> Projection {
    let mut partitions: BTreeMap<&str, BTreeMap<NaiveDate, u64>> = BTreeMap::new();
    for ((env, date), bytes) in env_dates {
        partitions
            .entry(env.as_str())
            .or_default()
            .insert(*date, *bytes);
    }
    let rows: Vec<EnvRow<'_>> = partitions
        .iter()
        .filter_map(|(&env, dates)| {
            Some(EnvRow {
                env,
                max_age_days: retention.max_age_days_for(env),
                oldest: *dates.keys().next()?,
                stored_bytes: dates.values().copied().fold(0, u64::saturating_add),
                observed: observed_days(today, dates),
            })
        })
        .collect();

    let (reaches, growth_excluded) = match basis {
        Err(reason) => (
            rows.iter().map(|_| Reach::Withheld { reason }).collect(),
            Vec::new(),
        ),
        Ok(basis) => (
            reach_all(&rows, basis, retention.min_free_disk_bytes),
            rows.iter()
                .filter(|row| !row.keeps_forever() && !row.has_rate())
                .map(|row| row.env.to_owned())
                .collect(),
        ),
    };
    let environments = rows
        .iter()
        .zip(reaches)
        .map(|(row, reach)| EnvironmentCapacity {
            env: row.env.to_owned(),
            max_age_days: row.max_age_days,
            oldest_date: date_string(row.oldest),
            stored_bytes: row.stored_bytes,
            reach,
        })
        .collect();
    Projection {
        environments,
        growth_excluded,
    }
}

/// Every row's reach from an admitted basis, in row order.
fn reach_all(rows: &[EnvRow<'_>], basis: ProjectionBasis, floor_bytes: u64) -> Vec<Reach> {
    let stored = |row: &EnvRow<'_>| u128::from(row.stored_bytes);
    let budget = rows
        .iter()
        .map(stored)
        .fold(u128::from(basis.data_available_bytes), u128::saturating_add)
        .saturating_sub(u128::from(floor_bytes));
    let reserved = rows
        .iter()
        .filter(|row| !row.is_rated())
        .map(stored)
        .fold(0, u128::saturating_add);
    let spare = budget.saturating_sub(reserved);
    let high = run_ends(rows, spare, floor_bytes, EnvRow::scaled_mean_day);
    let low = run_ends(rows, spare, floor_bytes, EnvRow::scaled_largest_day);

    rows.iter()
        .zip(low.into_iter().zip(high))
        .map(|(row, ends)| {
            let Some((first, last)) = row.observed.first().zip(row.observed.last()) else {
                return Reach::Withheld {
                    reason: WithheldReason::InsufficientHistory,
                };
            };
            let observed_days = u8::try_from(row.observed.len()).unwrap_or(u8::MAX);
            match ends {
                (Some(low), Some(high)) => Reach::Projected {
                    observed_first: date_string(first.0),
                    observed_last: date_string(last.0),
                    observed_days,
                    low,
                    high,
                },
                _ if row.keeps_forever() && row.has_rate() => {
                    let days = u128::try_from(row.observed.len()).unwrap_or(u128::MAX);
                    Reach::KeepForever {
                        observed_first: date_string(first.0),
                        observed_last: date_string(last.0),
                        observed_days,
                        mean_daily_bytes: u64::try_from(row.observed_bytes() / days)
                            .unwrap_or(u64::MAX),
                    }
                }
                _ => Reach::Withheld {
                    reason: WithheldReason::InsufficientHistory,
                },
            }
        })
        .collect()
}

/// One run of the solve for `f`, with each rated row's daily bytes from
/// `scaled_day`. `Some` end for every rated row, `None` for the rest.
fn run_ends<'a>(
    rows: &[EnvRow<'a>],
    spare: u128,
    floor_bytes: u64,
    scaled_day: impl Fn(&EnvRow<'a>) -> u128,
) -> Vec<Option<ReachEnd>> {
    // Σ daily_bytes × max_age_days, times RATE_SCALE: the bytes every
    // rated policy needs in full.
    let demand = rows
        .iter()
        .filter(|row| row.is_rated())
        .map(|row| scaled_day(row).saturating_mul(u128::from(row.max_age_days)))
        .fold(0, u128::saturating_add);
    let supply = spare.saturating_mul(RATE_SCALE);
    rows.iter()
        .map(|row| {
            row.is_rated().then(|| {
                // f = supply / demand. No demand, or f >= 1, is the full
                // policy, decided on the exact ratio before any rounding.
                if demand == 0 || supply >= demand {
                    ReachEnd::FullPolicy
                } else if floor_bytes == 0 {
                    ReachEnd::DiskFillsFirst
                } else {
                    let max_age = u128::from(row.max_age_days);
                    let days = (supply.saturating_mul(max_age) / demand).min(max_age);
                    ReachEnd::Days {
                        days: u64::try_from(days).unwrap_or(row.max_age_days),
                    }
                }
            })
        })
        .collect()
}

/// The cached readings one dashboard snapshot assembles capacity from.
///
/// Each pair is one cache read: a measurement and the sample retained
/// with it, so a status never describes a different sample's numbers.
#[derive(Debug, Clone, Copy)]
pub struct CapacityReadings<'a> {
    /// The Parquet scan's measurement.
    pub parquet: &'a StorageMeasurement,
    /// That scan's partition bytes; `None` before a complete scan.
    pub env_dates: Option<&'a EnvDateBytes>,
    /// Whether a repin held the data root during that scan's attempt;
    /// `false` before a complete scan, which is withheld as unmeasured.
    pub parquet_repin_in_flight: bool,
    /// The headroom measurement.
    pub headroom: &'a StorageMeasurement,
    /// That measurement's retained sample; `None` before a complete one.
    pub sample: Option<&'a HeadroomSample>,
}

/// The dashboard's capacity object (ADR-0042).
///
/// `today` is the UTC date, read once per snapshot so every environment is
/// projected against the same day. `pressure` is retention's evidence,
/// with the last sweep's age already evaluated. The deletion floor comes
/// from `retention`, the config the retention loop enforces, for the
/// headroom rows and the projection alike.
#[must_use]
pub fn assemble(
    today: NaiveDate,
    readings: CapacityReadings<'_>,
    pressure: PressureEvidence,
    retention: &RetentionConfig,
) -> Capacity {
    let basis = projection_basis(
        readings.parquet,
        readings.parquet_repin_in_flight,
        readings.headroom,
        readings.sample,
    );
    let Projection {
        environments,
        growth_excluded,
    } = project(
        today,
        readings.env_dates.unwrap_or(&EnvDateBytes::new()),
        basis,
        retention,
    );
    Capacity {
        headroom: HeadroomReading {
            measurement: *readings.headroom,
            filesystems: readings
                .sample
                .map(|sample| filesystem_rows(sample, retention.min_free_disk_bytes))
                .unwrap_or_default(),
        },
        pressure,
        environments,
        growth_excluded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: FilesystemRole = FilesystemRole::Data;
    const WAL: FilesystemRole = FilesystemRole::Wal;
    const SPILL: FilesystemRole = FilesystemRole::Spill;

    fn reading(role: FilesystemRole, device: u64, total: u64, available: u64) -> RoleReading {
        RoleReading {
            role,
            device,
            total_bytes: total,
            available_bytes: available,
        }
    }

    fn device(roles: &[FilesystemRole], total: u64, available: u64) -> DeviceHeadroom {
        DeviceHeadroom {
            roles: roles.to_vec(),
            total_bytes: total,
            available_bytes: available,
        }
    }

    /// A stat seam over fixed `(path, device, total, available)` rows.
    fn stat_table(
        table: &[(&Path, u64, u64, u64)],
    ) -> impl Fn(&Path) -> std::io::Result<(u64, u64, u64)> + use<> {
        let table: Vec<_> = table
            .iter()
            .map(|&(path, device, total, available)| (path.to_owned(), device, total, available))
            .collect();
        move |path| {
            table
                .iter()
                .find(|(name, ..)| name == path)
                .map(|&(_, device, total, available)| (device, total, available))
                .ok_or_else(|| std::io::Error::other("unknown path"))
        }
    }

    #[test]
    fn capacity_filesystems_group_by_device() {
        // Every role on one device: one row, the data reading, nothing summed.
        assert_eq!(
            group_by_device(&[
                reading(SPILL, 1, 100, 40),
                reading(DATA, 1, 100, 40),
                reading(WAL, 1, 100, 40),
            ]),
            [device(&[DATA, WAL, SPILL], 100, 40)]
        );
        // A WAL on its own device gets its own row; spill shares data's.
        assert_eq!(
            group_by_device(&[
                reading(DATA, 1, 100, 40),
                reading(WAL, 2, 50, 5),
                reading(SPILL, 1, 100, 40),
            ]),
            [device(&[DATA, SPILL], 100, 40), device(&[WAL], 50, 5)]
        );
        // Three devices, three rows, each with its own available bytes.
        assert_eq!(
            group_by_device(&[
                reading(DATA, 1, 100, 40),
                reading(WAL, 2, 50, 5),
                reading(SPILL, 3, 10, 9),
            ]),
            [
                device(&[DATA], 100, 40),
                device(&[WAL], 50, 5),
                device(&[SPILL], 10, 9),
            ]
        );
        // Spill shares the WAL's device, not data's.
        assert_eq!(
            group_by_device(&[
                reading(DATA, 1, 100, 40),
                reading(WAL, 2, 50, 5),
                reading(SPILL, 2, 50, 5),
            ]),
            [device(&[DATA], 100, 40), device(&[WAL, SPILL], 50, 5)]
        );

        // The same grouping through the sampling seam, with and without a
        // WAL: ingest off stats no WAL path at all.
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path();
        let jobs = crate::repin::JobGeneration::default();
        let stat = stat_table(&[
            (data, 1, 100, 40),
            (Path::new("/wal"), 2, 50, 5),
            (Path::new("/tmp"), 1, 100, 40),
        ]);
        let sample = sample_headroom(
            data,
            Some(Path::new("/wal")),
            Path::new("/tmp"),
            &jobs,
            &stat,
        )
        .unwrap();
        assert_eq!(
            sample.filesystems,
            [device(&[DATA, SPILL], 100, 40), device(&[WAL], 50, 5)]
        );
        assert!(!sample.repin_in_flight);
        // The repin fence's answer rides the sample: a running job, and
        // on-disk evidence with no job.
        let job = jobs.begin();
        let query_only = sample_headroom(data, None, Path::new("/tmp"), &jobs, &stat).unwrap();
        assert_eq!(query_only.filesystems, [device(&[DATA, SPILL], 100, 40)]);
        assert!(query_only.repin_in_flight);
        drop(job);
        std::fs::write(crate::repin::marker::marker_path(data), "{}").unwrap();
        let leftover = sample_headroom(data, None, Path::new("/tmp"), &jobs, &stat).unwrap();
        assert!(leftover.repin_in_flight);

        // Any role's failure fails the whole attempt, the data root's
        // included.
        assert!(
            sample_headroom(
                data,
                Some(Path::new("/missing-wal")),
                Path::new("/tmp"),
                &jobs,
                &stat,
            )
            .is_err()
        );
        let missing = data.join("missing");
        let stat = stat_table(&[(Path::new("/tmp"), 1, 100, 40)]);
        assert!(sample_headroom(&missing, None, Path::new("/tmp"), &jobs, &stat).is_err());
    }

    /// A repin job cancelled or refused before its cutover builds a big
    /// shadow while the stat reads free space, then sweeps the shadow and
    /// drops its marker without replacing any env directory. Nothing on
    /// disk tells the fence's close it happened. The stat seam here runs
    /// that whole job, with the engine's real abandon sweep, inside the
    /// stats: the sample reports the repin rather than admitting the
    /// transiently low free space as clean.
    #[test]
    fn headroom_sample_sees_a_repin_aborted_before_cutover() {
        use crate::repin::marker::{marker_path, remove_marker, shadow_root};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let live = root.join("prod/2026-09-20");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("x.parquet"), "0123456789").unwrap();
        let before = std::fs::metadata(root.join("prod")).unwrap();
        let jobs = crate::repin::JobGeneration::default();
        let abort_a_repin = || {
            let _job = jobs.begin();
            std::fs::write(marker_path(&root), "{}").unwrap();
            let built = shadow_root(&root).join("prod/2026-09-20");
            std::fs::create_dir_all(&built).unwrap();
            std::fs::write(built.join("x.parquet"), vec![b'x'; 4096]).unwrap();
            assert!(crate::repin::cutover::sweep_pre_swap_staging(&root));
            remove_marker(&root).unwrap();
        };
        let stat = |path: &Path| {
            if path == root {
                abort_a_repin();
            }
            Ok((1, 1_000, 10))
        };
        let sample = sample_headroom(&root, None, Path::new("/tmp"), &jobs, stat).unwrap();
        assert!(sample.repin_in_flight);
        // The job left the live generation as it found it and no evidence.
        assert_eq!(crate::repin::in_flight_evidence(&root).unwrap(), None);
        {
            use std::os::unix::fs::MetadataExt as _;
            let after = std::fs::metadata(root.join("prod")).unwrap();
            assert_eq!(after.ino(), before.ino());
        }
        // Idle again, the next sample is clean.
        let quiet = sample_headroom(&root, None, Path::new("/tmp"), &jobs, |_| {
            Ok((1, 1_000, 10))
        })
        .unwrap();
        assert!(!quiet.repin_in_flight);
    }

    #[test]
    fn capacity_headroom_deficit_against_floor() {
        assert_eq!(
            assess_floor(100, 40),
            DeletionFloor::Armed {
                floor_bytes: 100,
                deficit_bytes: 60
            }
        );
        // Pressure deletion starts strictly below the floor: equality is
        // no deficit, and neither is anything above it.
        for available in [100, 101, u64::MAX] {
            assert_eq!(
                assess_floor(100, available),
                DeletionFloor::Armed {
                    floor_bytes: 100,
                    deficit_bytes: 0
                }
            );
        }
        assert_eq!(
            assess_floor(100, 99),
            DeletionFloor::Armed {
                floor_bytes: 100,
                deficit_bytes: 1
            }
        );
        // A floor of 0 is pressure deletion off, even on a full disk.
        assert_eq!(assess_floor(0, 0), DeletionFloor::Off);
        assert_eq!(assess_floor(0, 500), DeletionFloor::Off);

        // Only the row holding data carries the floor.
        let sample = HeadroomSample {
            filesystems: vec![device(&[DATA, SPILL], 100, 40), device(&[WAL], 50, 5)],
            repin_in_flight: false,
        };
        let rows = filesystem_rows(&sample, 64);
        assert_eq!(
            rows[0].floor,
            Some(DeletionFloor::Armed {
                floor_bytes: 64,
                deficit_bytes: 24
            })
        );
        assert_eq!(rows[1].floor, None);
        assert_eq!(
            (rows[1].total_bytes, rows[1].available_bytes),
            (50, 5),
            "a non-data row keeps its own bytes"
        );
        assert_eq!(
            filesystem_rows(&sample, 0)[0].floor,
            Some(DeletionFloor::Off)
        );
    }

    /// The production seam reads a real directory, and two paths on one
    /// filesystem report one device.
    #[test]
    fn capacity_stat_reads_a_real_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("wal");
        std::fs::create_dir(&nested).unwrap();
        let (device, total, available) = stat_filesystem(tmp.path()).unwrap();
        assert!(total > 0 && available <= total);
        assert_eq!(stat_filesystem(&nested).unwrap().0, device);
        assert!(stat_filesystem(&tmp.path().join("absent")).is_err());
    }

    // -- reach ----------------------------------------------------------------

    use trawl_config::EnvRetention;

    /// 2026-09-27: observed days are 09-19 (today−8) through 09-25 (today−2).
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 27).unwrap()
    }

    fn ago(days: u64) -> NaiveDate {
        today().checked_sub_days(Days::new(days)).unwrap()
    }

    fn day(days_ago: u64) -> String {
        date_string(ago(days_ago))
    }

    /// Plant one env's partitions as `(days ago, bytes)`.
    fn plant(env_dates: &mut EnvDateBytes, env: &str, partitions: &[(u64, u64)]) {
        for &(days_ago, bytes) in partitions {
            env_dates.insert((env.to_owned(), ago(days_ago)), bytes);
        }
    }

    /// The seven observed days, today−8 first, with these bytes.
    fn week(bytes: [u64; 7]) -> Vec<(u64, u64)> {
        (2..=8).rev().zip(bytes).collect()
    }

    fn retention(global: u64, envs: &[(&str, u64)]) -> RetentionConfig {
        RetentionConfig {
            max_age_days: global,
            env: envs
                .iter()
                .map(|&(env, max_age_days)| (env.to_owned(), EnvRetention { max_age_days }))
                .collect(),
            ..RetentionConfig::default()
        }
    }

    /// `config` with a deletion floor of `floor` bytes.
    fn floored(config: &RetentionConfig, floor: u64) -> RetentionConfig {
        RetentionConfig {
            min_free_disk_bytes: floor,
            ..config.clone()
        }
    }

    fn measured(status: StorageMeasurementStatus) -> StorageMeasurement {
        StorageMeasurement {
            status,
            sample_age_secs: match status {
                StorageMeasurementStatus::Complete | StorageMeasurementStatus::Failed => Some(30),
                StorageMeasurementStatus::NotSampled | StorageMeasurementStatus::NotConfigured => {
                    None
                }
            },
        }
    }

    fn headroom_sample(available: u64, repin_in_flight: bool) -> HeadroomSample {
        HeadroomSample {
            filesystems: vec![
                device(&[DATA, SPILL], available.saturating_mul(4), available),
                device(&[WAL], 1_000, 1_000),
            ],
            repin_in_flight,
        }
    }

    /// A basis admitted from two complete measurements.
    fn basis(available: u64) -> Result<ProjectionBasis, WithheldReason> {
        let complete = measured(StorageMeasurementStatus::Complete);
        projection_basis(
            &complete,
            false,
            &complete,
            Some(&headroom_sample(available, false)),
        )
    }

    fn reach_of<'a>(projection: &'a Projection, env: &str) -> &'a Reach {
        &projection
            .environments
            .iter()
            .find(|capacity| capacity.env == env)
            .unwrap_or_else(|| panic!("{env} is listed"))
            .reach
    }

    fn projected(first: u64, observed_days: u8, low: ReachEnd, high: ReachEnd) -> Reach {
        Reach::Projected {
            observed_first: day(first),
            observed_last: day(2),
            observed_days,
            low,
            high,
        }
    }

    fn days(days: u64) -> ReachEnd {
        ReachEnd::Days { days }
    }

    const WITHHELD_HISTORY: Reach = Reach::Withheld {
        reason: WithheldReason::InsufficientHistory,
    };

    #[test]
    fn reach_observed_days_window_and_gaps() {
        let partitions = |planted: &[(u64, u64)]| -> BTreeMap<NaiveDate, u64> {
            planted
                .iter()
                .map(|&(days_ago, bytes)| (ago(days_ago), bytes))
                .collect()
        };
        // The window edges: today−9 and today−1 (and today) are outside,
        // today−8 and today−2 are its ends. Gaps inside count as zero.
        assert_eq!(
            observed_days(
                today(),
                &partitions(&[(9, 900), (8, 1), (5, 5), (2, 2), (1, 100), (0, 1_000)])
            ),
            [
                (ago(8), 1),
                (ago(7), 0),
                (ago(6), 0),
                (ago(5), 5),
                (ago(4), 0),
                (ago(3), 0),
                (ago(2), 2),
            ]
        );
        // A date older than the oldest surviving partition is absent, not
        // zero: an env first stored at today−5 has four observed days.
        assert_eq!(
            observed_days(today(), &partitions(&[(5, 50), (3, 30)])),
            [(ago(5), 50), (ago(4), 0), (ago(3), 30), (ago(2), 0)]
        );
        // Oldest at the newest edge: one observed day. Past it: none.
        assert_eq!(
            observed_days(today(), &partitions(&[(2, 7), (0, 70)])),
            [(ago(2), 7)]
        );
        assert!(observed_days(today(), &partitions(&[(1, 7), (0, 70)])).is_empty());
        assert!(observed_days(today(), &partitions(&[(0, 70)])).is_empty());
        assert!(observed_days(today(), &BTreeMap::new()).is_empty());
        // A partition dated after today is stored, never observed.
        let tomorrow = today().checked_add_days(Days::new(1)).unwrap();
        assert!(observed_days(today(), &BTreeMap::from([(tomorrow, 5)])).is_empty());

        // The same window through the projection: an env first stored at
        // today−4 has three observed days (rated); one at today−3 has two.
        let mut env_dates = EnvDateBytes::new();
        plant(&mut env_dates, "three", &[(4, 10), (0, 99)]);
        plant(&mut env_dates, "two", &[(3, 10), (2, 10)]);
        let projection = project(
            today(),
            &env_dates,
            basis(1 << 40),
            &floored(&retention(30, &[]), 0),
        );
        assert!(
            matches!(
                reach_of(&projection, "three"),
                Reach::Projected { observed_first, observed_last, observed_days: 3, .. }
                    if *observed_first == day(4) && *observed_last == day(2)
            ),
            "{projection:?}"
        );
        assert_eq!(reach_of(&projection, "two"), &WITHHELD_HISTORY);
        let three = &projection.environments[0];
        assert_eq!(
            (
                three.env.as_str(),
                three.oldest_date.as_str(),
                three.stored_bytes
            ),
            ("three", day(4).as_str(), 109)
        );
    }

    #[test]
    fn reach_equalises_expiry_ratio() {
        // prod keeps 365 days, lab 7, archive forever. Steady days, so the
        // mean and the largest day agree and both ends match.
        //   D_prod = 100 × 365 = 36_500, D_lab = 1_000 × 7 = 7_000,
        //   Σ D = 43_500. Stored: prod 700, lab 7_000, archive 1_000.
        //   Budget = 8_700 + available − floor; reserved = archive 1_000.
        let mut env_dates = EnvDateBytes::new();
        plant(&mut env_dates, "prod", &week([100; 7]));
        plant(&mut env_dates, "lab", &week([1_000; 7]));
        plant(&mut env_dates, "archive", &week([50; 7]));
        plant(&mut env_dates, "archive", &[(30, 650)]);
        let config = retention(365, &[("lab", 7), ("archive", 0)]);
        let floor = 1_000;
        let run = |available| {
            project(
                today(),
                &env_dates,
                basis(available),
                &floored(&config, floor),
            )
        };

        // available 15_050: budget − reserved = 8_700 + 14_050 − 1_000 =
        // 21_750, f = 21_750 / 43_500 = 1/2. Each env keeps the same half
        // of its own policy, floored: prod 182.5 → 182, lab 3.5 → 3.
        let half = run(15_050);
        assert_eq!(
            reach_of(&half, "prod"),
            &projected(8, 7, days(182), days(182))
        );
        assert_eq!(reach_of(&half, "lab"), &projected(8, 7, days(3), days(3)));
        // The keep-forever env is reserved whole and makes no time claim:
        // 350 observed bytes over 7 days.
        assert_eq!(
            reach_of(&half, "archive"),
            &Reach::KeepForever {
                observed_first: day(8),
                observed_last: day(2),
                observed_days: 7,
                mean_daily_bytes: 50,
            }
        );
        assert!(half.growth_excluded.is_empty());
        let archive = &half.environments[0];
        assert_eq!(
            (
                archive.env.as_str(),
                archive.max_age_days,
                archive.stored_bytes
            ),
            ("archive", 0, 1_000)
        );
        assert_eq!(archive.oldest_date, day(30));

        // available 36_800: budget − reserved = 43_500 = Σ D, f = 1 exactly:
        // the full policy.
        let full = run(36_800);
        for env in ["prod", "lab"] {
            assert_eq!(
                reach_of(&full, env),
                &projected(8, 7, ReachEnd::FullPolicy, ReachEnd::FullPolicy)
            );
        }
        // One byte less: f = 43_499 / 43_500 < 1, decided before rounding.
        // prod 364.99… → 364, lab 6.9998… → 6, never the full policy.
        let short = run(36_799);
        assert_eq!(
            reach_of(&short, "prod"),
            &projected(8, 7, days(364), days(364))
        );
        assert_eq!(reach_of(&short, "lab"), &projected(8, 7, days(6), days(6)));

        // An env whose observed days are all quiet adds no demand. With no
        // demand at all, f is unbounded: the full policy, whatever the
        // budget. Its oldest partition is outside the window.
        let mut quiet = EnvDateBytes::new();
        plant(&mut quiet, "prod", &[(9, 500)]);
        assert_eq!(
            reach_of(
                &project(today(), &quiet, basis(0), &floored(&config, floor)),
                "prod"
            ),
            &projected(8, 7, ReachEnd::FullPolicy, ReachEnd::FullPolicy)
        );

        // An effectively-never age limit and absurd byte counts saturate
        // instead of panicking, and never claim more than the policy.
        let mut huge = EnvDateBytes::new();
        plant(&mut huge, "prod", &week([u64::MAX / 8; 7]));
        plant(&mut huge, "lab", &week([u64::MAX / 8; 7]));
        let config = retention(u64::MAX, &[("lab", 7)]);
        for available in [0, u64::MAX] {
            let projection = project(today(), &huge, basis(available), &floored(&config, 1));
            for (env, max_age) in [("prod", u64::MAX), ("lab", 7)] {
                let Reach::Projected { low, high, .. } = reach_of(&projection, env) else {
                    panic!("{env} is projected: {projection:?}");
                };
                for end in [low, high] {
                    assert!(
                        matches!(end, ReachEnd::Days { days } if *days <= max_age),
                        "{env}: {end:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn reach_range_mean_to_largest_day() {
        // prod keeps 90 days. Observed 10, 20, … 70: mean 40, largest 70.
        //   D_mean = 40 × 90 = 3_600, D_largest = 70 × 90 = 6_300.
        //   Budget = stored 280 + available 2_350 − floor 100 = 2_530.
        //   high (mean):    2_530 × 90 / 3_600 = 63.25 → 63
        //   low (largest):  2_530 × 90 / 6_300 = 36.14… → 36
        let mut env_dates = EnvDateBytes::new();
        plant(&mut env_dates, "prod", &week([10, 20, 30, 40, 50, 60, 70]));
        let config = retention(90, &[]);
        assert_eq!(
            reach_of(
                &project(today(), &env_dates, basis(2_350), &floored(&config, 100)),
                "prod"
            ),
            &projected(8, 7, days(36), days(63))
        );

        // A mean that is not a whole byte: lab keeps 30 days and has three
        // observed days, 1, 2, 2 (mean 5/3, largest 2), plus 100 bytes
        // today that count as stored but are not observed.
        //   D_mean = 5/3 × 30 = 50, D_largest = 2 × 30 = 60.
        //   Budget = stored 105 + available 40 − floor 100 = 45.
        //   high: 45 / 50 × 30 = 27, low: 45 / 60 × 30 = 22.5 → 22.
        let mut env_dates = EnvDateBytes::new();
        plant(&mut env_dates, "lab", &[(4, 1), (3, 2), (2, 2), (0, 100)]);
        let lab_30 = retention(90, &[("lab", 30)]);
        assert_eq!(
            reach_of(
                &project(today(), &env_dates, basis(40), &floored(&lab_30, 100)),
                "lab"
            ),
            &projected(4, 3, days(22), days(27))
        );

        // Across envs, each end uses one rate for every env: prod at a
        // steady 100 (D = 9_000 both runs) beside lab above, now at 90 days.
        //   D_mean  = 9_000 + 5/3 × 90 = 9_150
        //   D_large = 9_000 + 2 × 90   = 9_180
        //   Budget = 805 + available 5_000 − floor 1_230 = 4_575.
        //   high: f = 4_575 / 9_150 = 1/2    → prod 45, lab 45
        //   low:  f = 4_575 / 9_180 = 0.498… → prod 44.85… → 44, lab 44
        let mut env_dates = EnvDateBytes::new();
        plant(&mut env_dates, "prod", &week([100; 7]));
        plant(&mut env_dates, "lab", &[(4, 1), (3, 2), (2, 2), (0, 100)]);
        let projection = project(today(), &env_dates, basis(5_000), &floored(&config, 1_230));
        assert_eq!(
            reach_of(&projection, "prod"),
            &projected(8, 7, days(44), days(45))
        );
        assert_eq!(
            reach_of(&projection, "lab"),
            &projected(4, 3, days(44), days(45))
        );
    }

    #[test]
    fn reach_keep_forever_and_floor_zero() {
        let config = retention(10, &[("archive", 0), ("vault", 0)]);
        // Keep-forever envs report growth, never a time. vault's gaps
        // inside its window count as zero: 30, 0, 60, 0 over four observed
        // days is a mean of 22.5, reported as 22 whole bytes.
        let mut env_dates = EnvDateBytes::new();
        plant(&mut env_dates, "archive", &week([0, 0, 0, 0, 0, 0, 700]));
        plant(&mut env_dates, "vault", &[(5, 30), (3, 60)]);
        let projection = project(today(), &env_dates, basis(0), &floored(&config, 1));
        assert_eq!(
            reach_of(&projection, "archive"),
            &Reach::KeepForever {
                observed_first: day(8),
                observed_last: day(2),
                observed_days: 7,
                mean_daily_bytes: 100,
            }
        );
        assert_eq!(
            reach_of(&projection, "vault"),
            &Reach::KeepForever {
                observed_first: day(5),
                observed_last: day(2),
                observed_days: 4,
                mean_daily_bytes: 22,
            }
        );

        // Keep-forever bytes crowd out finite envs: with the budget at or
        // below them, a finite env keeps 0 days.
        //   archive 10_000 reserved; prod 7 × 100 at 90 days.
        //   Budget = 10_700 + 100 − 1_000 = 9_800 < 10_000 → 0 days.
        let mut crowded = EnvDateBytes::new();
        plant(&mut crowded, "archive", &week([0, 0, 0, 0, 0, 0, 10_000]));
        plant(&mut crowded, "prod", &week([100; 7]));
        let crowded_config = retention(90, &[("archive", 0)]);
        assert_eq!(
            reach_of(
                &project(
                    today(),
                    &crowded,
                    basis(100),
                    &floored(&crowded_config, 1_000)
                ),
                "prod"
            ),
            &projected(8, 7, days(0), days(0))
        );

        // Floor 0: nothing is subtracted, and an end that does not fit is
        // the disk filling first, with no day count. prod keeps 10 days at
        // 100 a day: D = 1_000, stored 700.
        let mut steady = EnvDateBytes::new();
        plant(&mut steady, "prod", &week([100; 7]));
        let fills = project(today(), &steady, basis(299), &floored(&config, 0));
        assert_eq!(
            reach_of(&fills, "prod"),
            &projected(8, 7, ReachEnd::DiskFillsFirst, ReachEnd::DiskFillsFirst)
        );
        // 700 + 300 = 1_000 = D: the policy fits exactly.
        let fits = project(today(), &steady, basis(300), &floored(&config, 0));
        assert_eq!(
            reach_of(&fits, "prod"),
            &projected(8, 7, ReachEnd::FullPolicy, ReachEnd::FullPolicy)
        );
        // A mixed range: six days of 100 and one of 800 (mean 200, largest
        // 800) at 10 days is D_mean 2_000, D_largest 8_000. Budget
        // 1_400 + 1_000 = 2_400 fits the mean, not the largest.
        let mut bursty = EnvDateBytes::new();
        plant(
            &mut bursty,
            "prod",
            &week([100, 100, 100, 800, 100, 100, 100]),
        );
        assert_eq!(
            reach_of(
                &project(today(), &bursty, basis(1_000), &floored(&config, 0)),
                "prod"
            ),
            &projected(8, 7, ReachEnd::DiskFillsFirst, ReachEnd::FullPolicy)
        );
    }

    /// prod rated (7 × 100 at 90 days, D = 9000), fresh finite with two
    /// observed days (stored 4000), vault keep-forever with two observed
    /// days (stored 6000). The deletion floor is 200.
    fn history_fixture() -> (EnvDateBytes, RetentionConfig) {
        let mut env_dates = EnvDateBytes::new();
        plant(&mut env_dates, "prod", &week([100; 7]));
        plant(
            &mut env_dates,
            "fresh",
            &[(3, 2_000), (1, 1_000), (0, 1_000)],
        );
        plant(&mut env_dates, "vault", &[(3, 6_000)]);
        (
            env_dates,
            floored(&retention(90, &[("fresh", 30), ("vault", 0)]), 200),
        )
    }

    #[test]
    fn reach_withheld_insufficient_history_reserves_bytes() {
        let (env_dates, config) = history_fixture();
        // Reserved = fresh 4_000 + vault 6_000 = 10_000. Budget =
        // 10_700 + available 4_000 − floor 200 = 14_500, so 4_500 is left
        // for prod's D = 9_000: f = 1/2, 45 of 90 days. fresh's growth is
        // left out and named.
        let projection = project(today(), &env_dates, basis(4_000), &config);
        assert_eq!(
            reach_of(&projection, "prod"),
            &projected(8, 7, days(45), days(45))
        );
        assert_eq!(reach_of(&projection, "fresh"), &WITHHELD_HISTORY);
        assert_eq!(reach_of(&projection, "vault"), &WITHHELD_HISTORY);
        assert_eq!(projection.growth_excluded, ["fresh"]);
        let listed: Vec<_> = projection
            .environments
            .iter()
            .map(|capacity| {
                (
                    capacity.env.as_str(),
                    capacity.max_age_days,
                    capacity.oldest_date.clone(),
                    capacity.stored_bytes,
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("fresh", 30, day(3), 4_000),
                ("prod", 90, day(8), 700),
                ("vault", 0, day(3), 6_000),
            ]
        );
    }

    #[test]
    fn reach_withheld_retention_suppressed() {
        let (env_dates, config) = history_fixture();
        let complete = measured(StorageMeasurementStatus::Complete);
        let basis = projection_basis(
            &complete,
            false,
            &complete,
            Some(&headroom_sample(1 << 40, true)),
        );
        assert_eq!(basis, Err(WithheldReason::RetentionSuppressed));
        let projection = project(today(), &env_dates, basis, &config);
        // Every env is withheld, even those that would lack history; the
        // stored facts stay listed, and nothing was projected to exclude.
        for capacity in &projection.environments {
            assert_eq!(
                capacity.reach,
                Reach::Withheld {
                    reason: WithheldReason::RetentionSuppressed
                },
                "{}",
                capacity.env
            );
        }
        assert_eq!(projection.environments.len(), 3);
        assert_eq!(projection.environments[1].stored_bytes, 700);
        assert!(projection.growth_excluded.is_empty());
    }

    /// The Parquet scan and the headroom sample are separate caches with
    /// separate ages, so a scan taken during a repin can pair with an older
    /// headroom sample that saw none. Either sample's repin evidence
    /// withholds the projection.
    #[test]
    fn reach_withheld_retention_suppressed_when_either_sample_saw_a_repin() {
        let (env_dates, config) = history_fixture();
        let complete = measured(StorageMeasurementStatus::Complete);
        let quiet = headroom_sample(1 << 40, false);
        let repinning = headroom_sample(1 << 40, true);
        let suppressed = Err(WithheldReason::RetentionSuppressed);
        for (parquet_saw_repin, sample) in [(true, &quiet), (false, &repinning), (true, &repinning)]
        {
            assert_eq!(
                projection_basis(&complete, parquet_saw_repin, &complete, Some(sample)),
                suppressed,
                "parquet {parquet_saw_repin}, headroom {}",
                sample.repin_in_flight
            );
        }
        assert!(projection_basis(&complete, false, &complete, Some(&quiet)).is_ok());

        // Assembly reads the scan's own evidence, not only the headroom's.
        let capacity = assemble(
            today(),
            CapacityReadings {
                parquet: &complete,
                env_dates: Some(&env_dates),
                parquet_repin_in_flight: true,
                headroom: &complete,
                sample: Some(&quiet),
            },
            PressureEvidence {
                removals_age: 0,
                removals_disk_pressure: 0,
                pressure_attempts: 0,
                last_sweep: None,
            },
            &config,
        );
        assert_eq!(capacity.environments.len(), 3);
        for env in &capacity.environments {
            assert_eq!(
                env.reach,
                Reach::Withheld {
                    reason: WithheldReason::RetentionSuppressed
                },
                "{}",
                env.env
            );
        }
    }

    #[test]
    fn reach_withheld_measurement_unavailable_retained_failed() {
        use StorageMeasurementStatus::{Complete, Failed, NotConfigured, NotSampled};
        let sample = headroom_sample(1 << 40, false);
        let repinning = headroom_sample(1 << 40, true);
        let unavailable = Err(WithheldReason::MeasurementUnavailable);
        // A retained, failed sample of either measurement never feeds a
        // projection, and outranks a repin in flight seen by either sample.
        for (parquet, headroom, sample) in [
            (Failed, Complete, Some(&sample)),
            (Complete, Failed, Some(&sample)),
            (Failed, Failed, Some(&sample)),
            (Failed, Complete, Some(&repinning)),
            (Complete, Failed, Some(&repinning)),
            (NotSampled, Complete, Some(&sample)),
            (Complete, NotSampled, None),
            (NotConfigured, Complete, Some(&sample)),
            // Complete without a sample, or without a data row, is still
            // no measurement of the data filesystem.
            (Complete, Complete, None),
        ] {
            for parquet_saw_repin in [false, true] {
                assert_eq!(
                    projection_basis(
                        &measured(parquet),
                        parquet_saw_repin,
                        &measured(headroom),
                        sample
                    ),
                    unavailable,
                    "{parquet:?} {headroom:?} {parquet_saw_repin}"
                );
            }
        }
        let no_data_row = HeadroomSample {
            filesystems: vec![device(&[WAL], 10, 10)],
            repin_in_flight: false,
        };
        let complete = measured(Complete);
        assert_eq!(
            projection_basis(&complete, false, &complete, Some(&no_data_row)),
            unavailable
        );
        assert_eq!(
            projection_basis(&complete, false, &complete, Some(&sample)),
            Ok(ProjectionBasis {
                data_available_bytes: 1 << 40
            })
        );

        let (env_dates, config) = history_fixture();
        let projection = project(
            today(),
            &env_dates,
            projection_basis(&measured(Failed), true, &complete, Some(&repinning)),
            &config,
        );
        for capacity in &projection.environments {
            assert_eq!(
                capacity.reach,
                Reach::Withheld {
                    reason: WithheldReason::MeasurementUnavailable
                },
                "{}",
                capacity.env
            );
        }
        assert_eq!(projection.environments.len(), 3);
        assert!(projection.growth_excluded.is_empty());
    }

    /// Assembly reads the floor from the retention config for the rows and
    /// the projection alike, and lists stored facts even while withheld.
    #[test]
    fn capacity_assemble_takes_the_floor_from_config() {
        use trawl_api::{LastSweep, PressureEvidence, SweepOutcome};
        let (env_dates, config) = history_fixture();
        let complete = measured(StorageMeasurementStatus::Complete);
        let sample = headroom_sample(4_000, false);
        let pressure = PressureEvidence {
            removals_age: 1,
            removals_disk_pressure: 2,
            pressure_attempts: 3,
            last_sweep: Some(LastSweep {
                outcome: SweepOutcome::Suppressed,
                age_secs: 9,
            }),
        };
        let readings = CapacityReadings {
            parquet: &complete,
            env_dates: Some(&env_dates),
            parquet_repin_in_flight: false,
            headroom: &complete,
            sample: Some(&sample),
        };
        let capacity = assemble(today(), readings, pressure.clone(), &config);
        assert_eq!(capacity.headroom.measurement, complete);
        assert_eq!(
            capacity.headroom.filesystems,
            filesystem_rows(&sample, 200),
            "the data row's floor is the config's 200"
        );
        assert_eq!(capacity.pressure, pressure);
        // The same numbers as reach_withheld_insufficient_history_reserves_bytes,
        // whose floor is also 200.
        assert_eq!(
            capacity.environments,
            project(today(), &env_dates, basis(4_000), &config).environments
        );
        assert_eq!(capacity.growth_excluded, ["fresh"]);

        // A retained, failed Parquet scan still lists its stored facts,
        // every reach withheld; with no scan at all, nothing is listed.
        let failed = measured(StorageMeasurementStatus::Failed);
        let withheld = assemble(
            today(),
            CapacityReadings {
                parquet: &failed,
                ..readings
            },
            pressure.clone(),
            &config,
        );
        assert_eq!(withheld.environments.len(), 3);
        assert!(withheld.environments.iter().all(|env| env.reach
            == Reach::Withheld {
                reason: WithheldReason::MeasurementUnavailable
            }));
        let not_sampled = measured(StorageMeasurementStatus::NotSampled);
        let empty = assemble(
            today(),
            CapacityReadings {
                parquet: &not_sampled,
                env_dates: None,
                parquet_repin_in_flight: false,
                headroom: &not_sampled,
                sample: None,
            },
            pressure,
            &config,
        );
        assert_eq!(empty.headroom.measurement, not_sampled);
        assert!(empty.headroom.filesystems.is_empty());
        assert!(empty.environments.is_empty() && empty.growth_excluded.is_empty());
    }

    #[test]
    fn reach_withheld_serializes_without_numbers() {
        fn assert_no_number(value: &serde_json::Value) {
            match value {
                serde_json::Value::Number(number) => panic!("withheld reach carries {number}"),
                serde_json::Value::Array(values) => values.iter().for_each(assert_no_number),
                serde_json::Value::Object(fields) => fields.values().for_each(assert_no_number),
                serde_json::Value::Null
                | serde_json::Value::Bool(_)
                | serde_json::Value::String(_) => {}
            }
        }
        let (env_dates, config) = history_fixture();
        let complete = measured(StorageMeasurementStatus::Complete);
        let failed = measured(StorageMeasurementStatus::Failed);
        let bases = [
            basis(4_000),
            projection_basis(&complete, false, &complete, Some(&headroom_sample(1, true))),
            projection_basis(&failed, false, &complete, Some(&headroom_sample(1, false))),
        ];
        let mut reasons = Vec::new();
        for basis in bases {
            for capacity in project(today(), &env_dates, basis, &config).environments {
                let Reach::Withheld { reason } = capacity.reach else {
                    continue;
                };
                let value = serde_json::to_value(&capacity.reach).unwrap();
                assert_no_number(&value);
                assert_eq!(value["state"], "withheld");
                assert_eq!(value.as_object().unwrap().len(), 2, "{value}");
                reasons.push(reason);
            }
        }
        reasons.dedup();
        assert_eq!(
            reasons,
            [
                WithheldReason::InsufficientHistory,
                WithheldReason::RetentionSuppressed,
                WithheldReason::MeasurementUnavailable,
            ]
        );
    }
}
