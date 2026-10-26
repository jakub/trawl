// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The mount table behind the second ground of the apartness proof
//! ([`super::hold_apart_from`], ADR-0057): Linux's `/proc/self/mountinfo`,
//! read under a fixed ceiling and checked to be procfs, the parser of the
//! one record a comparison needs, and the pure comparison of two
//! directories' paths within one filesystem.
//!
//! A bind mount keeps the device and inode of its source, so the walk over
//! parents in `scratch.rs` cannot see that a directory under one mount
//! point is a directory under another. The table can: each record names a
//! mount by the ID `statx` gives (field 1), the filesystem it is of
//! (field 3, the kernel's `major:minor`, one value for every mount of one
//! filesystem and for every subvolume of one btrfs, where `st_dev` would
//! differ), the mount's root within that filesystem (field 4) and its
//! mount point (field 5). A directory's path within its filesystem is the
//! root followed by the directory's path below the mount point, and two
//! such paths of one filesystem compare by component.
//!
//! Everything here is bytes and [`OsStr`] paths, never `str`: a mount
//! point may hold any byte but NUL, and the kernel escapes only a space,
//! a tab, a newline and a backslash.

use std::ffi::OsStr;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use super::{Overlap, ProofFault};

/// The kernel's table of this process's mounts.
pub(super) const MOUNTINFO: &str = "/proc/self/mountinfo";

/// The most of the table trawld reads; a larger table refuses. Fixed, not
/// configurable: a real table is kilobytes, and the ceiling only bounds
/// what a hostile procfs could make trawld hold.
pub(super) const TABLE_MAX_BYTES: u64 = 64 << 20;

/// What one record of the table says that the comparison needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MountRecord {
    /// Field 3: the filesystem's `major:minor`, as the kernel prints it.
    /// The only thing compared across two records.
    pub(super) filesystem: Vec<u8>,
    /// Field 4: the mount's root within the filesystem, absolute.
    pub(super) root: PathBuf,
    /// Field 5: the mount point, absolute from the process's root.
    pub(super) mount_point: PathBuf,
}

/// One side of a comparison: a directory that exists, in a mount, with
/// the components trawld would make below it. It carries no device on
/// purpose: the filesystem is the record's to say, never `st_dev`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Anchor {
    /// The ID of the mount the directory is in, as `statx` names it.
    pub(super) mount: u64,
    /// The directory's path, as `/proc/self/fd` names it: absolute from
    /// the process's root, and checked to still open the same inode.
    pub(super) path: PathBuf,
    /// The components below the directory that do not exist yet, each a
    /// plain name (no `.` or `..`); empty when the compared directory is
    /// the one at `path`.
    pub(super) tail: PathBuf,
}

/// The table, as read: its bytes, parsed one record at a time on demand so
/// that a malformed record the comparison does not need refuses nothing.
#[derive(Debug)]
pub(super) struct MountTable {
    bytes: Vec<u8>,
}

impl MountTable {
    /// Read the table from the kernel: open [`MOUNTINFO`], refuse it unless
    /// `fstatfs` says procfs, and read at most [`TABLE_MAX_BYTES`].
    ///
    /// Every descriptor whose mount the caller will look up must be open
    /// before this is called and stay open until the lookups are done: an
    /// open descriptor pins its mount, so the mount's ID cannot be freed
    /// and reused by another mount between the read and the lookup.
    pub(super) fn read() -> Result<Self, ProofFault> {
        #[cfg(test)]
        if let Some(table) = TABLE_SOURCE.with_borrow(|source| match source {
            TableSource::Proc => None,
            TableSource::Path(path) => Some(Self::read_file(path)),
            TableSource::Reader(make) => Some(Self::read_from(make())),
            TableSource::Unreachable => panic!("the mount table was read"),
        }) {
            return table;
        }
        Self::read_file(Path::new(MOUNTINFO))
    }

    /// [`Self::read`] from the file at `path`, which must be on procfs.
    fn read_file(path: &Path) -> Result<Self, ProofFault> {
        let file = std::fs::File::open(path).map_err(ProofFault::TableUnreadable)?;
        let statfs =
            rustix::fs::fstatfs(&file).map_err(|e| ProofFault::TableUnreadable(e.into()))?;
        if statfs.f_type != rustix::fs::PROC_SUPER_MAGIC {
            return Err(ProofFault::TableNotProcfs);
        }
        Self::read_from(file)
    }

    /// [`Self::read`] from `reader`, already known to be the table: at most
    /// [`TABLE_MAX_BYTES`], read through a limit one byte wider so that
    /// a larger table is told from one exactly at the ceiling. procfs
    /// reports every file as empty, so the size cannot be asked first.
    fn read_from(reader: impl std::io::Read) -> Result<Self, ProofFault> {
        let mut bytes = Vec::new();
        reader
            .take(TABLE_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(ProofFault::TableUnreadable)?;
        if bytes.len() as u64 > TABLE_MAX_BYTES {
            return Err(ProofFault::TableTooLarge);
        }
        Ok(Self { bytes })
    }

    /// The one record of mount `id`. Only field 1 of every line is read to
    /// find it; none refuses as [`ProofFault::NoRecord`], more than one as
    /// [`ProofFault::DuplicateRecord`], and only then is the one line
    /// parsed in full, refusing as [`ProofFault::Malformed`].
    pub(super) fn record(&self, id: u64) -> Result<MountRecord, ProofFault> {
        let mut found: Option<&[u8]> = None;
        for line in self.bytes.split(|&b| b == b'\n') {
            if line.is_empty() || field_id(line) != Some(id) {
                continue;
            }
            if found.is_some() {
                return Err(ProofFault::DuplicateRecord(id));
            }
            found = Some(line);
        }
        let line = found.ok_or(ProofFault::NoRecord(id))?;
        parse_record(line).map_err(|why| ProofFault::Malformed { id, why })
    }
}

/// Field 1 of `line`, when it is a number.
fn field_id(line: &[u8]) -> Option<u64> {
    parse_u64(line.split(|&b| b == b' ').next()?)
}

/// `digits` as the kernel prints a `u64`: ASCII digits only, at least one.
fn parse_u64(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Parse one line of the table. The format (`proc_mountinfo` in the
/// kernel, `Documentation/filesystems/proc.rst`) is six space-separated
/// fields, zero or more optional fields, a lone `-`, then the filesystem
/// type, source and super options, which the comparison does not need and
/// so does not check. A space, tab, newline or backslash in fields 4 and 5
/// is printed as `\040`, `\011`, `\012` or `\134`; the kernel escapes
/// nothing else, so any other escape is refused rather than guessed.
fn parse_record(line: &[u8]) -> Result<MountRecord, &'static str> {
    const FEWER: &str = "fewer than six fields";
    let mut fields = line.split(|&b| b == b' ');
    parse_u64(fields.next().ok_or(FEWER)?).ok_or("its mount ID is not a number")?;
    fields.next().ok_or(FEWER)?;
    let filesystem = fields.next().ok_or(FEWER)?;
    if !is_device(filesystem) {
        return Err("its device is not major:minor");
    }
    let root = path_field(fields.next().ok_or(FEWER)?, &ROOT)?;
    let mount_point = path_field(fields.next().ok_or(FEWER)?, &MOUNT_POINT)?;
    fields.next().ok_or(FEWER)?;
    if !fields.any(|field| field == b"-") {
        return Err("no lone - separates its optional fields");
    }
    Ok(MountRecord {
        filesystem: filesystem.to_vec(),
        root,
        mount_point,
    })
}

/// Whether `field` reads `major:minor`, as the kernel prints field 3.
fn is_device(field: &[u8]) -> bool {
    field
        .split(|&b| b == b':')
        .map(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
        .fold((0, true), |(n, ok), part| (n + 1, ok && part))
        == (2, true)
}

/// What is wrong with a path field, named for its field.
struct PathFault {
    escape: &'static str,
    relative: &'static str,
    unclean: &'static str,
}

/// Faults of field 4.
const ROOT: PathFault = PathFault {
    escape: "its root has an escape other than \\040, \\011, \\012 or \\134",
    relative: "its root is not an absolute path",
    unclean: "its root has a repeated /, a trailing /, or a . or .. component",
};

/// Faults of field 5.
const MOUNT_POINT: PathFault = PathFault {
    escape: "its mount point has an escape other than \\040, \\011, \\012 or \\134",
    relative: "its mount point is not an absolute path",
    unclean: "its mount point has a repeated /, a trailing /, or a . or .. component",
};

/// Field 4 or 5: unescaped, and checked to be the kernel's spelling of an
/// absolute path, one `/` between plain names. The kernel prints a mount
/// root whose directory was removed as `<path>//deleted`, which a
/// component walk would read as a directory named `deleted`; the repeated
/// `/` refuses it, as it refuses any other spelling the kernel does not
/// print.
fn path_field(field: &[u8], fault: &PathFault) -> Result<PathBuf, &'static str> {
    let bytes = unescape(field).ok_or(fault.escape)?;
    let Some(rest) = bytes.strip_prefix(b"/") else {
        return Err(fault.relative);
    };
    let clean = rest.is_empty()
        || rest
            .split(|&b| b == b'/')
            .all(|name| !name.is_empty() && name != b"." && name != b"..");
    if !clean {
        return Err(fault.unclean);
    }
    Ok(PathBuf::from(OsStr::from_bytes(&bytes)))
}

/// `field` with the kernel's four escapes decoded, or `None` at any other
/// backslash.
fn unescape(field: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some((&byte, after)) = rest.split_first() {
        if byte != b'\\' {
            out.push(byte);
            rest = after;
            continue;
        }
        let (escape, after) = after.split_at_checked(3)?;
        out.push(match escape {
            b"040" => b' ',
            b"011" => b'\t',
            b"012" => b'\n',
            b"134" => b'\\',
            _ => return None,
        });
        rest = after;
    }
    Some(out)
}

/// The path within its filesystem of the directory `anchor` describes,
/// given its mount's `record`: the mount's root, then the directory's path
/// below the mount point, then the components still to be made. Refuses
/// when the path does not start with the mount point, which a record of
/// another mount, or a path from outside the process's root, would show.
pub(super) fn within_filesystem(
    anchor: &Anchor,
    record: &MountRecord,
) -> Result<PathBuf, ProofFault> {
    let below =
        anchor
            .path
            .strip_prefix(&record.mount_point)
            .map_err(|_| ProofFault::OutsideMount {
                path: anchor.path.clone(),
                mount_point: record.mount_point.clone(),
            })?;
    let mut path = record.root.clone();
    path.extend(below.components());
    path.extend(anchor.tail.components());
    Ok(path)
}

/// How the directory `held` overlaps the directory `root` through their
/// mounts, by `table`: `None` when their mounts are of different
/// filesystems, which cannot alias, or their paths within the one
/// filesystem are apart; else the overlap and the mount point that exposed
/// it. Of the two mounts, the one whose root within the filesystem is
/// deeper is the alias of the other's interior, so its mount point is
/// named; with equal roots, the held directory's.
pub(super) fn alias(
    held: &Anchor,
    root: &Anchor,
    table: &MountTable,
) -> Result<Option<(Overlap, PathBuf)>, ProofFault> {
    let held_record = table.record(held.mount)?;
    let root_record = table.record(root.mount)?;
    if held_record.filesystem != root_record.filesystem {
        return Ok(None);
    }
    let held_path = within_filesystem(held, &held_record)?;
    let root_path = within_filesystem(root, &root_record)?;
    let overlap = if held_path == root_path {
        Overlap::Is
    } else if held_path.starts_with(&root_path) {
        Overlap::Inside
    } else if root_path.starts_with(&held_path) {
        Overlap::Above
    } else {
        return Ok(None);
    };
    let depth = |record: &MountRecord| record.root.components().count();
    let through = if depth(&root_record) > depth(&held_record) {
        root_record.mount_point
    } else {
        held_record.mount_point
    };
    Ok(Some((overlap, through)))
}

/// Where [`MountTable::read`] reads from on this thread, for tests that
/// stand in for a kernel or a procfs that answers differently.
#[cfg(test)]
pub(super) enum TableSource {
    /// The kernel's table, as in production.
    Proc,
    /// The real open and procfs check, on the file at this path.
    Path(PathBuf),
    /// Bytes from this reader, past the open and the procfs check; the
    /// ceiling and the parser still apply.
    Reader(Box<dyn Fn() -> Box<dyn std::io::Read>>),
    /// Reading the table is a test failure.
    Unreachable,
}

#[cfg(test)]
impl TableSource {
    /// A table of these bytes.
    pub(super) fn bytes(table: impl Into<Vec<u8>>) -> Self {
        let table = table.into();
        Self::Reader(Box::new(move || {
            Box::new(std::io::Cursor::new(table.clone()))
        }))
    }

    /// A table one byte over the ceiling.
    pub(super) fn too_large() -> Self {
        Self::Reader(Box::new(|| {
            Box::new(std::io::repeat(b'x').take(TABLE_MAX_BYTES + 1))
        }))
    }
}

#[cfg(test)]
thread_local! {
    /// See [`TableSource`].
    pub(super) static TABLE_SOURCE: std::cell::RefCell<TableSource> =
        const { std::cell::RefCell::new(TableSource::Proc) };
}

/// Read the table from `source` for the rest of this test's thread.
#[cfg(test)]
pub(super) fn set_table_source(source: TableSource) {
    TABLE_SOURCE.set(source);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record as the kernel prints one, with the given optional fields.
    fn line(id: u64, device: &str, root: &str, mount_point: &str, optional: &str) -> String {
        format!("{id} 1 {device} {root} {mount_point} rw,relatime {optional}- ext4 /dev/sda1 rw\n")
    }

    fn table(lines: &[String]) -> MountTable {
        MountTable {
            bytes: lines.concat().into_bytes(),
        }
    }

    fn record(device: &str, root: &str, mount_point: &str) -> MountRecord {
        MountRecord {
            filesystem: device.as_bytes().to_vec(),
            root: PathBuf::from(root),
            mount_point: PathBuf::from(mount_point),
        }
    }

    fn anchor(mount: u64, path: &str, tail: &str) -> Anchor {
        Anchor {
            mount,
            path: PathBuf::from(path),
            tail: PathBuf::from(tail),
        }
    }

    /// The parser decodes exactly the kernel's four escapes, in both path
    /// fields, and skips any number of optional fields.
    #[test]
    fn a_record_parses_with_escapes_and_optional_fields() {
        type Case<'a> = (&'a str, &'a str, &'a str, &'a [u8], &'a [u8]);
        let cases: [Case<'_>; 4] = [
            ("/", "/", "", b"/", b"/"),
            (
                "/a\\040b/c\\011d",
                "/mnt/x\\012y\\134z",
                "",
                b"/a b/c\td",
                b"/mnt/x\ny\\z",
            ),
            ("/@/home", "/home", "shared:5 ", b"/@/home", b"/home"),
            (
                "/data/prod",
                "/srv/results\\040alias",
                "shared:12 master:1 propagate_from:3 ",
                b"/data/prod",
                b"/srv/results alias",
            ),
        ];
        for (root, mount_point, optional, want_root, want_mount_point) in cases {
            let text = line(42, "0:29", root, mount_point, optional);
            let parsed = parse_record(text.trim_end().as_bytes()).unwrap_or_else(|why| {
                panic!("{text:?}: {why}");
            });
            assert_eq!(field_id(text.as_bytes()), Some(42), "{text:?}");
            assert_eq!(parsed.filesystem, b"0:29", "{text:?}");
            assert_eq!(parsed.root.as_os_str().as_bytes(), want_root, "{text:?}");
            assert_eq!(
                parsed.mount_point.as_os_str().as_bytes(),
                want_mount_point,
                "{text:?}"
            );
        }
    }

    /// Each malformation the comparison must not guess at refuses with a
    /// reason, and the reason reads as the refusal prints it.
    #[test]
    fn a_malformed_record_refuses_with_its_reason() {
        let cases: [(&str, &str); 14] = [
            ("42 1 0:29 / /mnt", "fewer than six fields"),
            (
                "42 1 0:29 / /mnt rw",
                "no lone - separates its optional fields",
            ),
            (
                "42 1 0:29 / /mnt rw shared:5 ext4 /dev/sda1 rw",
                "no lone - separates its optional fields",
            ),
            (
                "x 1 0:29 / /mnt rw - ext4 /dev/sda1 rw",
                "its mount ID is not a number",
            ),
            (
                "42 1 29 / /mnt rw - ext4 /dev/sda1 rw",
                "its device is not major:minor",
            ),
            (
                "42 1 0:29:1 / /mnt rw - ext4 /dev/sda1 rw",
                "its device is not major:minor",
            ),
            (
                "42 1 0:29 /a\\101b /mnt rw - ext4 /dev/sda1 rw",
                "its root has an escape other than \\040, \\011, \\012 or \\134",
            ),
            (
                "42 1 0:29 / /mnt\\04 rw - ext4 /dev/sda1 rw",
                "its mount point has an escape other than \\040, \\011, \\012 or \\134",
            ),
            (
                "42 1 0:29 / /mnt\\ rw - ext4 /dev/sda1 rw",
                "its mount point has an escape other than \\040, \\011, \\012 or \\134",
            ),
            (
                "42 1 0:29 data /mnt rw - ext4 /dev/sda1 rw",
                "its root is not an absolute path",
            ),
            (
                "42 1 0:29 / mnt rw - ext4 /dev/sda1 rw",
                "its mount point is not an absolute path",
            ),
            (
                "42 1 0:29 /data//deleted /mnt rw - ext4 /dev/sda1 rw",
                "its root has a repeated /, a trailing /, or a . or .. component",
            ),
            (
                "42 1 0:29 /data/../x /mnt rw - ext4 /dev/sda1 rw",
                "its root has a repeated /, a trailing /, or a . or .. component",
            ),
            (
                "42 1 0:29 / /mnt/ rw - ext4 /dev/sda1 rw",
                "its mount point has a repeated /, a trailing /, or a . or .. component",
            ),
        ];
        for (text, why) in cases {
            assert_eq!(parse_record(text.as_bytes()), Err(why), "{text:?}");
        }
    }

    /// `record` finds exactly one line by its first field, parses only
    /// that line, and refuses none or more than one.
    #[test]
    fn a_mount_has_exactly_one_record() {
        let table = table(&[
            line(26, "0:24", "/", "/proc", ""),
            "this line is not a record at all\n".to_owned(),
            line(31, "0:29", "/@", "/", "shared:1 "),
            "57 31 0:29 /@home /home rw,relatime shared:2 missing the separator\n".to_owned(),
            line(60, "0:29", "/@/srv/data/prod", "/srv/results", ""),
            line(60, "0:29", "/@/srv/data/prod", "/srv/results", ""),
            "\n".to_owned(),
        ]);
        assert_eq!(
            table.record(31).expect("one record"),
            record("0:29", "/@", "/")
        );
        assert_eq!(
            table.record(26).expect("one record"),
            record("0:24", "/", "/proc")
        );
        assert!(
            matches!(table.record(99), Err(ProofFault::NoRecord(99))),
            "no record"
        );
        assert!(
            matches!(table.record(60), Err(ProofFault::DuplicateRecord(60))),
            "two records"
        );
        assert!(
            matches!(
                table.record(57),
                Err(ProofFault::Malformed {
                    id: 57,
                    why: "no lone - separates its optional fields"
                })
            ),
            "the malformed record itself"
        );
        assert!(
            matches!(
                MountTable { bytes: Vec::new() }.record(31),
                Err(ProofFault::NoRecord(31))
            ),
            "an empty table"
        );
    }

    /// The path within the filesystem is the root, the path below the
    /// mount point (empty when the directory is the mount point, the whole
    /// path when the mount point is `/`), then the tail; a path outside
    /// the mount point refuses.
    #[test]
    fn a_path_within_the_filesystem_joins_root_below_and_tail() {
        let cases: [(Anchor, MountRecord, &str); 5] = [
            (anchor(31, "/etc", ""), record("0:29", "/@", "/"), "/@/etc"),
            (
                anchor(60, "/srv/results", ""),
                record("0:29", "/@/srv/data/prod", "/srv/results"),
                "/@/srv/data/prod",
            ),
            (
                anchor(60, "/srv/results/new/env", ""),
                record("0:29", "/@/srv/data/prod", "/srv/results"),
                "/@/srv/data/prod/new/env",
            ),
            (
                anchor(60, "/srv/results", "new/env"),
                record("0:29", "/", "/srv/results"),
                "/new/env",
            ),
            (
                anchor(31, "/", "srv/data"),
                record("0:29", "/", "/"),
                "/srv/data",
            ),
        ];
        for (anchor, record, want) in cases {
            let path = within_filesystem(&anchor, &record).expect("below the mount point");
            assert_eq!(path, Path::new(want), "{anchor:?}");
        }

        let outside = within_filesystem(
            &anchor(60, "/srv/results-2", ""),
            &record("0:29", "/@/srv/data/prod", "/srv/results"),
        )
        .expect_err("not below the mount point");
        assert!(
            matches!(
                &outside,
                ProofFault::OutsideMount { path, mount_point }
                    if path == Path::new("/srv/results-2") && mount_point == Path::new("/srv/results")
            ),
            "{outside}"
        );
    }

    /// The relation of two directories through their mounts: equal paths
    /// within one filesystem are `Is`, a prefix is `Inside` or `Above`,
    /// siblings and different filesystems are apart, and the mount point
    /// named is the one whose root is deeper, the held side's on a tie.
    #[test]
    fn an_alias_is_judged_by_paths_within_one_filesystem() {
        type Case<'a> = (&'a str, Anchor, Anchor, Option<(Overlap, &'a str)>);
        let table = table(&[
            line(31, "0:29", "/@", "/", ""),
            line(
                60,
                "0:29",
                "/@/srv/data/prod/2024-01-16/10",
                "/srv/results",
                "",
            ),
            line(61, "0:29", "/@/srv/real/data/prod", "/srv/data", ""),
            line(62, "0:29", "/@/srv/a", "/mnt/a", ""),
            line(63, "0:29", "/@/srv/b", "/mnt/b", ""),
            line(70, "0:41", "/", "/elsewhere", ""),
            line(71, "0:29", "/@/srv/data", "/srv/data-twice", ""),
        ]);
        let cases: [Case<'_>; 9] = [
            (
                "a results root bound from inside the data root",
                anchor(60, "/srv/results", ""),
                anchor(31, "/srv/data", ""),
                Some((Overlap::Inside, "/srv/results")),
            ),
            (
                "a missing results root below that bind",
                anchor(60, "/srv/results", "new"),
                anchor(31, "/srv/data", ""),
                Some((Overlap::Inside, "/srv/results")),
            ),
            (
                "a data root bound from inside a plain results root",
                anchor(31, "/srv/real/data", ""),
                anchor(61, "/srv/data", ""),
                Some((Overlap::Above, "/srv/data")),
            ),
            (
                "a results root bound from the data root itself",
                anchor(71, "/srv/data-twice", ""),
                anchor(31, "/srv/data", ""),
                Some((Overlap::Is, "/srv/data-twice")),
            ),
            (
                "a missing data root the held root would hold, in one mount, named on the tie",
                anchor(31, "/srv", ""),
                anchor(31, "/srv", "data"),
                Some((Overlap::Above, "/")),
            ),
            (
                "two binds of sibling directories",
                anchor(62, "/mnt/a", ""),
                anchor(63, "/mnt/b", ""),
                None,
            ),
            (
                "a bind of a sibling beside the data root",
                anchor(62, "/mnt/a", ""),
                anchor(31, "/srv/data", ""),
                None,
            ),
            (
                "another filesystem, whatever the paths say",
                anchor(70, "/elsewhere", ""),
                anchor(31, "/", ""),
                None,
            ),
            (
                "the held root above a bind of a data interior, named by the deeper root",
                anchor(31, "/srv", ""),
                anchor(60, "/srv/results", ""),
                Some((Overlap::Above, "/srv/results")),
            ),
        ];
        for (case, held, root, want) in cases {
            let got = alias(&held, &root, &table).unwrap_or_else(|fault| panic!("{case}: {fault}"));
            let want = want.map(|(overlap, through)| (overlap, PathBuf::from(through)));
            assert_eq!(got, want, "{case}");
        }
        let fault = alias(&anchor(99, "/x", ""), &anchor(31, "/srv/data", ""), &table)
            .expect_err("no record");
        assert!(matches!(fault, ProofFault::NoRecord(99)), "{fault}");
    }

    /// The ceiling: a table exactly at it reads, one byte over refuses.
    #[test]
    fn a_table_over_the_ceiling_refuses() {
        let at = MountTable::read_from(std::io::repeat(b'x').take(TABLE_MAX_BYTES))
            .expect("exactly at the ceiling");
        assert_eq!(at.bytes.len() as u64, TABLE_MAX_BYTES);
        let over = MountTable::read_from(std::io::repeat(b'x').take(TABLE_MAX_BYTES + 1))
            .expect_err("over the ceiling");
        assert!(matches!(over, ProofFault::TableTooLarge), "{over}");
    }

    /// The reader checks procfs before it reads a byte: a missing file
    /// is unreadable, a file on another filesystem is not the table, and
    /// the kernel's own table reads and names this process's root mount.
    #[test]
    fn the_reader_accepts_only_procfs() {
        let missing =
            MountTable::read_file(Path::new("/proc/self/no-such-table")).expect_err("missing");
        assert!(
            matches!(missing, ProofFault::TableUnreadable(_)),
            "{missing}"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let copy = dir.path().join("mountinfo");
        std::fs::copy(MOUNTINFO, &copy).expect("copy the table");
        let not_procfs = MountTable::read_file(&copy).expect_err("a copy is not the table");
        assert!(
            matches!(not_procfs, ProofFault::TableNotProcfs),
            "{not_procfs}"
        );

        let table = MountTable::read().expect("the kernel's table");
        let root = std::fs::File::open("/").expect("open /");
        let stat = rustix::fs::statx(
            &root,
            c"",
            rustix::fs::AtFlags::EMPTY_PATH,
            rustix::fs::StatxFlags::MNT_ID,
        )
        .expect("statx");
        assert!(
            stat.stx_mask & rustix::fs::StatxFlags::MNT_ID.bits() != 0,
            "this kernel names no mount"
        );
        let record = table
            .record(stat.stx_mnt_id)
            .expect("the root mount's record");
        assert_eq!(record.mount_point, Path::new("/"));
    }
}
