// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Disk headroom for the filesystems trawl writes to (ADR-0042).
//!
//! Pure logic plus injected IO seams. [`sample_headroom`] reads through a
//! `stat` seam and a repin seam; nothing here reads the clock. The
//! measurement's status and age belong to the ADR-0033 cache that holds
//! the samples ([`crate::metrics`]), not to this module.

use std::path::Path;

use trawl_api::{DeletionFloor, FilesystemHeadroom, FilesystemRole};

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
/// device, and whether a repin held the data root at the time.
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
/// ([`stat_filesystem`] in production). `repin_in_flight` answers whether
/// a repin holds the data root. Any error fails the whole attempt, so the
/// cache keeps the last complete sample rather than publish a partial one
/// that would lose a device's row. `wal_dir` is `None` when ingest is off.
///
/// # Errors
/// The first `stat` error in role order, then the repin seam's error.
pub fn sample_headroom(
    data_root: &Path,
    wal_dir: Option<&Path>,
    spill_dir: &Path,
    stat: impl Fn(&Path) -> std::io::Result<(u64, u64, u64)>,
    repin_in_flight: impl FnOnce(&Path) -> std::io::Result<bool>,
) -> std::io::Result<HeadroomSample> {
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
        repin_in_flight: repin_in_flight(data_root)?,
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
        table: &[(&'static str, u64, u64, u64)],
    ) -> impl Fn(&Path) -> std::io::Result<(u64, u64, u64)> {
        let table = table.to_vec();
        move |path| {
            table
                .iter()
                .find(|(name, ..)| Path::new(name) == path)
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
        let stat = stat_table(&[
            ("/data", 1, 100, 40),
            ("/wal", 2, 50, 5),
            ("/tmp", 1, 100, 40),
        ]);
        let sample = sample_headroom(
            Path::new("/data"),
            Some(Path::new("/wal")),
            Path::new("/tmp"),
            &stat,
            |_| Ok(false),
        )
        .unwrap();
        assert_eq!(
            sample.filesystems,
            [device(&[DATA, SPILL], 100, 40), device(&[WAL], 50, 5)]
        );
        assert!(!sample.repin_in_flight);
        let query_only =
            sample_headroom(Path::new("/data"), None, Path::new("/tmp"), &stat, |_| {
                Ok(true)
            })
            .unwrap();
        assert_eq!(query_only.filesystems, [device(&[DATA, SPILL], 100, 40)]);
        assert!(query_only.repin_in_flight);

        // Any role's failure fails the whole attempt; so does the repin seam.
        assert!(
            sample_headroom(
                Path::new("/data"),
                Some(Path::new("/missing-wal")),
                Path::new("/tmp"),
                &stat,
                |_| Ok(false),
            )
            .is_err()
        );
        assert!(
            sample_headroom(Path::new("/data"), None, Path::new("/tmp"), &stat, |_| {
                Err(std::io::Error::other("unreadable"))
            })
            .is_err()
        );
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
}
