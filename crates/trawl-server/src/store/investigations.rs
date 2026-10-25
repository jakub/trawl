// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed investigations and their sequenced logs (ADR-0059).
//!
//! An investigation is installation-owned. No statement here is scoped by
//! a key id, the deliberate break from [`super::saved`]: the permissions
//! decide who reads and writes, and the key ids stored here are attribution
//! beside a name snapshot.
//!
//! Every write advances the investigation's `seq` by one inside one
//! transaction that first locks the investigation row `FOR UPDATE`. The
//! lock is what orders concurrent writers: two appends from two
//! connections both commit, one at `seq + 1` and the next at `seq + 2`.
//! Appends carry no expected sequence. Overwrites compare the sequence the
//! caller observed with the locked row's and fail with
//! [`StoreError::SequenceConflict`] when it moved. A closed investigation
//! accepts nothing new ([`StoreError::InvestigationClosed`]) but a reopen,
//! and work admitted before it closed still completes.
//!
//! A transaction that locks investigations and their results locks the
//! investigations first, several in id order:
//! [`InvestigationStore::delete`] its one,
//! [`InvestigationStore::append_citing`] and
//! [`super::ResultsStore::insert_kept`] the one their evidence joins, and
//! [`super::ResultsStore::prune_evidence`] every one past its deadline. So
//! the evidence prune waits for a write that moves the deadline and sees
//! the deadline it committed, and no evidence attaches behind a prune that
//! could not see it.
//!
//! Every write a principal makes advances the sequence and slides the
//! evidence clock: `retain_until = GREATEST(retain_until,
//! LEAST(retain_ceiling, now + idle))`. Boot's interruptions advance the
//! sequence and slide nothing, since no principal changed anything
//! ([`InvestigationStore::interrupt_running_steps`]). Reads, refused writes
//! and a replayed create slide nothing.
//!
//! A write never revives evidence its deadline already ended: the due-write
//! rule. Each write locks through [`lock_for_write`], which prunes the
//! investigation's kept results first when its `retain_until` is at or
//! before `now`, exactly as the sweep would, and only then writes and
//! slides. So a read that answered pruned stays consistent with any later
//! write, and the sweep's hourly cadence never decides what survives.
//! [`InvestigationStore::append`], [`InvestigationStore::append_citing`],
//! [`InvestigationStore::begin_step`] and
//! [`super::ResultsStore::insert_kept`] return the files of what they
//! pruned, for the caller to unlink. The
//! other writes leave those files to the sweep, which removes a file no
//! live row names: the rows are pruned, so no read reaches them.
//!
//! The two writes that admit work, [`InvestigationStore::begin_step`] and
//! [`InvestigationStore::append_citing`], run in tasks that outlive their
//! request, and so does the `aborted` outcome such a task writes when the
//! request is gone ([`InvestigationStore::complete_step_until`]). Their
//! wait for a connection and every statement before the commit end at the
//! caller's deadline, with nothing written ([`StoreError::WaitExpired`]).
//! The connection's wait is the one bounded here, and the only await cut
//! short: nothing has been sent yet. The `BEGIN` is awaited to its answer,
//! since a `BEGIN` cut short would hand the pool a session still inside
//! the transaction ([`PooledTransaction`]). Every statement after it is
//! bounded in Postgres, by a `statement_timeout` set from what remains of
//! the deadline before each. The setting covers all of a statement's lock
//! waits and the work between them, so a statement that waits on one row
//! after another, or one that waits after an earlier statement did, still
//! ends at the deadline. A write's transaction is a [`WriteTx`], whose
//! connection is reachable only one statement at a time, each bounded so
//! ([`WriteTx::run`]); every other write runs the same code unbounded. So
//! no statement is cut short from this side, and every wait ends with the
//! database's own answer. A write that does not commit rolls back and
//! waits for the rollback to be answered before it returns, since a
//! transaction dropped open ends only when Postgres sees its session
//! close: when the write answers, Postgres has stopped. Their commit is
//! never cut short: the timeout is lifted first.
//!
//! Every instant is a parameter from the caller's clock; no statement reads
//! SQL `now()`. Payloads are the trawl-api JSON of [`EntryPayload`], stored
//! as text (JSONB refuses `\u0000`). A row that does not decode is a
//! [`StoreError::Unavailable`] carrying a [`sqlx::Error::ColumnDecode`]
//! that names the column.

use chrono::{DateTime, Duration, Utc};
use sqlx::postgres::{PgConnection, PgRow};
use sqlx::{PgPool, Row as _};
use trawl_api::ErrorCode;
use trawl_api::investigations::{
    Activity, Actor, CreateInvestigation, EntryKind, EntryPayload, FindingStatus, Investigation,
    InvestigationId, InvestigationStatus, InvestigationSummary, LogEntry, StepOutcome,
    StepOutcomeKind,
};
use trawl_api::results::ResultState;

use super::error::StoreError;
use super::history::bind_u64;
use super::pooled::PooledTransaction;
use super::results::{ExpiredFile, wire_time};
use crate::deadline::{Deadline, Expired};

/// Most investigations [`InvestigationStore::list`] answers.
pub const MAX_LIST: u64 = 200;

/// How many times a create retries when the id it conflicted on vanished
/// before it could be read: an operator deleted it in between.
const CREATE_ATTEMPTS: usize = 3;

/// Every column of `investigations` but `creation`, as a row decodes.
macro_rules! investigation_columns {
    () => {
        "id, title, question, scope, status, seq, created_at, updated_at, retain_until, \
         retain_ceiling, created_by_key_id, created_by_prefix, created_by_name, \
         updated_by_key_id, updated_by_prefix, updated_by_name"
    };
}

/// `SELECT` [`investigation_columns`] from `investigations`, followed by
/// `$tail`.
macro_rules! select_investigation {
    ($tail:expr) => {
        concat!(
            "SELECT ",
            investigation_columns!(),
            " FROM investigations ",
            $tail
        )
    };
}

/// `SELECT` the columns a [`LogEntry`] decodes from, followed by `$tail`.
macro_rules! select_entries {
    ($tail:expr) => {
        concat!(
            "SELECT seq, updated_seq, kind, actor_prefix, actor_name, at, payload \
             FROM investigation_log ",
            $tail
        )
    };
}

/// How far a change slides the evidence clock (ADR-0059, amending
/// ADR-0018): `retain_until` moves to `now + idle`, never past the
/// investigation's `retain_ceiling` and never back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionRule {
    /// How long evidence lives after the investigation's last change.
    pub idle: Duration,
}

impl RetentionRule {
    /// The deadline a change at `now` slides toward, before the ceiling.
    fn slid(self, now: DateTime<Utc>) -> DateTime<Utc> {
        now.checked_add_signed(self.idle)
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
    }
}

/// Who a write is attributed to, as the store records it: the verified
/// key's id for ownership and the replay check, and snapshots of its prefix
/// and name for attribution. The id stays server side; [`Self::to_wire`] is
/// the only form that leaves the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreActor {
    /// The fleet key id. Never serialized.
    pub key_id: i64,
    /// The key's stable 8-character prefix.
    pub prefix: String,
    /// The key's name at the time of the write.
    pub name: String,
}

impl StoreActor {
    /// The attribution the wire carries: prefix and name, never the key id.
    #[must_use]
    pub fn to_wire(&self) -> Actor {
        Actor {
            prefix: self.prefix.clone(),
            name: self.name.clone(),
        }
    }
}

/// An investigation to create if its id is free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInvestigation {
    /// The id the MCP server minted.
    pub id: InvestigationId,
    /// The creator.
    pub actor: StoreActor,
    /// The request as sent. Stored apart from the mutable fields and
    /// compared on a replay.
    pub creation: CreateInvestigation,
    /// When it is created.
    pub now: DateTime<Utc>,
    /// The first evidence deadline, at most `retain_ceiling`.
    pub retain_until: DateTime<Utc>,
    /// The latest the evidence deadline can slide to.
    pub retain_ceiling: DateTime<Utc>,
}

/// One `investigations` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationRow {
    /// The id.
    pub id: InvestigationId,
    /// The title.
    pub title: String,
    /// The question.
    pub question: String,
    /// The scope text; empty when none was given.
    pub scope: String,
    /// Open or closed.
    pub status: InvestigationStatus,
    /// The sequence of the latest write; 0 at creation.
    pub seq: u64,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
    /// When its evidence is pruned unless it changes again.
    pub retain_until: DateTime<Utc>,
    /// The latest `retain_until` can slide to.
    pub retain_ceiling: DateTime<Utc>,
    /// Who created it.
    pub created_by: StoreActor,
    /// Who changed it last.
    pub updated_by: StoreActor,
}

impl InvestigationRow {
    /// The row as the wire spells it.
    #[must_use]
    pub fn to_wire(&self) -> Investigation {
        Investigation {
            id: self.id.clone(),
            title: self.title.clone(),
            question: self.question.clone(),
            scope: self.scope.clone(),
            status: self.status,
            seq: self.seq,
            created_at: wire_time(self.created_at),
            updated_at: wire_time(self.updated_at),
            retain_until: wire_time(self.retain_until),
            retain_ceiling: wire_time(self.retain_ceiling),
            created_by: self.created_by.to_wire(),
            updated_by: self.updated_by.to_wire(),
        }
    }

    /// The row as a list entry.
    #[must_use]
    pub fn to_summary(&self) -> InvestigationSummary {
        InvestigationSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            status: self.status,
            seq: self.seq,
            created_at: wire_time(self.created_at),
            updated_at: wire_time(self.updated_at),
            created_by: self.created_by.to_wire(),
            updated_by: self.updated_by.to_wire(),
        }
    }
}

/// What [`InvestigationStore::create_if_absent`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateOutcome {
    /// The investigation is new.
    Created(InvestigationRow),
    /// The same creator already created it with the same request: a
    /// replay. Nothing changed, the sequence and the clock included.
    Existing(InvestigationRow),
    /// The id is taken by another creator or another request.
    Mismatch,
}

/// An investigation and the entries changed after a sequence, read from
/// one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    /// The investigation as of the snapshot.
    pub investigation: InvestigationRow,
    /// The entries, in `updated_seq` order.
    pub entries: Vec<LogEntry>,
    /// The sequence the page reaches: the last entry's `updated_seq` when
    /// more remain, the investigation's `seq` otherwise.
    pub through_seq: u64,
    /// Whether entries past `through_seq` remain.
    pub more: bool,
}

/// An overwrite of one of the investigation's own fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overwrite {
    /// A new title.
    Title(String),
    /// A new question.
    Question(String),
    /// Close or reopen.
    Status(InvestigationStatus),
}

/// What [`InvestigationStore::delete`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The investigation and its log are gone. Its kept results are pruned
    /// and their files are these, for the caller to unlink.
    Deleted(Vec<ExpiredFile>),
    /// No investigation has the id.
    NotFound,
    /// A step's work is still running; nothing changed.
    StepsRunning,
}

/// What [`InvestigationStore::append_citing`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Citing {
    /// The entry is appended and every result it promotes is kept.
    Appended(Box<Appended>),
    /// The named result is kept by another investigation; nothing was
    /// written.
    KeptElsewhere(String),
    /// The named result can no longer be promoted: it is gone, past its
    /// deadline, or no longer the caller's to promote. Nothing was written.
    NotCitable(String),
}

/// The locked head of an investigation inside a write transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Locked {
    /// Open or closed.
    pub(crate) status: InvestigationStatus,
    /// The sequence of the latest write.
    pub(crate) seq: u64,
}

/// What [`InvestigationStore::append`] and
/// [`InvestigationStore::begin_step`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appended {
    /// The new entry.
    pub entry: LogEntry,
    /// The kept results the due-write rule pruned before the write, for
    /// the caller to unlink once the write committed.
    pub pruned: Vec<ExpiredFile>,
}

/// Postgres-backed storage for investigations. Cheap to clone (shared
/// pool).
#[derive(Debug, Clone)]
pub struct InvestigationStore {
    pool: PgPool,
    /// Faults a test injected ([`super::results::fault`]), shared with the
    /// results store this one was made from.
    #[cfg(any(test, feature = "test-support"))]
    faults: std::sync::Arc<super::results::fault::Faults>,
}

impl InvestigationStore {
    /// Wrap the shared app-state pool, which must point at a migrated trawl
    /// app-state database.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            #[cfg(any(test, feature = "test-support"))]
            faults: std::sync::Arc::default(),
        }
    }

    /// [`Self::new`], reading `faults`: the results store's table
    /// ([`super::ResultsStore::investigations`]). Test-support code, gated
    /// on the feature alone: a `cfg(test)` item whose signature names
    /// `PgPool` reads as a test region to the pg-admission guard, which
    /// would then want every unit test of this crate in the postgres group.
    #[cfg(feature = "test-support")]
    pub(crate) fn sharing(
        pool: PgPool,
        faults: std::sync::Arc<super::results::fault::Faults>,
    ) -> Self {
        Self { pool, faults }
    }

    /// Create the investigation unless its id is taken.
    ///
    /// A taken id is read back and compared: the same creator key with the
    /// same request, byte for byte as this store encodes it, is a replay
    /// and answers [`CreateOutcome::Existing`] without advancing anything;
    /// anything else is [`CreateOutcome::Mismatch`].
    pub async fn create_if_absent(
        &self,
        new: &NewInvestigation,
    ) -> Result<CreateOutcome, StoreError> {
        let creation = serde_json::to_string(&new.creation).map_err(|e| {
            StoreError::Validation(format!("the create request does not encode: {e}"))
        })?;
        for _ in 0..CREATE_ATTEMPTS {
            let inserted = sqlx::query(concat!(
                "INSERT INTO investigations (
                     id, title, question, scope, status, seq, created_at, updated_at,
                     retain_until, retain_ceiling, created_by_key_id, created_by_prefix,
                     created_by_name, updated_by_key_id, updated_by_prefix,
                     updated_by_name, creation)
                 VALUES ($1, $2, $3, $4, 'open', 0, $5, $5, $6, $7, $8, $9, $10, $8, $9, $10, $11)
                 ON CONFLICT (id) DO NOTHING
                 RETURNING ",
                investigation_columns!()
            ))
            .bind(new.id.as_str())
            .bind(&new.creation.title)
            .bind(&new.creation.question)
            .bind(&new.creation.scope)
            .bind(new.now)
            .bind(new.retain_until)
            .bind(new.retain_ceiling)
            .bind(new.actor.key_id)
            .bind(&new.actor.prefix)
            .bind(&new.actor.name)
            .bind(&creation)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(row) = inserted {
                return Ok(CreateOutcome::Created(decode_investigation(&row)?));
            }

            // The conflicting row is committed by now (the insert waited
            // for it), and this statement's snapshot is newer than it.
            let existing = sqlx::query(concat!(
                "SELECT ",
                investigation_columns!(),
                ", creation FROM investigations WHERE id = $1"
            ))
            .bind(new.id.as_str())
            .fetch_optional(&self.pool)
            .await?;
            let Some(row) = existing else {
                // Deleted between the conflict and the read: try again.
                continue;
            };
            let stored: String = row.try_get("creation")?;
            let row = decode_investigation(&row)?;
            return Ok(
                if row.created_by.key_id == new.actor.key_id && stored == creation {
                    CreateOutcome::Existing(row)
                } else {
                    CreateOutcome::Mismatch
                },
            );
        }
        Err(StoreError::Unavailable(sqlx::Error::Protocol(
            "the investigation id kept vanishing between its create and its read".into(),
        )))
    }

    /// The investigation `id`, if it exists.
    pub async fn get(&self, id: &InvestigationId) -> Result<Option<InvestigationRow>, StoreError> {
        let row = sqlx::query(select_investigation!("WHERE id = $1"))
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(decode_investigation).transpose()?)
    }

    /// At most `limit` investigations, capped at [`MAX_LIST`]: open ones
    /// first, then closed ones, each group most recently changed first.
    pub async fn list(&self, limit: u64) -> Result<Vec<InvestigationRow>, StoreError> {
        // 'open' sorts after 'closed', so descending puts it first.
        let rows = sqlx::query(select_investigation!(
            "ORDER BY status DESC, updated_at DESC, id DESC LIMIT $1"
        ))
        .bind(bind_u64(limit.min(MAX_LIST)))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(decode_investigation)
            .collect::<Result<_, _>>()?)
    }

    /// The investigation and at most `limit` entries with `updated_seq`
    /// past `since`, in `updated_seq` order, read from one snapshot so the
    /// head and the entries describe one state. `None` when the
    /// investigation does not exist.
    pub async fn read_since(
        &self,
        id: &InvestigationId,
        since: u64,
        limit: u64,
    ) -> Result<Option<LogPage>, StoreError> {
        self.read_page(id, since, None, limit).await
    }

    /// [`Self::read_since`] limited to the entries written by keys other
    /// than `except_key_id`: what other actors did since the caller last
    /// looked. `None` when the investigation does not exist.
    pub async fn activity(
        &self,
        id: &InvestigationId,
        since: u64,
        except_key_id: i64,
        limit: u64,
    ) -> Result<Option<Activity>, StoreError> {
        Ok(self
            .read_page(id, since, Some(except_key_id), limit)
            .await?
            .map(|page| Activity {
                entries: page.entries,
                through_seq: page.through_seq,
                more: page.more,
            }))
    }

    async fn read_page(
        &self,
        id: &InvestigationId,
        since: u64,
        except_key_id: Option<i64>,
        limit: u64,
    ) -> Result<Option<LogPage>, StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(head) = sqlx::query(select_investigation!("WHERE id = $1"))
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.rollback().await?;
            return Ok(None);
        };
        let investigation = decode_investigation(&head)?;
        let rows = sqlx::query(select_entries!(
            "WHERE investigation_id = $1 AND updated_seq > $2
               AND ($3::bigint IS NULL OR actor_key_id <> $3)
             ORDER BY updated_seq LIMIT $4"
        ))
        .bind(id.as_str())
        .bind(bind_u64(since))
        .bind(except_key_id)
        .bind(bind_u64(limit.saturating_add(1)))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut entries = rows
            .iter()
            .map(decode_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let more = entries.len() as u64 > limit;
        entries.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let through_seq = match entries.last() {
            Some(last) if more => last.updated_seq,
            _ => investigation.seq.max(since),
        };
        Ok(Some(LogPage {
            investigation,
            entries,
            through_seq,
            more,
        }))
    }

    /// Append a keep, a finding or a note: no expected sequence. The
    /// investigation must be open.
    ///
    /// Other kinds are refused with [`StoreError::Validation`]: a step
    /// goes through [`Self::begin_step`], and the overwrites through
    /// [`Self::overwrite`] and [`Self::set_finding_status`].
    pub async fn append(
        &self,
        id: &InvestigationId,
        actor: &StoreActor,
        payload: &EntryPayload,
        now: DateTime<Utc>,
        retention: RetentionRule,
    ) -> Result<Appended, StoreError> {
        if !matches!(
            payload,
            EntryPayload::Keep { .. } | EntryPayload::Finding { .. } | EntryPayload::Note { .. }
        ) {
            return Err(StoreError::Validation(format!(
                "a {} entry is not an append",
                kind_str(payload.kind())
            )));
        }
        self.append_open(id, actor, payload, now, retention).await
    }

    /// Append a keep or a finding that promotes the caller's own live
    /// temporary results `promote` into the investigation (ADR-0059), in
    /// one transaction: the investigation locked first, through the
    /// due-write rule, then each promotion ([`super::results::promote_in`]),
    /// then the entry. The investigation must be open.
    ///
    /// The promotions run once per result, in ascending id order whatever
    /// the order of `promote`, so two appends into different
    /// investigations that cite the same results lock them in one order:
    /// the second waits for the first rather than closing a deadlock with
    /// it, and then finds them kept elsewhere.
    ///
    /// A promotion that changes no row is classified under the lock: a
    /// result already kept by this investigation is fine, one kept by
    /// another is [`Citing::KeptElsewhere`], and anything else
    /// [`Citing::NotCitable`]; either refusal names the first such result
    /// in that order and writes nothing. An
    /// investigation whose deadline is pinned at its ceiling stays due
    /// after the write slides it, and the rule prunes again, the promoted
    /// results included. Other kinds are refused with
    /// [`StoreError::Validation`].
    ///
    /// The [`super::results::fault`] table's
    /// [`super::results::fault::Faults::fail_next_citing`] applies.
    ///
    /// The wait before the commit ends at `deadline`
    /// ([`StoreError::WaitExpired`], nothing written); the commit itself is
    /// never cut short ([`WriteTx`]).
    #[allow(clippy::too_many_arguments)] // one append, each part named
    pub async fn append_citing(
        &self,
        id: &InvestigationId,
        actor: &StoreActor,
        payload: &EntryPayload,
        promote: &[&str],
        now: DateTime<Utc>,
        retention: RetentionRule,
        deadline: Deadline,
    ) -> Result<Citing, StoreError> {
        if !matches!(
            payload,
            EntryPayload::Keep { .. } | EntryPayload::Finding { .. }
        ) {
            return Err(StoreError::Validation(format!(
                "a {} entry cites no result",
                kind_str(payload.kind())
            )));
        }
        #[cfg(any(test, feature = "test-support"))]
        let committed_fault = match self.faults.take_citing() {
            None => false,
            Some(super::results::fault::InsertFault::Committed) => true,
            Some(_) => return Err(super::results::fault::injected()),
        };
        let cited = self
            .citing_transaction(id, actor, payload, promote, now, retention, deadline)
            .await;
        #[cfg(any(test, feature = "test-support"))]
        if committed_fault && matches!(cited, Ok(Citing::Appended(_))) {
            return Err(super::results::fault::injected());
        }
        cited
    }

    #[allow(clippy::too_many_arguments)] // one append, each part named
    async fn citing_transaction(
        &self,
        id: &InvestigationId,
        actor: &StoreActor,
        payload: &EntryPayload,
        promote: &[&str],
        now: DateTime<Utc>,
        retention: RetentionRule,
        deadline: Deadline,
    ) -> Result<Citing, StoreError> {
        let text = encode_payload(payload)?;
        let mut tx = WriteTx::begin(&self.pool, LockWait::Until(deadline)).await?;
        let cited = citing_in(&mut tx, id, actor, payload, &text, promote, now, retention).await;
        let cited = match cited {
            Ok(cited) => cited,
            Err(e) => return Err(tx.end(e).await),
        };
        if matches!(cited, Citing::Appended(_)) {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        Ok(cited)
    }

    /// Append a step at admission, with no outcome yet. The investigation
    /// must be open. The returned entry's `seq` names the step for
    /// [`Self::complete_step_until`].
    ///
    /// The wait before the commit ends at `deadline`
    /// ([`StoreError::WaitExpired`], nothing written); the commit itself is
    /// never cut short ([`WriteTx`]).
    pub async fn begin_step(
        &self,
        id: &InvestigationId,
        actor: &StoreActor,
        payload: &EntryPayload,
        now: DateTime<Utc>,
        retention: RetentionRule,
        deadline: Deadline,
    ) -> Result<Appended, StoreError> {
        if !matches!(payload, EntryPayload::Step { outcome: None, .. }) {
            return Err(StoreError::Validation(
                "a step is appended without an outcome".into(),
            ));
        }
        let text = encode_payload(payload)?;
        let mut tx = WriteTx::begin(&self.pool, LockWait::Until(deadline)).await?;
        let appended = append_open_in(&mut tx, id, actor, payload, &text, now, retention).await;
        tx.finish(appended).await
    }

    async fn append_open(
        &self,
        id: &InvestigationId,
        actor: &StoreActor,
        payload: &EntryPayload,
        now: DateTime<Utc>,
        retention: RetentionRule,
    ) -> Result<Appended, StoreError> {
        let text = encode_payload(payload)?;
        let mut tx = WriteTx::begin(&self.pool, LockWait::Unbounded).await?;
        let appended = append_open_in(&mut tx, id, actor, payload, &text, now, retention).await;
        tx.finish(appended).await
    }

    /// Overwrite the title, the question or the status, if the
    /// investigation's sequence is still `expected_seq`, and record the
    /// matching entry. A closed investigation takes only a reopen: another
    /// close, like any other change, is [`StoreError::InvestigationClosed`]
    /// and records nothing.
    pub async fn overwrite(
        &self,
        id: &InvestigationId,
        expected_seq: u64,
        actor: &StoreActor,
        change: &Overwrite,
        now: DateTime<Utc>,
        retention: RetentionRule,
    ) -> Result<LogEntry, StoreError> {
        let payload = match change {
            Overwrite::Title(title) => EntryPayload::Title {
                title: title.clone(),
            },
            Overwrite::Question(question) => EntryPayload::Question {
                question: question.clone(),
            },
            Overwrite::Status(status) => EntryPayload::Status { status: *status },
        };
        let text = encode_payload(&payload)?;
        let mut tx = WriteTx::begin(&self.pool, LockWait::Unbounded).await?;
        let written = async {
            let (locked, _) = lock_for_write(&mut tx, id, now).await?;
            if locked.status == InvestigationStatus::Closed
                && !matches!(change, Overwrite::Status(InvestigationStatus::Open))
            {
                return Err(StoreError::InvestigationClosed);
            }
            if locked.seq != expected_seq {
                return Err(StoreError::SequenceConflict {
                    expected: expected_seq,
                    current: locked.seq,
                });
            }
            let seq = locked.seq + 1;
            insert_entry(&mut tx, id, seq, actor, now, payload.kind(), &text).await?;
            let (column, value) = match change {
                Overwrite::Title(title) => ("title", title.as_str()),
                Overwrite::Question(question) => ("question", question.as_str()),
                Overwrite::Status(status) => ("status", status_str(*status)),
            };
            // The column name is one of three literals above; only the
            // value is bound.
            let statement = format!("UPDATE investigations SET {column} = $2 WHERE id = $1");
            tx.run(async |tx| {
                sqlx::query(sqlx::AssertSqlSafe(statement))
                    .bind(id.as_str())
                    .bind(value)
                    .execute(tx)
                    .await
            })
            .await?;
            advance(&mut tx, id, seq, actor, now, Some(retention)).await?;
            Ok(seq)
        }
        .await;
        let seq = tx.finish(written).await?;
        Ok(LogEntry {
            seq,
            updated_seq: seq,
            actor: actor.to_wire(),
            at: wire_time(now),
            payload,
        })
    }

    /// Overwrite the status of the finding whose entry is `finding_seq`, if
    /// the investigation's sequence is still `expected_seq`, by appending a
    /// `finding_status` entry. The investigation must be open. A
    /// `finding_seq` that names no finding of this investigation is
    /// [`StoreError::NotFound`] for a `finding`.
    #[allow(clippy::too_many_arguments)] // one overwrite, each part named
    pub async fn set_finding_status(
        &self,
        id: &InvestigationId,
        finding_seq: u64,
        expected_seq: u64,
        status: FindingStatus,
        actor: &StoreActor,
        now: DateTime<Utc>,
        retention: RetentionRule,
    ) -> Result<LogEntry, StoreError> {
        let payload = EntryPayload::FindingStatus {
            finding_seq,
            status,
        };
        let text = encode_payload(&payload)?;
        let mut tx = WriteTx::begin(&self.pool, LockWait::Unbounded).await?;
        let written = async {
            let (locked, _) = lock_for_write(&mut tx, id, now).await?;
            if locked.status == InvestigationStatus::Closed {
                return Err(StoreError::InvestigationClosed);
            }
            let is_finding: bool = tx
                .run(async |tx| {
                    sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM investigation_log
                                        WHERE investigation_id = $1 AND seq = $2
                                          AND kind = 'finding')",
                    )
                    .bind(id.as_str())
                    .bind(bind_u64(finding_seq))
                    .fetch_one(tx)
                    .await
                })
                .await?;
            if !is_finding {
                return Err(StoreError::NotFound {
                    id: bind_u64(finding_seq),
                    resource: "finding",
                });
            }
            if locked.seq != expected_seq {
                return Err(StoreError::SequenceConflict {
                    expected: expected_seq,
                    current: locked.seq,
                });
            }
            let seq = locked.seq + 1;
            insert_entry(
                &mut tx,
                id,
                seq,
                actor,
                now,
                EntryKind::FindingStatus,
                &text,
            )
            .await?;
            advance(&mut tx, id, seq, actor, now, Some(retention)).await?;
            Ok(seq)
        }
        .await;
        let seq = tx.finish(written).await?;
        Ok(LogEntry {
            seq,
            updated_seq: seq,
            actor: actor.to_wire(),
            at: wire_time(now),
            payload,
        })
    }

    /// Write the terminal outcome of step `step_seq`, once, waiting for a
    /// connection and the locks no later than `deadline` (ADR-0059). Returns
    /// whether this call wrote it: a step that already has an outcome, or a
    /// `seq` that names no step, changes nothing and answers `false`. Past
    /// the deadline, [`StoreError::WaitExpired`] with nothing written, once
    /// the transaction has ended in Postgres; the commit itself is never
    /// cut short ([`WriteTx`]). Every step outcome is written so, bounded,
    /// and only boot's interruptions wait unbounded
    /// ([`Self::interrupt_running_steps`]).
    ///
    /// The step's `updated_seq` becomes the investigation's next sequence,
    /// so a reader since its admission sees it again with its outcome, and
    /// the clock slides. A closed investigation still takes it: the work
    /// was admitted while it was open. The investigation's `updated_by`
    /// becomes the step's actor, whose request admitted the work. Evidence
    /// the due-write rule prunes here is left to the sweep (module docs).
    pub async fn complete_step_until(
        &self,
        id: &InvestigationId,
        step_seq: u64,
        outcome: &StepOutcome,
        now: DateTime<Utc>,
        retention: RetentionRule,
        deadline: Deadline,
    ) -> Result<bool, StoreError> {
        self.fill_step(
            id,
            step_seq,
            outcome,
            now,
            Some(retention),
            LockWait::Until(deadline),
        )
        .await
    }

    /// [`Self::complete_step_until`], sliding the evidence clock by `slide`,
    /// or leaving it for `None`, and waiting as `wait` allows.
    async fn fill_step(
        &self,
        id: &InvestigationId,
        step_seq: u64,
        outcome: &StepOutcome,
        now: DateTime<Utc>,
        slide: Option<RetentionRule>,
        wait: LockWait,
    ) -> Result<bool, StoreError> {
        let mut tx = WriteTx::begin(&self.pool, wait).await?;
        let filled = match lock_for_write(&mut tx, id, now).await {
            Ok((locked, _)) => {
                fill_step_in(&mut tx, id, locked, step_seq, outcome, now, slide).await
            }
            Err(StoreError::InvestigationNotFound) => Ok(false),
            Err(e) => Err(e),
        };
        match filled {
            Ok(true) => {
                tx.commit().await?;
                Ok(true)
            }
            Ok(false) => {
                tx.rollback().await?;
                Ok(false)
            }
            Err(e) => Err(tx.end(e).await),
        }
    }

    /// At boot, record `interrupted` on every step whose work never
    /// recorded an outcome: the process that ran it is gone. Each is the
    /// one-shot fill of [`Self::complete_step_until`], with one difference: an
    /// interruption is bookkeeping, not a principal's change, so it writes
    /// the outcome and advances the sequence but leaves `retain_until` and
    /// `retain_ceiling` where the last change put them (ADR-0059). A boot
    /// long after the crash must not hand the evidence a fresh week.
    /// Returns how many steps changed.
    pub async fn interrupt_running_steps(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let running: Vec<(String, i64)> = sqlx::query_as(
            "SELECT investigation_id, seq FROM investigation_log
             WHERE kind = 'step' AND outcome IS NULL ORDER BY investigation_id, seq",
        )
        .fetch_all(&self.pool)
        .await?;
        let interrupted = StepOutcome {
            outcome: StepOutcomeKind::Interrupted,
            code: None,
            result_id: None,
            rows: None,
            duration_ms: None,
        };
        let mut changed = 0;
        for (id, seq) in running {
            let id = InvestigationId::parse(&id)
                .map_err(|e| StoreError::Unavailable(column_error("investigation_id", e)))?;
            let seq =
                u64::try_from(seq).map_err(|e| StoreError::Unavailable(column_error("seq", e)))?;
            if self
                .fill_step(&id, seq, &interrupted, now, None, LockWait::Unbounded)
                .await?
            {
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// End the evidence of investigation `id` if its `retain_until` is at or
    /// before `now`: lock it, as every write does, and prune its kept
    /// results as the due-write rule would ([`prune_if_due`]). Returns the
    /// files to unlink, empty when the sweep or a write pruned them first,
    /// or `None` when the retention has not ended under the lock (a write
    /// slid it after the caller looked) or the investigation is gone. The
    /// door a read of a kept row past its deadline takes (ADR-0059).
    ///
    /// The [`super::results::fault`] table's
    /// [`super::results::fault::Faults::hold_next_evidence_end`] applies.
    pub async fn end_evidence_if_due(
        &self,
        id: &InvestigationId,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<ExpiredFile>>, StoreError> {
        #[cfg(any(test, feature = "test-support"))]
        self.faults.wait_if_evidence_end_held().await;
        let mut tx = WriteTx::begin(&self.pool, LockWait::Unbounded).await?;
        let ended = async {
            match lock(&mut tx, id).await {
                Ok(_) => {}
                Err(StoreError::InvestigationNotFound) => return Ok(None),
                Err(e) => return Err(e),
            }
            let due: bool = tx
                .run(async |tx| {
                    sqlx::query_scalar(
                        "SELECT retain_until <= $2 FROM investigations WHERE id = $1",
                    )
                    .bind(id.as_str())
                    .bind(now)
                    .fetch_one(tx)
                    .await
                })
                .await?;
            if !due {
                return Ok(None);
            }
            Ok(Some(prune_if_due(&mut tx, id, now).await?))
        }
        .await;
        match ended {
            Ok(Some(files)) => {
                tx.commit().await?;
                Ok(Some(files))
            }
            Ok(None) => {
                tx.rollback().await?;
                Ok(None)
            }
            Err(e) => Err(tx.end(e).await),
        }
    }

    /// Keep the evidence until at least `retain_until`, raising the
    /// ceiling to it if need be (`server_manage`, checked by the caller),
    /// if the sequence is still `expected_seq`. Never shortens either
    /// deadline. Advances the sequence and records no entry: the read
    /// carries the new deadlines.
    pub async fn extend(
        &self,
        id: &InvestigationId,
        expected_seq: u64,
        retain_until: DateTime<Utc>,
        actor: &StoreActor,
        now: DateTime<Utc>,
    ) -> Result<InvestigationRow, StoreError> {
        let mut tx = WriteTx::begin(&self.pool, LockWait::Unbounded).await?;
        let extended = async {
            let (locked, _) = lock_for_write(&mut tx, id, now).await?;
            if locked.seq != expected_seq {
                return Err(StoreError::SequenceConflict {
                    expected: expected_seq,
                    current: locked.seq,
                });
            }
            let row = tx
                .run(async |tx| {
                    sqlx::query(concat!(
                        "UPDATE investigations
                         SET seq = $2, updated_at = $3, updated_by_key_id = $4,
                             updated_by_prefix = $5, updated_by_name = $6,
                             retain_until = GREATEST(retain_until, $7),
                             retain_ceiling = GREATEST(retain_ceiling, $7)
                         WHERE id = $1
                         RETURNING ",
                        investigation_columns!()
                    ))
                    .bind(id.as_str())
                    .bind(bind_u64(locked.seq + 1))
                    .bind(now)
                    .bind(actor.key_id)
                    .bind(&actor.prefix)
                    .bind(&actor.name)
                    .bind(retain_until)
                    .fetch_one(tx)
                    .await
                })
                .await?;
            Ok(decode_investigation(&row)?)
        }
        .await;
        tx.finish(extended).await
    }

    /// Delete the investigation and its log, unless a step's work is still
    /// running. Its kept results are pruned in the same transaction, with
    /// `expires_at` and `gone_at` at `now`, and lose their investigation
    /// id with it, so they age out as tombstones; their files are returned
    /// for the caller to unlink.
    ///
    /// The investigation row is locked first, as every write into it locks
    /// it first, kept inserts and promotions included, so none lands
    /// between the prune and the delete.
    pub async fn delete(
        &self,
        id: &InvestigationId,
        now: DateTime<Utc>,
    ) -> Result<DeleteOutcome, StoreError> {
        let mut tx = WriteTx::begin(&self.pool, LockWait::Unbounded).await?;
        let deleted = async {
            match lock(&mut tx, id).await {
                Ok(_) => {}
                Err(StoreError::InvestigationNotFound) => return Ok(DeleteOutcome::NotFound),
                Err(e) => return Err(e),
            }
            let running: bool = tx
                .run(async |tx| {
                    sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM investigation_log
                                        WHERE investigation_id = $1 AND kind = 'step'
                                          AND outcome IS NULL)",
                    )
                    .bind(id.as_str())
                    .fetch_one(tx)
                    .await
                })
                .await?;
            if running {
                return Ok(DeleteOutcome::StepsRunning);
            }
            let files: Vec<(String, i64)> = tx
                .run(async |tx| {
                    sqlx::query_as(
                        "UPDATE captured_results
                         SET state = 'pruned', expires_at = $2, gone_at = $2
                         WHERE investigation_id = $1 AND state = 'kept'
                         RETURNING id, file_bytes",
                    )
                    .bind(id.as_str())
                    .bind(now)
                    .fetch_all(tx)
                    .await
                })
                .await?;
            tx.run(async |tx| {
                sqlx::query("DELETE FROM investigations WHERE id = $1")
                    .bind(id.as_str())
                    .execute(tx)
                    .await
            })
            .await?;
            Ok(DeleteOutcome::Deleted(expired_files(files)?))
        }
        .await;
        match deleted {
            Ok(deleted @ DeleteOutcome::Deleted(_)) => {
                tx.commit().await?;
                Ok(deleted)
            }
            Ok(refused) => {
                tx.rollback().await?;
                Ok(refused)
            }
            Err(e) => Err(tx.end(e).await),
        }
    }

    /// The file bytes of the investigation's kept evidence. Each result is
    /// one row holding one file, so each file counts once.
    pub async fn evidence_usage(&self, id: &InvestigationId) -> Result<u64, StoreError> {
        let bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(file_bytes), 0)::bigint FROM captured_results
             WHERE state = 'kept' AND investigation_id = $1",
        )
        .bind(id.as_str())
        .fetch_one(&self.pool)
        .await?;
        u64::try_from(bytes).map_err(|e| StoreError::Unavailable(column_error("file_bytes", e)))
    }

    /// The file bytes of every investigation's kept evidence.
    pub async fn installation_evidence_usage(&self) -> Result<u64, StoreError> {
        let bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(file_bytes), 0)::bigint FROM captured_results
             WHERE state = 'kept' AND investigation_id IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        u64::try_from(bytes).map_err(|e| StoreError::Unavailable(column_error("file_bytes", e)))
    }
}

/// How long the waits of a write's transaction may last.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LockWait {
    /// As long as the pool's acquire timeout and the database's own
    /// settings allow.
    Unbounded,
    /// No later than the deadline (module docs): the wait for a
    /// connection here, and every statement in Postgres, by a
    /// `statement_timeout` set from what remains before each
    /// ([`WriteTx::run`]). Either ending is [`StoreError::WaitExpired`].
    /// The commit is never cut short.
    Until(Deadline),
}

/// The transaction of an investigation write (module docs). Its
/// connection is reachable only through [`Self::run`], one statement at a
/// time, so under [`LockWait::Until`] no statement of the write waits past
/// the deadline. Under [`LockWait::Unbounded`] it sets nothing and runs
/// the same code.
#[derive(Debug)]
pub(crate) struct WriteTx {
    tx: PooledTransaction,
    wait: LockWait,
}

impl WriteTx {
    /// A transaction from `pool`, its wait for a connection bounded as
    /// `wait` allows.
    ///
    /// Only the acquire is cut short at the deadline: nothing has been sent
    /// yet. The `BEGIN` is then awaited to its answer, since a `BEGIN` cut
    /// short leaves the session in a transaction no one ends
    /// ([`PooledTransaction`]). A deadline that passed meanwhile ends the
    /// transaction, its rollback answered, as [`StoreError::WaitExpired`].
    pub(crate) async fn begin(pool: &PgPool, wait: LockWait) -> Result<Self, StoreError> {
        let conn = match wait {
            LockWait::Unbounded => pool.acquire().await?,
            LockWait::Until(deadline) => match deadline.run(pool.acquire()).await {
                Ok(conn) => conn?,
                Err(Expired) => return Err(StoreError::WaitExpired),
            },
        };
        let tx = Self {
            tx: PooledTransaction::begin(conn).await?,
            wait,
        };
        match wait {
            LockWait::Until(deadline) if deadline.expired() => {
                Err(tx.end(StoreError::WaitExpired).await)
            }
            _ => Ok(tx),
        }
    }

    /// Run one statement of the write on its connection.
    ///
    /// Under a deadline, Postgres bounds the statement first by what
    /// remains: `statement_timeout` covers the whole statement, each of its
    /// lock waits included, and set before every statement the waits of
    /// the write together end at the deadline. What remains is rounded up
    /// to the millisecond, so no wait ends before the deadline. A timeout
    /// of zero waits forever, so a deadline already past still bounds the
    /// statement at one millisecond. The setting is a 32-bit count of
    /// milliseconds, about 24 days, and a deadline may be a year away: such
    /// a wait ends early, as `WaitExpired`.
    pub(crate) async fn run<T, E: From<sqlx::Error>>(
        &mut self,
        statement: impl AsyncFnOnce(&mut PgConnection) -> Result<T, E>,
    ) -> Result<T, E> {
        if let LockWait::Until(deadline) = self.wait {
            let millis = deadline
                .remaining()
                .as_micros()
                .div_ceil(1_000)
                .clamp(1, u128::from(i32::MAX.unsigned_abs()));
            statement_timeout(&mut self.tx, &format!("{millis}ms")).await?;
        }
        statement(&mut self.tx).await
    }

    /// Commit, and wait for Postgres to answer. Under a deadline the
    /// statement timeout is lifted first, so the commit itself is never cut
    /// short; the lift is a statement of the write, and one the deadline
    /// cuts short ends the transaction as [`StoreError::WaitExpired`].
    pub(crate) async fn commit(mut self) -> Result<(), StoreError> {
        if matches!(self.wait, LockWait::Until(_))
            && let Err(e) = statement_timeout(&mut self.tx, "0").await
        {
            return Err(self.end(e.into()).await);
        }
        self.tx.commit().await?;
        Ok(())
    }

    /// Roll back what the write did, and wait for Postgres to answer.
    pub(crate) async fn rollback(self) -> Result<(), StoreError> {
        self.tx.rollback().await?;
        Ok(())
    }

    /// Commit after `written`, or end the transaction after its error
    /// ([`Self::end`]).
    async fn finish<T>(self, written: Result<T, StoreError>) -> Result<T, StoreError> {
        match written {
            Ok(written) => {
                self.commit().await?;
                Ok(written)
            }
            Err(e) => Err(self.end(e).await),
        }
    }

    /// End the transaction after `error`: roll back and wait for Postgres
    /// to answer the rollback, because the caller may give a request's
    /// count back the moment this returns. A failed rollback closes the
    /// connection, which ends the transaction too; `error` is still the
    /// answer. Under a deadline, a statement that reached its timeout, or a
    /// lock wait the database's own `lock_timeout` ended, is
    /// [`StoreError::WaitExpired`].
    pub(crate) async fn end(self, error: StoreError) -> StoreError {
        let wait = self.wait;
        let _ = self.tx.rollback().await;
        match wait {
            LockWait::Until(_) if is_wait_ended(&error) => StoreError::WaitExpired,
            _ => error,
        }
    }
}

/// Set the transaction's `statement_timeout` to `value`, until it ends.
async fn statement_timeout(tx: &mut PgConnection, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(value)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Whether `error` is a wait Postgres ended: a statement that reached
/// `statement_timeout` (SQLSTATE `query_canceled`) or a lock wait that
/// reached `lock_timeout` (`lock_not_available`).
fn is_wait_ended(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Unavailable(sqlx::Error::Database(e))
            if matches!(e.code().as_deref(), Some("57014" | "55P03"))
    )
}

/// Lock the open investigation `id` through the due-write rule and append
/// `payload`, encoded as `text`, inside `tx`.
#[allow(clippy::too_many_arguments)] // one append, each part named
async fn append_open_in(
    tx: &mut WriteTx,
    id: &InvestigationId,
    actor: &StoreActor,
    payload: &EntryPayload,
    text: &str,
    now: DateTime<Utc>,
    retention: RetentionRule,
) -> Result<Appended, StoreError> {
    let (locked, pruned) = lock_for_write(tx, id, now).await?;
    if locked.status == InvestigationStatus::Closed {
        return Err(StoreError::InvestigationClosed);
    }
    let entry = append_in(tx, id, locked, actor, payload, text, now, retention).await?;
    Ok(Appended { entry, pruned })
}

/// The citing append of [`InvestigationStore::append_citing`] inside `tx`:
/// the investigation locked through the due-write rule, the promotions in
/// ascending id order, then the entry. A refusal writes nothing and leaves
/// `tx` for the caller to roll back.
#[allow(clippy::too_many_arguments)] // one append, each part named
async fn citing_in(
    tx: &mut WriteTx,
    id: &InvestigationId,
    actor: &StoreActor,
    payload: &EntryPayload,
    text: &str,
    promote: &[&str],
    now: DateTime<Utc>,
    retention: RetentionRule,
) -> Result<Citing, StoreError> {
    let (locked, mut pruned) = lock_for_write(tx, id, now).await?;
    if locked.status == InvestigationStatus::Closed {
        return Err(StoreError::InvestigationClosed);
    }
    let mut promote = promote.to_vec();
    promote.sort_unstable();
    promote.dedup();
    for result in promote {
        if super::results::promote_in(tx, result, actor.key_id, id, now).await? {
            continue;
        }
        let refusal = match super::results::attachment_in(tx, result).await? {
            Some((ResultState::Kept, Some(of))) if of == *id => continue,
            Some((ResultState::Kept, Some(_))) => Citing::KeptElsewhere(result.to_owned()),
            _ => Citing::NotCitable(result.to_owned()),
        };
        return Ok(refusal);
    }
    let entry = append_in(tx, id, locked, actor, payload, text, now, retention).await?;
    pruned.extend(prune_if_due(tx, id, now).await?);
    Ok(Citing::Appended(Box::new(Appended { entry, pruned })))
}

/// Lock the investigation `id` for the rest of `tx` and read its head.
async fn lock(tx: &mut WriteTx, id: &InvestigationId) -> Result<Locked, StoreError> {
    let row = tx
        .run(async |tx| {
            sqlx::query("SELECT status, seq FROM investigations WHERE id = $1 FOR UPDATE")
                .bind(id.as_str())
                .fetch_optional(tx)
                .await
        })
        .await?
        .ok_or(StoreError::InvestigationNotFound)?;
    Ok(Locked {
        status: decode_status(&row)?,
        seq: decode_u64(&row, "seq")?,
    })
}

/// Lock the investigation `id` for a write at `now` and apply the due-write
/// rule (module docs): when its `retain_until` is at or before `now`, prune
/// its kept results first, as [`super::ResultsStore::prune_evidence`] does.
/// Returns the head and the files of what it pruned. Each lock is waited
/// for as `tx` allows: the investigation's, then its kept results' in the
/// prune.
///
/// The investigation row is locked before any of its results, the order
/// every transaction that touches both takes.
pub(crate) async fn lock_for_write(
    tx: &mut WriteTx,
    id: &InvestigationId,
    now: DateTime<Utc>,
) -> Result<(Locked, Vec<ExpiredFile>), StoreError> {
    let locked = lock(tx, id).await?;
    let pruned = prune_if_due(tx, id, now).await?;
    Ok((locked, pruned))
}

/// Prune the kept results of the locked investigation `id` when its
/// `retain_until` is at or before `now`: `pruned`, with `expires_at` and
/// `gone_at` at the deadline it passed. Returns their files. The caller
/// holds the investigation's lock.
pub(crate) async fn prune_if_due(
    tx: &mut WriteTx,
    id: &InvestigationId,
    now: DateTime<Utc>,
) -> Result<Vec<ExpiredFile>, StoreError> {
    let files: Vec<(String, i64)> = tx
        .run(async |tx| {
            sqlx::query_as(
                "UPDATE captured_results AS c
                 SET state = 'pruned', expires_at = i.retain_until, gone_at = i.retain_until
                 FROM investigations AS i
                 WHERE i.id = $1 AND i.retain_until <= $2
                   AND c.investigation_id = i.id AND c.state = 'kept'
                 RETURNING c.id, c.file_bytes",
            )
            .bind(id.as_str())
            .bind(now)
            .fetch_all(tx)
            .await
        })
        .await?;
    expired_files(files)
}

/// Append `payload`, encoded as `text`, as the next entry of the locked
/// investigation `id`, and advance it.
#[allow(clippy::too_many_arguments)] // one write, each part named
async fn append_in(
    tx: &mut WriteTx,
    id: &InvestigationId,
    locked: Locked,
    actor: &StoreActor,
    payload: &EntryPayload,
    text: &str,
    now: DateTime<Utc>,
    retention: RetentionRule,
) -> Result<LogEntry, StoreError> {
    let seq = locked.seq + 1;
    insert_entry(tx, id, seq, actor, now, payload.kind(), text).await?;
    advance(tx, id, seq, actor, now, Some(retention)).await?;
    Ok(LogEntry {
        seq,
        updated_seq: seq,
        actor: actor.to_wire(),
        at: wire_time(now),
        payload: payload.clone(),
    })
}

/// Write `outcome` on step `step_seq` of the locked investigation `id`, if
/// the step has none yet, and advance the investigation: the step fill of
/// [`InvestigationStore::complete_step_until`], inside the caller's transaction,
/// which locked through [`lock_for_write`]. The evidence clock slides by
/// `slide`, or stays put for `None` ([`advance`]). Returns whether it
/// wrote; a step that already has an outcome, or a `seq` that names no
/// step, changes nothing.
#[allow(clippy::too_many_arguments)] // one write, each part named
pub(crate) async fn fill_step_in(
    tx: &mut WriteTx,
    id: &InvestigationId,
    locked: Locked,
    step_seq: u64,
    outcome: &StepOutcome,
    now: DateTime<Utc>,
    slide: Option<RetentionRule>,
) -> Result<bool, StoreError> {
    let running = tx
        .run(async |tx| {
            sqlx::query(
                "SELECT payload, actor_key_id, actor_prefix, actor_name FROM investigation_log
                 WHERE investigation_id = $1 AND seq = $2 AND kind = 'step' AND outcome IS NULL",
            )
            .bind(id.as_str())
            .bind(bind_u64(step_seq))
            .fetch_optional(tx)
            .await
        })
        .await?;
    let Some(running) = running else {
        return Ok(false);
    };
    let mut payload = decode_payload(&running, EntryKind::Step)?;
    if let EntryPayload::Step {
        outcome: written, ..
    } = &mut payload
    {
        *written = Some(outcome.clone());
    }
    let text = encode_payload(&payload)?;
    let actor = StoreActor {
        key_id: running.try_get("actor_key_id")?,
        prefix: running.try_get("actor_prefix")?,
        name: running.try_get("actor_name")?,
    };
    let seq = locked.seq + 1;
    let code = outcome.code.as_ref().map(code_str).transpose()?;
    tx.run(async |tx| {
        sqlx::query(
            "UPDATE investigation_log
             SET outcome = $3, outcome_code = $4, result_id = $5, payload = $6,
                 updated_seq = $7
             WHERE investigation_id = $1 AND seq = $2 AND outcome IS NULL",
        )
        .bind(id.as_str())
        .bind(bind_u64(step_seq))
        .bind(outcome_str(outcome.outcome))
        .bind(code)
        .bind(outcome.result_id.as_deref())
        .bind(&text)
        .bind(bind_u64(seq))
        .execute(tx)
        .await
    })
    .await?;
    advance(tx, id, seq, &actor, now, slide).await?;
    Ok(true)
}

/// Rows of `(id, file_bytes)` as the files to unlink.
fn expired_files(files: Vec<(String, i64)>) -> Result<Vec<ExpiredFile>, StoreError> {
    files
        .into_iter()
        .map(|(id, file_bytes)| {
            u64::try_from(file_bytes)
                .map(|file_bytes| ExpiredFile { id, file_bytes })
                .map_err(|e| StoreError::Unavailable(column_error("file_bytes", e)))
        })
        .collect()
}

/// Record entry `seq` of kind `kind` with `payload`, as of `now`.
async fn insert_entry(
    tx: &mut WriteTx,
    id: &InvestigationId,
    seq: u64,
    actor: &StoreActor,
    now: DateTime<Utc>,
    kind: EntryKind,
    payload: &str,
) -> Result<(), StoreError> {
    tx.run(async |tx| {
        sqlx::query(
            "INSERT INTO investigation_log (
                 investigation_id, seq, updated_seq, kind, actor_key_id, actor_prefix,
                 actor_name, at, payload)
             VALUES ($1, $2, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id.as_str())
        .bind(bind_u64(seq))
        .bind(kind_str(kind))
        .bind(actor.key_id)
        .bind(&actor.prefix)
        .bind(&actor.name)
        .bind(now)
        .bind(payload)
        .execute(tx)
        .await
    })
    .await?;
    Ok(())
}

/// Move the locked investigation to `seq`, changed at `now` by `actor`,
/// and slide its evidence clock by `slide`, or leave both deadlines where
/// they are for `None`: bookkeeping no principal asked for, as boot's
/// interruptions ([`InvestigationStore::interrupt_running_steps`]).
async fn advance(
    tx: &mut WriteTx,
    id: &InvestigationId,
    seq: u64,
    actor: &StoreActor,
    now: DateTime<Utc>,
    slide: Option<RetentionRule>,
) -> Result<(), StoreError> {
    tx.run(async |tx| {
        sqlx::query(
            "UPDATE investigations
             SET seq = $2, updated_at = $3,
                 updated_by_key_id = $4, updated_by_prefix = $5, updated_by_name = $6,
                 retain_until = CASE WHEN $7::timestamptz IS NULL THEN retain_until
                                     ELSE GREATEST(retain_until, LEAST(retain_ceiling, $7)) END
             WHERE id = $1",
        )
        .bind(id.as_str())
        .bind(bind_u64(seq))
        .bind(now)
        .bind(actor.key_id)
        .bind(&actor.prefix)
        .bind(&actor.name)
        .bind(slide.map(|rule| rule.slid(now)))
        .execute(tx)
        .await
    })
    .await?;
    Ok(())
}

fn encode_payload(payload: &EntryPayload) -> Result<String, StoreError> {
    serde_json::to_string(payload)
        .map_err(|e| StoreError::Validation(format!("the entry does not encode: {e}")))
}

/// The table's spelling of an entry kind, as `investigation_log_kind_check`
/// lists them.
fn kind_str(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Step => "step",
        EntryKind::Keep => "keep",
        EntryKind::Finding => "finding",
        EntryKind::FindingStatus => "finding_status",
        EntryKind::Note => "note",
        EntryKind::Status => "status",
        EntryKind::Title => "title",
        EntryKind::Question => "question",
    }
}

fn parse_kind(text: &str) -> Option<EntryKind> {
    Some(match text {
        "step" => EntryKind::Step,
        "keep" => EntryKind::Keep,
        "finding" => EntryKind::Finding,
        "finding_status" => EntryKind::FindingStatus,
        "note" => EntryKind::Note,
        "status" => EntryKind::Status,
        "title" => EntryKind::Title,
        "question" => EntryKind::Question,
        _ => return None,
    })
}

fn status_str(status: InvestigationStatus) -> &'static str {
    match status {
        InvestigationStatus::Open => "open",
        InvestigationStatus::Closed => "closed",
    }
}

/// The table's spelling of a step outcome, as
/// `investigation_log_outcome_check` lists them.
fn outcome_str(outcome: StepOutcomeKind) -> &'static str {
    match outcome {
        StepOutcomeKind::Success => "success",
        StepOutcomeKind::Refused => "refused",
        StepOutcomeKind::Timeout => "timeout",
        StepOutcomeKind::Failed => "failed",
        StepOutcomeKind::Aborted => "aborted",
        StepOutcomeKind::Interrupted => "interrupted",
    }
}

/// An error code's wire spelling, for the `outcome_code` column.
fn code_str(code: &ErrorCode) -> Result<String, StoreError> {
    match serde_json::to_value(code) {
        Ok(serde_json::Value::String(text)) => Ok(text),
        _ => Err(StoreError::Validation(format!(
            "error code {code:?} has no wire spelling"
        ))),
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

fn decode_u64(row: &PgRow, column: &str) -> Result<u64, sqlx::Error> {
    let value: i64 = row.try_get(column)?;
    u64::try_from(value).map_err(|e| column_error(column, e))
}

fn decode_status(row: &PgRow) -> Result<InvestigationStatus, sqlx::Error> {
    let text: String = row.try_get("status")?;
    match text.as_str() {
        "open" => Ok(InvestigationStatus::Open),
        "closed" => Ok(InvestigationStatus::Closed),
        _ => Err(column_error("status", format!("unknown value {text:?}"))),
    }
}

fn decode_investigation(row: &PgRow) -> Result<InvestigationRow, sqlx::Error> {
    let id: String = row.try_get("id")?;
    Ok(InvestigationRow {
        id: InvestigationId::parse(&id).map_err(|e| column_error("id", e))?,
        title: row.try_get("title")?,
        question: row.try_get("question")?,
        scope: row.try_get("scope")?,
        status: decode_status(row)?,
        seq: decode_u64(row, "seq")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        retain_until: row.try_get("retain_until")?,
        retain_ceiling: row.try_get("retain_ceiling")?,
        created_by: StoreActor {
            key_id: row.try_get("created_by_key_id")?,
            prefix: row.try_get("created_by_prefix")?,
            name: row.try_get("created_by_name")?,
        },
        updated_by: StoreActor {
            key_id: row.try_get("updated_by_key_id")?,
            prefix: row.try_get("updated_by_prefix")?,
            name: row.try_get("updated_by_name")?,
        },
    })
}

/// The row's payload, which must be tagged with `kind`.
fn decode_payload(row: &PgRow, kind: EntryKind) -> Result<EntryPayload, sqlx::Error> {
    let text: String = row.try_get("payload")?;
    let payload: EntryPayload =
        serde_json::from_str(&text).map_err(|e| column_error("payload", e))?;
    if payload.kind() != kind {
        return Err(column_error(
            "payload",
            format!(
                "a {} payload in a {} entry",
                kind_str(payload.kind()),
                kind_str(kind)
            ),
        ));
    }
    Ok(payload)
}

fn decode_entry(row: &PgRow) -> Result<LogEntry, sqlx::Error> {
    let kind_text: String = row.try_get("kind")?;
    let kind = parse_kind(&kind_text)
        .ok_or_else(|| column_error("kind", format!("unknown value {kind_text:?}")))?;
    let at: DateTime<Utc> = row.try_get("at")?;
    Ok(LogEntry {
        seq: decode_u64(row, "seq")?,
        updated_seq: decode_u64(row, "updated_seq")?,
        actor: Actor {
            prefix: row.try_get("actor_prefix")?,
            name: row.try_get("actor_name")?,
        },
        at: wire_time(at),
        payload: decode_payload(row, kind)?,
    })
}
