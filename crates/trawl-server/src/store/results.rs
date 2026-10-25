// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed metadata for captured results (ADR-0057).
//!
//! One `captured_results` row describes one immutable capture: who owns it,
//! when it stops being readable, what ran, its typed columns, and the size
//! and blake3 digest of its parquet file. The file itself lives under the
//! results root; this store never touches it. Statements that retire rows
//! return what the caller must unlink.
//!
//! Every statement that compares against the current time binds `now` as a
//! parameter and none reads SQL `now()`, so the caller's injected clock
//! decides every lifecycle outcome (expiry, tombstone age, live usage).
//!
//! A row is live while it is `kept`, or `temporary` with `expires_at > now`.
//! A temporary row past its deadline is no longer live even before a sweep
//! or a read marks it `expired`. Only temporary rows count toward the
//! capture quotas (ADR-0057) and are listed to their owner: a kept row
//! belongs to an investigation, is charged to the evidence budget instead
//! (ADR-0059), and is pruned only when its investigation's retention ends.
//!
//! The two JSON documents, `columns` and `column_facts`, are stored as
//! text, encoded and decoded with the trawl-api types. Not as JSONB, which
//! refuses `\u0000`: a column name or a listed value is any string, and a
//! capture holding one must commit like any other. A row whose metadata
//! does not decode is a [`StoreError::Unavailable`] carrying a
//! [`sqlx::Error::ColumnDecode`] that names the column, never a panic.

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::postgres::{PgConnection, PgRow};
use sqlx::{PgPool, Row as _};
use trawl_api::investigations::{InvestigationId, StepOutcome};
use trawl_api::results::{
    CapturedResult, ColumnFacts, ResultColumn, ResultShape, ResultSource, ResultState,
};

use super::error::{PgViolation, StoreError, classify_violation};
use super::history::bind_u64;
use super::investigations::{self, InvestigationStore, RetentionRule};

/// A row predicate: the result is a temporary capture still live at `$N`.
/// Spelled once so the list, both usage reads, the boot key walk and the
/// prune agree on what the capture quotas count. Kept rows never match.
macro_rules! temporary_live_at {
    ($now:literal) => {
        concat!("(state = 'temporary' AND expires_at > ", $now, ")")
    };
}

/// `SELECT` every column of `captured_results`, and the `retain_until` of
/// the investigation a row is attached to, followed by `$tail`.
macro_rules! select_rows {
    ($tail:expr) => {
        concat!(
            "SELECT id, key_id, created_at, expires_at, gone_at, state, query, \
             executed_query, source, shape, earliest, latest, columns, column_facts, \
             captured_rows, total_rows, wire_bytes, file_bytes, digest, file_missing, \
             format_version, investigation_id, \
             (SELECT retain_until FROM investigations AS i \
              WHERE i.id = captured_results.investigation_id) AS retain_until \
             FROM captured_results ",
            $tail
        )
    };
}

/// A capture to record: `temporary` with a deadline, or `kept` from its
/// commit when it is attached to an investigation.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCapture {
    /// 32 lowercase hex digits; the file's name too.
    pub id: String,
    /// The fleet key that made the capture.
    pub key_id: i64,
    /// When the capture committed.
    pub created_at: DateTime<Utc>,
    /// When the temporary capture stops being readable. Not stored for a
    /// kept capture, which has no deadline of its own.
    pub expires_at: DateTime<Utc>,
    /// The query text as the caller sent it.
    pub query: String,
    /// The text that ran.
    pub executed_query: String,
    /// Where the rows came from.
    pub source: ResultSource,
    /// Which rule kept the rows.
    pub shape: ResultShape,
    /// The resolved inclusive start; `None` for a stored source.
    pub earliest: Option<DateTime<Utc>>,
    /// The resolved exclusive end; `None` for a stored source.
    pub latest: Option<DateTime<Utc>>,
    /// The columns in stored positional order.
    pub columns: Vec<ResultColumn>,
    /// Facts per column, positional with `columns`.
    pub column_facts: Vec<ColumnFacts>,
    /// Rows the file holds.
    pub captured_rows: u64,
    /// Rows the executed query produced before the capture cut.
    pub total_rows: u64,
    /// Bytes of wire JSON over every captured cell.
    pub wire_bytes: u64,
    /// Bytes of the parquet file; zero when there is no file.
    pub file_bytes: u64,
    /// The file's blake3 digest; `None` exactly when there is no file.
    pub digest: Option<[u8; 32]>,
    /// The stored file's layout version.
    pub format_version: u16,
    /// The investigation the capture is attached to (ADR-0059). `Some`
    /// records the row `kept`, with no deadline, and only through
    /// [`ResultsStore::insert_kept`]; `None` records it `temporary` until
    /// `expires_at`.
    pub investigation_id: Option<InvestigationId>,
}

/// One `captured_results` row, in any state.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureRow {
    /// 32 lowercase hex digits.
    pub id: String,
    /// The owning fleet key.
    pub key_id: i64,
    /// When the capture committed.
    pub created_at: DateTime<Utc>,
    /// The deadline; `None` exactly when the row is `kept`.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the result stopped being readable; set exactly when the row is
    /// `expired` (its deadline) or `pruned` (the prune time).
    pub gone_at: Option<DateTime<Utc>>,
    /// The lifecycle state.
    pub state: ResultState,
    /// The query text as the caller sent it.
    pub query: String,
    /// The text that ran.
    pub executed_query: String,
    /// Where the rows came from.
    pub source: ResultSource,
    /// Which rule kept the rows.
    pub shape: ResultShape,
    /// The resolved inclusive start; `None` for a stored source.
    pub earliest: Option<DateTime<Utc>>,
    /// The resolved exclusive end; `None` for a stored source.
    pub latest: Option<DateTime<Utc>>,
    /// The columns in stored positional order.
    pub columns: Vec<ResultColumn>,
    /// Facts per column, positional with `columns`.
    pub column_facts: Vec<ColumnFacts>,
    /// Rows the file holds.
    pub captured_rows: u64,
    /// Rows the executed query produced before the capture cut.
    pub total_rows: u64,
    /// Bytes of wire JSON over every captured cell.
    pub wire_bytes: u64,
    /// Bytes of the parquet file; zero when there is no file.
    pub file_bytes: u64,
    /// The file's blake3 digest; `None` exactly when there is no file.
    pub digest: Option<[u8; 32]>,
    /// Whether the file was found absent after the row committed.
    pub file_missing: bool,
    /// The stored file's layout version.
    pub format_version: u16,
    /// The investigation holding a kept result, or that held a pruned one
    /// (ADR-0059). Cleared when the investigation is deleted.
    pub investigation_id: Option<InvestigationId>,
    /// That investigation's evidence deadline, read in the same statement
    /// as the row: a kept row read at or past it is due for the prune.
    /// `None` without an investigation. Never on the wire.
    pub retain_until: Option<DateTime<Utc>>,
}

impl CaptureRow {
    /// The row as the wire spells it: RFC 3339 UTC timestamps with
    /// microseconds, and `truncated` derived from the row counts.
    #[must_use]
    pub fn to_wire(&self) -> CapturedResult {
        CapturedResult {
            id: self.id.clone(),
            query: self.query.clone(),
            executed_query: self.executed_query.clone(),
            source: self.source,
            shape: self.shape,
            earliest: self.earliest.map(wire_time),
            latest: self.latest.map(wire_time),
            created_at: wire_time(self.created_at),
            expires_at: self.expires_at.map(wire_time),
            state: self.state,
            columns: self.columns.clone(),
            captured_rows: self.captured_rows,
            total_rows: self.total_rows,
            wire_bytes: self.wire_bytes,
            file_bytes: self.file_bytes,
            truncated: self.captured_rows < self.total_rows,
            column_facts: self.column_facts.clone(),
            format_version: self.format_version,
            investigation_id: self.investigation_id.clone(),
        }
    }
}

/// What [`ResultsStore::insert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureInsert {
    /// The row is recorded.
    Inserted,
    /// A row with this id already exists (23505 on `captured_results_pkey`)
    /// and nothing was written. Ids are random 128-bit values, so this is a
    /// generator fault; the caller draws a new id or gives up, never
    /// overwrites.
    IdTaken,
}

/// The step an attached capture's kept insert fills
/// ([`ResultsStore::insert_kept`]).
#[derive(Debug, Clone, Copy)]
pub struct KeptStep<'a> {
    /// The step's entry in the investigation's log.
    pub seq: u64,
    /// The outcome it records: the capture's success.
    pub outcome: &'a StepOutcome,
    /// How far the write slides the evidence clock.
    pub retention: RetentionRule,
}

/// What [`ResultsStore::insert_kept`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeptInsert {
    /// The row is recorded kept, its step filled and the investigation
    /// advanced. `pruned` are the files of what the due-write rule pruned,
    /// for the caller to unlink.
    Inserted {
        /// The pruned evidence's files.
        pruned: Vec<ExpiredFile>,
    },
    /// A row with this id already exists and nothing was written, as for
    /// [`CaptureInsert::IdTaken`].
    IdTaken,
    /// The step already has an outcome, so nothing was written: the work
    /// that would be kept is no longer the step's.
    StepSettled,
}

/// Committed live usage: how many results and how many file bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Live results.
    pub results: u64,
    /// Sum of their `file_bytes`.
    pub bytes: u64,
}

/// A retired result whose file the caller must unlink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredFile {
    /// The result id, which is also the file's name.
    pub id: String,
    /// The file's size, zero when the result has no file.
    pub file_bytes: u64,
}

/// Faults a test injects into a capture's row commit, so the capture
/// service's answer to an insert whose outcome it cannot learn is proven
/// rather than reasoned about (ADR-0057; the service's `commit`), the way
/// the pool's seams prove its orderings.
///
/// The insert faults queue: [`Faults::fail_next_insert`],
/// [`Faults::land_next_insert_late`] and [`Faults::hold_next_commit`] each
/// arm the first insert that no earlier call armed, so a test arms a
/// capture's insert and then the fence that follows it, in that order.
///
/// [`Faults::fail_next_insert`]: fault::Faults::fail_next_insert
/// [`Faults::land_next_insert_late`]: fault::Faults::land_next_insert_late
/// [`Faults::hold_next_commit`]: fault::Faults::hold_next_commit
///
/// Compiled for this crate's unit tests and, via the `test-support`
/// feature, its integration tests. No production consumer enables it.
#[cfg(any(test, feature = "test-support"))]
pub mod fault {
    use std::collections::VecDeque;

    use parking_lot::Mutex;
    use sqlx::postgres::PgConnection;
    use sqlx::{Connection as _, Executor as _};
    use tokio::sync::oneshot;

    use super::{Encoded, NewCapture, ResultsStore, StoreError, run_insert};

    /// How an armed [`super::ResultsStore::insert`] fails. Each answers an
    /// error, as a connection lost mid-statement does; they differ in what
    /// the table holds afterwards and in what reading the row back finds.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum InsertFault {
        /// The row commits and the answer is lost: reading it back finds it.
        Committed,
        /// Nothing commits: reading the row back finds nothing.
        Lost,
        /// Nothing commits, and the next read of that row fails too.
        Unreadable,
    }

    /// The next [`super::ResultsStore::insert`], held before its
    /// statement runs: a commit slowed past the capture deadline.
    #[derive(Debug)]
    pub struct InsertHold {
        /// Answers once the insert is parked.
        pub parked: oneshot::Receiver<()>,
        /// Send, or drop, to let the insert run.
        pub release: oneshot::Sender<()>,
    }

    /// The insert's half of an [`InsertHold`], and the expiry's of an
    /// [`ExpiryHold`].
    type HeldInsert = (oneshot::Sender<()>, oneshot::Receiver<()>);

    /// The next [`super::ResultsStore::mark_expired`], held before its
    /// statement runs: a read that found its result past its deadline,
    /// stopped between that read and the expiry it asks for.
    #[derive(Debug)]
    pub struct ExpiryHold {
        /// Answers once the expiry is parked.
        pub parked: oneshot::Receiver<()>,
        /// Send, or drop, to let the expiry run.
        pub release: oneshot::Sender<()>,
    }

    /// The next [`crate::store::InvestigationStore::end_evidence_if_due`],
    /// held before it locks the investigation: a read that found a kept
    /// result past its investigation's retention, stopped between loading
    /// the row and the locked check it asks for.
    #[derive(Debug)]
    pub struct EvidenceEndHold {
        /// Answers once the check is parked.
        pub parked: oneshot::Receiver<()>,
        /// Send, or drop, to let the check run.
        pub release: oneshot::Sender<()>,
    }

    /// An armed [`super::ResultsStore::insert`] answers an error at once,
    /// as a connection lost mid-statement does, while its statement runs
    /// later on another connection: a commit that lands after the answer,
    /// or one that never reached the backend before the fence did.
    #[derive(Debug)]
    pub struct LateInsert {
        /// Send, or drop, to run the statement.
        pub release: oneshot::Sender<()>,
        /// Answers once the statement ran: whether it committed.
        pub landed: oneshot::Receiver<bool>,
    }

    /// The insert's half of a [`LateInsert`].
    #[derive(Debug)]
    pub(super) struct LandingInsert {
        release: oneshot::Receiver<()>,
        landed: oneshot::Sender<bool>,
    }

    impl LandingInsert {
        /// Run `row`'s statement on a connection of `store`'s pool once
        /// released, and answer the error a lost connection answers now.
        pub(super) fn run(
            self,
            store: &ResultsStore,
            row: &NewCapture,
            encoded: Encoded,
        ) -> StoreError {
            let pool = store.pool.clone();
            let row = row.clone();
            tokio::spawn(async move {
                let _ = self.release.await;
                let committed = run_insert(&pool, &row, encoded).await.is_ok();
                let _ = self.landed.send(committed);
            });
            injected()
        }
    }

    /// How a [`CommitHold`]'s transaction ends.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CommitEnd {
        /// `COMMIT`: the row lands.
        Commit,
        /// `ROLLBACK`: the row never commits.
        Rollback,
    }

    /// An armed [`super::ResultsStore::insert`] runs its statement inside a
    /// transaction on a connection the test opened, outside the store's
    /// pool, and answers an error once the statement ran, as a connection
    /// lost while the commit is on its way does. The transaction stays
    /// open, holding the row's primary-key entry, until the test ends it.
    ///
    /// The transaction stands for a Postgres backend, which does not stop
    /// with trawld. So it waits for its end on the runtime that armed the
    /// hold, the test's, and a server runtime that shuts down cannot end it.
    #[derive(Debug)]
    pub struct CommitHold {
        /// Answers once the statement ran.
        pub executed: oneshot::Receiver<()>,
        /// Send how the transaction ends. Dropped, it rolls back.
        pub end: oneshot::Sender<CommitEnd>,
        /// Answers once the transaction ended: whether it committed.
        pub landed: oneshot::Receiver<bool>,
    }

    /// The insert's half of a [`CommitHold`].
    #[derive(Debug)]
    pub(super) struct HeldCommit {
        conn: PgConnection,
        /// The runtime that armed the hold, where the open transaction
        /// waits for its end.
        holder: tokio::runtime::Handle,
        executed: oneshot::Sender<()>,
        end: oneshot::Receiver<CommitEnd>,
        landed: oneshot::Sender<bool>,
    }

    impl HeldCommit {
        /// Run `row`'s statement in a transaction on the hold's connection,
        /// leave the transaction open until the hold ends it, and answer
        /// the error a lost connection answers. A statement that cannot run
        /// answers its own error.
        pub(super) async fn run(self, row: &NewCapture, encoded: Encoded) -> StoreError {
            let Self {
                mut conn,
                holder,
                executed,
                end,
                landed,
            } = self;
            if let Err(e) = conn.execute("BEGIN").await {
                return StoreError::from(e);
            }
            if let Err(e) = run_insert(&mut conn, row, encoded).await {
                return StoreError::from(e);
            }
            let _ = executed.send(());
            holder.spawn(async move {
                let end = end.await.unwrap_or(CommitEnd::Rollback);
                let statement = match end {
                    CommitEnd::Commit => "COMMIT",
                    CommitEnd::Rollback => "ROLLBACK",
                };
                let ended = conn.execute(statement).await.is_ok();
                let _ = conn.close().await;
                let _ = landed.send(ended && end == CommitEnd::Commit);
            });
            injected()
        }
    }

    /// The grace of the next capture or promotion whose commit is in doubt
    /// (the capture service's `COMMIT_GRACE`), held open until released
    /// instead of slept out.
    #[derive(Debug)]
    pub struct GraceHold {
        /// Answers once the grace began.
        pub parked: oneshot::Receiver<()>,
        /// Send, or drop, to end the grace.
        pub release: oneshot::Sender<()>,
        /// Answers once the commit is settled and nothing is held: a
        /// capture's file decided, its place in flight and its reservation
        /// given up, or a promotion's charge done.
        pub settled: oneshot::Receiver<()>,
    }

    /// The capture service's half of a [`GraceHold`].
    #[derive(Debug)]
    pub struct HeldGrace {
        parked: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
        settled: oneshot::Sender<()>,
    }

    impl HeldGrace {
        /// Say the grace began and wait for its end. Returns what to send
        /// once the capture is settled.
        pub(crate) async fn wait(self) -> oneshot::Sender<()> {
            let _ = self.parked.send(());
            let _ = self.release.await;
            self.settled
        }
    }

    /// What one armed insert meets.
    #[derive(Debug)]
    pub(super) enum Armed {
        /// [`Faults::fail_next_insert`].
        Fail(InsertFault),
        /// [`Faults::land_next_insert_late`].
        Late(LandingInsert),
        /// [`Faults::hold_next_commit`].
        Held(HeldCommit),
    }

    /// One store's injected faults, shared by every clone of it and by the
    /// investigation store made from it
    /// ([`super::ResultsStore::investigations`]).
    #[derive(Debug, Default)]
    pub struct Faults {
        /// The armed inserts, first armed first met.
        inserts: Mutex<VecDeque<Armed>>,
        /// The armed promotions
        /// ([`crate::store::InvestigationStore::append_citing`]), first
        /// armed first met.
        citings: Mutex<VecDeque<InsertFault>>,
        /// The id whose next [`super::ResultsStore::get`] fails.
        unreadable: Mutex<Option<String>>,
        /// Whether the next insert arms [`Self::unreadable`] for its row
        /// ([`Self::fail_read_back_of_next_insert`]).
        read_back_fails: Mutex<bool>,
        held_insert: Mutex<Option<HeldInsert>>,
        held_expiry: Mutex<Option<HeldInsert>>,
        held_evidence_end: Mutex<Option<HeldInsert>>,
        held_grace: Mutex<Option<HeldGrace>>,
    }

    impl Faults {
        /// Fail the first insert no earlier call armed as `fault` says. One
        /// insert only; an insert nothing armed runs as usual.
        pub fn fail_next_insert(&self, fault: InsertFault) {
            self.inserts.lock().push_back(Armed::Fail(fault));
        }

        /// What the insert of row `id` meets, arming the read fault of an
        /// [`InsertFault::Unreadable`] one, or of
        /// [`Self::fail_read_back_of_next_insert`], for that row.
        pub(super) fn take_insert(&self, id: &str) -> Option<Armed> {
            let armed = self.inserts.lock().pop_front();
            let read_back_fails = std::mem::take(&mut *self.read_back_fails.lock());
            if read_back_fails || matches!(armed, Some(Armed::Fail(InsertFault::Unreadable))) {
                *self.unreadable.lock() = Some(id.to_owned());
            }
            armed
        }

        /// Fail the first read of the row the next insert writes, as a
        /// connection lost on the read back does, whatever the insert
        /// meets. One insert only.
        pub fn fail_read_back_of_next_insert(&self) {
            *self.read_back_fails.lock() = true;
        }

        /// Park the next insert before its statement until the hold's
        /// `release` is sent or dropped. One insert only; it then runs as
        /// usual, meeting the fault armed for it.
        pub fn hold_next_insert(&self) -> InsertHold {
            let (parked_tx, parked) = oneshot::channel();
            let (release, release_rx) = oneshot::channel();
            *self.held_insert.lock() = Some((parked_tx, release_rx));
            InsertHold { parked, release }
        }

        /// Wait out a held insert, if this one is held.
        pub(super) async fn wait_if_held(&self) {
            let held = self.held_insert.lock().take();
            if let Some((parked, release)) = held {
                let _ = parked.send(());
                let _ = release.await;
            }
        }

        /// Park the next [`super::ResultsStore::mark_expired`] before its
        /// statement until the hold's `release` is sent or dropped. One
        /// expiry only.
        pub fn hold_next_expiry(&self) -> ExpiryHold {
            let (parked_tx, parked) = oneshot::channel();
            let (release, release_rx) = oneshot::channel();
            *self.held_expiry.lock() = Some((parked_tx, release_rx));
            ExpiryHold { parked, release }
        }

        /// Wait out a held expiry, if this one is held.
        pub(super) async fn wait_if_expiry_held(&self) {
            let held = self.held_expiry.lock().take();
            if let Some((parked, release)) = held {
                let _ = parked.send(());
                let _ = release.await;
            }
        }

        /// Park the next
        /// [`crate::store::InvestigationStore::end_evidence_if_due`] before
        /// it locks the investigation, until the hold's `release` is sent
        /// or dropped. One check only.
        pub fn hold_next_evidence_end(&self) -> EvidenceEndHold {
            let (parked_tx, parked) = oneshot::channel();
            let (release, release_rx) = oneshot::channel();
            *self.held_evidence_end.lock() = Some((parked_tx, release_rx));
            EvidenceEndHold { parked, release }
        }

        /// Wait out a held evidence check, if this one is held.
        pub(crate) async fn wait_if_evidence_end_held(&self) {
            let held = self.held_evidence_end.lock().take();
            if let Some((parked, release)) = held {
                let _ = parked.send(());
                let _ = release.await;
            }
        }

        /// Answer the first insert no earlier call armed with an error at
        /// once, and run its statement on another connection once the
        /// returned `release` is sent or dropped ([`LateInsert`]).
        pub fn land_next_insert_late(&self) -> LateInsert {
            let (release, release_rx) = oneshot::channel();
            let (landed_tx, landed) = oneshot::channel();
            self.inserts.lock().push_back(Armed::Late(LandingInsert {
                release: release_rx,
                landed: landed_tx,
            }));
            LateInsert { release, landed }
        }

        /// Run the first insert no earlier call armed on `conn`, in a
        /// transaction held open until the returned hold ends it
        /// ([`CommitHold`]). `conn` reaches the store's database; the test
        /// opens it, so the open transaction takes none of the pool's
        /// connections. Call it on the runtime the transaction waits on.
        pub fn hold_next_commit(&self, conn: PgConnection) -> CommitHold {
            let (executed_tx, executed) = oneshot::channel();
            let (end, end_rx) = oneshot::channel();
            let (landed_tx, landed) = oneshot::channel();
            self.inserts.lock().push_back(Armed::Held(HeldCommit {
                conn,
                holder: tokio::runtime::Handle::current(),
                executed: executed_tx,
                end: end_rx,
                landed: landed_tx,
            }));
            CommitHold {
                executed,
                end,
                landed,
            }
        }

        /// Hold the grace of the next capture or promotion whose commit is
        /// in doubt until the returned `release` is sent or dropped
        /// ([`GraceHold`]). One grace only.
        pub fn hold_next_grace(&self) -> GraceHold {
            let (parked_tx, parked) = oneshot::channel();
            let (release, release_rx) = oneshot::channel();
            let (settled_tx, settled) = oneshot::channel();
            *self.held_grace.lock() = Some(HeldGrace {
                parked: parked_tx,
                release: release_rx,
                settled: settled_tx,
            });
            GraceHold {
                parked,
                release,
                settled,
            }
        }

        /// The held grace, if one is armed.
        pub(crate) fn take_grace(&self) -> Option<HeldGrace> {
            self.held_grace.lock().take()
        }

        /// Fail the first promotion no earlier call armed as `fault` says:
        /// [`InsertFault::Committed`] commits it and answers an error, and
        /// either other fault answers an error with nothing written. One
        /// promotion only.
        pub fn fail_next_citing(&self, fault: InsertFault) {
            self.citings.lock().push_back(fault);
        }

        /// What the next promotion meets.
        pub(crate) fn take_citing(&self) -> Option<InsertFault> {
            self.citings.lock().pop_front()
        }

        /// Whether this read of row `id` fails, disarming it if so.
        pub(super) fn take_unreadable(&self, id: &str) -> bool {
            let mut unreadable = self.unreadable.lock();
            let fails = unreadable.as_deref() == Some(id);
            if fails {
                *unreadable = None;
            }
            fails
        }
    }

    /// What an injected fault answers: the store unavailable, as a dropped
    /// connection leaves it.
    pub(crate) fn injected() -> StoreError {
        StoreError::Unavailable(sqlx::Error::Protocol("injected store fault".into()))
    }
}

/// Postgres-backed storage for captured-result metadata. Cheap to clone
/// (shared pool).
#[derive(Debug, Clone)]
pub struct ResultsStore {
    pool: PgPool,
    /// Faults a test injected ([`fault`]); clones share them.
    #[cfg(any(test, feature = "test-support"))]
    faults: std::sync::Arc<fault::Faults>,
}

impl ResultsStore {
    /// Wrap the shared app-state pool, which must point at a migrated trawl
    /// app-state database.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            #[cfg(any(test, feature = "test-support"))]
            faults: std::sync::Arc::default(),
        }
    }

    /// This store's fault table, shared by its clones: the one the server's
    /// captured-results state reads through.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn faults(&self) -> &fault::Faults {
        &self.faults
    }

    /// Record a new temporary capture.
    ///
    /// A row that names an investigation is refused with
    /// [`StoreError::Validation`]: a kept row is recorded only by
    /// [`Self::insert_kept`], inside its investigation's lock.
    ///
    /// A row that breaks a table constraint (a malformed id, a digest
    /// without a file) is a server fault and surfaces as
    /// [`StoreError::Unavailable`] carrying the database error, which names
    /// the constraint.
    pub async fn insert(&self, row: &NewCapture) -> Result<CaptureInsert, StoreError> {
        if row.investigation_id.is_some() {
            return Err(StoreError::Validation(
                "a kept capture is recorded with its investigation's step".into(),
            ));
        }
        let encoded = Encoded::of(row)?;
        #[cfg(any(test, feature = "test-support"))]
        self.faults.wait_if_held().await;
        #[cfg(any(test, feature = "test-support"))]
        let committed_fault = match self.faults.take_insert(&row.id) {
            None => false,
            Some(fault::Armed::Fail(fault::InsertFault::Committed)) => true,
            Some(fault::Armed::Fail(fault::InsertFault::Lost | fault::InsertFault::Unreadable)) => {
                return Err(fault::injected());
            }
            Some(fault::Armed::Late(late)) => return Err(late.run(self, row, encoded)),
            Some(fault::Armed::Held(held)) => return Err(held.run(row, encoded).await),
        };
        let inserted = run_insert(&self.pool, row, encoded).await;
        #[cfg(any(test, feature = "test-support"))]
        if committed_fault && inserted.is_ok() {
            return Err(fault::injected());
        }
        match inserted {
            Ok(_) => Ok(CaptureInsert::Inserted),
            Err(e) => match classify_violation(&e) {
                Some(PgViolation::ResultIdTaken) => Ok(CaptureInsert::IdTaken),
                Some(PgViolation::UnknownInvestigation) => Err(StoreError::InvestigationNotFound),
                _ => Err(StoreError::from(e)),
            },
        }
    }

    /// Record an attached capture kept from birth (ADR-0059): the row
    /// `kept` with no deadline in its investigation, the step's outcome and
    /// the investigation's next sequence, in one transaction.
    ///
    /// The transaction locks the investigation row `FOR UPDATE` before it
    /// touches `captured_results`, through the due-write rule's
    /// [`investigations::lock_for_write`]: evidence whose deadline ended is
    /// pruned first, so the new row never joins evidence that should be
    /// gone, and the row's foreign key finds its key share already
    /// covered. Investigation before results is the order every
    /// transaction that takes both locks uses
    /// ([`Self::prune_evidence`], [`InvestigationStore::delete`]); a
    /// statement that locked a result first and then waited on the
    /// investigation would deadlock with them. Once the step is filled and
    /// the clock slid, an investigation whose deadline is pinned at its
    /// ceiling is still due, and the rule prunes again, the new row
    /// included.
    ///
    /// `row.created_at` is the write's instant. The transaction does not
    /// read the investigation's status: work admitted while it was open
    /// finishes and records its outcome after a close. A missing
    /// investigation is [`StoreError::InvestigationNotFound`]; a taken id
    /// is [`KeptInsert::IdTaken`] and a step with an outcome
    /// [`KeptInsert::StepSettled`], each with nothing written. The
    /// [`fault`] table's [`fault::Faults::fail_next_insert`] and
    /// [`fault::Faults::hold_next_insert`] apply as to [`Self::insert`].
    pub async fn insert_kept(
        &self,
        row: &NewCapture,
        step: KeptStep<'_>,
    ) -> Result<KeptInsert, StoreError> {
        let Some(investigation) = row.investigation_id.as_ref() else {
            return Err(StoreError::Validation(
                "a kept capture names its investigation".into(),
            ));
        };
        let encoded = Encoded::of(row)?;
        #[cfg(any(test, feature = "test-support"))]
        self.faults.wait_if_held().await;
        #[cfg(any(test, feature = "test-support"))]
        let committed_fault = match self.faults.take_insert(&row.id) {
            None => false,
            Some(fault::Armed::Fail(fault::InsertFault::Committed)) => true,
            Some(fault::Armed::Fail(fault::InsertFault::Lost | fault::InsertFault::Unreadable)) => {
                return Err(fault::injected());
            }
            Some(fault::Armed::Late(_) | fault::Armed::Held(_)) => {
                return Err(StoreError::Validation(
                    "a late or held commit is not armed for a kept insert".into(),
                ));
            }
        };
        let inserted = self
            .kept_transaction(row, investigation, encoded, step)
            .await;
        #[cfg(any(test, feature = "test-support"))]
        if committed_fault && matches!(inserted, Ok(KeptInsert::Inserted { .. })) {
            return Err(fault::injected());
        }
        inserted
    }

    async fn kept_transaction(
        &self,
        row: &NewCapture,
        investigation: &InvestigationId,
        encoded: Encoded,
        step: KeptStep<'_>,
    ) -> Result<KeptInsert, StoreError> {
        let now = row.created_at;
        let mut tx = self.pool.begin().await?;
        let (locked, mut pruned) =
            investigations::lock_for_write(&mut tx, investigation, now).await?;
        if let Err(e) = run_insert(&mut *tx, row, encoded).await {
            tx.rollback().await?;
            return match classify_violation(&e) {
                Some(PgViolation::ResultIdTaken) => Ok(KeptInsert::IdTaken),
                Some(PgViolation::UnknownInvestigation) => Err(StoreError::InvestigationNotFound),
                _ => Err(StoreError::from(e)),
            };
        }
        let filled = investigations::fill_step_in(
            &mut tx,
            investigation,
            locked,
            step.seq,
            step.outcome,
            now,
            Some(step.retention),
        )
        .await?;
        if !filled {
            tx.rollback().await?;
            return Ok(KeptInsert::StepSettled);
        }
        pruned.extend(investigations::prune_if_due(&mut tx, investigation, now).await?);
        tx.commit().await?;
        Ok(KeptInsert::Inserted { pruned })
    }

    /// A kept row with no step, for a test's fixture: inserted inside its
    /// investigation's lock, through the due-write rule, without advancing
    /// the investigation. Answers what [`Self::insert`] answers.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn insert_kept_unlogged(
        &self,
        row: &NewCapture,
    ) -> Result<CaptureInsert, StoreError> {
        let Some(investigation) = row.investigation_id.as_ref() else {
            return Err(StoreError::Validation(
                "a kept capture names its investigation".into(),
            ));
        };
        let encoded = Encoded::of(row)?;
        let mut tx = self.pool.begin().await?;
        investigations::lock_for_write(&mut tx, investigation, row.created_at).await?;
        let inserted = run_insert(&mut *tx, row, encoded).await;
        match inserted {
            Ok(_) => {
                tx.commit().await?;
                Ok(CaptureInsert::Inserted)
            }
            Err(e) => {
                tx.rollback().await?;
                match classify_violation(&e) {
                    Some(PgViolation::ResultIdTaken) => Ok(CaptureInsert::IdTaken),
                    _ => Err(StoreError::from(e)),
                }
            }
        }
    }

    /// The investigation store on this store's pool: the evidence usage the
    /// capture gate reads, and the server's investigation store. It shares
    /// this store's [`fault`] table.
    #[must_use]
    pub(crate) fn investigations(&self) -> InvestigationStore {
        #[cfg(feature = "test-support")]
        return InvestigationStore::sharing(self.pool.clone(), std::sync::Arc::clone(&self.faults));
        #[cfg(not(feature = "test-support"))]
        InvestigationStore::new(self.pool.clone())
    }

    /// The row `id` owned by `key_id`, in any state. Another key's result
    /// reads as absent.
    pub async fn get(&self, id: &str, key_id: i64) -> Result<Option<CaptureRow>, StoreError> {
        #[cfg(any(test, feature = "test-support"))]
        if self.faults.take_unreadable(id) {
            return Err(fault::injected());
        }
        let row = sqlx::query(select_rows!("WHERE id = $1 AND key_id = $2"))
            .bind(id)
            .bind(key_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(decode_row).transpose()?)
    }

    /// The row `id` as an investigation reader holding `key_id` reads it
    /// (ADR-0059): the key's own row in any state, or another key's row
    /// that is evidence of an investigation, kept or pruned by its
    /// retention. Another key's temporary, expired or quota-pruned row
    /// reads as absent, the same as no row.
    pub async fn get_readable(
        &self,
        id: &str,
        key_id: i64,
    ) -> Result<Option<CaptureRow>, StoreError> {
        #[cfg(any(test, feature = "test-support"))]
        if self.faults.take_unreadable(id) {
            return Err(fault::injected());
        }
        let row = sqlx::query(select_rows!(
            "WHERE id = $1 AND (key_id = $2
                OR (investigation_id IS NOT NULL AND state IN ('kept', 'pruned')))"
        ))
        .bind(id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(decode_row).transpose()?)
    }

    /// The key's live temporary results at `now`, newest first: the
    /// owner's recovery list. A kept result is reached through its
    /// investigation instead.
    pub async fn list(
        &self,
        key_id: i64,
        now: DateTime<Utc>,
    ) -> Result<Vec<CaptureRow>, StoreError> {
        let rows = sqlx::query(select_rows!(concat!(
            "WHERE key_id = $1 AND ",
            temporary_live_at!("$2"),
            " ORDER BY created_at DESC, id DESC"
        )))
        .bind(key_id)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(decode_row).collect::<Result<_, _>>()?)
    }

    /// Mark one temporary result `expired` because a read found it past its
    /// deadline. Its `gone_at` is the deadline, not `now`: the tombstone
    /// clock starts at the instant the result was lost.
    ///
    /// Only a temporary row with `expires_at <= now` changes, so a call can
    /// never retire a live result early. Returns whether a row changed; a
    /// second call returns `false`.
    pub async fn mark_expired(&self, id: &str, now: DateTime<Utc>) -> Result<bool, StoreError> {
        #[cfg(any(test, feature = "test-support"))]
        self.faults.wait_if_expiry_held().await;
        let changed = sqlx::query(
            "UPDATE captured_results SET state = 'expired', gone_at = expires_at
             WHERE id = $1 AND state = 'temporary' AND expires_at <= $2",
        )
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed == 1)
    }

    /// Mark every temporary result past its deadline at `now` as `expired`,
    /// with `gone_at` at its deadline, in one statement. Returns the files
    /// to unlink.
    pub async fn expire_due(&self, now: DateTime<Utc>) -> Result<Vec<ExpiredFile>, StoreError> {
        let rows = sqlx::query(
            "UPDATE captured_results SET state = 'expired', gone_at = expires_at
             WHERE state = 'temporary' AND expires_at <= $1
             RETURNING id, file_bytes",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(decode_file).collect::<Result<_, _>>()?)
    }

    /// Record that a live result's file is absent: a `temporary` or `kept`
    /// row. The server unlinks a gone row's file after marking the row, so
    /// a gone row lost nothing and is left alone, whatever a reader that
    /// read it live a moment earlier asks. Returns whether the flag
    /// changed; a second call returns `false`.
    pub async fn mark_file_missing(&self, id: &str) -> Result<bool, StoreError> {
        let changed = sqlx::query(
            "UPDATE captured_results SET file_missing = TRUE
             WHERE id = $1 AND NOT file_missing AND state IN ('temporary', 'kept')",
        )
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed == 1)
    }

    /// Delete the tombstones that became unreadable at or before `before`
    /// (the caller passes `now` minus the tombstone lifetime). Returns how
    /// many rows went.
    ///
    /// A pruned row of an investigation stays: a finding still cites it,
    /// and it reads as pruned for as long as the investigation exists.
    /// Deleting the investigation clears the row's `investigation_id`, and
    /// the row then ages out like any other tombstone.
    pub async fn delete_tombstones(&self, before: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "DELETE FROM captured_results WHERE gone_at <= $1 AND investigation_id IS NULL",
        )
        .bind(before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// The key's committed live temporary usage at `now`: what its capture
    /// quota counts. Kept rows are evidence and count elsewhere.
    ///
    /// Committed rows only: reservations for captures still running are
    /// the capture gate's business, and it adds them to this.
    pub async fn usage(&self, key_id: i64, now: DateTime<Utc>) -> Result<Usage, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT count(*) AS results, COALESCE(sum(file_bytes), 0)::bigint AS bytes
             FROM captured_results WHERE key_id = $1 AND ",
            temporary_live_at!("$2")
        ))
        .bind(key_id)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(decode_usage(&row)?)
    }

    /// The installation's committed live temporary usage at `now`, across
    /// every key.
    ///
    /// Committed rows only, as for [`Self::usage`].
    pub async fn installation_usage(&self, now: DateTime<Utc>) -> Result<Usage, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT count(*) AS results, COALESCE(sum(file_bytes), 0)::bigint AS bytes
             FROM captured_results WHERE ",
            temporary_live_at!("$1")
        ))
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(decode_usage(&row)?)
    }

    /// When the key's first live temporary result expires, after `now`, for
    /// a quota refusal's `Retry-After`. `None` when the key holds no live
    /// temporary result: a kept one never expires.
    pub async fn earliest_expiry(
        &self,
        key_id: i64,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT min(expires_at) FROM captured_results
             WHERE key_id = $1 AND state = 'temporary' AND expires_at > $2",
        )
        .bind(key_id)
        .bind(now)
        .fetch_one(&self.pool)
        .await?)
    }

    /// [`Self::earliest_expiry`] across every key.
    pub async fn installation_earliest_expiry(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT min(expires_at) FROM captured_results
             WHERE state = 'temporary' AND expires_at > $1",
        )
        .bind(now)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Every result in `temporary` or `kept` with its file size, ordered by
    /// id, for boot reconciliation against the results root. A temporary
    /// row past its deadline is included: no sweep has retired it yet, so
    /// its file is still expected.
    pub async fn live_ids(&self) -> Result<Vec<(String, u64)>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, file_bytes FROM captured_results
             WHERE state IN ('temporary', 'kept') ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| decode_file(row).map(|file| (file.id, file.file_bytes)))
            .collect::<Result<_, _>>()
            .map_err(StoreError::from)
    }

    /// The keys holding a live temporary result at `now`, ascending, for
    /// boot's per-key quota pass.
    pub async fn live_keys(&self, now: DateTime<Utc>) -> Result<Vec<i64>, StoreError> {
        Ok(sqlx::query_scalar(concat!(
            "SELECT DISTINCT key_id FROM captured_results WHERE ",
            temporary_live_at!("$1"),
            " ORDER BY key_id"
        ))
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Prune the key's oldest temporary results until its live temporary
    /// usage at `now` is at most `keep_results` results and at most
    /// `keep_bytes` file bytes (ADR-0057: reached when an operator lowers a
    /// quota below usage). Pruned rows get `gone_at = now`. Returns the
    /// files to unlink, oldest first.
    ///
    /// Kept results neither count toward the usage nor are pruned here:
    /// they are evidence, bounded by the evidence budget and pruned when
    /// their investigation's retention ends ([`Self::prune_evidence`]).
    ///
    /// The key's live temporary rows are read `FOR UPDATE` and pruned in
    /// the same transaction, so a concurrent expiry or prune of those rows
    /// waits for it. A capture committed meanwhile is not counted; boot
    /// runs this before the server takes captures.
    pub async fn prune_oldest(
        &self,
        key_id: i64,
        keep_results: u64,
        keep_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<Vec<ExpiredFile>, StoreError> {
        self.prune(Some(key_id), keep_results, keep_bytes, now)
            .await
    }

    /// [`Self::prune_oldest`] across every key: prune the installation's
    /// oldest temporary results, whoever owns them, until its live
    /// temporary usage at `now` is at most `keep_results` results and
    /// `keep_bytes` file bytes. Kept results neither count nor are pruned,
    /// as for one key.
    ///
    /// Every live temporary row is read `FOR UPDATE`; boot runs this after
    /// the per-key pass and before the server takes captures.
    pub async fn prune_oldest_installation(
        &self,
        keep_results: u64,
        keep_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<Vec<ExpiredFile>, StoreError> {
        self.prune(None, keep_results, keep_bytes, now).await
    }

    /// Promote the key's own live temporary result `id` into
    /// `investigation`, for a test's fixture: inside the investigation's
    /// lock, through the due-write rule, without a log entry or an advance.
    /// Answers what [`promote_in`] answers.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn promote_unlogged(
        &self,
        id: &str,
        key_id: i64,
        investigation: &InvestigationId,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        investigations::lock_for_write(&mut tx, investigation, now).await?;
        let promoted = promote_in(&mut tx, id, key_id, investigation, now).await?;
        tx.commit().await?;
        Ok(promoted)
    }

    /// Prune every kept result whose investigation's `retain_until` is at
    /// or before `now` (ADR-0059: the evidence clock ended). The row stays
    /// with its investigation, `pruned`, with `expires_at` and `gone_at` at
    /// the deadline it passed, so it reads as pruned while the findings
    /// that cite it remain. Returns the files to unlink.
    ///
    /// One transaction locks every due investigation `FOR UPDATE` in id
    /// order, the investigation before its results as every investigation
    /// write does, so the prune serialises with each write that moves the
    /// deadline. A write that commits a later deadline while the prune
    /// waits for its row takes that investigation out of the prune; one
    /// that waits behind the prune finds its evidence already pruned.
    pub async fn prune_evidence(&self, now: DateTime<Utc>) -> Result<Vec<ExpiredFile>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let files = prune_due_evidence(&mut tx, now).await?;
        tx.commit().await?;
        Ok(files)
    }

    /// [`Self::prune_evidence`] inside the caller's transaction, which then
    /// holds the due investigations' locks until it ends: the seam a test
    /// stages a write behind an uncommitted prune with.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn prune_evidence_in(
        tx: &mut PgConnection,
        now: DateTime<Utc>,
    ) -> Result<Vec<ExpiredFile>, StoreError> {
        prune_due_evidence(tx, now).await
    }

    /// Prune the oldest temporary results of `key_id`, or of the whole
    /// installation for `None`, down to the limits.
    async fn prune(
        &self,
        key_id: Option<i64>,
        keep_results: u64,
        keep_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<Vec<ExpiredFile>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let rows = match key_id {
            Some(key_id) => {
                sqlx::query(concat!(
                    "SELECT id, file_bytes FROM captured_results
                     WHERE key_id = $1 AND ",
                    temporary_live_at!("$2"),
                    " ORDER BY created_at, id FOR UPDATE"
                ))
                .bind(key_id)
                .bind(now)
                .fetch_all(&mut *tx)
                .await?
            }
            None => {
                sqlx::query(concat!(
                    "SELECT id, file_bytes FROM captured_results WHERE ",
                    temporary_live_at!("$1"),
                    " ORDER BY created_at, id FOR UPDATE"
                ))
                .bind(now)
                .fetch_all(&mut *tx)
                .await?
            }
        };

        let mut results = 0_u64;
        let mut bytes = 0_u64;
        let mut temporary = Vec::new();
        for row in &rows {
            let file = decode_file(row)?;
            results += 1;
            bytes = bytes.saturating_add(file.file_bytes);
            temporary.push(file);
        }

        let mut pruned = Vec::new();
        for file in temporary {
            if results <= keep_results && bytes <= keep_bytes {
                break;
            }
            results -= 1;
            bytes = bytes.saturating_sub(file.file_bytes);
            pruned.push(file);
        }
        if pruned.is_empty() {
            tx.rollback().await?;
            return Ok(pruned);
        }

        let ids: Vec<&str> = pruned.iter().map(|file| file.id.as_str()).collect();
        sqlx::query(
            "UPDATE captured_results SET state = 'pruned', gone_at = $2
             WHERE id = ANY($1)",
        )
        .bind(&ids)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(pruned)
    }
}

/// Promote the key's own live temporary result `id` to kept evidence of
/// `investigation` (ADR-0059), inside the caller's transaction, which
/// holds the investigation's lock ([`investigations::lock_for_write`]):
/// the whole result, no copy. It loses its deadline and leaves the
/// capture quota.
///
/// One conditional statement, so it succeeds once: a result past its
/// deadline at `now`, another key's, one already kept, one gone, or one
/// whose file is missing does not change and the call returns `false`. Only inside the
/// investigation's lock: an update that locked the result first and then
/// waited on the investigation for its foreign key would attach evidence
/// behind a prune of that investigation that could not see it.
pub(crate) async fn promote_in(
    tx: &mut PgConnection,
    id: &str,
    key_id: i64,
    investigation: &InvestigationId,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let changed = sqlx::query(
        "UPDATE captured_results
         SET state = 'kept', expires_at = NULL, investigation_id = $3
         WHERE id = $1 AND key_id = $2 AND state = 'temporary' AND expires_at > $4
           AND investigation_id IS NULL AND NOT file_missing",
    )
    .bind(id)
    .bind(key_id)
    .bind(investigation.as_str())
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|e| match classify_violation(&e) {
        Some(PgViolation::UnknownInvestigation) => StoreError::InvestigationNotFound,
        _ => StoreError::from(e),
    })?
    .rows_affected();
    Ok(changed == 1)
}

/// The state of result `id` and the investigation it is attached to, read
/// inside the caller's transaction; `None` when no row has the id.
pub(crate) async fn attachment_in(
    tx: &mut PgConnection,
    id: &str,
) -> Result<Option<(ResultState, Option<InvestigationId>)>, StoreError> {
    let row = sqlx::query("SELECT state, investigation_id FROM captured_results WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    Ok(row
        .map(|row| Ok::<_, sqlx::Error>((decode_state(&row)?, decode_investigation_id(&row)?)))
        .transpose()?)
}

/// Lock the investigations whose `retain_until` is at or before `now` in
/// id order, then prune their kept results at that deadline, within `tx`.
///
/// Under READ COMMITTED a locked row whose deadline moved while the lock
/// waited is re-read, so the `SELECT` skips it; the `UPDATE` reads the
/// deadline again under the held locks and writes exactly the one it
/// checked.
async fn prune_due_evidence(
    tx: &mut PgConnection,
    now: DateTime<Utc>,
) -> Result<Vec<ExpiredFile>, StoreError> {
    let due: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM investigations WHERE retain_until <= $1 ORDER BY id FOR UPDATE",
    )
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    if due.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "UPDATE captured_results AS c
         SET state = 'pruned', expires_at = i.retain_until, gone_at = i.retain_until
         FROM investigations AS i
         WHERE i.id = ANY($1) AND i.retain_until <= $2
           AND c.investigation_id = i.id AND c.state = 'kept'
         RETURNING c.id, c.file_bytes",
    )
    .bind(&due)
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows.iter().map(decode_file).collect::<Result<_, _>>()?)
}

/// The values of a [`NewCapture`] that the table stores in another shape.
struct Encoded {
    format_version: i16,
    columns: String,
    column_facts: String,
}

impl Encoded {
    fn of(row: &NewCapture) -> Result<Self, StoreError> {
        let format_version = i16::try_from(row.format_version).map_err(|_| {
            StoreError::Validation(format!(
                "format version {} does not fit the table",
                row.format_version
            ))
        })?;
        let columns = serde_json::to_string(&row.columns)
            .map_err(|e| StoreError::Validation(format!("columns do not encode: {e}")))?;
        let column_facts = serde_json::to_string(&row.column_facts)
            .map_err(|e| StoreError::Validation(format!("column facts do not encode: {e}")))?;
        Ok(Self {
            format_version,
            columns,
            column_facts,
        })
    }
}

/// The insert of `row` as one statement: autocommitted on a pool, inside
/// the open transaction of a connection that holds one.
async fn run_insert<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    row: &NewCapture,
    encoded: Encoded,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    // A kept row has no deadline: `captured_results_deadline_state` holds
    // exactly when `expires_at` is NULL for it and set otherwise.
    sqlx::query(
        "INSERT INTO captured_results (
             id, key_id, created_at, expires_at, state, query, executed_query,
             source, shape, earliest, latest, columns, column_facts,
             captured_rows, total_rows, wire_bytes, file_bytes, digest, format_version,
             investigation_id)
         VALUES ($1, $2, $3,
                 CASE WHEN $19::text IS NULL THEN $4 END,
                 CASE WHEN $19::text IS NULL THEN 'temporary' ELSE 'kept' END,
                 $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)",
    )
    .bind(&row.id)
    .bind(row.key_id)
    .bind(row.created_at)
    .bind(row.expires_at)
    .bind(&row.query)
    .bind(&row.executed_query)
    .bind(source_str(row.source))
    .bind(shape_str(row.shape))
    .bind(row.earliest)
    .bind(row.latest)
    .bind(encoded.columns)
    .bind(encoded.column_facts)
    .bind(bind_u64(row.captured_rows))
    .bind(bind_u64(row.total_rows))
    .bind(bind_u64(row.wire_bytes))
    .bind(bind_u64(row.file_bytes))
    .bind(row.digest)
    .bind(encoded.format_version)
    .bind(row.investigation_id.as_ref().map(InvestigationId::as_str))
    .execute(executor)
    .await
}

/// A timestamp as the wire spells it.
pub(super) fn wire_time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn source_str(source: ResultSource) -> &'static str {
    match source {
        ResultSource::Live => "live",
        ResultSource::Stored => "stored",
    }
}

fn shape_str(shape: ResultShape) -> &'static str {
    match shape {
        ResultShape::Events => "events",
        ResultShape::Whole => "whole",
    }
}

/// A decode failure in the named column.
fn column_error(
    column: &str,
    source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.to_owned(),
        source: source.into(),
    }
}

fn decode_text<T>(
    row: &PgRow,
    column: &str,
    parse: fn(&str) -> Option<T>,
) -> Result<T, sqlx::Error> {
    let text: String = row.try_get(column)?;
    parse(&text).ok_or_else(|| column_error(column, format!("unknown value {text:?}")))
}

fn decode_state(row: &PgRow) -> Result<ResultState, sqlx::Error> {
    decode_text(row, "state", |text| match text {
        "temporary" => Some(ResultState::Temporary),
        "kept" => Some(ResultState::Kept),
        "expired" => Some(ResultState::Expired),
        "pruned" => Some(ResultState::Pruned),
        _ => None,
    })
}

fn decode_u64(row: &PgRow, column: &str) -> Result<u64, sqlx::Error> {
    let value: i64 = row.try_get(column)?;
    u64::try_from(value).map_err(|e| column_error(column, e))
}

fn decode_json<T: serde::de::DeserializeOwned>(
    row: &PgRow,
    column: &str,
) -> Result<T, sqlx::Error> {
    let text: String = row.try_get(column)?;
    serde_json::from_str(&text).map_err(|e| column_error(column, e))
}

fn decode_row(row: &PgRow) -> Result<CaptureRow, sqlx::Error> {
    let format_version: i16 = row.try_get("format_version")?;
    Ok(CaptureRow {
        id: row.try_get("id")?,
        key_id: row.try_get("key_id")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        gone_at: row.try_get("gone_at")?,
        state: decode_state(row)?,
        query: row.try_get("query")?,
        executed_query: row.try_get("executed_query")?,
        source: decode_text(row, "source", |text| match text {
            "live" => Some(ResultSource::Live),
            "stored" => Some(ResultSource::Stored),
            _ => None,
        })?,
        shape: decode_text(row, "shape", |text| match text {
            "events" => Some(ResultShape::Events),
            "whole" => Some(ResultShape::Whole),
            _ => None,
        })?,
        earliest: row.try_get("earliest")?,
        latest: row.try_get("latest")?,
        columns: decode_json(row, "columns")?,
        column_facts: decode_json(row, "column_facts")?,
        captured_rows: decode_u64(row, "captured_rows")?,
        total_rows: decode_u64(row, "total_rows")?,
        wire_bytes: decode_u64(row, "wire_bytes")?,
        file_bytes: decode_u64(row, "file_bytes")?,
        digest: row.try_get("digest")?,
        file_missing: row.try_get("file_missing")?,
        format_version: u16::try_from(format_version)
            .map_err(|e| column_error("format_version", e))?,
        investigation_id: decode_investigation_id(row)?,
        retain_until: row.try_get("retain_until")?,
    })
}

fn decode_investigation_id(row: &PgRow) -> Result<Option<InvestigationId>, sqlx::Error> {
    let text: Option<String> = row.try_get("investigation_id")?;
    text.map(|text| InvestigationId::parse(&text).map_err(|e| column_error("investigation_id", e)))
        .transpose()
}

fn decode_file(row: &PgRow) -> Result<ExpiredFile, sqlx::Error> {
    Ok(ExpiredFile {
        id: row.try_get("id")?,
        file_bytes: decode_u64(row, "file_bytes")?,
    })
}

fn decode_usage(row: &PgRow) -> Result<Usage, sqlx::Error> {
    Ok(Usage {
        results: decode_u64(row, "results")?,
        bytes: decode_u64(row, "bytes")?,
    })
}
