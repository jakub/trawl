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
//! [`super::ResultsStore::prune_evidence`] every one past its deadline, as
//! the capture gate's [`super::ResultsStore::prune_evidence_until`] does
//! with the same statements. So the evidence prune waits for a write that
//! moves the deadline and sees the deadline it committed, and no evidence
//! attaches behind a prune that could not see it.
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
//! write, and the sweep's hourly cadence never decides what survives. A
//! pruned row is final: only a promotion makes a row kept, and only a
//! temporary one. So evidence a prune ended at its `now` stays ended when
//! a write that took an earlier `now` slides the deadline back past it;
//! that write finds the rows pruned, as it would after the sweep.
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
//! wait for a connection, their `BEGIN`, every statement before the commit
//! and their rollback end at the caller's deadline, with nothing written
//! ([`StoreError::WaitExpired`]), and both sides bound the statements.
//! Postgres bounds each statement by a `statement_timeout` set from what
//! remains of the deadline before each. The setting covers all of a
//! statement's lock waits and the work between them, so a statement that
//! waits on one row after another, or one that waits after an earlier
//! statement did, still ends at the deadline. The client bounds every
//! await of the write at the same deadline, the connection's, the
//! `BEGIN`'s, each statement's and the rollback's, so an answer that never
//! comes (a stalled socket, a paused backend) ends the write there too. A
//! deadline that passed between two statements, as it does for a task
//! descheduled there, refuses the next statement, or the commit, before
//! anything is sent. A write's transaction is a [`WriteTx`], whose
//! connection is reachable only one statement at a time, each bounded so
//! ([`WriteTx::run`]); every other write runs the same code unbounded. The
//! capture gate's evidence prune is bounded the same way, by its
//! reservation's deadline ([`super::ResultsStore::prune_evidence_until`]).
//!
//! A write that Postgres stopped, or that does not commit, rolls back and
//! waits for the rollback to be answered before it returns, and its
//! session goes back to the pool; under the deadline that wait ends there
//! too. A write the client cut short closes its session instead: the
//! answer it stopped awaiting is still on the way, and a rollback would
//! first have to read it. So does a write whose deadline passed before its
//! rollback was sent, or whose rollback is not answered by the deadline.
//! Postgres ends the transaction with the session ([`PooledTransaction`]),
//! and its own wait at the deadline by the timeout. A session goes back to
//! the pool only after an answered rollback or commit. Their commit is
//! never cut short: the timeout is lifted first, and the deadline is
//! checked again once the lift is answered, so a `COMMIT` is sent only
//! before the deadline, and only a `COMMIT` already sent is awaited past
//! it.
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
use super::results::{ExpiredFile, drop_facts, wire_time};
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
    /// the deadline, [`StoreError::WaitExpired`] with nothing written,
    /// whatever the write awaited then; the commit itself is never cut
    /// short ([`WriteTx`]). Every step outcome is written so, bounded,
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
                    sqlx::query_as(concat!(
                        "UPDATE captured_results
                         SET state = 'pruned', expires_at = $2, gone_at = $2, ",
                        drop_facts!(),
                        " WHERE investigation_id = $1 AND state = 'kept'
                         RETURNING id, file_bytes"
                    ))
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

    /// The stored bytes of the investigation's kept evidence: each file and
    /// its facts document, of every kept row, whatever its deadline. Each
    /// result is one row holding one file and at most one facts document,
    /// so each counts once. The capture gate's binding evidence pass reads
    /// this after it prunes every investigation past its deadline
    /// ([`crate::results::charge`], "Evidence"), so a kept row it counts is
    /// live, or one the prune could not reach and that still holds its
    /// bytes.
    pub async fn evidence_usage(&self, id: &InvestigationId) -> Result<u64, StoreError> {
        let bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(file_bytes + metadata_bytes), 0)::bigint FROM captured_results
             WHERE state = 'kept' AND investigation_id = $1",
        )
        .bind(id.as_str())
        .fetch_one(&self.pool)
        .await?;
        u64::try_from(bytes).map_err(|e| {
            StoreError::Unavailable(column_error("sum(file_bytes + metadata_bytes)", e))
        })
    }

    /// The stored bytes of every investigation's kept evidence, as
    /// [`Self::evidence_usage`] counts them.
    pub async fn installation_evidence_usage(&self) -> Result<u64, StoreError> {
        let bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(file_bytes + metadata_bytes), 0)::bigint FROM captured_results
             WHERE state = 'kept' AND investigation_id IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        u64::try_from(bytes).map_err(|e| {
            StoreError::Unavailable(column_error("sum(file_bytes + metadata_bytes)", e))
        })
    }

    /// The stored bytes of the investigation's live evidence at `now`, each
    /// file and its facts document: its kept results while its
    /// `retain_until` is after `now`, pruned or not (the due-write rule,
    /// module docs). Only the capture gate's evidence
    /// pre-check reads this. The pre-check only refuses, so leaving dead
    /// rows out there can only let a charge on to the binding pass, which
    /// prunes them before it counts ([`Self::evidence_usage`]). A count
    /// that admitted on this would be unsafe: a write whose `now` is before
    /// the deadline slides it, and the rows left out are live again.
    pub async fn live_evidence_usage(
        &self,
        id: &InvestigationId,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(r.file_bytes + r.metadata_bytes), 0)::bigint
             FROM captured_results r
             JOIN investigations i ON i.id = r.investigation_id
             WHERE r.state = 'kept' AND r.investigation_id = $1 AND i.retain_until > $2",
        )
        .bind(id.as_str())
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        u64::try_from(bytes).map_err(|e| {
            StoreError::Unavailable(column_error("sum(file_bytes + metadata_bytes)", e))
        })
    }

    /// The stored bytes of every investigation's live evidence at `now`, as
    /// [`Self::live_evidence_usage`] counts them, for the same pre-check.
    pub async fn live_installation_evidence_usage(
        &self,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(r.file_bytes + r.metadata_bytes), 0)::bigint
             FROM captured_results r
             JOIN investigations i ON i.id = r.investigation_id
             WHERE r.state = 'kept' AND i.retain_until > $1",
        )
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        u64::try_from(bytes).map_err(|e| {
            StoreError::Unavailable(column_error("sum(file_bytes + metadata_bytes)", e))
        })
    }
}

/// How long the waits of a write's transaction may last.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LockWait {
    /// As long as the pool's acquire timeout and the database's own
    /// settings allow.
    Unbounded,
    /// No later than the deadline (module docs), on both sides: the client
    /// stops awaiting the connection, the `BEGIN`, every statement and the
    /// rollback at the deadline, and Postgres ends every statement there
    /// too, by a `statement_timeout` set from what remains before each
    /// ([`WriteTx::run`]). Either ending is [`StoreError::WaitExpired`].
    /// A commit already sent is never cut short.
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
    /// An await on the connection was cut short at the deadline, its
    /// answer still on the way. The transaction then ends by closing the
    /// session, never by a rollback that would first have to read that
    /// answer ([`PooledTransaction`]).
    cut: bool,
}

impl WriteTx {
    /// A transaction from `pool`, its wait for a connection and its `BEGIN`
    /// bounded as `wait` allows.
    ///
    /// A `BEGIN` cut short at the deadline closes its connection, which
    /// ends the transaction in Postgres with the session
    /// ([`PooledTransaction`]). A deadline that passed after the answer
    /// ends the transaction the same way, with no rollback sent, as
    /// [`StoreError::WaitExpired`] ([`Self::end`]).
    pub(crate) async fn begin(pool: &PgPool, wait: LockWait) -> Result<Self, StoreError> {
        let tx = match wait {
            LockWait::Unbounded => PooledTransaction::begin(pool.acquire().await?).await?,
            LockWait::Until(deadline) => {
                let conn = match deadline.run(pool.acquire()).await {
                    Ok(conn) => conn?,
                    Err(Expired) => return Err(StoreError::WaitExpired),
                };
                match deadline.run(PooledTransaction::begin(conn)).await {
                    Ok(tx) => tx?,
                    Err(Expired) => return Err(StoreError::WaitExpired),
                }
            }
        };
        let tx = Self {
            tx,
            wait,
            cut: false,
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
    /// the write together end at the deadline ([`timeout_millis`]). A
    /// deadline already past refuses the statement before anything is
    /// sent, as `WaitExpired`, and the write ends by closing its session,
    /// since no time is left for a rollback either ([`Self::end`]): a
    /// passed deadline grants no statement a fresh millisecond.
    ///
    /// The client bounds the same awaits, the setting's and the
    /// statement's, at the deadline itself, so an answer that never comes
    /// (a stalled socket, a paused backend) ends the write there too: cut
    /// short, as `WaitExpired`, and the transaction closed with its
    /// session ([`Self::end`]).
    pub(crate) async fn run<T, E>(
        &mut self,
        statement: impl AsyncFnOnce(&mut PgConnection) -> Result<T, E>,
    ) -> Result<T, StoreError>
    where
        StoreError: From<E>,
    {
        let LockWait::Until(deadline) = self.wait else {
            return Ok(statement(&mut self.tx).await?);
        };
        let Some(millis) = timeout_millis(deadline.remaining()) else {
            return Err(StoreError::WaitExpired);
        };
        self.within(deadline, async move |tx| {
            statement_timeout(tx, &format!("{millis}ms")).await?;
            Ok(statement(tx).await?)
        })
        .await
    }

    /// Await `io` on the connection no later than `deadline`. Cut short,
    /// the write is cut ([`Self::end`]) and answers
    /// [`StoreError::WaitExpired`], as does any later await of a write
    /// already cut.
    async fn within<T>(
        &mut self,
        deadline: Deadline,
        io: impl AsyncFnOnce(&mut PgConnection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if self.cut {
            return Err(StoreError::WaitExpired);
        }
        let tx = &mut self.tx;
        deadline.run(io(tx)).await.unwrap_or_else(|Expired| {
            self.cut = true;
            Err(StoreError::WaitExpired)
        })
    }

    /// Commit, and wait for Postgres to answer. Under a deadline the
    /// statement timeout is lifted first, so the commit itself is never cut
    /// short; the lift is a statement of the write, and one the deadline
    /// cuts short ends the transaction as [`StoreError::WaitExpired`]. A
    /// deadline already past refuses the commit before anything is sent:
    /// the write ends without it, as `WaitExpired`, its session closed
    /// ([`Self::end`]). The deadline is checked again once the lift is
    /// answered, since an answer already waiting when the write is next
    /// polled is taken however late that poll comes: only a `COMMIT` sent
    /// before the deadline is awaited past it.
    pub(crate) async fn commit(mut self) -> Result<(), StoreError> {
        if let LockWait::Until(deadline) = self.wait {
            if deadline.expired() {
                return Err(self.end(StoreError::WaitExpired).await);
            }
            if let Err(e) = self
                .within(deadline, async |tx| Ok(statement_timeout(tx, "0").await?))
                .await
            {
                return Err(self.end(e).await);
            }
            if deadline.expired() {
                return Err(self.end(StoreError::WaitExpired).await);
            }
        }
        self.tx.commit().await?;
        Ok(())
    }

    /// Roll back what the write did, and wait for Postgres to answer. Under
    /// a deadline the wait ends there: a write cut short, a deadline
    /// already past, or an answer that has not come by the deadline closes
    /// the session instead, as [`StoreError::WaitExpired`] ([`Self::end`]).
    pub(crate) async fn rollback(self) -> Result<(), StoreError> {
        if self.cut {
            return Err(StoreError::WaitExpired);
        }
        let LockWait::Until(deadline) = self.wait else {
            return Ok(self.tx.rollback().await?);
        };
        if deadline.expired() {
            return Err(StoreError::WaitExpired);
        }
        match deadline.run(self.tx.rollback()).await {
            Ok(rolled_back) => Ok(rolled_back?),
            Err(Expired) => Err(StoreError::WaitExpired),
        }
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
    ///
    /// Under a deadline the rollback is one more await of the write, and
    /// it ends at the deadline too ([`Self::rollback`]). A write cut short
    /// is not rolled back at all: the answer it stopped awaiting is still
    /// on the way, and a rollback would first have to read it. Nor is one
    /// whose deadline has already passed, and a rollback whose answer has
    /// not come by the deadline is given up there. Each drops the open
    /// transaction, which closes the session: Postgres ends the
    /// transaction with it, and the answer is [`StoreError::WaitExpired`].
    /// So a session goes back to the pool only after an answered rollback.
    pub(crate) async fn end(self, error: StoreError) -> StoreError {
        let wait = self.wait;
        match self.rollback().await {
            Err(StoreError::WaitExpired) => StoreError::WaitExpired,
            _ => match wait {
                LockWait::Until(_) if is_wait_ended(&error) => StoreError::WaitExpired,
                _ => error,
            },
        }
    }
}

/// The `statement_timeout`, in milliseconds, of a statement with
/// `remaining` of its deadline left, or `None` once nothing remains: a
/// timeout of zero waits forever. Rounded up, so no wait ends before the
/// deadline. The setting is a 32-bit count of milliseconds, about 24 days,
/// and a deadline may be a year away: such a wait ends early, as
/// [`StoreError::WaitExpired`].
fn timeout_millis(remaining: std::time::Duration) -> Option<u128> {
    (!remaining.is_zero()).then(|| {
        remaining
            .as_nanos()
            .div_ceil(1_000_000)
            .min(u128::from(i32::MAX.unsigned_abs()))
    })
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
            sqlx::query_as(concat!(
                "UPDATE captured_results AS c
                 SET state = 'pruned', expires_at = i.retain_until, gone_at = i.retain_until, ",
                drop_facts!(),
                " FROM investigations AS i
                 WHERE i.id = $1 AND i.retain_until <= $2
                   AND c.investigation_id = i.id AND c.state = 'kept'
                 RETURNING c.id, c.file_bytes"
            ))
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::timeout_millis;

    /// A positive budget is rounded up to the millisecond, however small,
    /// and capped at what the setting holds; none left is no timeout at
    /// all, never a fresh millisecond.
    #[test]
    fn only_a_positive_budget_has_a_statement_timeout() {
        assert_eq!(timeout_millis(Duration::ZERO), None);
        assert_eq!(timeout_millis(Duration::from_nanos(1)), Some(1));
        assert_eq!(timeout_millis(Duration::from_micros(1_500)), Some(2));
        assert_eq!(timeout_millis(Duration::from_secs(15)), Some(15_000));
        assert_eq!(
            timeout_millis(Duration::from_hours(365 * 24)),
            Some(u128::from(i32::MAX.unsigned_abs()))
        );
    }
}

#[cfg(test)]
mod pg_tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeDelta, TimeZone as _, Utc};
    use sqlx::PgPool;
    use trawl_api::investigations::{CreateInvestigation, EntryPayload, InvestigationId};

    use super::{
        InvestigationStore, LockWait, NewInvestigation, RetentionRule, StoreActor, StoreError,
        WriteTx, append_open_in, encode_payload,
    };
    use crate::deadline::Deadline;

    const RULE: RetentionRule = RetentionRule {
        idle: TimeDelta::days(7),
    };

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 9, 0, 0).unwrap()
    }

    fn agent() -> StoreActor {
        StoreActor {
            key_id: 7,
            prefix: "AbCd_-12".into(),
            name: "agent".into(),
        }
    }

    async fn investigation(store: &InvestigationStore) -> InvestigationId {
        let id = InvestigationId::parse(&format!("{:032x}", 310)).unwrap();
        store
            .create_if_absent(&NewInvestigation {
                id: id.clone(),
                actor: agent(),
                creation: CreateInvestigation {
                    title: "502s on checkout".into(),
                    question: "Why does checkout return 502?".into(),
                    scope: "checkout, last day".into(),
                },
                now: at(),
                retain_until: at() + TimeDelta::days(7),
                retain_ceiling: at() + TimeDelta::days(30),
            })
            .await
            .unwrap();
        id
    }

    /// Wait, bounded, until session `pid` is gone: the client closed it,
    /// and Postgres ended it and any transaction with it.
    async fn until_closed(pool: &PgPool, pid: i32) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let alive: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                )
                .bind(pid)
                .fetch_one(pool)
                .await
                .unwrap();
                if !alive {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the write's session was not closed");
    }

    /// A deadline that passed between two statements of a bounded write,
    /// as it does for a task descheduled there, refuses the next statement
    /// before anything is sent: it never runs, the answer is `WaitExpired`,
    /// and with no time left for a rollback either, the write ends by
    /// closing its session.
    #[sqlx::test]
    async fn a_statement_past_the_deadline_is_refused_unrun(pool: PgPool) {
        let deadline = Deadline::after(Duration::from_millis(300));
        let mut tx = WriteTx::begin(&pool, LockWait::Until(deadline))
            .await
            .unwrap();
        let pid: i32 = tx
            .run(async |tx| {
                sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(tx)
                    .await
            })
            .await
            .unwrap();
        tokio::time::sleep_until(deadline.instant() + Duration::from_millis(50)).await;

        let mut ran = false;
        let refused = tx
            .run(async |tx| {
                ran = true;
                sqlx::query("SELECT 1").execute(tx).await
            })
            .await;
        assert!(
            matches!(refused, Err(StoreError::WaitExpired)),
            "{refused:?}"
        );
        assert!(!ran, "the statement ran after its deadline");
        let ended = tx.end(StoreError::WaitExpired).await;
        assert!(matches!(ended, StoreError::WaitExpired), "{ended:?}");
        until_closed(&pool, pid).await;
    }

    /// A deadline that passed after a bounded write's last statement
    /// refuses its commit before anything is sent, and with no time left
    /// for a rollback, the write ends by closing its session: Postgres
    /// rolls the whole write back with it, and nothing of it is written,
    /// neither the entry nor the investigation's sequence.
    #[sqlx::test]
    async fn a_commit_past_the_deadline_rolls_the_write_back(pool: PgPool) {
        let store = InvestigationStore::new(pool.clone());
        let id = investigation(&store).await;
        let note = EntryPayload::Note {
            text: "the pool is exhausted".into(),
        };
        let text = encode_payload(&note).unwrap();
        let deadline = Deadline::after(Duration::from_millis(500));
        let mut tx = WriteTx::begin(&pool, LockWait::Until(deadline))
            .await
            .unwrap();
        let pid: i32 = tx
            .run(async |tx| {
                sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(tx)
                    .await
            })
            .await
            .unwrap();
        append_open_in(&mut tx, &id, &agent(), &note, &text, at(), RULE)
            .await
            .unwrap();
        tokio::time::sleep_until(deadline.instant() + Duration::from_millis(50)).await;

        let refused = tx.commit().await;
        assert!(
            matches!(refused, Err(StoreError::WaitExpired)),
            "{refused:?}"
        );
        until_closed(&pool, pid).await;
        assert_eq!(store.get(&id).await.unwrap().unwrap().seq, 0, "committed");
        let page = store.read_since(&id, 0, 10).await.unwrap().unwrap();
        assert!(page.entries.is_empty(), "{page:?}");
    }

    /// A lift whose answer the write first sees after its deadline sends no
    /// commit: the write answers `WaitExpired`, and nothing of it is
    /// written.
    ///
    /// The deadline cannot cut such a lift short: `deadline.run` polls the
    /// lift before its timer, so an answer already in the socket wins. The
    /// test makes that order certain rather than likely. The proxy, on a
    /// runtime of its own, holds the lift's answer; the test blocks this
    /// runtime's only thread past the deadline, lets the answer through,
    /// and waits until the proxy has forwarded it. The write is next polled
    /// with its answer waiting, after its deadline.
    #[sqlx::test]
    async fn a_lift_answered_past_the_deadline_sends_no_commit(pool: PgPool) {
        let store = InvestigationStore::new(pool.clone());
        let id = investigation(&store).await;
        let note = EntryPayload::Note {
            text: "the pool is exhausted".into(),
        };
        let text = encode_payload(&note).unwrap();
        let proxy_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let upstream = (*pool.connect_options()).clone();
        let proxy = proxy_runtime
            .spawn(async move { crate::answer_hold::Proxy::start(&upstream).await })
            .await
            .unwrap();
        #[allow(clippy::disallowed_methods)] // one session, through the proxy
        let through = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(proxy.options(&pool.connect_options()))
            .await
            .unwrap();
        let deadline = Deadline::after(Duration::from_secs(1));
        let mut tx = WriteTx::begin(&through, LockWait::Until(deadline))
            .await
            .unwrap();
        append_open_in(&mut tx, &id, &agent(), &note, &text, at(), RULE)
            .await
            .unwrap();

        // The next message the session sends is the lift's `Bind`, whose
        // one parameter is `0`, one byte long.
        proxy.hold_next(b"\x00\x00\x00\x010");
        let committing = tokio::spawn(tx.commit());
        proxy.holding().await;
        assert!(!deadline.expired(), "the write reached its lift too late");
        std::thread::sleep(deadline.remaining() + Duration::from_millis(50));
        assert!(deadline.expired());
        proxy.release();
        while proxy.let_through() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }

        let answer = committing.await.unwrap();
        assert!(matches!(answer, Err(StoreError::WaitExpired)), "{answer:?}");
        assert_eq!(store.get(&id).await.unwrap().unwrap().seq, 0, "committed");
        let page = store.read_since(&id, 0, 10).await.unwrap().unwrap();
        assert!(page.entries.is_empty(), "{page:?}");
        proxy_runtime.shutdown_background();
    }

    /// A kept capture of `investigation` holding a file of `file_bytes`,
    /// and the facts of a sightings read when `facts` is set.
    fn kept(
        n: u32,
        investigation: Option<&InvestigationId>,
        file_bytes: u64,
        facts: bool,
    ) -> crate::store::NewCapture {
        use trawl_api::results::{
            CaptureKind, Correlation, ResultShape, ResultSource, SightingsDenominators,
            SightingsFacts,
        };
        let correlation = facts.then(|| {
            crate::store::FactsDocument::encode(Correlation::Sightings(SightingsFacts {
                literal: "10.0.4.17".into(),
                matches_total: 2,
                captured: 2,
                attribution_fields: vec!["client_ip".into()],
                groups: vec![],
                fields: vec![],
                fields_omitted: 0,
                raw_only_rows: 0,
                denominators: SightingsDenominators {
                    field_counts_over: 2,
                    service_counts_over: 2,
                },
                follow_ups: vec![],
            }))
            .unwrap()
        });
        crate::store::NewCapture {
            id: format!("{n:032x}"),
            key_id: 7,
            created_at: at(),
            expires_at: at() + TimeDelta::minutes(15),
            query: "10.0.4.17".into(),
            executed_query: "\"10.0.4.17\"".into(),
            source: ResultSource::Stored,
            shape: ResultShape::Events,
            earliest: None,
            latest: None,
            columns: vec![],
            column_facts: vec![],
            captured_rows: 0,
            total_rows: 0,
            wire_bytes: 0,
            file_bytes,
            digest: (file_bytes > 0).then_some([1; 32]),
            format_version: 1,
            investigation_id: investigation.cloned(),
            kind: if facts {
                CaptureKind::Sightings
            } else {
                CaptureKind::Search
            },
            correlation,
        }
    }

    /// Every evidence byte count adds each kept row's facts document to
    /// its file: the binding per-investigation and installation sums, and
    /// the live pre-check pair. A temporary row's facts count in none.
    #[sqlx::test]
    async fn evidence_usage_counts_each_kept_rows_facts_document(pool: PgPool) {
        let store = InvestigationStore::new(pool.clone());
        let results = crate::store::ResultsStore::new(pool.clone());
        let a = investigation(&store).await;
        let b = InvestigationId::parse(&format!("{:032x}", 311)).unwrap();
        store
            .create_if_absent(&NewInvestigation {
                id: b.clone(),
                actor: agent(),
                creation: CreateInvestigation {
                    title: "504s".into(),
                    question: "Why 504?".into(),
                    scope: String::new(),
                },
                now: at(),
                retain_until: at() + TimeDelta::days(8),
                retain_ceiling: at() + TimeDelta::days(30),
            })
            .await
            .unwrap();
        let facts = kept(0, None, 0, true).metadata_bytes();
        assert!(facts > 0);
        for row in [
            kept(1, Some(&a), 100, false),
            kept(2, Some(&a), 200, true),
            kept(3, Some(&b), 400, true),
        ] {
            results.insert_kept_unlogged(&row).await.unwrap();
        }
        results.insert(&kept(4, None, 800, true)).await.unwrap();

        assert_eq!(store.evidence_usage(&a).await.unwrap(), 300 + facts);
        assert_eq!(store.evidence_usage(&b).await.unwrap(), 400 + facts);
        assert_eq!(
            store.installation_evidence_usage().await.unwrap(),
            700 + 2 * facts
        );
        let before = at() + TimeDelta::days(7) - TimeDelta::microseconds(1);
        assert_eq!(
            store.live_evidence_usage(&a, before).await.unwrap(),
            300 + facts
        );
        assert_eq!(
            store.live_evidence_usage(&b, before).await.unwrap(),
            400 + facts
        );
        assert_eq!(
            store
                .live_installation_evidence_usage(before)
                .await
                .unwrap(),
            700 + 2 * facts
        );
        let due = at() + TimeDelta::days(7);
        assert_eq!(store.live_evidence_usage(&a, due).await.unwrap(), 0);
        assert_eq!(
            store.live_installation_evidence_usage(due).await.unwrap(),
            400 + facts
        );
    }
}
