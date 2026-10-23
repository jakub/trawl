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
//! or a read marks it `expired`.
//!
//! The JSONB columns travel as text: they are bound as `$n::jsonb` and read
//! as `::text`, then decoded into the trawl-api types. A row whose metadata
//! does not decode is a [`StoreError::Unavailable`] carrying a
//! [`sqlx::Error::ColumnDecode`] that names the column, never a panic.

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row as _};
use trawl_api::results::{
    CapturedResult, ColumnFacts, ResultColumn, ResultShape, ResultSource, ResultState,
};

use super::error::{PgViolation, StoreError, classify_violation};
use super::history::bind_u64;

/// A row predicate: the result is live at `$N`. Spelled once so the list,
/// both usage reads and the prune agree on what counts.
macro_rules! live_at {
    ($now:literal) => {
        concat!(
            "(state = 'kept' OR (state = 'temporary' AND expires_at > ",
            $now,
            "))"
        )
    };
}

/// `SELECT` every column of `captured_results`, JSONB as text, followed by
/// `$tail`.
macro_rules! select_rows {
    ($tail:expr) => {
        concat!(
            "SELECT id, key_id, created_at, expires_at, gone_at, state, query, \
             executed_query, source, shape, earliest, latest, \
             columns::text AS columns, column_facts::text AS column_facts, \
             captured_rows, total_rows, wire_bytes, file_bytes, digest, file_missing, \
             format_version, investigation_id \
             FROM captured_results ",
            $tail
        )
    };
}

/// A capture to record: always `temporary`, with a deadline.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCapture {
    /// 32 lowercase hex digits; the file's name too.
    pub id: String,
    /// The fleet key that made the capture.
    pub key_id: i64,
    /// When the capture committed.
    pub created_at: DateTime<Utc>,
    /// When the temporary capture stops being readable.
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
    /// The investigation holding a kept result (ADR-0059).
    pub investigation_id: Option<i64>,
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

/// Postgres-backed storage for captured-result metadata. Cheap to clone
/// (shared pool).
#[derive(Debug, Clone)]
pub struct ResultsStore {
    pool: PgPool,
}

impl ResultsStore {
    /// Wrap the shared app-state pool, which must point at a migrated trawl
    /// app-state database.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a new temporary capture.
    ///
    /// A row that breaks a table constraint (a malformed id, a digest
    /// without a file) is a server fault and surfaces as
    /// [`StoreError::Unavailable`] carrying the database error, which names
    /// the constraint.
    pub async fn insert(&self, row: &NewCapture) -> Result<CaptureInsert, StoreError> {
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
        let inserted = sqlx::query(
            "INSERT INTO captured_results (
                 id, key_id, created_at, expires_at, state, query, executed_query,
                 source, shape, earliest, latest, columns, column_facts,
                 captured_rows, total_rows, wire_bytes, file_bytes, digest, format_version)
             VALUES ($1, $2, $3, $4, 'temporary', $5, $6, $7, $8, $9, $10,
                     $11::jsonb, $12::jsonb, $13, $14, $15, $16, $17, $18)",
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
        .bind(columns)
        .bind(column_facts)
        .bind(bind_u64(row.captured_rows))
        .bind(bind_u64(row.total_rows))
        .bind(bind_u64(row.wire_bytes))
        .bind(bind_u64(row.file_bytes))
        .bind(row.digest)
        .bind(format_version)
        .execute(&self.pool)
        .await;
        match inserted {
            Ok(_) => Ok(CaptureInsert::Inserted),
            Err(e) if classify_violation(&e) == Some(PgViolation::ResultIdTaken) => {
                Ok(CaptureInsert::IdTaken)
            }
            Err(e) => Err(StoreError::from(e)),
        }
    }

    /// The row `id` owned by `key_id`, in any state. Another key's result
    /// reads as absent.
    pub async fn get(&self, id: &str, key_id: i64) -> Result<Option<CaptureRow>, StoreError> {
        let row = sqlx::query(select_rows!("WHERE id = $1 AND key_id = $2"))
            .bind(id)
            .bind(key_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(decode_row).transpose()?)
    }

    /// The key's live results at `now`, newest first.
    pub async fn list(
        &self,
        key_id: i64,
        now: DateTime<Utc>,
    ) -> Result<Vec<CaptureRow>, StoreError> {
        let rows = sqlx::query(select_rows!(concat!(
            "WHERE key_id = $1 AND ",
            live_at!("$2"),
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

    /// Record that the result's file is absent. Returns whether the flag
    /// changed; a second call returns `false`.
    pub async fn mark_file_missing(&self, id: &str) -> Result<bool, StoreError> {
        let changed = sqlx::query(
            "UPDATE captured_results SET file_missing = TRUE
             WHERE id = $1 AND NOT file_missing",
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
    pub async fn delete_tombstones(&self, before: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(
            sqlx::query("DELETE FROM captured_results WHERE gone_at <= $1")
                .bind(before)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// The key's committed live usage at `now`.
    ///
    /// Committed rows only: reservations for captures still running are
    /// the capture gate's business, and it adds them to this.
    pub async fn usage(&self, key_id: i64, now: DateTime<Utc>) -> Result<Usage, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT count(*) AS results, COALESCE(sum(file_bytes), 0)::bigint AS bytes
             FROM captured_results WHERE key_id = $1 AND ",
            live_at!("$2")
        ))
        .bind(key_id)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(decode_usage(&row)?)
    }

    /// The installation's committed live usage at `now`, across every key.
    ///
    /// Committed rows only, as for [`Self::usage`].
    pub async fn installation_usage(&self, now: DateTime<Utc>) -> Result<Usage, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT count(*) AS results, COALESCE(sum(file_bytes), 0)::bigint AS bytes
             FROM captured_results WHERE ",
            live_at!("$1")
        ))
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(decode_usage(&row)?)
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

    /// The keys holding a live result at `now`, ascending, for boot's
    /// per-key quota pass.
    pub async fn live_keys(&self, now: DateTime<Utc>) -> Result<Vec<i64>, StoreError> {
        Ok(sqlx::query_scalar(concat!(
            "SELECT DISTINCT key_id FROM captured_results WHERE ",
            live_at!("$1"),
            " ORDER BY key_id"
        ))
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Prune the key's oldest temporary results until its live usage at
    /// `now` is at most `keep_results` results and at most `keep_bytes`
    /// file bytes (ADR-0057: reached when an operator lowers a quota below
    /// usage). Pruned rows get `gone_at = now`. Returns the files to unlink,
    /// oldest first.
    ///
    /// Kept results count toward usage but are never pruned, so a key whose
    /// kept results alone exceed a limit loses every temporary result and
    /// stays over it.
    ///
    /// The key's live rows are read `FOR UPDATE` and pruned in the same
    /// transaction, so a concurrent expiry or prune of those rows waits for
    /// it. A capture committed meanwhile is not counted; boot runs this
    /// before the server takes captures.
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
    /// oldest temporary results, whoever owns them, until its live usage at
    /// `now` is at most `keep_results` results and `keep_bytes` file bytes.
    /// Kept results count and are never pruned, as for one key.
    ///
    /// Every live row is read `FOR UPDATE`; boot runs this after the
    /// per-key pass and before the server takes captures.
    pub async fn prune_oldest_installation(
        &self,
        keep_results: u64,
        keep_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<Vec<ExpiredFile>, StoreError> {
        self.prune(None, keep_results, keep_bytes, now).await
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
                    "SELECT id, state, file_bytes FROM captured_results
                     WHERE key_id = $1 AND ",
                    live_at!("$2"),
                    " ORDER BY created_at, id FOR UPDATE"
                ))
                .bind(key_id)
                .bind(now)
                .fetch_all(&mut *tx)
                .await?
            }
            None => {
                sqlx::query(concat!(
                    "SELECT id, state, file_bytes FROM captured_results WHERE ",
                    live_at!("$1"),
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
            if decode_state(row)? == ResultState::Temporary {
                temporary.push(file);
            }
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

/// A timestamp as the wire spells it.
fn wire_time(at: DateTime<Utc>) -> String {
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
        investigation_id: row.try_get("investigation_id")?,
    })
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
