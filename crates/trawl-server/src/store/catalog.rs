// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed field catalog: type pins, per-service observations, and
//! conflict evidence (ADR-0009 slice 2).
//!
//! The catalog is the write-time type authority: compaction pins every
//! dynamic field's canonical type here BEFORE the first parquet file
//! carrying it is written, and conforms every batch to the pins — so
//! `union_by_name` across any set of trawl-written files can never
//! conflict. Pins are add-only until the repin machinery (#53) — and
//! because a pin is therefore permanent while its name is a client-chosen
//! JSON key, the catalog is bounded on both axes: name length by
//! [`trawl_core::schema::is_storable_field_name`], count by
//! [`MAX_PINNED_FIELDS`].

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row as _};
use trawl_core::schema::CanonicalType;

use super::error::StoreError;

/// A proposed pin for a field absent from the catalog.
#[derive(Debug, Clone)]
pub struct PinProposal {
    /// Field name (the global key).
    pub field: String,
    /// The type the proposing batch's pin algorithm derived.
    pub ty: CanonicalType,
    /// The service whose batch proposed the pin (audit trail).
    pub pinned_from: String,
}

/// One append-only conflict record: a batch column whose `TRY_CAST` to its
/// pin NULLED at least one value. A cast that nulls nothing is convergence,
/// not conflict, and is never recorded (see `ConformPlan::tally_conflicts`)
/// — an append-only row per lossless cast per compaction tick would grow
/// without bound.
#[derive(Debug, Clone)]
pub struct FieldConflict {
    /// Field name.
    pub field: String,
    /// The service whose batch disagreed with the pin.
    pub service: String,
    /// The `DuckDB` type the batch actually carried (free-form spelling —
    /// may be `JSON` or a complex type, not only the canonical five).
    pub observed_type: String,
    /// The pinned type the values were cast to.
    pub expected_type: CanonicalType,
    /// Rows whose value the cast nulled (recoverable from `_raw`).
    pub rows_nulled: u64,
}

/// A `field_services` observation row.
#[derive(Debug, Clone)]
pub struct FieldServiceRow {
    /// Service name.
    pub service: String,
    /// First time compaction observed the field for this service.
    pub first_seen: DateTime<Utc>,
    /// Most recent observation (consumers window on this — rows are
    /// ever-observed and retention never reconciles them).
    pub last_seen: DateTime<Utc>,
    /// Cumulative rows compacted in batches that wrote the field — the sum
    /// of per-batch row counts, not a per-value non-null tally.
    pub row_count: i64,
}

/// A `field_conflicts` row as read back (types as stored text).
#[derive(Debug, Clone)]
pub struct FieldConflictRow {
    /// Service that disagreed.
    pub service: String,
    /// Observed `DuckDB` type spelling.
    pub observed_type: String,
    /// Expected (pinned) type spelling.
    pub expected_type: String,
    /// Rows nulled by the conforming cast.
    pub rows_nulled: i64,
    /// When the conflict was recorded.
    pub at: DateTime<Utc>,
}

/// Maximum number of fields the catalog will ever pin.
///
/// Field names are client-chosen JSON keys, and a pin is PERMANENT (pins
/// are add-only until the repin machinery, #53, and retention never
/// reconciles `field_services`). Without a count bound, a sender that
/// embeds identifiers in its keys — `user_12345_status`, accidental or
/// hostile — grows postgres, the in-process [`crate::catalog::FieldCatalog`]
/// cache, and every snapshot taken of it without limit. Name LENGTH is
/// bounded by [`trawl_core::schema::is_storable_field_name`]; this bounds
/// the COUNT.
///
/// A field denied a pin gets the same treatment as an unstorable name: it
/// is absent from the pin map, so compaction's conform step drops the
/// column and the values stay findable in `_raw`. Deliberately generous —
/// a real corpus that legitimately reaches five figures of distinct field
/// names has a modelling problem this cap should surface, not a capacity
/// problem trawl should silently absorb.
pub const MAX_PINNED_FIELDS: i64 = 10_000;

/// One field name shortened for logging: a name can be as long as a client
/// made it, so echoing it whole turns the log line into an amplifier of
/// whatever was sent.
fn short_name(name: &str) -> String {
    let head: String = name.chars().take(48).collect();
    format!("{head}... ({} bytes)", name.len())
}

/// The names in `names` that cannot be a catalog key, de-duplicated and
/// each shortened for logging (see [`short_name`]).
fn unstorable_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = names
        .filter(|n| !trawl_core::schema::is_storable_field_name(n))
        .map(short_name)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// One warning per call site for skipped names. Deliberately a warning and
/// not an error: the alternative — failing the statement — wedges the
/// caller (compaction retains the batch and re-fails forever), which is the
/// failure this skipping exists to prevent.
fn warn_unstorable(op: &str, rejected: &[String]) {
    tracing::warn!(
        event_type = "catalog_field_name_unstorable",
        op,
        max_bytes = trawl_core::schema::MAX_FIELD_NAME_BYTES,
        fields = ?rejected,
        "field name(s) too long to be a catalog key — skipped; the column is \
         not pinned (and so not stored), values remain in _raw"
    );
    bump_rejected("name_too_long", rejected.len());
}

/// Count fields denied a pin, by reason. No field-name label: the names are
/// client-chosen and unbounded, which is the very problem being counted.
fn bump_rejected(reason: &'static str, count: usize) {
    metrics::counter!(
        crate::metrics::CATALOG_PINS_REJECTED_TOTAL,
        "reason" => reason
    )
    .increment(count as u64);
}

/// Postgres-backed field catalog. Cheap to clone (shared pool).
#[derive(Debug, Clone)]
pub struct CatalogStore {
    pool: PgPool,
    pin_cap: i64,
}

impl CatalogStore {
    /// Wrap the shared app-state pool (must already be migrated — see
    /// [`super::HistoryStore::new`] for the contract).
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            pin_cap: MAX_PINNED_FIELDS,
        }
    }

    /// Override the pin-count cap. Exists so tests can drive the cap
    /// boundary without inserting [`MAX_PINNED_FIELDS`] rows; production
    /// uses the constant.
    #[must_use]
    pub fn with_pin_cap(mut self, cap: i64) -> Self {
        self.pin_cap = cap;
        self
    }

    /// Load every pin in the catalog.
    ///
    /// A stored spelling outside the canonical five fails loudly — the
    /// catalog is written by code, so that is corruption, not data.
    pub async fn load_pins(&self) -> Result<Vec<(String, CanonicalType)>, StoreError> {
        let rows = sqlx::query("SELECT field, duckdb_type FROM field_types ORDER BY field")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let field: String = row.try_get("field")?;
                let spelling: String = row.try_get("duckdb_type")?;
                let ty = CanonicalType::from_duckdb(&spelling).ok_or_else(|| {
                    sqlx::Error::Decode(
                        format!("field_types.{field} holds non-canonical type {spelling:?}").into(),
                    )
                })?;
                Ok((field, ty))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    /// Pin every proposal whose field is absent from the catalog, then
    /// return the AUTHORITATIVE pins for all proposed fields.
    ///
    /// First-writer-wins: `INSERT ... ON CONFLICT (field) DO NOTHING`, then
    /// a re-read — so two racing batches (or a proposal against an existing
    /// pin) both come back with the same pin, never their own proposal.
    ///
    /// A field whose name cannot be a catalog key (see
    /// [`unstorable_names`]) is DROPPED from the proposal set rather than
    /// allowed to error the insert: pinning gates every parquet write, so
    /// one such name would otherwise retain the batch for retry on every
    /// tick, forever. Absent from the returned map, the column is simply
    /// unpinned — which the conform step already treats as "drop it".
    ///
    /// The same treatment bounds the catalog's SIZE: the insert only fills
    /// the slots left under [`MAX_PINNED_FIELDS`], so a batch arriving at a
    /// full catalog pins nothing new and its novel columns are dropped from
    /// the parquet with their values still in `_raw` (never an `Err` — that
    /// would wedge compaction for a condition retrying cannot clear).
    pub async fn pin_missing(
        &self,
        proposals: &[PinProposal],
    ) -> Result<HashMap<String, CanonicalType>, StoreError> {
        let rejected = unstorable_names(proposals.iter().map(|p| p.field.as_str()));
        if !rejected.is_empty() {
            warn_unstorable("pin", &rejected);
        }
        let proposals: Vec<&PinProposal> = proposals
            .iter()
            .filter(|p| trawl_core::schema::is_storable_field_name(&p.field))
            .collect();
        if proposals.is_empty() {
            return Ok(HashMap::new());
        }
        let fields: Vec<&str> = proposals.iter().map(|p| p.field.as_str()).collect();
        let types: Vec<&str> = proposals.iter().map(|p| p.ty.as_duckdb()).collect();
        let sources: Vec<&str> = proposals.iter().map(|p| p.pinned_from.as_str()).collect();

        // Only the free slots under the cap are filled. Already-pinned
        // proposals are excluded from `candidate` so they never consume a
        // slot, and the surplus is ordered by name so an overflowing batch
        // picks deterministically rather than by arrival accident.
        sqlx::query(
            "WITH candidate AS (
                 SELECT u.f, u.t, u.s, row_number() OVER (ORDER BY u.f) AS rn
                 FROM UNNEST($1::text[], $2::text[], $3::text[]) AS u(f, t, s)
                 WHERE NOT EXISTS (SELECT 1 FROM field_types x WHERE x.field = u.f)
             ),
             capacity AS (
                 SELECT GREATEST($4::bigint - (SELECT count(*) FROM field_types), 0) AS slots
             )
             INSERT INTO field_types (field, duckdb_type, pinned_from)
             SELECT c.f, c.t, c.s FROM candidate c, capacity
             WHERE c.rn <= capacity.slots
             ON CONFLICT (field) DO NOTHING",
        )
        .bind(&fields)
        .bind(&types)
        .bind(&sources)
        .bind(self.pin_cap)
        .execute(&self.pool)
        .await?;

        let rows = sqlx::query("SELECT field, duckdb_type FROM field_types WHERE field = ANY($1)")
            .bind(&fields)
            .fetch_all(&self.pool)
            .await?;
        let pins = rows
            .iter()
            .map(|row| {
                let field: String = row.try_get("field")?;
                let spelling: String = row.try_get("duckdb_type")?;
                let ty = CanonicalType::from_duckdb(&spelling).ok_or_else(|| {
                    sqlx::Error::Decode(
                        format!("field_types.{field} holds non-canonical type {spelling:?}").into(),
                    )
                })?;
                Ok((field, ty))
            })
            .collect::<Result<HashMap<_, _>, sqlx::Error>>()
            .map_err(StoreError::from)?;

        // Whatever the insert could not seat comes back missing from the
        // authoritative re-read — the one place that sees the cap bite,
        // however the slots were lost (full catalog, or a racing batch that
        // took the last ones).
        let denied: Vec<String> = {
            let mut d: Vec<String> = fields
                .iter()
                .filter(|f| !pins.contains_key(**f))
                .map(|f| short_name(f))
                .collect();
            d.sort_unstable();
            d.dedup();
            d
        };
        if !denied.is_empty() {
            tracing::warn!(
                event_type = "catalog_pin_cap_reached",
                cap = self.pin_cap,
                fields = ?denied,
                "field catalog is full — field(s) left unpinned; their columns \
                 are not stored, values remain in _raw"
            );
            bump_rejected("cap", denied.len());
        }
        Ok(pins)
    }

    /// Upsert per-service observations for a compacted batch: `first_seen`
    /// is set once, `last_seen` advances, `row_count` accumulates.
    ///
    /// `row_count` is ADDED to the stored value, so callers must pass the
    /// rows THIS batch wrote — never a whole-file total, which would
    /// re-count every earlier batch on every tick.
    ///
    /// Names that cannot be a catalog key are skipped for the same reason
    /// as in [`Self::pin_missing`] — `field_services` keys on
    /// `(field, service)`, so an over-long name overflows this btree too.
    pub async fn touch_services(
        &self,
        service: &str,
        fields: &[String],
        row_count: u64,
    ) -> Result<(), StoreError> {
        let rejected = unstorable_names(fields.iter().map(String::as_str));
        if !rejected.is_empty() {
            warn_unstorable("observe", &rejected);
        }
        let fields: Vec<&str> = fields
            .iter()
            .map(String::as_str)
            .filter(|f| trawl_core::schema::is_storable_field_name(f))
            .collect();
        if fields.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO field_services (field, service, row_count)
             SELECT f, $2, $3 FROM UNNEST($1::text[]) AS t(f)
             ON CONFLICT (field, service) DO UPDATE
             SET last_seen = now(),
                 row_count = field_services.row_count + EXCLUDED.row_count",
        )
        .bind(&fields)
        .bind(service)
        .bind(i64::try_from(row_count).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Append conflict evidence rows (never aggregated).
    pub async fn record_conflicts(&self, conflicts: &[FieldConflict]) -> Result<(), StoreError> {
        if conflicts.is_empty() {
            return Ok(());
        }
        let fields: Vec<&str> = conflicts.iter().map(|c| c.field.as_str()).collect();
        let services: Vec<&str> = conflicts.iter().map(|c| c.service.as_str()).collect();
        let observed: Vec<&str> = conflicts.iter().map(|c| c.observed_type.as_str()).collect();
        let expected: Vec<&str> = conflicts
            .iter()
            .map(|c| c.expected_type.as_duckdb())
            .collect();
        let nulled: Vec<i64> = conflicts
            .iter()
            .map(|c| i64::try_from(c.rows_nulled).unwrap_or(i64::MAX))
            .collect();

        sqlx::query(
            "INSERT INTO field_conflicts
                 (field, service, observed_type, expected_type, rows_nulled)
             SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[])",
        )
        .bind(&fields)
        .bind(&services)
        .bind(&observed)
        .bind(&expected)
        .bind(&nulled)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read the observation rows for one field, most recent first.
    pub async fn field_services(&self, field: &str) -> Result<Vec<FieldServiceRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT service, first_seen, last_seen, row_count
             FROM field_services WHERE field = $1
             ORDER BY last_seen DESC, service",
        )
        .bind(field)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(FieldServiceRow {
                    service: row.try_get("service")?,
                    first_seen: row.try_get("first_seen")?,
                    last_seen: row.try_get("last_seen")?,
                    row_count: row.try_get("row_count")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    /// Read the conflict rows for one field, most recent first.
    pub async fn conflicts_for_field(
        &self,
        field: &str,
    ) -> Result<Vec<FieldConflictRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT service, observed_type, expected_type, rows_nulled, at
             FROM field_conflicts WHERE field = $1
             ORDER BY at DESC, id DESC",
        )
        .bind(field)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(FieldConflictRow {
                    service: row.try_get("service")?,
                    observed_type: row.try_get("observed_type")?,
                    expected_type: row.try_get("expected_type")?,
                    rows_nulled: row.try_get("rows_nulled")?,
                    at: row.try_get("at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    /// The catalog's stable identity (mirrored into the `data/CATALOG`
    /// marker so `DATABASE_URL` repoints and data-root restores are
    /// self-detecting).
    pub async fn catalog_id(&self) -> Result<String, StoreError> {
        let id: String = sqlx::query_scalar("SELECT catalog_id::text FROM catalog_state")
            .fetch_one(&self.pool)
            .await?;
        Ok(id)
    }

    /// Whether the boot conformance pass has completed for this catalog.
    pub async fn is_conformed(&self) -> Result<bool, StoreError> {
        let conformed: bool =
            sqlx::query_scalar("SELECT conformed_at IS NOT NULL FROM catalog_state")
                .fetch_one(&self.pool)
                .await?;
        Ok(conformed)
    }

    /// Record boot-conformance completion.
    pub async fn mark_conformed(&self) -> Result<(), StoreError> {
        sqlx::query("UPDATE catalog_state SET conformed_at = now()")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
