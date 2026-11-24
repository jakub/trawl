// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed investigations and their sequenced logs (ADR-0059).
//!
//! A transaction that locks investigations and their results locks the
//! investigations first, several in id order. So the evidence prune waits
//! for a write that moves the deadline and sees the deadline it committed,
//! and no evidence attaches behind a prune that could not see it.
//!
//! Every instant is a parameter from the caller's clock; no statement reads
//! SQL `now()`. Payloads are stored as text (JSONB refuses `\u0000`).

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

pub const MAX_LIST: u64 = 200;

/// How many times a create retries when the id it conflicted on vanished
/// before it could be read: an operator deleted it in between.
const CREATE_ATTEMPTS: usize = 3;

macro_rules! investigation_columns {
    () => {
        "id, title, question, scope, status, seq, created_at, updated_at, retain_until, \
         retain_ceiling, created_by_key_id, created_by_prefix, created_by_name, \
         updated_by_key_id, updated_by_prefix, updated_by_name"
    };
}

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

macro_rules! select_entries {
    ($tail:expr) => {
        concat!(
            "SELECT seq, updated_seq, kind, actor_prefix, actor_name, at, payload \
             FROM investigation_log ",
            $tail
        )
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionRule {
    pub idle: Duration,
}

impl RetentionRule {
    fn slid(self, now: DateTime<Utc>) -> DateTime<Utc> {
        now.checked_add_signed(self.idle)
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreActor {
    pub key_id: i64,
    /// The key's stable 8-character prefix.
    pub prefix: String,
    /// The key's name at the time of the write.
    pub name: String,
}

impl StoreActor {
    #[must_use]
    pub fn to_wire(&self) -> Actor {
        Actor {
            prefix: self.prefix.clone(),
            name: self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInvestigation {
    pub id: InvestigationId,
    pub actor: StoreActor,
    pub creation: CreateInvestigation,
    pub now: DateTime<Utc>,
    /// The first evidence deadline, at most `retain_ceiling`.
    pub retain_until: DateTime<Utc>,
    pub retain_ceiling: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationRow {
    pub id: InvestigationId,
    pub title: String,
    pub question: String,
    /// The scope text; empty when none was given.
    pub scope: String,
    pub status: InvestigationStatus,
    /// The sequence of the latest write; 0 at creation.
    pub seq: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub retain_until: DateTime<Utc>,
    pub retain_ceiling: DateTime<Utc>,
    pub created_by: StoreActor,
    pub updated_by: StoreActor,
}

impl InvestigationRow {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateOutcome {
    Created(InvestigationRow),
    /// The same creator already created it with the same request: a
    /// replay. Nothing changed, the sequence and the clock included.
    Existing(InvestigationRow),
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    pub investigation: InvestigationRow,
    pub entries: Vec<LogEntry>,
    /// The sequence the page reaches: the last entry's `updated_seq` when
    /// more remain, the investigation's `seq` otherwise.
    pub through_seq: u64,
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overwrite {
    Title(String),
    Question(String),
    Status(InvestigationStatus),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The investigation and its log are gone. Its kept results are pruned
    /// and their files are these, for the caller to unlink.
    Deleted(Vec<ExpiredFile>),
    NotFound,
    StepsRunning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Citing {
    Appended(Box<Appended>),
    KeptElsewhere(String),
    /// The named result can no longer be promoted: it is gone, past its
    /// deadline, or no longer the caller's to promote. Nothing was written.
    NotCitable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Locked {
    pub(crate) status: InvestigationStatus,
    pub(crate) seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appended {
    pub entry: LogEntry,
    /// The kept results the due-write rule pruned before the write, for
    /// the caller to unlink once the write committed.
    pub pruned: Vec<ExpiredFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overwritten {
    pub entry: LogEntry,
    /// The investigation as the write's own transaction left it, read
    /// after the sequence advanced: its `seq` is the entry's.
    pub investigation: InvestigationRow,
}

#[derive(Debug, Clone)]
pub struct InvestigationStore {
    pool: PgPool,
    #[cfg(any(test, feature = "test-support"))]
    faults: std::sync::Arc<super::results::fault::Faults>,
    #[cfg(any(test, feature = "test-support"))]
    statements: std::sync::Arc<StatementRecord>,
}

impl InvestigationStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            #[cfg(any(test, feature = "test-support"))]
            faults: std::sync::Arc::default(),
            #[cfg(any(test, feature = "test-support"))]
            statements: std::sync::Arc::default(),
        }
    }

    /// Gated on the feature alone: a `cfg(test)` item whose signature names
    /// `PgPool` reads as a test region to the pg-admission guard, which
    /// would then want every unit test of this crate in the postgres group.
    #[cfg(feature = "test-support")]
    pub(crate) fn sharing(
        pool: PgPool,
        faults: std::sync::Arc<super::results::fault::Faults>,
    ) -> Self {
        Self {
            pool,
            faults,
            statements: std::sync::Arc::default(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn statement_record(&self) -> &StatementRecord {
        &self.statements
    }

    pub async fn create_if_absent(
        &self,
        new: &NewInvestigation,
    ) -> Result<CreateOutcome, StoreError> {
        let creation = serde_json::to_string(&new.creation).map_err(|e| {
            tracing::debug!(error = %e, "an investigation create request did not encode");
            StoreError::Validation("the create request does not encode".to_owned())
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

    pub async fn get(&self, id: &InvestigationId) -> Result<Option<InvestigationRow>, StoreError> {
        let row = sqlx::query(select_investigation!("WHERE id = $1"))
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(decode_investigation).transpose()?)
    }

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

    pub async fn read_since(
        &self,
        id: &InvestigationId,
        since: u64,
        limit: u64,
    ) -> Result<Option<LogPage>, StoreError> {
        self.read_page(id, since, None, limit).await
    }

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

    /// The promotions run once per result, in ascending id order whatever
    /// the order of `promote`, so two appends into different
    /// investigations that cite the same results lock them in one order:
    /// the second waits for the first rather than closing a deadlock with
    /// it, and then finds them kept elsewhere.
    ///
    /// An investigation whose deadline is pinned at its ceiling stays due
    /// after the write slides it, and the due-write rule prunes again, the
    /// promoted results included.
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
        #[cfg(any(test, feature = "test-support"))]
        tx.record_in(&self.statements, "append_citing");
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
        #[cfg(any(test, feature = "test-support"))]
        tx.record_in(&self.statements, "begin_step");
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

    /// The investigation comes back from inside the same transaction, so
    /// no read after the commit can fail, or find it deleted, for a write
    /// that landed (ADR-0059).
    pub async fn overwrite(
        &self,
        id: &InvestigationId,
        expected_seq: u64,
        actor: &StoreActor,
        change: &Overwrite,
        now: DateTime<Utc>,
        retention: RetentionRule,
    ) -> Result<Overwritten, StoreError> {
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
            let row = tx
                .run(async |tx| {
                    sqlx::query(select_investigation!("WHERE id = $1"))
                        .bind(id.as_str())
                        .fetch_one(tx)
                        .await
                })
                .await?;
            Ok((seq, decode_investigation(&row)?))
        }
        .await;
        let (seq, investigation) = tx.finish(written).await?;
        Ok(Overwritten {
            entry: LogEntry {
                seq,
                updated_seq: seq,
                actor: actor.to_wire(),
                at: wire_time(now),
                payload,
            },
            investigation,
        })
    }

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

    /// A closed investigation still takes the step's outcome: the work was
    /// admitted while it was open. Evidence the due-write rule prunes here is left
    /// to the sweep.
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
        #[cfg(any(test, feature = "test-support"))]
        tx.record_in(&self.statements, "complete_step_until");
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
            total_rows: None,
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
    /// ceiling to it if need be (`server_manage`, checked by the caller).
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

    /// The investigation's kept results are pruned in the deleting transaction and
    /// lose their investigation id, so they age out as tombstones.
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

    /// Leaves out kept rows once `retain_until` is not after `now`. Only the
    /// capture gate's evidence pre-check reads this. A count that admitted on this
    /// would be unsafe: a write whose `now` is before the deadline slides it, and
    /// the rows left out are live again.
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

#[derive(Debug, Clone, Copy)]
pub(crate) enum LockWait {
    Unbounded,
    Until(Deadline),
}

/// The `statement_timeout` each statement of a store's bounded
/// investigation writes was given, in milliseconds, for a test to read.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct StatementRecord(parking_lot::Mutex<Option<Vec<(&'static str, Budgets)>>>);

#[cfg(any(test, feature = "test-support"))]
type Budgets = std::sync::Arc<parking_lot::Mutex<Vec<u128>>>;

#[cfg(any(test, feature = "test-support"))]
impl StatementRecord {
    pub fn arm(&self) {
        *self.0.lock() = Some(Vec::new());
    }

    #[must_use]
    pub fn writes(&self) -> Vec<(&'static str, Vec<u128>)> {
        self.0
            .lock()
            .iter()
            .flatten()
            .map(|(write, budgets)| (*write, budgets.lock().clone()))
            .collect()
    }

    fn open(&self, write: &'static str) -> Option<Budgets> {
        let budgets = Budgets::default();
        self.0
            .lock()
            .as_mut()?
            .push((write, std::sync::Arc::clone(&budgets)));
        Some(budgets)
    }
}

#[derive(Debug)]
pub(crate) struct WriteTx {
    tx: PooledTransaction,
    wait: LockWait,
    /// An await on the connection was cut short at the deadline, its
    /// answer still on the way. The transaction then ends by closing the
    /// session, never by a rollback that would first have to read that
    /// answer ([`PooledTransaction`]).
    cut: bool,
    #[cfg(any(test, feature = "test-support"))]
    budgets: Option<Budgets>,
}

impl WriteTx {
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
            #[cfg(any(test, feature = "test-support"))]
            budgets: None,
        };
        match wait {
            LockWait::Until(deadline) if deadline.expired() => {
                Err(tx.end(StoreError::WaitExpired).await)
            }
            _ => Ok(tx),
        }
    }

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
        #[cfg(any(test, feature = "test-support"))]
        let budgets = self.budgets.clone();
        self.within(deadline, async move |tx| {
            statement_timeout(tx, &format!("{millis}ms")).await?;
            #[cfg(any(test, feature = "test-support"))]
            if let Some(budgets) = budgets {
                budgets.lock().push(millis);
            }
            Ok(statement(tx).await?)
        })
        .await
    }

    #[cfg(any(test, feature = "test-support"))]
    fn record_in(&mut self, record: &StatementRecord, write: &'static str) {
        if matches!(self.wait, LockWait::Until(_)) {
            self.budgets = record.open(write);
        }
    }

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

    /// Under a deadline, the deadline is checked again once lifting
    /// `statement_timeout` is answered, since an answer already waiting when the
    /// write is next polled is taken however late that poll comes.
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

    async fn finish<T>(self, written: Result<T, StoreError>) -> Result<T, StoreError> {
        match written {
            Ok(written) => {
                self.commit().await?;
                Ok(written)
            }
            Err(e) => Err(self.end(e).await),
        }
    }

    /// Roll back and wait for Postgres to answer, because the caller may give a
    /// request's count back the moment this returns. A write cut short or past
    /// its deadline sends no rollback, and an unanswered one ends at the deadline.
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

pub(crate) async fn lock_for_write(
    tx: &mut WriteTx,
    id: &InvestigationId,
    now: DateTime<Utc>,
) -> Result<(Locked, Vec<ExpiredFile>), StoreError> {
    let locked = lock(tx, id).await?;
    let pruned = prune_if_due(tx, id, now).await?;
    Ok((locked, pruned))
}

/// The caller holds the investigation's lock.
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
    serde_json::to_string(payload).map_err(|e| {
        tracing::debug!(error = %e, "an investigation entry did not encode");
        StoreError::Validation("the entry does not encode".to_owned())
    })
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

fn code_str(code: &ErrorCode) -> Result<String, StoreError> {
    match serde_json::to_value(code) {
        Ok(serde_json::Value::String(text)) => Ok(text),
        _ => Err(StoreError::Validation(format!(
            "error code {code:?} has no wire spelling"
        ))),
    }
}

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
