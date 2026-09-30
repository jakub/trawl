// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `trawld --doctor` checks of the data root and its recovery markers.
//!
//! Every check asks the question boot's gate asks, through the classifier
//! boot itself calls, so the two cannot drift: the storage epoch through
//! [`epoch::classify_data_root`], catalog identity through
//! [`conform::judge_archive_identity`], conformance through
//! [`conform::conformance_recorded`], the repin marker through
//! [`marker::parse_marker`] and [`marker::RepinPhase::query_only_refuses`],
//! and the publication and rollup markers through the decoders boot's
//! recovery reads them with. Where boot reads a small file its own way, the
//! doctor injects [`super::fsread`] instead, so a FIFO, a symlink or an
//! oversized file under the data root costs a row, never the run.
//!
//! Nothing under the data root is created, renamed, or removed. The group
//! reads [`Ctx::app`], which the database group filled, and no row names
//! the data root, the WAL directory, a marker's content or a catalog
//! identifier: rows say what boot will do, in fixed sentences.

use std::path::{Path, PathBuf};
use std::time::Duration;

use trawl_api::doctor::reason;

use super::fsread::{self, Links, ReadFault};
use super::output::{Row, Text};
use super::{Ctx, Runner, ServerCheck};
use crate::catalog::conform::{self, IdentityJudgement};
use crate::epoch::{self, DataRootState, RootFault, RootReader};
use crate::ingest::{compaction, publication_marker};
use crate::repin::marker;

/// This group's checks, in [`ServerCheck::ALL`] order.
pub(super) const CHECKS: [ServerCheck; 6] = [
    ServerCheck::DataRoot,
    ServerCheck::DataEpoch,
    ServerCheck::DataIdentity,
    ServerCheck::DataConformance,
    ServerCheck::RecoveryRepin,
    ServerCheck::RecoveryPublication,
];

/// How long one look at the data tree may take: a stat, a listing, a few
/// small reads, or the parquet walk catalog identity makes when the marker
/// does not prove the pairing. Each file read inside it has its own
/// [`fsread::READ_DEADLINE`].
const LOOK_DEADLINE: Duration = Duration::from_secs(10);

/// What a data-root check's source names.
fn data_source(ctx: &Ctx) -> Text {
    Text::new("[data] path in ").path(&ctx.config_path)
}

/// Where the settings of every storage check come from.
fn storage_source(ctx: &Ctx) -> Text {
    Text::new("[data] path and [ingest] in ").path(&ctx.config_path)
}

/// Run this group's checks through `runner`, in [`CHECKS`] order. A check
/// the runner blocks never looks.
pub(super) async fn run(ctx: &mut Ctx, runner: &mut Runner) {
    let data_root = ctx.config.data.base_dir();
    let wal_dir = ctx.config.wal_dir();
    let ingest = ctx.config.ingest.enabled;
    // The `CATALOG` marker `server.data.identity` read, for
    // `server.data.conformance`, which waits on it.
    let mut marker = None;
    for check in CHECKS {
        let Some(gate) = runner.gate(check) else {
            continue;
        };
        let row = match check {
            ServerCheck::DataRoot => {
                check_root(data_root.clone(), wal_dir.clone(), ingest, !ctx.run_as.root)
                    .await
                    .source(storage_source(ctx))
            }
            ServerCheck::DataEpoch => check_epoch(data_root.clone(), wal_dir.clone(), ingest)
                .await
                .source(storage_source(ctx)),
            ServerCheck::DataIdentity => {
                let catalog_id = ctx.app.catalog.as_ref().map(|c| c.catalog_id.0.clone());
                let (row, read) = check_identity(data_root.clone(), catalog_id, ingest).await;
                marker = read;
                row.source(data_source(ctx))
            }
            ServerCheck::DataConformance => {
                check_conformance(ctx, ingest, marker.as_deref()).source(storage_source(ctx))
            }
            ServerCheck::RecoveryRepin => check_repin(data_root.clone(), ingest)
                .await
                .source(storage_source(ctx)),
            ServerCheck::RecoveryPublication => {
                check_markers(data_root.clone(), wal_dir.clone(), ingest)
                    .await
                    .source(storage_source(ctx))
            }
            other => unreachable!("{other:?} is not a storage check"),
        };
        runner.record(gate, row);
    }
}

/// Why a look at the data tree gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Missed {
    /// It ran past [`LOOK_DEADLINE`].
    TimedOut,
    /// Its task did not finish.
    Lost,
}

/// Run `look` on the blocking pool under [`LOOK_DEADLINE`]. A look past the
/// deadline is left behind on its thread; the doctor's runtime shuts down
/// with a bounded wait, so it cannot hold the process.
async fn look<T: Send + 'static>(look: impl FnOnce() -> T + Send + 'static) -> Result<T, Missed> {
    let task = tokio::task::spawn_blocking(look);
    match tokio::time::timeout(LOOK_DEADLINE, task).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(Missed::Lost),
        Err(_) => Err(Missed::TimedOut),
    }
}

/// The row of a check whose look gave no answer.
fn missed(check: ServerCheck, missed: Missed) -> Row {
    match missed {
        Missed::TimedOut => Row::not_sampled(check, reason::TIMED_OUT),
        Missed::Lost => Row::not_sampled(check, reason::UNREADABLE),
    }
}

// ---------------------------------------------------------------------------
// server.data.root
// ---------------------------------------------------------------------------

/// What the running user may do with a directory, from `accessat` with the
/// effective ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    /// Everything asked for.
    Granted,
    /// Reading or searching it is denied.
    NoRead,
    /// Writing it is denied.
    NoWrite,
    /// It is on a read-only filesystem.
    ReadOnlyFs,
    /// The question itself failed.
    Unknown,
    /// Not asked: the doctor runs as root, whose answer says nothing about
    /// the service user's, so no `accessat` ran and no answer, not even
    /// a read-only filesystem's, was consulted.
    NotAsked,
}

/// What `server.data.root` saw at the configured path, or what
/// [`observe_created`] saw at another directory boot creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RootSeen {
    /// A directory, and what the running user may do with it.
    Directory(Access),
    /// Nothing. For an ingest node, what the running user may do with the
    /// nearest directory that exists above it, where boot creates it.
    Absent(Option<Access>),
    /// Something that is not a directory.
    NotADirectory,
    /// A directory above it is not a directory.
    AncestorNotADirectory,
    /// A directory above it is a symlink to nothing, which boot's create
    /// path meets as an existing entry and refuses.
    AncestorDangling,
    /// A symlink to nothing.
    Dangling,
    /// Something that was there at the first look and gone at the second.
    Vanished,
    /// The running user may not look up the path.
    Denied,
    /// Inspecting it failed otherwise.
    Unreadable,
}

impl RootSeen {
    /// Whether what was seen is one boot uses or creates, as far as the
    /// doctor asked: access not asked about holds.
    pub(super) const fn holds(self) -> bool {
        matches!(
            self,
            Self::Directory(Access::Granted | Access::NotAsked)
                | Self::Absent(None | Some(Access::Granted | Access::NotAsked))
        )
    }
}

/// `server.data.root`: the root is a directory, or boot creates it, and the
/// running user may read it and, on an ingest node, write it. On an ingest
/// node the same holds for the effective WAL directory, which boot creates
/// when it is absent, wherever it is: a path under the data root may leave
/// it through `..` or a symlink, and access to the root proves nothing
/// about a child that exists. The row reports the root's failure first and
/// looks at the WAL directory only when the root holds. The only access
/// check. Unless `ask_access`, as in a root run, only the structural half
/// is evaluated: each directory is a directory, or absent below a directory
/// boot could create it in. When that holds the row is `not_sampled`,
/// reason `ran_as_root`, and the checks that wait on it still look.
async fn check_root(data_root: PathBuf, wal_dir: PathBuf, ingest: bool, ask_access: bool) -> Row {
    let check = ServerCheck::DataRoot;
    let wal_dir = ingest.then_some(wal_dir);
    let observed = look(move || {
        let root = observe_root(&data_root, ingest, ask_access);
        let wal = wal_dir
            .filter(|_| root.holds())
            .map(|wal_dir| observe_root(&wal_dir, true, ask_access));
        (root, wal)
    })
    .await;
    match observed {
        Ok((root, wal)) => wal
            .and_then(wal_row)
            .unwrap_or_else(|| root_row(root, ingest)),
        Err(why) => missed(check, why),
    }
}

/// The `server.data.root` row for what was seen at the WAL directory, or
/// `None` when it [holds](RootSeen::holds). Boot creates
/// it as it creates the data root, so it is judged the same way: R, W and
/// X on it, or W and X on the nearest directory above it.
fn wal_row(seen: RootSeen) -> Option<Row> {
    let check = ServerCheck::DataRoot;
    let access = || Text::new("give the service user access to the WAL directory, or run as it");
    let point = || Text::new("point [ingest] wal_dir at a directory trawld can create");
    let row = match seen {
        seen if seen.holds() => return None,
        RootSeen::Directory(Access::NoRead) => {
            Row::failed(check, "the running user cannot read the WAL directory").next(access())
        }
        RootSeen::Directory(Access::NoWrite) => {
            Row::failed(check, "the running user cannot write the WAL directory").next(access())
        }
        RootSeen::Absent(Some(Access::NoRead | Access::NoWrite)) => Row::failed(
            check,
            "the running user cannot create the WAL directory in the directory above it",
        )
        .next(access()),
        RootSeen::Directory(Access::ReadOnlyFs) | RootSeen::Absent(Some(Access::ReadOnlyFs)) => {
            Row::failed(check, "the WAL directory is on a read-only filesystem").next(Text::new(
                "an ingest node writes its WAL directory: mount it read-write, or disable ingest",
            ))
        }
        RootSeen::NotADirectory => Row::failed(check, "the WAL directory is not a directory")
            .next(Text::new("point [ingest] wal_dir at a directory")),
        RootSeen::AncestorNotADirectory => {
            Row::failed(check, "a parent of the WAL directory is not a directory").next(point())
        }
        RootSeen::AncestorDangling => Row::failed(
            check,
            "a parent of the WAL directory is a symlink to nothing",
        )
        .next(point()),
        RootSeen::Dangling => Row::failed(check, "the WAL directory is a symlink to nothing")
            .next(Text::new("point [ingest] wal_dir at a directory")),
        RootSeen::Denied => Row::failed(check, "the running user cannot reach the WAL directory")
            .next(Text::new(
                "give the service user search access to every directory above the WAL directory",
            )),
        RootSeen::Vanished => Row::not_sampled(check, reason::MATERIAL_CHANGED)
            .next(Text::new("rerun once the WAL directory stops changing")),
        RootSeen::Directory(_) | RootSeen::Absent(_) | RootSeen::Unreadable => {
            Row::not_sampled(check, reason::UNREADABLE)
        }
    };
    Some(row)
}

/// The `server.data.root` row for what was seen at the configured path.
fn root_row(seen: RootSeen, ingest: bool) -> Row {
    let check = ServerCheck::DataRoot;
    let detail = |text: &'static str| Text::new(text);
    match seen {
        RootSeen::Directory(Access::Granted) => Row::complete(check).detail(detail(if ingest {
            "a directory the running user may read and write"
        } else {
            "a directory the running user may read"
        })),
        RootSeen::Absent(None) => Row::complete(check).detail(detail(
            "absent: this query-only node serves an empty archive",
        )),
        RootSeen::Absent(Some(Access::Granted)) => {
            Row::complete_because(check, reason::WILL_INITIALIZE).detail(detail(
                "absent: trawld creates it at its next start, and the running user may \
                 create it",
            ))
        }
        RootSeen::Directory(Access::NotAsked) => not_asked(check, "a directory"),
        RootSeen::Absent(Some(Access::NotAsked)) => not_asked(
            check,
            "absent below a directory: trawld creates it at its next start",
        ),
        RootSeen::Directory(access) => access_row(check, access, false),
        RootSeen::Absent(Some(access)) => access_row(check, access, true),
        RootSeen::NotADirectory => Row::failed(check, "the data root is not a directory")
            .next(detail("point [data] path at a directory")),
        RootSeen::AncestorNotADirectory => {
            Row::failed(check, "a parent of the data root is not a directory")
                .next(detail("point [data] path at a directory trawld can create"))
        }
        RootSeen::AncestorDangling => {
            Row::failed(check, "a parent of the data root is a symlink to nothing")
                .next(detail("point [data] path at a directory trawld can create"))
        }
        RootSeen::Dangling => Row::failed(check, "the data root is a symlink to nothing")
            .next(detail("point [data] path at a directory")),
        RootSeen::Denied => Row::failed(check, "the running user cannot reach the data root").next(
            detail("give the service user search access to every directory above the data root"),
        ),
        RootSeen::Vanished => Row::not_sampled(check, reason::MATERIAL_CHANGED)
            .next(detail("rerun once the data root stops changing")),
        RootSeen::Unreadable => Row::not_sampled(check, reason::UNREADABLE),
    }
}

/// The row of a root whose structure holds and whose access was not asked
/// about, because a root run's access proves nothing about the service
/// user's.
fn not_asked(check: ServerCheck, seen: &'static str) -> Row {
    Row::not_sampled(check, reason::RAN_AS_ROOT)
        .detail(Text::new(seen))
        .next(Text::new(
            "rerun as the service user to check what it can read and write",
        ))
}

/// The row for access the running user lacks, at the root or, when
/// `absent`, at the directory boot would create it in.
fn access_row(check: ServerCheck, access: Access, absent: bool) -> Row {
    let next = Text::new("give the service user access to the data root, or run as it");
    match (access, absent) {
        (Access::Granted, _) => Row::complete(check),
        (Access::NoRead, false) => {
            Row::failed(check, "the running user cannot read the data root").next(next)
        }
        (Access::NoWrite, false) => {
            Row::failed(check, "the running user cannot write the data root").next(next)
        }
        (Access::NoRead | Access::NoWrite, true) => Row::failed(
            check,
            "the running user cannot create the data root in the directory above it",
        )
        .next(next),
        (Access::ReadOnlyFs, _) => Row::failed(check, "the data root is on a read-only filesystem")
            .next(Text::new(
                "an ingest node writes its data root: mount it read-write, or disable ingest",
            )),
        (Access::Unknown, _) => Row::not_sampled(check, reason::UNREADABLE),
        (Access::NotAsked, _) => not_asked(check, "the data root's access was not asked about"),
    }
}

/// Look at the data root the way boot's gate first looks at it (a dangling
/// symlink is an error, never absence), then, when `ask_access`, ask what
/// the running user may do there.
fn observe_root(data_root: &Path, ingest: bool, ask_access: bool) -> RootSeen {
    match epoch::metadata_if_present::<std::convert::Infallible>(data_root) {
        Ok(Some(meta)) if meta.is_dir() => RootSeen::Directory(if ask_access {
            access(data_root, ingest)
        } else {
            Access::NotAsked
        }),
        Ok(Some(_)) => RootSeen::NotADirectory,
        Ok(None) if !ingest => RootSeen::Absent(None),
        Ok(None) => nearest_ancestor(data_root, ask_access),
        Err(RootFault::Inspect { error, .. }) => inspect_seen(error.kind(), is_symlink(data_root)),
        Err(_) => RootSeen::Unreadable,
    }
}

/// Look at `dir`, a directory boot writes and creates when it is absent,
/// as [`observe_root`] looks at an ingest node's root: what the running
/// user may do with it, or, when it is absent, with the nearest directory
/// above it that exists, reached by the same walk.
pub(super) fn observe_created(dir: &Path) -> RootSeen {
    observe_root(dir, true, true)
}

/// What a failed inspection of the data root says. `linked` says the path
/// is a symlink now: only then is "not found" after the path was seen a
/// symlink to nothing, which boot refuses; otherwise the path went away
/// between two looks, which proves nothing about the next boot.
fn inspect_seen(kind: std::io::ErrorKind, linked: bool) -> RootSeen {
    match kind {
        std::io::ErrorKind::NotFound if linked => RootSeen::Dangling,
        std::io::ErrorKind::NotFound => RootSeen::Vanished,
        std::io::ErrorKind::PermissionDenied => RootSeen::Denied,
        std::io::ErrorKind::NotADirectory => RootSeen::AncestorNotADirectory,
        _ => RootSeen::Unreadable,
    }
}

/// Whether `path` is a symlink, without following it.
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// For an absent root on an ingest node: whether the running user may
/// create entries in the nearest directory above it that exists, asked
/// only when `ask_access`.
///
/// Each ancestor is looked at without following its final symlink, so an
/// ancestor that exists as a symlink to nothing is not mistaken for one
/// boot creates: boot's exclusive create meets it as an existing entry and
/// refuses. A symlink to a directory counts as that directory, as boot
/// follows it.
fn nearest_ancestor(data_root: &Path, ask_access: bool) -> RootSeen {
    let mut current = data_root;
    loop {
        let parent = match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        match epoch::metadata_if_present::<std::convert::Infallible>(parent) {
            Ok(Some(meta)) if meta.is_dir() => {
                return RootSeen::Absent(Some(if ask_access {
                    write_access(parent)
                } else {
                    Access::NotAsked
                }));
            }
            Ok(Some(_)) => return RootSeen::AncestorNotADirectory,
            Ok(None) if parent != current => current = parent,
            Err(RootFault::Inspect { error, .. }) => {
                return match inspect_seen(error.kind(), is_symlink(parent)) {
                    RootSeen::Dangling => RootSeen::AncestorDangling,
                    seen => seen,
                };
            }
            Ok(None) | Err(_) => return RootSeen::Unreadable,
        }
    }
}

/// What the running user may do with the existing root: read and search
/// it, and on an ingest node also write it.
#[cfg(unix)]
fn access(dir: &Path, ingest: bool) -> Access {
    use rustix::fs::Access as Mode;
    match access_at(dir, Mode::READ_OK | Mode::EXEC_OK) {
        Access::Granted if ingest => access_at(dir, Mode::WRITE_OK | Mode::EXEC_OK),
        other => other,
    }
}

/// Whether the running user may create entries in `dir`.
#[cfg(unix)]
fn write_access(dir: &Path) -> Access {
    use rustix::fs::Access as Mode;
    access_at(dir, Mode::WRITE_OK | Mode::EXEC_OK)
}

/// `accessat(AT_FDCWD, dir, mode, AT_EACCESS)`: the effective ids, as the
/// service's own opens would be judged.
#[cfg(unix)]
fn access_at(dir: &Path, mode: rustix::fs::Access) -> Access {
    use rustix::io::Errno;
    match rustix::fs::accessat(rustix::fs::CWD, dir, mode, rustix::fs::AtFlags::EACCESS) {
        Ok(()) => Access::Granted,
        Err(Errno::ACCESS | Errno::PERM) if mode.contains(rustix::fs::Access::WRITE_OK) => {
            Access::NoWrite
        }
        Err(Errno::ACCESS | Errno::PERM) => Access::NoRead,
        Err(Errno::ROFS) => Access::ReadOnlyFs,
        Err(_) => Access::Unknown,
    }
}

/// Off Unix there are no effective ids to ask about.
#[cfg(not(unix))]
fn access(_dir: &Path, _ingest: bool) -> Access {
    Access::Unknown
}

#[cfg(not(unix))]
fn write_access(_dir: &Path) -> Access {
    Access::Unknown
}

// ---------------------------------------------------------------------------
// server.data.epoch
// ---------------------------------------------------------------------------

/// Why the doctor's reads of a small marker returned no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unread {
    /// The bounded reader refused or failed.
    Fault(ReadFault),
    /// It holds bytes that are not UTF-8 text.
    NotText,
}

/// The doctor's reads for [`epoch::classify_data_root`]: bounded, without
/// following a symlink at the marker, never waiting on a FIFO.
struct DoctorReader;

impl RootReader for DoctorReader {
    type Error = Unread;

    fn epoch(&mut self, path: &Path) -> Result<String, Unread> {
        let bytes = fsread::read_bounded(path, fsread::cap::EPOCH, Links::NoFollow)
            .map_err(Unread::Fault)?;
        String::from_utf8(bytes).map_err(|_| Unread::NotText)
    }

    fn staged(&mut self, path: &Path, max: u64) -> Result<Option<Vec<u8>>, Unread> {
        match fsread::read_bounded(path, max, Links::NoFollow) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(ReadFault::TooLarge) => Ok(None),
            Err(fault) => Err(Unread::Fault(fault)),
        }
    }
}

/// Classify the data root as boot's gate does, with the doctor's reader.
fn classify(
    data_root: &Path,
    wal_dir: &Path,
    ingest: bool,
) -> Result<DataRootState, RootFault<Unread>> {
    epoch::classify_data_root(data_root, wal_dir, ingest, &mut DoctorReader)
}

/// `server.data.epoch`: the epoch is current, or boot initializes the root,
/// or a query-only node reads it as an unversioned archive.
async fn check_epoch(data_root: PathBuf, wal_dir: PathBuf, ingest: bool) -> Row {
    let check = ServerCheck::DataEpoch;
    let observed = look(move || {
        let state = classify(&data_root, &wal_dir, ingest);
        let linked = fault_names_symlink(&state);
        (state, linked)
    })
    .await;
    match observed {
        Ok((state, linked)) => epoch_row(state, linked),
        Err(why) => missed(check, why),
    }
}

/// Whether the path the classifier's fault names is a symlink now: a marker
/// the doctor does not follow under the data root, or, for a path found
/// and then not found, a symlink to nothing rather than an entry that went
/// away between two looks.
fn fault_names_symlink(state: &Result<DataRootState, RootFault<Unread>>) -> bool {
    match state {
        Err(
            RootFault::ReadEpoch { path, .. }
            | RootFault::ReadStaged { path, .. }
            | RootFault::Inspect { path, .. },
        ) => is_symlink(path),
        _ => false,
    }
}

/// The `server.data.epoch` row for what the classifier decided. `linked`
/// is [`fault_names_symlink`]: the marker it could not read is a symlink,
/// which the doctor does not follow under the data root, or the path it
/// did not find is a symlink to nothing.
fn epoch_row(state: Result<DataRootState, RootFault<Unread>>, linked: bool) -> Row {
    let check = ServerCheck::DataEpoch;
    let restore = Text::new(
        "select new empty data and WAL directories, or restore a complete epoch-3 backup \
         including EPOCH; do not edit EPOCH",
    );
    match state {
        Ok(DataRootState::Current) => {
            Row::complete(check).detail(Text::new("EPOCH names the current storage format"))
        }
        Ok(DataRootState::Fresh) => Row::complete_because(check, reason::WILL_INITIALIZE).detail(
            Text::new("absent: trawld creates the data root and writes EPOCH at its next start"),
        ),
        Ok(DataRootState::Empty { staged }) => {
            let mut detail = Text::new("empty: trawld writes EPOCH at its next start");
            if !staged.is_empty() {
                detail = detail
                    .lit(", after removing ")
                    .int(staged.len() as u64)
                    .lit(" staged epoch file(s) an interrupted first start left");
            }
            Row::complete_because(check, reason::WILL_INITIALIZE).detail(detail)
        }
        Ok(DataRootState::ReadOnlyArchive) => Row::complete(check).detail(Text::new(
            "no EPOCH: this query-only node reads the data root as an unversioned archive",
        )),
        Err(RootFault::NotADirectory) => Row::failed(check, "the data root is not a directory"),
        Err(RootFault::UnsupportedEpoch { .. }) => {
            Row::failed(check, "the data root carries an unsupported storage epoch").next(restore)
        }
        Err(RootFault::Unmarked { staged }) => {
            let row = Row::failed(check, "the data root is nonempty but has no EPOCH marker");
            let row = if staged.is_some() {
                row.detail(Text::new(
                    "it holds a staged epoch entry that is not one trawld can remove; \
                     inspect it before choosing what to do",
                ))
            } else {
                row
            };
            row.next(Text::new(
                "select new empty data and WAL directories, or restore a complete epoch-3 \
                 backup including EPOCH; an unversioned generic archive needs ingest \
                 disabled and no Trawl ownership markers",
            ))
        }
        Err(RootFault::FlatWal { .. }) => Row::failed(
            check,
            "the WAL directory holds flat batches outside environment directories",
        )
        .next(Text::new(
            "select a new empty WAL directory, or restore the WAL from a complete epoch-3 backup",
        )),
        Err(RootFault::Inspect { error, .. }) => match error.kind() {
            std::io::ErrorKind::PermissionDenied => {
                Row::not_sampled(check, reason::PERMISSION_DENIED)
            }
            std::io::ErrorKind::NotFound if linked => Row::failed(
                check,
                "the data root or the WAL directory holds a symlink to nothing",
            )
            .next(Text::new("remove or repair the dangling symlink")),
            std::io::ErrorKind::NotFound => Row::not_sampled(check, reason::MATERIAL_CHANGED)
                .next(Text::new("rerun once the data root stops changing")),
            std::io::ErrorKind::NotADirectory => Row::failed(
                check,
                "the WAL directory, or a path the data root needs, is not a directory",
            )
            .next(Text::new(
                "point [ingest] wal_dir and [data] path at directories",
            )),
            _ => Row::not_sampled(check, reason::UNREADABLE),
        },
        Err(RootFault::ReadEpoch { error, .. }) => match error {
            Unread::NotText => Row::failed(check, "the EPOCH marker is not text").next(restore),
            Unread::Fault(fault) => marker_fault_row(
                check,
                fault,
                linked,
                "the EPOCH marker is not a regular file",
            ),
        },
        Err(RootFault::ReadStaged { error, .. }) => match error {
            Unread::NotText => Row::not_sampled(check, reason::UNREADABLE),
            Unread::Fault(fault) => marker_fault_row(
                check,
                fault,
                linked,
                "a staged epoch entry is not a regular file",
            ),
        },
    }
}

/// The row for a marker under the data root that the bounded reader did
/// not read. A marker that exists and is not a regular file (a directory,
/// a FIFO, a device) is one boot refuses or waits on forever, so it fails.
/// A symlink is not followed there, so the doctor cannot say what boot,
/// which follows it, would read.
fn marker_fault_row(
    check: ServerCheck,
    fault: ReadFault,
    linked: bool,
    not_regular: &'static str,
) -> Row {
    match fault {
        ReadFault::NotRegular if linked => {
            Row::not_sampled(check, reason::UNREADABLE).detail(Text::new(
                "the marker is a symlink, which the doctor does not follow under the data root",
            ))
        }
        ReadFault::NotRegular => Row::failed(check, not_regular).next(Text::new(
            "inspect it; trawld writes its markers as regular files",
        )),
        ReadFault::PermissionDenied => Row::not_sampled(check, reason::PERMISSION_DENIED)
            .next(Text::new("rerun as the service user")),
        ReadFault::TooLarge => Row::not_sampled(check, reason::TOO_LARGE),
        ReadFault::Changed | ReadFault::Missing => {
            Row::not_sampled(check, reason::MATERIAL_CHANGED)
                .next(Text::new("rerun once the data root stops changing"))
        }
        ReadFault::TimedOut => Row::not_sampled(check, reason::TIMED_OUT),
        ReadFault::SymlinkLoop | ReadFault::Io => Row::not_sampled(check, reason::UNREADABLE),
    }
}

// ---------------------------------------------------------------------------
// server.data.identity and server.data.conformance
// ---------------------------------------------------------------------------

/// Read the `CATALOG` marker with the bounded reader. `Ok(None)` when there
/// is none.
fn read_catalog_marker(data_root: &Path) -> Result<Option<String>, Unread> {
    conform::read_marker_with(data_root, |path| {
        match fsread::read_bounded(path, fsread::cap::CATALOG, Links::NoFollow) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| Unread::NotText),
            Err(ReadFault::Missing) => Ok(None),
            Err(fault) => Err(Unread::Fault(fault)),
        }
    })
}

/// What `server.data.identity` saw.
#[derive(Debug)]
struct IdentitySeen {
    /// The marker, as read, or why it was not.
    marker: Result<Option<String>, Unread>,
    /// Whether it is a symlink, when it was not read.
    linked: bool,
    /// The judgement, when the marker was read.
    judgement: Option<IdentityJudgement>,
    /// What the walk for parquet saw, when the judgement needed one.
    walked: Option<ArchiveSeen>,
}

/// What the doctor's walk of the data root for parquet saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveSeen {
    /// No parquet, and nothing the walk could not enumerate.
    Empty,
    /// A parquet file, whether or not the walk enumerated everything.
    Parquet,
    /// No parquet in what the walk enumerated, and a path it could not.
    Unwalked,
}

/// Walk the data root for parquet as boot's identity gate does. Boot reads
/// a walk that did not finish as standing data, which is conservative for
/// boot; the doctor keeps it apart as a look that did not finish, unless
/// the walk found parquet anyway. A root that is absent or not a directory
/// holds none, as boot reads it; one the doctor cannot inspect is a walk
/// that did not finish.
fn see_archive(data_root: &Path) -> ArchiveSeen {
    match std::fs::metadata(data_root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return ArchiveSeen::Empty,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ArchiveSeen::Empty,
        Err(_) => return ArchiveSeen::Unwalked,
    }
    let walk = conform::walk_archive(data_root);
    if walk.parquet {
        ArchiveSeen::Parquet
    } else if walk.failure.is_some() {
        ArchiveSeen::Unwalked
    } else {
        ArchiveSeen::Empty
    }
}

/// `server.data.identity`: the data root's `CATALOG` marker names the
/// app-state database's catalog, or the root holds no parquet.
///
/// Stricter than boot (#269 H1): a marker naming another catalog fails on
/// every node type. A query-only boot refuses it; an ingest boot adopts
/// the archive through the lossy, irreversible conformance pass, and the
/// doctor catches a wrong database URL or a mismatched restore before that
/// makes it permanent. Neither identifier is shown.
///
/// Also returns the marker it read, for `server.data.conformance`.
async fn check_identity(
    data_root: PathBuf,
    catalog_id: Option<String>,
    ingest: bool,
) -> (Row, Option<String>) {
    let check = ServerCheck::DataIdentity;
    let fresh_catalog = catalog_id.is_none();
    let seen = look(move || {
        let marker = read_catalog_marker(&data_root);
        let linked = marker.is_err()
            && std::fs::symlink_metadata(data_root.join("CATALOG"))
                .is_ok_and(|meta| meta.file_type().is_symlink());
        let mut walked = None;
        let judgement = marker.as_ref().ok().map(|marker| {
            conform::judge_archive_identity(marker.as_deref(), catalog_id.as_deref(), || {
                let seen = see_archive(&data_root);
                walked = Some(seen);
                seen == ArchiveSeen::Empty
            })
        });
        IdentitySeen {
            marker,
            linked,
            judgement,
            walked,
        }
    })
    .await;
    let seen = match seen {
        Ok(seen) => seen,
        Err(why) => return (missed(check, why), None),
    };
    let row = match (&seen.marker, seen.judgement) {
        (_, Some(_)) if seen.walked == Some(ArchiveSeen::Unwalked) => unwalked_row(),
        (_, Some(judgement)) => identity_row(judgement, fresh_catalog, ingest),
        (Err(Unread::NotText), _) => Row::not_sampled(check, reason::UNREADABLE)
            .detail(Text::new("the CATALOG marker is not text")),
        (Err(Unread::Fault(fault)), _) => marker_fault_row(
            check,
            *fault,
            seen.linked,
            "the CATALOG marker is not a regular file",
        ),
        (Ok(_), None) => Row::not_sampled(check, reason::UNREADABLE),
    };
    (row, seen.marker.ok().flatten())
}

/// The `server.data.identity` row when the marker does not prove the
/// pairing and the walk for parquet did not finish without finding any.
/// Boot reads that as standing data, so a foreign marker refuses it; the
/// doctor has not seen parquet the marker must account for, and does not
/// fail on what it could not see.
fn unwalked_row() -> Row {
    Row::not_sampled(ServerCheck::DataIdentity, reason::UNREADABLE)
        .detail(Text::new(
            "part of the data root could not be walked and the rest holds no parquet, so \
             nothing shows whether the CATALOG marker must name the app-state database's \
             catalog",
        ))
        .next(Text::new("rerun as the service user"))
}

/// The `server.data.identity` row for a judgement.
fn identity_row(judgement: IdentityJudgement, fresh_catalog: bool, ingest: bool) -> Row {
    let check = ServerCheck::DataIdentity;
    match judgement {
        IdentityJudgement::Proven => Row::complete(check).detail(Text::new(
            "the CATALOG marker names the app-state database's catalog",
        )),
        IdentityJudgement::Empty => {
            Row::complete(check).detail(Text::new("the data root holds no parquet"))
        }
        IdentityJudgement::Unproven => Row::not_sampled(check, reason::UNPROVEN)
            .detail(Text::new(
                "the data root holds parquet and no CATALOG marker, so nothing proves which \
                 catalog wrote it",
            ))
            .next(Text::new(if ingest {
                "trawld's boot conformance pass adopts the archive and writes the marker; \
                 check its log for skipped paths if it already ran"
            } else {
                "start trawld once with [ingest] enabled = true to run the conformance pass"
            })),
        IdentityJudgement::Foreign => {
            let detail = if fresh_catalog {
                "the data root holds parquet and a CATALOG marker, and the app-state database \
                 has no catalog yet: its first start creates one that no marker names"
            } else {
                "the data root's CATALOG marker names another catalog than the app-state \
                 database's"
            };
            Row::failed(check, reason::CATALOG_IDENTITY_MISMATCH)
                .detail(Text::new(detail))
                .next(Text::new(
                    "point the app-state database at the catalog that owns this archive; or \
                     restore the database dump and the data archive from the same backup; or \
                     point [data] path at a new data root; or start trawld with [ingest] \
                     enabled = true to adopt the archive on purpose, which is lossy when the \
                     catalog already pins fields",
                ))
        }
    }
}

/// `server.data.conformance`: conformance is recorded for this catalog and
/// data root, or boot runs the pass. A query-only node runs no pass.
fn check_conformance(ctx: &Ctx, ingest: bool, marker: Option<&str>) -> Row {
    let check = ServerCheck::DataConformance;
    if !ingest {
        return Row::not_configured(check, "ingest is disabled, so this node runs no pass")
            .detail(Text::new("a query-only node reads the archive as it is"));
    }
    let recorded = ctx.app.catalog.as_ref().is_some_and(|catalog| {
        conform::conformance_recorded(
            catalog.conformed,
            || marker.map(str::to_owned),
            &catalog.catalog_id.0,
        )
    });
    if recorded {
        Row::complete(check).detail(Text::new(
            "recorded for this catalog and data root: boot skips the pass",
        ))
    } else {
        Row::complete_because(check, reason::WILL_INITIALIZE).detail(Text::new(
            "not recorded for this catalog and data root: trawld runs the boot conformance \
             pass at its next start, before it serves",
        ))
    }
}

// ---------------------------------------------------------------------------
// server.recovery.repin
// ---------------------------------------------------------------------------

/// What `server.recovery.repin` read.
#[derive(Debug)]
enum RepinSeen {
    /// No marker.
    Absent,
    /// A marker that parses.
    Marker(marker::RepinMarker),
    /// Bytes that are not text, or text that is not a marker.
    Malformed,
    /// The bounded reader did not read it; whether it is a symlink.
    Fault(ReadFault, bool),
}

/// `server.recovery.repin`: no repin marker, or one whose phase boot
/// finishes. A query-only node refuses a marker past `building`.
async fn check_repin(data_root: PathBuf, ingest: bool) -> Row {
    let check = ServerCheck::RecoveryRepin;
    let seen = look(move || {
        let path = marker::marker_path(&data_root);
        match fsread::read_bounded(&path, fsread::cap::REPIN, Links::NoFollow) {
            Ok(bytes) => match String::from_utf8(bytes)
                .ok()
                .and_then(|raw| marker::parse_marker(&raw).ok())
            {
                Some(marker) => RepinSeen::Marker(marker),
                None => RepinSeen::Malformed,
            },
            Err(ReadFault::Missing) => RepinSeen::Absent,
            Err(fault) => RepinSeen::Fault(
                fault,
                std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()),
            ),
        }
    })
    .await;
    let seen = match seen {
        Ok(seen) => seen,
        Err(why) => return missed(check, why),
    };
    match seen {
        RepinSeen::Absent => Row::complete(check).detail(Text::new("no repin marker")),
        RepinSeen::Malformed => Row::failed(check, "the repin marker is malformed").next(
            Text::new("trawld refuses to start over it; restore the data root from a backup"),
        ),
        RepinSeen::Fault(fault, linked) => marker_fault_row(
            check,
            fault,
            linked,
            "the repin marker is not a regular file",
        ),
        RepinSeen::Marker(found) => repin_row(&found, ingest),
    }
}

/// The `server.recovery.repin` row for a marker that parsed.
fn repin_row(found: &marker::RepinMarker, ingest: bool) -> Row {
    let check = ServerCheck::RecoveryRepin;
    let phase = match found.phase {
        marker::RepinPhase::Building => "building",
        marker::RepinPhase::Cutover => "cutover",
        marker::RepinPhase::Cleanup => "cleanup",
    };
    if !ingest {
        if found.phase.query_only_refuses() {
            return Row::failed(
                check,
                "a repin job died mid-cutover and this node runs with ingest disabled",
            )
            .detail(Text::new("the repin marker is in phase ").lit(phase))
            .next(Text::new(
                "start trawld once with [ingest] enabled = true to finish the repin",
            ));
        }
        return Row::complete(check).detail(Text::new(
            "a repin marker in phase building: this query-only node serves the untouched \
             corpus, and the ingest node finishes the job",
        ));
    }
    if found.phase != marker::RepinPhase::Building && found.target_type().is_none() {
        return Row::failed(check, "the repin marker names a type no pin can have")
            .detail(Text::new("the repin marker is in phase ").lit(phase))
            .next(Text::new(
                "trawld refuses to start over it; restore the data root from a backup",
            ));
    }
    Row::complete_because(check, reason::PENDING_AT_NEXT_BOOT).detail(
        Text::new("a repin marker in phase ")
            .lit(phase)
            .lit(": trawld finishes the job at its next start"),
    )
}

// ---------------------------------------------------------------------------
// server.recovery.publication
// ---------------------------------------------------------------------------

/// The publication markers `server.recovery.publication` counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Publications {
    /// A query-only node: its boot recovers no publication marker and
    /// never reads the WAL, so none are counted.
    NotCounted,
    /// The WAL root could not be listed.
    Unlisted,
    /// What the census of the WAL root found.
    Counted(publication_marker::MarkerCensus),
}

/// What `server.recovery.publication` counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MarkersSeen {
    /// The publication markers under the WAL root.
    publication: Publications,
    /// Whether the data root's day directories could all be listed.
    rollup_listed: bool,
    /// Rollup markers that read.
    rollup_pending: usize,
    /// Rollup markers boot's recovery can never use.
    rollup_malformed: usize,
    /// Rollup markers that could not be read.
    rollup_unreadable: usize,
}

/// Count the rollup markers in the data root's day directories and, on an
/// ingest node, the publication markers under the WAL root, each by the
/// decoder boot recovers it with, opened by the doctor's
/// [`fsread::open_for_decoder`].
///
/// The rollup census runs on every node type: the publication gate's boot
/// scan registers every rollup marker on a query-only node too, and its
/// reads refuse `rollup_pending` while one remains. Publication markers
/// are an ingest node's, whose boot recovers them.
fn count_markers(data_root: &Path, wal_dir: &Path, ingest: bool) -> MarkersSeen {
    let publication = if ingest {
        publication_marker::census(wal_dir, fsread::open_for_decoder)
            .map_or(Publications::Unlisted, Publications::Counted)
    } else {
        Publications::NotCounted
    };
    let mut seen = MarkersSeen {
        publication,
        rollup_listed: false,
        rollup_pending: 0,
        rollup_malformed: 0,
        rollup_unreadable: 0,
    };
    let mut rollups = std::collections::HashSet::new();
    seen.rollup_listed = crate::publication::scan_markers(data_root, &mut rollups).is_ok();
    for path in rollups {
        match compaction::read_rollup_marker_with(&path, fsread::open_for_decoder) {
            Ok(_) => seen.rollup_pending += 1,
            Err(e) if e.is_missing() => {}
            Err(e) if e.is_malformed() => seen.rollup_malformed += 1,
            Err(_) => seen.rollup_unreadable += 1,
        }
    }
    seen
}

/// `server.recovery.publication`: the rollup markers and, on an ingest
/// node, the publication markers are readable and well formed. A pending
/// one is boot's to finish.
async fn check_markers(data_root: PathBuf, wal_dir: PathBuf, ingest: bool) -> Row {
    let check = ServerCheck::RecoveryPublication;
    match look(move || count_markers(&data_root, &wal_dir, ingest)).await {
        Ok(seen) => markers_row(seen),
        Err(why) => missed(check, why),
    }
}

/// The `server.recovery.publication` row for what was counted. A malformed
/// marker outweighs one that could not be read, which outweighs pending
/// ones.
fn markers_row(seen: MarkersSeen) -> Row {
    let check = ServerCheck::RecoveryPublication;
    let rollups = |text: Text| {
        text.int(seen.rollup_pending as u64)
            .lit(" pending and ")
            .int(seen.rollup_malformed as u64)
            .lit(" malformed rollup marker(s)")
    };
    // `None` on a query-only node, which counts no publication marker.
    let census = match seen.publication {
        Publications::NotCounted => None,
        Publications::Unlisted => Some(publication_marker::MarkerCensus::default()),
        Publications::Counted(census) => Some(census),
    };
    let counts = rollups(census.map_or_else(
        || Text::new(""),
        |census| {
            Text::new("")
                .int(census.pending as u64)
                .lit(" pending and ")
                .int(census.invalid as u64)
                .lit(" malformed publication marker(s); ")
        },
    ));
    let inspect = Text::new(if census.is_some() {
        "trawld leaves a malformed marker in place and keeps what it claims blocked; \
         inspect the markers in the WAL directory's environments and the data root's \
         day directories"
    } else {
        "this query-only node refuses corpus reads while a rollup marker remains, and the \
         ingest node's recovery leaves a malformed one in place; inspect the markers in \
         the data root's day directories"
    });
    if census.is_some_and(|census| census.invalid > 0) {
        return Row::failed(check, "a publication marker is malformed")
            .detail(counts)
            .next(inspect);
    }
    if seen.rollup_malformed > 0 {
        return Row::failed(check, "a rollup marker is malformed")
            .detail(counts)
            .next(inspect);
    }
    let publication_unread = match seen.publication {
        Publications::NotCounted => false,
        Publications::Unlisted => true,
        Publications::Counted(census) => {
            census.root_incomplete || census.unlisted_envs > 0 || census.unreadable > 0
        }
    };
    if publication_unread || !seen.rollup_listed || seen.rollup_unreadable > 0 {
        return Row::not_sampled(check, reason::UNREADABLE)
            .detail(Text::new(
                "a marker, or a directory that may hold one, could not be read",
            ))
            .next(Text::new("rerun as the service user"));
    }
    let publication_pending = census.map_or(0, |census| census.pending);
    if publication_pending + seen.rollup_pending > 0 {
        let finish = if census.is_some() {
            ": trawld finishes the pending ones at its next start"
        } else {
            ": this query-only node refuses corpus reads while one remains, and the ingest \
             node that writes this data root finishes it"
        };
        return Row::complete_because(check, reason::PENDING_AT_NEXT_BOOT)
            .detail(counts.lit(finish));
    }
    Row::complete(check).detail(Text::new(if census.is_some() {
        "no publication or rollup marker"
    } else {
        "no rollup marker; publication markers are an ingest node's, and this query-only \
         node reads none"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::doctor::Outcome;

    /// One data-root state: what to plant, and on which node type.
    struct Case {
        name: &'static str,
        ingest: bool,
        plant: fn(&Path, &Path),
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[allow(clippy::too_many_lines)] // one table of states, clearer unsplit
    fn cases() -> Vec<Case> {
        let mut cases = Vec::new();
        for ingest in [false, true] {
            cases.extend([
                Case {
                    name: "missing root",
                    ingest,
                    plant: |_, _| {},
                },
                Case {
                    name: "empty root",
                    ingest,
                    plant: |data, _| std::fs::create_dir(data).unwrap(),
                },
                Case {
                    name: "current epoch",
                    ingest,
                    plant: |data, _| write(&data.join("EPOCH"), b"3\n"),
                },
                Case {
                    name: "current epoch beside environment WAL",
                    ingest,
                    plant: |data, _| {
                        write(&data.join("EPOCH"), b"3\n");
                        write(&data.join("wal/prod/svc_1_0001.ndjson"), b"{}\n");
                    },
                },
                Case {
                    name: "unsupported epoch",
                    ingest,
                    plant: |data, _| {
                        write(&data.join("EPOCH"), b"2\n");
                        write(&data.join("prod/2026-01-01/10/a.parquet"), b"corpus");
                    },
                },
                Case {
                    name: "empty epoch marker",
                    ingest,
                    plant: |data, _| write(&data.join("EPOCH"), b""),
                },
                Case {
                    name: "epoch marker that is not text",
                    ingest,
                    plant: |data, _| write(&data.join("EPOCH"), &[0xff, 0xfe, b'\n']),
                },
                Case {
                    name: "epoch marker that is a directory",
                    ingest,
                    plant: |data, _| std::fs::create_dir_all(data.join("EPOCH")).unwrap(),
                },
                Case {
                    name: "owned root without an epoch",
                    ingest,
                    plant: |data, _| write(&data.join("CATALOG"), b"id\n"),
                },
                Case {
                    name: "owned date partition without an epoch",
                    ingest,
                    plant: |data, _| write(&data.join("2026-01-01/a.parquet"), b"corpus"),
                },
                Case {
                    name: "generic archive without an epoch",
                    ingest,
                    plant: |data, _| write(&data.join("export.parquet"), b"generic"),
                },
                Case {
                    name: "interrupted first epoch",
                    ingest,
                    plant: |data, _| write(&data.join("EPOCH.next.123"), b"3"),
                },
                Case {
                    name: "staged epoch entry that is no remnant",
                    ingest,
                    plant: |data, _| {
                        write(&data.join("EPOCH.next.123"), b"3\n");
                        write(&data.join("EPOCH.next.backup"), b"3\n");
                    },
                },
                Case {
                    name: "oversized staged epoch",
                    ingest,
                    plant: |data, _| write(&data.join("EPOCH.next.123"), b"3\n\n"),
                },
                Case {
                    name: "root that is a file",
                    ingest,
                    plant: |data, _| write(data, b"not a directory"),
                },
                Case {
                    name: "flat WAL batch beside a current root",
                    ingest,
                    plant: |data, wal| {
                        write(&data.join("EPOCH"), b"3\n");
                        write(&wal.join("svc.ndjson"), b"unread batch");
                    },
                },
                Case {
                    name: "flat WAL batch beside a missing root",
                    ingest,
                    plant: |_, wal| write(&wal.join("svc.ndjson"), b"unread batch"),
                },
                Case {
                    name: "WAL path that is a file",
                    ingest,
                    plant: |_, wal| write(wal, b"not a directory"),
                },
            ]);
            #[cfg(unix)]
            cases.extend([
                Case {
                    name: "dangling symlink in the root",
                    ingest,
                    plant: |data, _| {
                        std::fs::create_dir(data).unwrap();
                        std::os::unix::fs::symlink(data.join("gone"), data.join("x.parquet"))
                            .unwrap();
                    },
                },
                Case {
                    name: "dangling epoch marker",
                    ingest,
                    plant: |data, _| {
                        std::fs::create_dir(data).unwrap();
                        std::os::unix::fs::symlink(data.join("gone"), data.join("EPOCH")).unwrap();
                    },
                },
            ]);
            #[cfg(target_os = "linux")]
            cases.push(Case {
                name: "staged epoch that is a FIFO",
                ingest,
                plant: |data, _| {
                    std::fs::create_dir(data).unwrap();
                    crate::ingest::no_follow::test_support::make_fifo(&data.join("EPOCH.next.123"));
                },
            });
        }
        cases
    }

    fn same_decision(state: &DataRootState, outcome: &epoch::Outcome) -> bool {
        matches!(
            (state, outcome),
            (DataRootState::Fresh, epoch::Outcome::FreshRoot)
                | (DataRootState::Current, epoch::Outcome::Current)
                | (
                    DataRootState::Empty { .. },
                    epoch::Outcome::InitializedEmpty
                )
                | (
                    DataRootState::ReadOnlyArchive,
                    epoch::Outcome::ReadOnlyArchive
                )
        )
    }

    /// For every data-root state, the doctor's classification (with its
    /// bounded reader) is the decision boot's gate then makes: the same
    /// admission, or a refusal, with boot's own message word for word from
    /// the same classifier. The doctor's row is `complete` for every
    /// admission and `failed` for every refusal, and classifying writes
    /// nothing.
    #[test]
    fn classify_agrees_with_admission() {
        for case in cases() {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            let wal = tmp.path().join("wal");
            (case.plant)(&data, &wal);
            let label = format!("{} (ingest {})", case.name, case.ingest);

            let before = tree(tmp.path());
            let doctor = crate::ingest::no_follow::test_support::returns_promptly({
                let (data, wal) = (data.clone(), wal.clone());
                move || classify(&data, &wal, case.ingest)
            });
            let boot_view =
                epoch::classify_data_root(&data, &wal, case.ingest, &mut epoch::BootReader);
            assert_eq!(tree(tmp.path()), before, "{label}: classifying wrote");

            let boot = epoch::ensure_current_epoch(&data, &wal, case.ingest);
            if let Err(message) = &boot {
                let fault = boot_view.expect_err(&label);
                assert_eq!(&fault.into_boot_message(&data), message, "{label}");
            }

            let admitted = doctor.as_ref().ok().cloned();
            let linked = fault_names_symlink(&doctor);
            let row = epoch_row(doctor, linked);
            match (&boot, admitted) {
                (Ok(outcome), Some(state)) => {
                    assert!(
                        same_decision(&state, outcome),
                        "{label}: {state:?} vs {outcome:?}"
                    );
                    assert_eq!(row.outcome(), Outcome::Complete, "{label}: {row:?}");
                }
                (Err(message), None) => {
                    assert_eq!(
                        row.outcome(),
                        Outcome::Failed,
                        "{label}: {message}: {row:?}"
                    );
                }
                (boot, doctor) => panic!("{label}: boot {boot:?}, doctor {doctor:?}"),
            }
        }
    }

    /// Every entry under `root` with its bytes, and its kind.
    fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_owned()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    stack.push(path.clone());
                    found.push((path, None));
                } else if meta.is_file() {
                    found.push((path.clone(), Some(std::fs::read(&path).unwrap())));
                } else {
                    found.push((path, Some(Vec::new())));
                }
            }
        }
        found.sort();
        found
    }

    /// Where the doctor does not follow what boot follows or reads past its
    /// cap, it says it could not look: never that boot admits or refuses.
    #[cfg(unix)]
    #[test]
    fn what_the_doctor_does_not_read_is_not_sampled() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = tmp.path().join("wal");
        std::fs::create_dir(&data).unwrap();
        let target = tmp.path().join("real-epoch");
        std::fs::write(&target, b"3\n").unwrap();
        std::os::unix::fs::symlink(&target, data.join("EPOCH")).unwrap();
        assert_eq!(
            epoch::ensure_current_epoch(&data, &wal, false),
            Ok(epoch::Outcome::Current)
        );
        let state = classify(&data, &wal, false);
        assert!(
            matches!(state, Err(RootFault::ReadEpoch { .. })),
            "{state:?}"
        );
        let row = epoch_row(state, true);
        assert_eq!(row.outcome(), Outcome::NotSampled);
        assert_eq!(row.reason(), Some(reason::UNREADABLE));

        // Padded past the cap, the marker still says 3 to boot.
        std::fs::remove_file(data.join("EPOCH")).unwrap();
        let mut padded = b"3".to_vec();
        padded.extend(std::iter::repeat_n(b' ', 100));
        std::fs::write(data.join("EPOCH"), &padded).unwrap();
        assert_eq!(
            epoch::ensure_current_epoch(&data, &wal, false),
            Ok(epoch::Outcome::Current)
        );
        let row = epoch_row(classify(&data, &wal, false), false);
        assert_eq!(row.outcome(), Outcome::NotSampled);
        assert_eq!(row.reason(), Some(reason::TOO_LARGE));
    }

    #[test]
    fn identity_rows_follow_h1_on_both_node_types() {
        for ingest in [false, true] {
            for fresh in [false, true] {
                let row = identity_row(IdentityJudgement::Foreign, fresh, ingest);
                assert_eq!(row.outcome(), Outcome::Failed);
                assert_eq!(row.reason(), Some(reason::CATALOG_IDENTITY_MISMATCH));
                let check = row.clone().into_check();
                let next = check.next_action.unwrap();
                for way_out in [
                    "point the app-state database at the catalog that owns this archive",
                    "restore the database dump and the data archive from the same backup",
                    "point [data] path at a new data root",
                    "adopt the archive on purpose",
                    "lossy",
                ] {
                    assert!(next.contains(way_out), "{next}");
                }
            }
            assert_eq!(
                identity_row(IdentityJudgement::Proven, false, ingest).outcome(),
                Outcome::Complete
            );
            assert_eq!(
                identity_row(IdentityJudgement::Empty, true, ingest).outcome(),
                Outcome::Complete
            );
            let unproven = identity_row(IdentityJudgement::Unproven, false, ingest);
            assert_eq!(unproven.outcome(), Outcome::NotSampled);
            assert_eq!(unproven.reason(), Some(reason::UNPROVEN));
        }
    }

    /// The judgement is the one boot's gate makes, including for a
    /// catalog not created yet, which no marker can name.
    #[test]
    fn identity_is_judged_as_boot_judges_it() {
        use IdentityJudgement as J;
        let empty: fn() -> bool = || true;
        let full: fn() -> bool = || false;
        let judge = conform::judge_archive_identity;
        assert_eq!(judge(Some("a"), Some("a"), full), J::Proven);
        assert_eq!(judge(Some("b"), Some("a"), full), J::Foreign);
        assert_eq!(judge(Some("b"), Some("a"), empty), J::Empty);
        assert_eq!(judge(None, Some("a"), full), J::Unproven);
        assert_eq!(judge(None, Some("a"), empty), J::Empty);
        assert_eq!(judge(Some("b"), None, full), J::Foreign);
        assert_eq!(judge(None, None, full), J::Unproven);
        assert_eq!(judge(None, None, empty), J::Empty);
        let marker = |id: &'static str| move || Some(id.to_owned());
        assert!(conform::conformance_recorded(true, marker("a"), "a"));
        assert!(!conform::conformance_recorded(
            false,
            || panic!("an unconformed catalog reads no marker"),
            "a"
        ));
        assert!(!conform::conformance_recorded(true, marker("b"), "a"));
        assert!(!conform::conformance_recorded(true, || None, "a"));
    }

    fn repin(phase: marker::RepinPhase, to_type: &str) -> marker::RepinMarker {
        marker::RepinMarker {
            job_id: 7,
            field: "status".to_owned(),
            from_type: "BIGINT".to_owned(),
            to_type: to_type.to_owned(),
            phase,
        }
    }

    #[test]
    fn repin_rows_refuse_what_boot_refuses() {
        use marker::RepinPhase as P;
        for phase in [P::Building, P::Cutover, P::Cleanup] {
            let row = repin_row(&repin(phase, "VARCHAR"), true);
            assert_eq!(
                (row.outcome(), row.reason()),
                (Outcome::Complete, Some(reason::PENDING_AT_NEXT_BOOT)),
                "{phase:?}"
            );
            let row = repin_row(&repin(phase, "VARCHAR"), false);
            let expected = if phase.query_only_refuses() {
                Outcome::Failed
            } else {
                Outcome::Complete
            };
            assert_eq!(row.outcome(), expected, "{phase:?}");
        }
        assert!(!P::Building.query_only_refuses());
        assert!(P::Cutover.query_only_refuses() && P::Cleanup.query_only_refuses());
        // A target no pin can have: boot's store half refuses a cutover or
        // cleanup over it; a building marker never parses it.
        assert_eq!(
            repin_row(&repin(P::Cutover, "NOT_A_TYPE"), true).outcome(),
            Outcome::Failed
        );
        assert_eq!(
            repin_row(&repin(P::Building, "NOT_A_TYPE"), true).outcome(),
            Outcome::Complete
        );
    }

    #[test]
    fn marker_rows_keep_malformed_and_unreadable_apart() {
        let census = |pending, invalid, unreadable| publication_marker::MarkerCensus {
            pending,
            invalid,
            unreadable,
            ..Default::default()
        };
        let seen = |publication, rollup_pending, rollup_malformed, rollup_unreadable| MarkersSeen {
            publication: Publications::Counted(publication),
            rollup_listed: true,
            rollup_pending,
            rollup_malformed,
            rollup_unreadable,
        };
        let row = |seen| {
            let row = markers_row(seen);
            (row.outcome(), row.reason())
        };
        assert_eq!(
            row(seen(census(0, 0, 0), 0, 0, 0)),
            (Outcome::Complete, None)
        );
        assert_eq!(
            row(seen(census(2, 0, 0), 1, 0, 0)),
            (Outcome::Complete, Some(reason::PENDING_AT_NEXT_BOOT))
        );
        assert_eq!(
            row(seen(census(1, 1, 1), 0, 0, 1)),
            (Outcome::Failed, Some("a publication marker is malformed"))
        );
        assert_eq!(
            row(seen(census(1, 0, 1), 0, 1, 0)),
            (Outcome::Failed, Some("a rollup marker is malformed"))
        );
        assert_eq!(
            row(seen(census(1, 0, 1), 0, 0, 0)),
            (Outcome::NotSampled, Some(reason::UNREADABLE))
        );
        assert_eq!(
            row(seen(census(0, 0, 0), 1, 0, 1)),
            (Outcome::NotSampled, Some(reason::UNREADABLE))
        );
        let unlisted = MarkersSeen {
            publication: Publications::Unlisted,
            ..seen(census(0, 0, 0), 0, 0, 0)
        };
        assert_eq!(
            row(unlisted),
            (Outcome::NotSampled, Some(reason::UNREADABLE))
        );
        let incomplete = seen(
            publication_marker::MarkerCensus {
                root_incomplete: true,
                ..census(0, 0, 0)
            },
            0,
            0,
            0,
        );
        assert_eq!(
            row(incomplete),
            (Outcome::NotSampled, Some(reason::UNREADABLE))
        );
    }

    /// The rollup and publication census read what is on disk with the
    /// decoders boot recovers with.
    #[test]
    fn markers_are_counted_by_what_boot_recovery_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = data.join("wal");
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        let day = data.join("prod/2026-09-23");
        write(
            &day.join(".rollup-nginx"),
            b"/data/prod/2026-09-23/07/nginx.parquet\n",
        );
        write(&day.join(".rollup-api"), &[0xff, 0xfe]);
        write(&wal.join("prod/.publish-nginx.json"), b"{ not json");
        let seen = count_markers(&data, &wal, true);
        assert_eq!(seen.rollup_pending, 1);
        assert_eq!(seen.rollup_malformed, 1);
        assert_eq!(seen.rollup_unreadable, 0);
        assert!(seen.rollup_listed);
        let Publications::Counted(publication) = seen.publication else {
            panic!("{seen:?}");
        };
        assert_eq!((publication.pending, publication.invalid), (0, 1));
        assert_eq!(markers_row(seen).outcome(), Outcome::Failed);
    }

    /// A query-only node's publication gate registers every rollup marker
    /// its boot scan finds and refuses corpus reads while one remains, so
    /// the census counts them there too. Publication markers are an ingest
    /// node's: a query-only node never reads the WAL and counts none.
    #[test]
    fn a_query_only_node_counts_rollup_markers_and_no_publication_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = data.join("wal");
        let day = data.join("prod/2026-09-23");
        write(
            &day.join(".rollup-nginx"),
            b"/data/prod/2026-09-23/07/nginx.parquet\n",
        );
        write(&wal.join("prod/.publish-nginx.json"), b"{ not json");
        let seen = count_markers(&data, &wal, false);
        assert_eq!(seen.publication, Publications::NotCounted);
        assert_eq!((seen.rollup_pending, seen.rollup_malformed), (1, 0));
        let row = markers_row(seen);
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::Complete, Some(reason::PENDING_AT_NEXT_BOOT))
        );
        let detail = row.into_check().detail.unwrap();
        assert!(
            detail.contains("1 pending and 0 malformed rollup marker(s)")
                && !detail.contains("publication marker(s)"),
            "{detail}"
        );

        write(&day.join(".rollup-api"), &[0xff, 0xfe]);
        let row = markers_row(count_markers(&data, &wal, false));
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::Failed, Some("a rollup marker is malformed"))
        );

        let query_only = |rollup_listed, rollup_unreadable| MarkersSeen {
            publication: Publications::NotCounted,
            rollup_listed,
            rollup_pending: 0,
            rollup_malformed: 0,
            rollup_unreadable,
        };
        for unread in [query_only(false, 0), query_only(true, 1)] {
            let row = markers_row(unread);
            assert_eq!(
                (row.outcome(), row.reason()),
                (Outcome::NotSampled, Some(reason::UNREADABLE)),
                "{unread:?}"
            );
        }
        let row = markers_row(query_only(true, 0));
        assert_eq!((row.outcome(), row.reason()), (Outcome::Complete, None));
    }

    /// A path the classifier saw and then did not find is a symlink to
    /// nothing only when it is a symlink; otherwise it went away between
    /// two looks, which says nothing about the next boot. Only the first
    /// fails, for the data root and for `server.data.epoch`.
    #[test]
    fn a_path_gone_between_two_looks_is_not_a_dangling_symlink() {
        let gone = |linked| {
            epoch_row(
                Err(RootFault::Inspect {
                    at: epoch::Inspected::Path,
                    path: PathBuf::from("entry"),
                    error: std::io::ErrorKind::NotFound.into(),
                }),
                linked,
            )
        };
        let row = gone(false);
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::NotSampled, Some(reason::MATERIAL_CHANGED))
        );
        assert_eq!(gone(true).outcome(), Outcome::Failed);

        for ingest in [false, true] {
            let row = root_row(inspect_seen(std::io::ErrorKind::NotFound, false), ingest);
            assert_eq!(
                (row.outcome(), row.reason()),
                (Outcome::NotSampled, Some(reason::MATERIAL_CHANGED))
            );
            let row = root_row(inspect_seen(std::io::ErrorKind::NotFound, true), ingest);
            assert_eq!(
                (row.outcome(), row.reason()),
                (
                    Outcome::Failed,
                    Some("the data root is a symlink to nothing")
                )
            );
        }
    }

    /// A walk that could not enumerate part of the root and found no
    /// parquet is kept apart from an empty archive and from one holding
    /// parquet; boot's reading of the same walk stays conservative.
    #[cfg(unix)]
    #[test]
    fn a_walk_that_did_not_finish_is_not_standing_data() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let sealed = data.join("prod");
        std::fs::create_dir_all(&sealed).unwrap();
        assert_eq!(see_archive(&data), ArchiveSeen::Empty);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = std::fs::read_dir(&sealed).is_ok();
        let unwalked = see_archive(&data);
        let boot_empty = conform::archive_is_empty(&data);
        write(&data.join("other/2026-01-01/10/svc.parquet"), b"corpus");
        let with_parquet = see_archive(&data);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
        if privileged {
            assert_eq!(unwalked, ArchiveSeen::Empty);
            assert!(boot_empty);
        } else {
            assert_eq!(unwalked, ArchiveSeen::Unwalked);
            assert!(!boot_empty, "boot reads a walk that did not finish as data");
        }
        assert_eq!(with_parquet, ArchiveSeen::Parquet);
        assert_eq!(see_archive(&tmp.path().join("absent")), ArchiveSeen::Empty);
    }

    #[test]
    fn a_marker_that_is_not_a_regular_file_fails_unless_it_is_a_symlink() {
        let check = ServerCheck::RecoveryRepin;
        let row = marker_fault_row(check, ReadFault::NotRegular, false, "not regular");
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::Failed, Some("not regular"))
        );
        let row = marker_fault_row(check, ReadFault::NotRegular, true, "not regular");
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::NotSampled, Some(reason::UNREADABLE))
        );
        for (fault, why) in [
            (ReadFault::PermissionDenied, reason::PERMISSION_DENIED),
            (ReadFault::TooLarge, reason::TOO_LARGE),
            (ReadFault::Changed, reason::MATERIAL_CHANGED),
            (ReadFault::TimedOut, reason::TIMED_OUT),
            (ReadFault::Io, reason::UNREADABLE),
        ] {
            let row = marker_fault_row(check, fault, false, "not regular");
            assert_eq!(
                (row.outcome(), row.reason()),
                (Outcome::NotSampled, Some(why))
            );
        }
    }

    /// `server.data.root` over `data_root`, with the WAL directory at its
    /// default place inside it.
    async fn root_check(data_root: PathBuf, ingest: bool, ask_access: bool) -> Row {
        let wal_dir = data_root.join("wal");
        check_root(data_root, wal_dir, ingest, ask_access).await
    }

    #[tokio::test]
    async fn the_root_is_absent_created_or_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("state/data");
        let row = root_check(data.clone(), true, true).await;
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::Complete, Some(reason::WILL_INITIALIZE))
        );
        let row = root_check(data.clone(), false, true).await;
        assert_eq!((row.outcome(), row.reason()), (Outcome::Complete, None));
        std::fs::create_dir_all(&data).unwrap();
        for ingest in [false, true] {
            let row = root_check(data.clone(), ingest, true).await;
            assert_eq!((row.outcome(), row.reason()), (Outcome::Complete, None));
        }
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let row = root_check(file.clone(), true, true).await;
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::Failed, Some("the data root is not a directory"))
        );
        let row = root_check(file.join("data"), true, true).await;
        assert_eq!(row.outcome(), Outcome::Failed, "{row:?}");
    }

    /// Above an absent root, an ancestor that is a symlink to nothing is
    /// not one boot creates: boot's exclusive create meets the symlink as
    /// an existing entry and refuses, so the row fails. A symlink to a
    /// directory counts as that directory, and boot creates the root
    /// through it.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_absent_root_below_a_dangling_symlink_fails_as_boot_does() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let link = tmp.path().join("state");
        std::os::unix::fs::symlink(tmp.path().join("gone"), &link).unwrap();
        let data = link.join("data");
        for ask_access in [false, true] {
            let row = root_check(data.clone(), true, ask_access).await;
            assert_eq!(
                (row.outcome(), row.reason()),
                (
                    Outcome::Failed,
                    Some("a parent of the data root is a symlink to nothing")
                ),
                "ask_access {ask_access}"
            );
        }
        let before = tree(tmp.path());
        let boot = epoch::ensure_current_epoch(&data, &wal, true);
        assert!(
            boot.as_ref()
                .is_err_and(|message| message.starts_with("failed to create data root")),
            "{boot:?}"
        );
        assert_eq!(tree(tmp.path()), before, "boot's refusal wrote");

        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = tmp.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let data = linked.join("data");
        let row = root_check(data.clone(), true, true).await;
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::Complete, Some(reason::WILL_INITIALIZE))
        );
        assert_eq!(
            epoch::ensure_current_epoch(&data, &wal, true),
            Ok(epoch::Outcome::FreshRoot)
        );
        assert!(real.join("data").is_dir());
    }

    /// Unless the doctor asks about access, as in a root run, a structure
    /// that holds is `ran_as_root`, whatever `accessat` would have said: a
    /// root no one may enter, and one below a directory no one may write,
    /// are not failed. A structure that does not hold still fails.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_root_run_asks_nothing_about_access() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let sealed = tmp.path().join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o500)).unwrap();
        // The WAL directory sits beside the root: a real root run looks it
        // up through a root it may enter, and this run is not root.
        let wal = tmp.path().join("wal");
        let mut rows = Vec::new();
        for ingest in [false, true] {
            rows.push(check_root(data.clone(), wal.clone(), ingest, false).await);
        }
        rows.push(root_check(sealed.join("data"), true, false).await);
        let asked = root_check(data.clone(), false, true).await;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
        for row in rows {
            assert_eq!(
                (row.outcome(), row.reason()),
                (Outcome::NotSampled, Some(reason::RAN_AS_ROOT)),
                "{row:?}"
            );
        }
        if !privileged_over(&tmp) {
            assert_eq!(asked.outcome(), Outcome::Failed, "{asked:?}");
        }

        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        for path in [file.clone(), file.join("data")] {
            let row = root_check(path, true, false).await;
            assert_eq!(row.outcome(), Outcome::Failed, "{row:?}");
        }
    }

    /// Whether the running user reads a directory its mode forbids, as root
    /// or a holder of `CAP_DAC_OVERRIDE` does.
    #[cfg(unix)]
    fn privileged_over(tmp: &tempfile::TempDir) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        let probe = tmp.path().join("probe");
        std::fs::create_dir(&probe).unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = std::fs::read_dir(&probe).is_ok();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).unwrap();
        privileged
    }

    /// Mode 0500 lets the running user read the root but not write it. A
    /// privileged user (root, or one holding `CAP_DAC_OVERRIDE`) may do
    /// both; the test asserts whichever the running user is.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_ingest_root_the_user_cannot_write_fails() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o500)).unwrap();
        let privileged = std::fs::File::create(data.join("probe")).is_ok();
        let ingest = root_check(data.clone(), true, true).await;
        let query = root_check(data.clone(), false, true).await;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(query.outcome(), Outcome::Complete);
        if privileged {
            assert_eq!(ingest.outcome(), Outcome::Complete);
        } else {
            assert_eq!(
                (ingest.outcome(), ingest.reason()),
                (
                    Outcome::Failed,
                    Some("the running user cannot write the data root")
                )
            );
        }
    }

    /// Whether boot can create in the WAL directory what its first write
    /// needs: the create `WalWriter::ensure_dir` makes, then the
    /// environment directory the first write makes, without their
    /// directory syncs.
    fn boot_creates_wal(wal_dir: &Path) -> bool {
        epoch::create_dir_all_durably(&wal_dir.join("prod"), |_| Ok(())).is_ok()
    }

    /// A WAL directory outside the data root is judged as the root is:
    /// boot creates it at its start, so the running user must read, write
    /// and search it, or create it in the nearest directory above it. Each
    /// state is checked against boot's own create. A privileged user may
    /// do everything its modes forbid; the test asserts whichever the
    /// running user is. A query-only node never looks at it, and a root run
    /// asks only about its structure.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_wal_directory_outside_the_root_is_judged_as_the_root_is() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let privileged = privileged_over(&tmp);
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let sealed = tmp.path().join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        let unwritable = tmp.path().join("unwritable");
        std::fs::create_dir(&unwritable).unwrap();
        let writable = tmp.path().join("writable");
        std::fs::create_dir(&writable).unwrap();
        let cases = [
            ("writable", writable.clone(), None),
            (
                "absent under a writable parent",
                tmp.path().join("spool/wal"),
                None,
            ),
            (
                "unwritable",
                unwritable.clone(),
                Some("the running user cannot write the WAL directory"),
            ),
            (
                "absent under an unwritable parent",
                sealed.join("spool/wal"),
                Some("the running user cannot create the WAL directory in the directory above it"),
            ),
        ];
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o500)).unwrap();
        std::fs::set_permissions(&unwritable, std::fs::Permissions::from_mode(0o500)).unwrap();
        let mut seen = Vec::new();
        for (name, wal, _) in &cases {
            let asked = check_root(data.clone(), wal.clone(), true, true).await;
            let root_run = check_root(data.clone(), wal.clone(), true, false).await;
            let query = check_root(data.clone(), wal.clone(), false, true).await;
            seen.push((*name, asked, root_run, query, boot_creates_wal(wal)));
        }
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&unwritable, std::fs::Permissions::from_mode(0o700)).unwrap();
        for ((name, asked, root_run, query, boot), (_, _, denied)) in seen.into_iter().zip(cases) {
            let expected = match denied {
                Some(why) if !privileged => (Outcome::Failed, Some(why)),
                _ => (Outcome::Complete, None),
            };
            assert_eq!((asked.outcome(), asked.reason()), expected, "{name}");
            assert_eq!(
                boot,
                expected.0 == Outcome::Complete,
                "{name}: boot disagrees"
            );
            assert_eq!(
                (root_run.outcome(), root_run.reason()),
                (Outcome::NotSampled, Some(reason::RAN_AS_ROOT)),
                "{name}"
            );
            assert_eq!(
                (query.outcome(), query.reason()),
                (Outcome::Complete, None),
                "{name}"
            );
        }

        // What boot's create refuses whoever runs it fails even when access
        // is not asked about.
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("gone"), &dangling).unwrap();
        for (wal, why) in [
            (file.clone(), "the WAL directory is not a directory"),
            (
                file.join("wal"),
                "a parent of the WAL directory is not a directory",
            ),
            (
                dangling.clone(),
                "the WAL directory is a symlink to nothing",
            ),
            (
                dangling.join("wal"),
                "a parent of the WAL directory is a symlink to nothing",
            ),
        ] {
            for ask_access in [false, true] {
                let row = check_root(data.clone(), wal.clone(), true, ask_access).await;
                assert_eq!(
                    (row.outcome(), row.reason()),
                    (Outcome::Failed, Some(why)),
                    "ask_access {ask_access}"
                );
            }
        }
        for wal in [file.join("wal"), dangling.join("wal")] {
            assert!(!boot_creates_wal(&wal), "boot created {wal:?}");
        }
    }

    /// A WAL path that starts with the data root is judged on its own: `..`
    /// or a symlink takes it out of the root, and a child that exists has
    /// its own modes. When the root fails too, the row reports the root's
    /// failure once. A privileged user may do everything the modes forbid;
    /// the test asserts whichever the running user is.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_wal_directory_under_the_root_path_is_judged_on_its_own() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let privileged = privileged_over(&tmp);
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let unwritable = tmp.path().join("unwritable");
        std::fs::create_dir(&unwritable).unwrap();
        let sealed = tmp.path().join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        let child = data.join("wal");
        std::fs::create_dir(&child).unwrap();
        let link = data.join("link");
        std::os::unix::fs::symlink(&unwritable, &link).unwrap();
        let write = "the running user cannot write the WAL directory";
        let cases = [
            ("escapes through ..", data.join("../unwritable"), write),
            (
                "absent, escapes through ..",
                data.join("../sealed/wal"),
                "the running user cannot create the WAL directory in the directory above it",
            ),
            ("a symlink under the root", link.clone(), write),
            ("an unwritable child of the root", child.clone(), write),
        ];
        for dir in [&unwritable, &sealed, &child] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        let mut seen = Vec::new();
        for (name, wal, _) in &cases {
            let asked = check_root(data.clone(), wal.clone(), true, true).await;
            let root_run = check_root(data.clone(), wal.clone(), true, false).await;
            seen.push((*name, asked, root_run));
        }
        // The data root fails as well: its failure is the row's.
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o500)).unwrap();
        let both = check_root(data.clone(), data.join("../unwritable"), true, true).await;
        for dir in [&data, &unwritable, &sealed, &child] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for ((name, asked, root_run), (_, _, why)) in seen.into_iter().zip(cases) {
            let expected = if privileged {
                (Outcome::Complete, None)
            } else {
                (Outcome::Failed, Some(why))
            };
            assert_eq!((asked.outcome(), asked.reason()), expected, "{name}");
            assert_eq!(
                (root_run.outcome(), root_run.reason()),
                (Outcome::NotSampled, Some(reason::RAN_AS_ROOT)),
                "{name}"
            );
        }
        let expected = if privileged {
            (Outcome::Complete, None)
        } else {
            (
                Outcome::Failed,
                Some("the running user cannot write the data root"),
            )
        };
        assert_eq!((both.outcome(), both.reason()), expected, "{both:?}");
    }
}
