// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query execution against `DuckDB`.
//!
//! Handles connection management, prepared statements, parameter binding,
//! and result extraction.

use std::path::Path;
use std::sync::Arc;

use duckdb::Connection;
use duckdb::types::{TimeUnit, ValueRef};
use trawl_core::emitter::{self, EmittedQuery, SqlValue};
use trawl_core::parser;

use crate::error::EngineError;
use crate::value::{Column, QueryResult, SchemaColumn, SchemaResult, Value};

/// Query executor backed by an in-memory `DuckDB` connection.
#[derive(Debug)]
pub struct Executor {
    conn: Connection,
}

impl Executor {
    /// Create a new executor with an in-memory `DuckDB` connection.
    ///
    /// Sets `temp_directory` to the system temp dir so `DuckDB` can spill
    /// to disk even when the process working directory is read-only (e.g.
    /// container overlay filesystems).
    pub fn new() -> Result<Self, EngineError> {
        let conn = Connection::open_in_memory()?;
        let tmp = std::env::temp_dir();
        conn.execute_batch(&format!(
            "SET temp_directory='{}'",
            tmp.to_string_lossy().replace('\'', "''")
        ))?;
        Ok(Self { conn })
    }

    /// Create a new executor sharing the same underlying database.
    ///
    /// The cloned connection benefits from `DuckDB`'s internal metadata
    /// caching (parquet file stats, column statistics) accumulated by
    /// other connections to the same database.
    pub fn try_clone(&self) -> Result<Self, EngineError> {
        let conn = self.conn.try_clone()?;
        Ok(Self { conn })
    }

    /// Get an interrupt handle for cancelling in-flight queries from another thread.
    pub fn interrupt_handle(&self) -> Arc<duckdb::InterruptHandle> {
        self.conn.interrupt_handle()
    }

    /// Lightweight health check: runs `SELECT 1` to verify the connection is alive.
    pub fn ping(&self) -> Result<(), EngineError> {
        self.conn
            .query_row("SELECT 1", [], |_row| Ok(()))
            .map_err(EngineError::from)
    }

    /// Full pipeline: parse DSL, emit SQL, execute.
    ///
    /// `utc_offset_secs` is applied to all timestamp values at format time.
    /// Pass `0` for UTC display.
    pub fn run_query(
        &self,
        dsl: &str,
        source: &str,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<QueryResult, EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit(&ast, source)?;
        let mut result = self.execute_emitted(&emitted, max_rows, utc_offset_secs)?;
        if !emitted.rust_stages.is_empty() {
            result = crate::post_process::apply_rust_stages(result, &emitted.rust_stages)?;
        }
        if emitted.needs_column_reorder {
            result.reorder_columns(trawl_api::value::WELL_KNOWN_LOG_FIELDS);
        }
        Ok(result)
    }

    /// Full pipeline with hot buffer: parse DSL, emit composite SQL, execute.
    ///
    /// The emitted SQL unions the primary parquet source with a hot buffer
    /// ndjson file via `UNION ALL BY NAME`.
    ///
    /// Handles cold-start gracefully: when the cold source matches no file at
    /// all, falls back to querying just the hot buffer so events ingested
    /// before the first compaction are visible. A columnless union result is
    /// not taken as proof of that — it is re-checked against the files on
    /// disk, because a list source reports "no files" for a single empty
    /// element too (ADR-0008).
    pub fn run_query_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<QueryResult, EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit_with_hot_source(&ast, source, hot_source)?;
        let mut outcome = self.execute_emitted(&emitted, max_rows, utc_offset_secs);

        // A column type conflict between the hot and cold sources (e.g. a
        // field that is BIGINT in parquet but VARCHAR in the hot snapshot)
        // would otherwise fail the union. Retry with the conflicting columns
        // coerced to VARCHAR on both sides, preserving hot AND cold data. A
        // failure that survives to the outcome policy below returns the error
        // whenever cold files exist — a cold-data drop is never silent.
        if let Some(cols) = self.hot_cold_conflict_columns(&outcome, source, hot_source) {
            let coerced = emitter::emit_with_hot_source_coerced(&ast, source, hot_source, &cols)?;
            outcome = self.execute_emitted(&coerced, max_rows, utc_offset_secs);
        }

        // `execute_emitted` maps DuckDB's "no files match the pattern" error
        // to an empty result. For a LIST source that error also fires when a
        // SINGLE element matches nothing — even when its siblings hold data —
        // so an empty result never proves a cold start. The server emits one
        // glob per hour in range and prunes only on parent-directory
        // existence, so a sparse-traffic service routinely gets elements
        // pointing at hour dirs holding no file of its own. Retry over just
        // the elements that match a file, so the cold rows that do exist are
        // never silently dropped (ADR-0008).
        //
        // The pruned read is the FIRST one that actually touches the cold
        // files, so it is also the first that can hit a hot/cold schema
        // conflict — it gets the same coerced retry, or a repairable conflict
        // would fall through to the outcome policy and hard-error.
        if matches!(&outcome, Ok(r) if r.columns.is_empty())
            && let Some(pruned) = self.pruned_cold_source(source)
        {
            let pruned_emitted = emitter::emit_with_hot_source(&ast, &pruned, hot_source)?;
            outcome = self.execute_emitted(&pruned_emitted, max_rows, utc_offset_secs);
            if let Some(cols) = self.hot_cold_conflict_columns(&outcome, &pruned, hot_source) {
                let coerced =
                    emitter::emit_with_hot_source_coerced(&ast, &pruned, hot_source, &cols)?;
                outcome = self.execute_emitted(&coerced, max_rows, utc_offset_secs);
            }
        }

        // Classify the (possibly retried) outcome, then route on the pure
        // `cold_action` decision so the outcome policy stays unit-testable.
        let class = match &outcome {
            // Columns present → real result (possibly empty rows).
            Ok(r) if !r.columns.is_empty() => HotColdOutcome::Columns,
            // No columns: the source matched no file at all, or the prune
            // above could not narrow it. Hot-only is safe only if a presence
            // check confirms there is no cold data to hide.
            Ok(_) => HotColdOutcome::NoColumns,
            // A binder error remapped to Emit (querying a nonexistent field)
            // — a user error, safe to keep the empty-result UX.
            Err(EngineError::Emit(_)) => HotColdOutcome::BenignBinder,
            // Any other database failure is only provably safe to mask when
            // no cold files exist.
            Err(EngineError::Database(_)) => HotColdOutcome::Recoverable,
            // ResultTooLarge, parse, etc. — propagate.
            Err(_) => HotColdOutcome::Fatal,
        };
        let hot_only = match cold_action(class) {
            ColdAction::ReturnOutcome => false,
            ColdAction::HotOnly => true,
            // Evaluated only on this path: hot-only is permitted exactly
            // when there is no cold data it could hide.
            ColdAction::HotOnlyIfNoColdFiles => !self.cold_files_present(source),
        };
        let mut result = if hot_only {
            let hot_emitted = emitter::emit(&ast, hot_source)?;
            match self.execute_emitted(&hot_emitted, max_rows, utc_offset_secs) {
                // Hot-only also hit a binder/emit error (e.g. empty ndjson
                // between compaction cycles). Treat as empty, not error.
                Err(EngineError::Emit(_)) => QueryResult::empty(),
                other => other?,
            }
        } else {
            outcome?
        };
        if !emitted.rust_stages.is_empty() {
            result = crate::post_process::apply_rust_stages(result, &emitted.rust_stages)?;
        }
        if emitted.needs_column_reorder {
            result.reorder_columns(trawl_api::value::WELL_KNOWN_LOG_FIELDS);
        }
        Ok(result)
    }

    /// Execute a pre-emitted query (SQL + params) against `DuckDB`.
    ///
    /// `max_rows` caps the number of result rows to prevent unbounded memory
    /// allocation. Returns [`EngineError::ResultTooLarge`] if exceeded.
    ///
    /// `utc_offset_secs` is applied to all timestamp values at format time.
    pub fn execute_emitted(
        &self,
        query: &EmittedQuery,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = match self.conn.prepare(&query.sql) {
            Ok(s) => s,
            Err(e) if is_no_files_error(&e) => return Ok(QueryResult::empty()),
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

        let params = bind_params(&query.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        // start query execution — column metadata is only available
        // after DuckDB resolves table-valued functions like read_parquet()
        let mut result_rows = match stmt.query(param_refs.as_slice()) {
            Ok(r) => r,
            Err(e) if is_no_files_error(&e) => return Ok(QueryResult::empty()),
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

        let stmt_ref =
            result_rows
                .as_ref()
                .ok_or(EngineError::Database(duckdb::Error::InvalidColumnName(
                    "statement unavailable after query execution".into(),
                )))?;
        let col_count = stmt_ref.column_count();
        let columns: Vec<Column> = stmt_ref
            .column_names()
            .into_iter()
            .map(|name| Column { name })
            .collect();

        let mut rows = Vec::new();
        while let Some(row) = result_rows.next()? {
            if rows.len() >= max_rows {
                return Err(EngineError::ResultTooLarge(max_rows));
            }
            let mut cells = Vec::with_capacity(col_count);
            for i in 0..col_count {
                cells.push(extract_value(row, i, utc_offset_secs));
            }
            rows.push(cells);
        }

        Ok(QueryResult { columns, rows })
    }

    /// Whether the cold source has any concrete files behind it.
    ///
    /// Consulted only on the fallback path of the hot+cold outcome policy: a
    /// hot-only read after an unexpected failure — or after a columnless
    /// result — is permitted exactly when there is no cold data it could
    /// hide. A plain glob is counted via
    /// `glob(?)`; a list source (`['a', 'b']`) is counted by globbing each
    /// element — its members are globs over hours that may hold no file yet,
    /// so membership alone proves nothing. Errs on the side of "present" so
    /// an unexpected failure surfaces as an error rather than degrading to
    /// hot-only success.
    fn cold_files_present(&self, source: &str) -> bool {
        match glob_list_items(source) {
            // Unparseable list → assume present (fail closed).
            Some(items) if items.is_empty() => true,
            Some(items) => items.iter().any(|item| self.glob_has_match(item)),
            None => self.glob_has_match(source),
        }
    }

    /// Narrow a list source (`['a', 'b']`) to the elements that match at
    /// least one file on disk.
    ///
    /// `read_parquet` rejects the whole list when ANY element matches nothing,
    /// so a list carrying both populated and empty hour globs — the normal
    /// shape for a sparse-traffic service — reads as "no files" and would drop
    /// the cold rows that do exist. Pruning the empty elements makes the read
    /// succeed over exactly the same data.
    ///
    /// Returns `None` when there is nothing to prune: a plain glob, a list
    /// this parser doesn't understand, a list where no element matches (the
    /// genuine cold start), or one where every element already matches.
    fn pruned_cold_source(&self, source: &str) -> Option<String> {
        let items = glob_list_items(source)?;
        let matching: Vec<&str> = items
            .iter()
            .copied()
            .filter(|item| self.glob_has_match(item))
            .collect();
        if matching.is_empty() || matching.len() == items.len() {
            return None;
        }
        let quoted: Vec<String> = matching.iter().map(|item| format!("'{item}'")).collect();
        Some(format!("[{}]", quoted.join(", ")))
    }

    /// Whether `pattern` matches at least one file on disk. Errs on the side
    /// of "matches" when the `glob` call itself fails.
    fn glob_has_match(&self, pattern: &str) -> bool {
        self.conn
            .query_row("SELECT count(*)::BIGINT FROM glob(?)", [pattern], |row| {
                row.get::<_, i64>(0)
            })
            .map_or(true, |n| n > 0)
    }

    /// Describe the `(name, type)` of every column produced by `query`.
    fn describe_types(&self, query: &str) -> Result<Vec<(String, String)>, EngineError> {
        let mut stmt = self.conn.prepare(&format!("DESCRIBE {query}"))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
        }
        Ok(out)
    }

    /// The columns to coerce when `outcome` failed on a repairable hot/cold
    /// schema conflict, or `None` when it is not one.
    ///
    /// The `Conversion` class is only the cheap pre-filter: a genuine
    /// data-conversion error carries it too. What makes this a *schema*
    /// conflict is the evidence — at least one column the two sources describe
    /// differently — so the conflicting columns are gathered first and
    /// `is_union_type_conflict` is asked with them in hand. A data error yields
    /// none, is not misread as a conflict, and skips the retry that could not
    /// have helped it (ADR-0008).
    ///
    /// Asked of every cold source the union is executed over — the original one
    /// and, when the list prune narrows it, the pruned one too: the pruned read
    /// is the first that actually touches the cold files, so it is the first
    /// that can raise the conflict at all.
    fn hot_cold_conflict_columns(
        &self,
        outcome: &Result<QueryResult, EngineError>,
        source: &str,
        hot_source: &str,
    ) -> Option<Vec<String>> {
        match outcome {
            Err(EngineError::Database(e)) if is_conversion_error(e) => self
                .hot_cold_conflicts(source, hot_source)
                .ok()
                .filter(|cols| is_union_type_conflict(e, cols)),
            _ => None,
        }
    }

    /// Find columns shared by the cold (parquet) and hot (ndjson) sources
    /// whose inferred types differ — the columns that must be coerced to
    /// VARCHAR for the hot+cold `UNION ALL BY NAME` to bind.
    ///
    /// Returns an error (caught by the caller, which then falls back to
    /// hot-only) if either source cannot be described — e.g. on cold start
    /// with no parquet files.
    fn hot_cold_conflicts(
        &self,
        source: &str,
        hot_source: &str,
    ) -> Result<Vec<String>, EngineError> {
        let cold_reader = emitter::source_reader(source)?;
        let hot_reader = emitter::hot_source_reader(hot_source)?;

        let cold = self.describe_types(&format!("SELECT * FROM {cold_reader}"))?;
        // Describe the hot side with the same timestamp cast the union
        // applies, so the always-TIMESTAMP key isn't flagged as a conflict.
        let hot = self.describe_types(&format!(
            "SELECT * REPLACE (TRY_CAST(\"timestamp\" AS TIMESTAMP) AS \"timestamp\") FROM {hot_reader}"
        ))?;

        let hot_types: std::collections::HashMap<String, String> = hot.into_iter().collect();
        Ok(cold
            .into_iter()
            .filter(|(name, ty)| hot_types.get(name).is_some_and(|h| h != ty))
            .map(|(name, _)| name)
            .collect())
    }

    /// Describe the schema without reading row data.
    ///
    /// Fast path: a single `DESCRIBE` over `read_parquet(union_by_name=true)`.
    /// `DuckDB` resolves that describe without reading rows, and its result is
    /// exactly the schema a *successful* query sees: for all-scalar columns and
    /// for union-able same-kind complex columns (`STRUCT`-vs-`STRUCT`,
    /// `LIST`-vs-`LIST`) it reports the MERGED type — e.g. `STRUCT(x INTEGER)`
    /// in one file and `STRUCT(y INTEGER)` in another merge to
    /// `STRUCT(x INTEGER, y INTEGER)`, which a query then reads fine.
    ///
    /// The fast path lies in exactly one situation: when a column's per-file
    /// types are NOT union-reconcilable (a complex type in one file and a
    /// `VARCHAR`/scalar in another, or different complex kinds like
    /// `STRUCT`-vs-`LIST`). There the describe still reports the complex side
    /// (it never reads rows) but a real query fails at read time with a Binder
    /// "remap" / `Conversion` error, and the executor's coerced retry casts the
    /// column to `VARCHAR` — so a query effectively sees `VARCHAR`.
    ///
    /// So we keep the fast-path describe as the baseline and only RECONCILE
    /// when it reports a complex type that *might* be masking an irreconcilable
    /// mix. The reconcile per-file-describes and overrides a column to
    /// `VARCHAR` ONLY when its observed per-file types span more than one
    /// top-level kind (scalar-vs-complex or mixed complex kinds) — the cases a
    /// real query cannot read. Union-able same-kind drift keeps the merged
    /// fast-path type, so `/api/v1/schema` reports what queries actually
    /// return. The error-triggered branch is kept as insurance for any
    /// `DuckDB` version/path where the describe itself raises a union conflict.
    pub fn describe_schema(&self, source: &str) -> Result<SchemaResult, EngineError> {
        // Validate source path before interpolation — DuckDB doesn't truly
        // parameterize table-valued function arguments.
        emitter::validate_source_path(source)?;

        let columns = match self.describe_schema_columns(source) {
            // No complex columns → the fast path is authoritative and cannot be
            // masking drift; return it without paying O(files) per-file
            // describes on every /schema call.
            Ok(cols) if !cols.iter().any(|c| is_complex_type(&c.data_type)) => cols,
            // A complex type MIGHT be masking an irreconcilable cross-file mix
            // the fast-path describe can't see. Reconcile per-file, but keep the
            // merged fast-path type for any column whose drift is union-able.
            // This is gated on complex-type PRESENCE, not actual drift: detecting
            // drift requires the per-file describes themselves, so a column that
            // is a consistent `STRUCT` across all files still pays O(files) here.
            // In practice compaction coerces complex columns to VARCHAR at write
            // time, so steady-state parquet is all-scalar and takes the fast path
            // above; this branch only fires on un-migrated/external parquet.
            Ok(cols) => self.describe_schema_columns_coerced(source, cols)?,
            // Insurance: some paths/versions raise the conflict at describe.
            // There is no second source to gather conflict evidence against
            // here — the per-file reconcile below IS the evidence gathering —
            // so the conversion class is the right (and only) trigger.
            Err(EngineError::Database(e)) if is_conversion_error(&e) => {
                self.describe_schema_columns_coerced(source, Vec::new())?
            }
            Err(e) => return Err(e),
        };

        // Count matching files.
        let file_count: i64 =
            self.conn
                .query_row("SELECT count(*)::BIGINT FROM glob(?)", [source], |row| {
                    row.get(0)
                })?;

        Ok(SchemaResult {
            columns,
            file_count: u64::try_from(file_count).unwrap_or(0),
        })
    }

    /// Fast-path schema describe: a single `DESCRIBE` over the union of all
    /// matching parquet files. Cheap (no row reads) but blind to cross-file
    /// drift — the caller reconciles per-file when it reports a complex type.
    fn describe_schema_columns(&self, source: &str) -> Result<Vec<SchemaColumn>, EngineError> {
        let mut stmt = self
            .conn
            .prepare("DESCRIBE SELECT * FROM read_parquet(?, union_by_name=true)")?;
        let mut rows = stmt.query([source])?;

        let mut columns = Vec::new();
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let data_type: String = row.get(1)?;
            columns.push(SchemaColumn { name, data_type });
        }
        Ok(columns)
    }

    /// Reconcile a parquet glob's schema against the fast-path describe by
    /// per-file-describing every file and overriding ONLY the columns whose
    /// drift a real query cannot read.
    ///
    /// `fast_path` is the `read_parquet(union_by_name=true)` describe — the
    /// schema a *successful* query sees, including the MERGED type for union-
    /// able same-kind complex drift (`STRUCT`-vs-`STRUCT`, `LIST`-vs-`LIST`).
    /// We keep it verbatim except for columns whose per-file observed types
    /// span more than one top-level kind (a complex type vs a scalar, or mixed
    /// complex kinds like `STRUCT`-vs-`LIST`): those make a real query error,
    /// so the executor's coerced retry casts them to `VARCHAR`, and that is
    /// what `/api/v1/schema` must advertise.
    ///
    /// `fast_path` may be empty (the insurance path, where the fast-path
    /// describe itself raised a union conflict); then every irreconcilable
    /// column is reported `VARCHAR`, a column with union-able drift gets
    /// `DuckDB`'s reconciled type (a per-column `union_by_name` describe — not
    /// a first-seen guess), and a column with a single observed type keeps it.
    /// Column order follows the fast path when present, else first-seen per-file
    /// order, so the result is deterministic.
    fn describe_schema_columns_coerced(
        &self,
        source: &str,
        fast_path: Vec<SchemaColumn>,
    ) -> Result<Vec<SchemaColumn>, EngineError> {
        use indexmap::IndexMap;

        // Glob-expand to concrete files. `glob(?)` is parameterized, so no
        // interpolation here; the per-file DESCRIBE below interpolates the
        // resulting paths (trusted — derived from the operator's data_dir),
        // escaping single quotes to stay consistent with the rest of the file.
        let mut glob_stmt = self.conn.prepare("SELECT file FROM glob(?)")?;
        let mut glob_rows = glob_stmt.query([source])?;
        let mut files: Vec<String> = Vec::new();
        while let Some(row) = glob_rows.next()? {
            files.push(row.get::<_, String>(0)?);
        }

        // First-seen column order → observed distinct per-file types.
        let mut col_types: IndexMap<String, Vec<String>> = IndexMap::new();
        for file in &files {
            let safe = file.replace('\'', "''");
            let types = self.describe_types(&format!("SELECT * FROM read_parquet('{safe}')"))?;
            for (name, ty) in types {
                let observed = col_types.entry(name).or_default();
                if !observed.contains(&ty) {
                    observed.push(ty);
                }
            }
        }

        // A column is irreconcilable when its per-file types span more than one
        // top-level kind — the union would error and a query falls back to a
        // VARCHAR cast. Same-kind drift (incl. all-scalar) is union-able.
        let is_irreconcilable = |types: &[String]| -> bool {
            let mut kinds = types.iter().map(|t| top_level_kind(t));
            let Some(first) = kinds.next() else {
                return false;
            };
            kinds.any(|k| k != first)
        };

        // Prefer the fast-path baseline: it carries the merged union type for
        // union-able complex drift. Override a column to VARCHAR only when its
        // per-file types prove an irreconcilable mix.
        if !fast_path.is_empty() {
            return Ok(fast_path
                .into_iter()
                .map(|col| {
                    let irreconcilable = col_types
                        .get(&col.name)
                        .is_some_and(|types| is_irreconcilable(types));
                    if irreconcilable {
                        SchemaColumn {
                            name: col.name,
                            data_type: "VARCHAR".to_owned(),
                        }
                    } else {
                        col
                    }
                })
                .collect());
        }

        // Insurance path: no fast-path baseline (the fast-path describe itself
        // raised a union conflict). Reconcile from scratch:
        //  - irreconcilable drift (mixed top-level kinds) → VARCHAR, the type a
        //    real query's coerced retry produces;
        //  - union-able drift (same kind, >1 observed type — e.g. BIGINT vs
        //    DOUBLE, or two STRUCT shapes) → ask DuckDB for the reconciled type
        //    rather than guessing the first-seen one, which would under-report
        //    (report BIGINT for a column a query reads as DOUBLE);
        //  - a single observed type → use it (no ambiguity).
        // This branch is cold/defensive — unreachable on DuckDB versions where
        // DESCRIBE tolerates union-able drift — so the per-column describe's
        // O(drifted columns) extra queries here are acceptable.
        let file_list = files
            .iter()
            .map(|f| format!("'{}'", f.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(col_types
            .into_iter()
            .map(|(name, types)| {
                let data_type = if is_irreconcilable(&types) {
                    "VARCHAR".to_owned()
                } else if types.len() > 1 {
                    // Same-kind drift: defer to DuckDB's own reconciliation. If
                    // even the single-column describe errors, the column is in
                    // fact irreconcilable → VARCHAR.
                    self.reconciled_column_type(&file_list, &name)
                        .unwrap_or_else(|| "VARCHAR".to_owned())
                } else {
                    types
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| "VARCHAR".to_owned())
                };
                SchemaColumn { name, data_type }
            })
            .collect())
    }

    /// Ask `DuckDB` for the `union_by_name`-reconciled type of a single column
    /// across `file_list` — a pre-built, single-quote-escaped `'f1', 'f2', ...`
    /// SQL list. The column identifier is double-quote-escaped (`"` → `""`) for
    /// the same interpolation-safety reason the paths are quote-escaped.
    ///
    /// Returns `None` if even the single-column describe errors — at which point
    /// the column is genuinely irreconcilable and the caller falls back to
    /// `VARCHAR`. Only reached from the cold insurance path above.
    fn reconciled_column_type(&self, file_list: &str, column: &str) -> Option<String> {
        let ident = column.replace('"', "\"\"");
        let described = self
            .describe_types(&format!(
                "SELECT \"{ident}\" FROM read_parquet([{file_list}], union_by_name=true)"
            ))
            .ok()?;
        described.into_iter().next().map(|(_, ty)| ty)
    }

    /// Sample distinct values for a field (for autocomplete).
    ///
    /// Returns up to `limit` distinct values for the specified field, ordered
    /// lexicographically. Only string values are returned; numeric/timestamp
    /// fields are skipped.
    ///
    /// # Security
    ///
    /// The field name is validated to prevent SQL injection. Only alphanumeric
    /// characters, underscores, and dots are allowed.
    pub fn sample_field_values(
        &self,
        source: &str,
        field: &str,
        limit: usize,
    ) -> Result<Vec<String>, EngineError> {
        // Validate field name to prevent SQL injection.
        if !field
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            return Err(EngineError::Emit(
                trawl_core::emitter::EmitError::UnsupportedOperation {
                    message: format!("invalid field name: {field}"),
                },
            ));
        }

        trawl_core::emitter::validate_source_path(source)?;

        // Use DuckDB identifier quoting for safety.
        let sql = format!(
            r#"SELECT DISTINCT "{field}" FROM read_parquet(?, union_by_name=true)
               WHERE "{field}" IS NOT NULL
               ORDER BY 1
               LIMIT ?"#
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = stmt.query(duckdb::params![source, limit_i64])?;

        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            // Only return string values (skip numeric/timestamp fields).
            if let Ok(val) = row.get::<_, String>(0) {
                values.push(val);
            }
        }

        Ok(values)
    }

    /// Export query results directly to a Parquet file via `DuckDB` `COPY TO`.
    ///
    /// Uses a temp table to stage the query results, then writes them to
    /// the output path as Snappy-compressed Parquet.
    ///
    /// Returns an error if the pipeline contains Rust post-processing stages
    /// (e.g. `extract kv`) since those can't be expressed as pure SQL.
    pub fn export_parquet(
        &self,
        dsl: &str,
        source: &str,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit(&ast, source)?;
        self.export_parquet_from_emitted(&emitted, output_path, max_rows)
    }

    /// Export with hot buffer union, falling back to hot-only on cold start.
    ///
    /// Routes through the same outcome gate as [`Self::run_query_with_hot`]:
    /// a database failure over an existing cold corpus returns the error
    /// instead of silently exporting hot-only data (ADR-0008).
    pub fn export_parquet_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit_with_hot_source(&ast, source, hot_source)?;
        let mut outcome = self.export_parquet_from_emitted(&emitted, output_path, max_rows);

        // Same prune retry as `run_query_with_hot`: `read_parquet` rejects a
        // LIST source wholesale when a SINGLE element matches nothing, even
        // when its siblings hold data. The server emits one glob per hour in
        // range and prunes only on parent-directory existence, so a
        // sparse-traffic service routinely gets elements pointing at hour dirs
        // holding no file of its own. Unlike the query path — where
        // `execute_emitted` maps the error to an empty result — the export
        // surfaces it raw, and the hot-only arm below cannot rescue it (a
        // matching sibling makes `cold_files_present` true). Retry over just
        // the elements that match a file so the cold rows that do exist are
        // exported (ADR-0008).
        if matches!(&outcome, Err(EngineError::Database(e)) if is_no_files_error(e))
            && let Some(pruned) = self.pruned_cold_source(source)
        {
            let pruned_emitted = emitter::emit_with_hot_source(&ast, &pruned, hot_source)?;
            outcome = self.export_parquet_from_emitted(&pruned_emitted, output_path, max_rows);
        }

        match outcome {
            // No parquet files / binder-shaped failure → hot-only is only
            // permitted when there is no cold data it could hide.
            Err(EngineError::Database(_) | EngineError::Emit(_))
                if !self.cold_files_present(source) =>
            {
                let hot_emitted = emitter::emit(&ast, hot_source)?;
                self.export_parquet_from_emitted(&hot_emitted, output_path, max_rows)
            }
            other => other,
        }
    }

    /// Internal: stage an emitted query into a temp table and COPY to parquet.
    fn export_parquet_from_emitted(
        &self,
        emitted: &EmittedQuery,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        if !emitted.rust_stages.is_empty() {
            return Err(EngineError::Emit(
                trawl_core::emitter::EmitError::UnsupportedOperation {
                    message: "parquet export is not supported for queries with post-processing stages (e.g. extract kv)".into(),
                },
            ));
        }

        let path_str = output_path.to_str().ok_or_else(|| {
            EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
                message: "output path is not valid UTF-8".into(),
            })
        })?;

        // Create temp table from query results.
        let create_sql = format!(
            "CREATE TEMP TABLE __trawl_export AS (SELECT * FROM ({}) LIMIT {max_rows})",
            emitted.sql
        );

        let params = bind_params(&emitted.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        let cleanup = |conn: &Connection| {
            let _ = conn.execute_batch("DROP TABLE IF EXISTS __trawl_export");
        };

        match self.conn.prepare(&create_sql) {
            Ok(mut stmt) => {
                if let Err(e) = stmt.execute(param_refs.as_slice()) {
                    cleanup(&self.conn);
                    return Err(e.into());
                }
            }
            Err(e) => {
                cleanup(&self.conn);
                return Err(e.into());
            }
        }

        // COPY to parquet. DuckDB's COPY TO doesn't support parameterized
        // paths, so we escape single quotes by doubling them (DuckDB convention).
        let safe_path = path_str.replace('\'', "''");
        let copy_sql =
            format!("COPY __trawl_export TO '{safe_path}' (FORMAT PARQUET, COMPRESSION SNAPPY)");
        if let Err(e) = self.conn.execute_batch(&copy_sql) {
            // Clean up temp table and partial file on error.
            cleanup(&self.conn);
            let _ = std::fs::remove_file(output_path);
            return Err(e.into());
        }

        cleanup(&self.conn);
        Ok(())
    }

    /// Write an in-memory [`QueryResult`] to a Parquet file.
    ///
    /// Serializes the result as temp ndjson, then uses `DuckDB` `COPY TO`
    /// for efficient columnar conversion with Snappy compression.
    /// Returns the number of rows written.
    pub fn write_query_result_to_parquet(
        &self,
        result: &QueryResult,
        path: &Path,
    ) -> Result<usize, EngineError> {
        if result.rows.is_empty() {
            return Ok(0);
        }

        let path_str = path.to_str().ok_or_else(|| {
            EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
                message: "output path is not valid UTF-8".into(),
            })
        })?;

        // Write result as temp ndjson.
        let tmp_json = format!("{path_str}.ndjson.tmp");
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp_json)?;
            for row in &result.rows {
                let mut map = serde_json::Map::with_capacity(result.columns.len());
                for (col, val) in result.columns.iter().zip(row.iter()) {
                    map.insert(col.name.clone(), value_to_json(val));
                }
                serde_json::to_writer(&mut file, &map).map_err(|e| {
                    EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
                        message: format!("failed to serialize result row: {e}"),
                    })
                })?;
                file.write_all(b"\n")?;
            }
            file.flush()?;
        }

        // Use DuckDB to convert ndjson → parquet.
        let safe_json = tmp_json.replace('\'', "''");
        let safe_path = path_str.replace('\'', "''");
        let copy_sql = format!(
            "COPY (SELECT * FROM read_json('{safe_json}', format='newline_delimited', \
             records=true, auto_detect=true, field_appearance_threshold=0)) \
             TO '{safe_path}' (FORMAT PARQUET, COMPRESSION SNAPPY)"
        );

        let copy_result = self.conn.execute_batch(&copy_sql);

        // Always clean up the temp ndjson file.
        let _ = std::fs::remove_file(&tmp_json);

        copy_result?;
        Ok(result.rows.len())
    }

    /// Read a Parquet file into a [`QueryResult`].
    ///
    /// Used by the API's `get_report_run` endpoint to serve historical
    /// scheduled query results.
    pub fn read_parquet_to_result(
        &self,
        path: &Path,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError> {
        let path_str = path.to_str().ok_or_else(|| {
            EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
                message: "parquet path is not valid UTF-8".into(),
            })
        })?;

        let safe_path = path_str.replace('\'', "''");
        let sql = format!(
            "SELECT * FROM read_parquet('{safe_path}', union_by_name=true) LIMIT {max_rows}"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let mut result_rows = stmt.query([])?;

        let stmt_ref =
            result_rows
                .as_ref()
                .ok_or(EngineError::Database(duckdb::Error::InvalidColumnName(
                    "statement unavailable after query execution".into(),
                )))?;
        let col_count = stmt_ref.column_count();
        let columns: Vec<Column> = stmt_ref
            .column_names()
            .into_iter()
            .map(|name| Column { name })
            .collect();

        let mut rows = Vec::new();
        while let Some(row) = result_rows.next()? {
            let mut cells = Vec::with_capacity(col_count);
            for i in 0..col_count {
                cells.push(extract_value(row, i, 0));
            }
            rows.push(cells);
        }

        Ok(QueryResult { columns, rows })
    }
}

/// Convert a [`Value`] to a [`serde_json::Value`] for ndjson serialization.
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Integer(n) => serde_json::json!(n),
        Value::Float(f) => serde_json::json!(f),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(value_to_json).collect()),
    }
}

/// `DuckDB` error substring for "no matching files" — verified against `DuckDB` 1.4.x.
const DUCKDB_NO_FILES_MSG: &str = "No files found that match the pattern";
/// `DuckDB` error prefix for binder errors — verified against `DuckDB` 1.4.x.
const DUCKDB_BINDER_ERROR_MSG: &str = "Binder Error";

/// Check if a `DuckDB` error is the "No files found" error from `read_parquet()`
/// when a glob matches zero files. Semantically this means "no data" — not a
/// server error.
fn is_no_files_error(e: &duckdb::Error) -> bool {
    e.to_string().contains(DUCKDB_NO_FILES_MSG)
}

/// Check if a `DuckDB` error is a binder error about a missing column. This is
/// a user error (querying a nonexistent field), not a server error.
fn is_binder_column_error(e: &duckdb::Error) -> bool {
    let msg = e.to_string();
    msg.contains(DUCKDB_BINDER_ERROR_MSG) && (msg.contains("column") || msg.contains("not found"))
}

/// Leading `"<Class> Error"` token of a `DuckDB` message — `"Conversion"`
/// for `"Conversion Error: ..."`, `"Binder"` for `"Binder Error: ..."`.
///
/// `DuckDB` prefixes every exception with its class, so the token is
/// structured information carried through the message. Returns `None` when
/// the message does not lead with a class token (e.g. non-database
/// `duckdb::Error` variants).
fn error_class(msg: &str) -> Option<&str> {
    msg.split(':').next()?.strip_suffix(" Error")
}

/// Check if a `DuckDB` error carries the `Conversion` class — the class every
/// failed cast reports, whether its cause is a *schema* the sources cannot
/// reconcile or a *value* that cannot be converted.
///
/// Keys off the error's class token (never a substring match anywhere in the
/// body — ADR-0008). Every irreconcilable kind mix raises this class on the
/// bundled `DuckDB` 1.5.5, on both the `UNION ALL BY NAME` path and the
/// `read_parquet(union_by_name)` per-file remap path (pinned by
/// `conflicting_kind_mixes_all_carry_the_conversion_class`), so it is the
/// right trigger for a cast-to-`VARCHAR` schema-reconciling fallback — the
/// use compaction's rollup and merge paths make of it.
///
/// It is a *necessary but not sufficient* condition for a union type
/// conflict: a genuine data-conversion error (`CAST('abc' AS INTEGER)`)
/// carries the very same class, and no substring of the body separates the
/// two either — both shapes say a value "can't be cast to the destination
/// type" and both name a "source column". Callers that must not confuse the
/// two ask [`is_union_type_conflict`], which additionally requires schema
/// evidence.
///
/// Deliberately does NOT match corruption ("too small to be a Parquet
/// file") or missing-column binder errors: those are handled by quarantine
/// and the benign-binder carve-out respectively.
pub fn is_conversion_error(e: &duckdb::Error) -> bool {
    error_class(&e.to_string()) == Some("Conversion")
}

/// Check if a `DuckDB` failure is a union *type conflict*: two sources
/// disagreeing on a column's type, which a cast-to-`VARCHAR` retry can fix.
///
/// The error class alone cannot decide this — a genuine data-conversion
/// error (a VALUE that cannot be converted, e.g. `CAST('abc' AS INTEGER)`)
/// is `Conversion`-class too, and the message body does not separate them
/// (ADR-0008). So the classifier requires *evidence*: `conflicting_columns`
/// is the set of columns whose described type actually differs between the
/// two sources being unioned. A schema conflict always produces at least
/// one; a data error produces none, so it is never misread as a schema
/// conflict — it falls through to the caller's outcome policy, which returns
/// the error rather than silently dropping cold data.
fn is_union_type_conflict(e: &duckdb::Error, conflicting_columns: &[String]) -> bool {
    is_conversion_error(e) && !conflicting_columns.is_empty()
}

/// Classification of the hot+cold union outcome that `run_query_with_hot`
/// branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotColdOutcome {
    /// The union produced columns (rows may be empty) — an authoritative
    /// result; return it as-is.
    Columns,
    /// The union produced no columns at all — `execute_emitted` mapped a "no
    /// files match the pattern" error to an empty result. Usually the cold
    /// start, but a list source raises the same error for one non-matching
    /// element, so this is an observation, not proof: hot-only still needs a
    /// cold-file presence check.
    NoColumns,
    /// A binder error remapped to `Emit` (querying a nonexistent field) — a
    /// user error; hot-only keeps the established empty-result UX.
    BenignBinder,
    /// Any other database failure — hot-only is only provably safe when a
    /// cold-file presence check says no cold files exist.
    Recoverable,
    /// A non-recoverable error (e.g. `ResultTooLarge`, parse) — propagate it.
    Fatal,
}

/// What `run_query_with_hot` should do with a classified outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdAction {
    /// Return the union outcome unchanged (result or error).
    ReturnOutcome,
    /// Read hot-only — structurally safe, no cold data can be hidden.
    HotOnly,
    /// Read hot-only ONLY if no cold files exist; otherwise return the
    /// error. A cold-data drop must never be silent, and an arbitrary
    /// database failure must never masquerade as success (ADR-0008).
    HotOnlyIfNoColdFiles,
}

/// Decide the next step for a classified hot+cold union outcome.
///
/// Split out as a pure function so the outcome policy — hot-only fallback
/// is permitted only when it cannot hide cold data — is unit-testable
/// without provoking every failure class against a live `DuckDB`.
fn cold_action(outcome: HotColdOutcome) -> ColdAction {
    match outcome {
        HotColdOutcome::Columns | HotColdOutcome::Fatal => ColdAction::ReturnOutcome,
        HotColdOutcome::BenignBinder => ColdAction::HotOnly,
        HotColdOutcome::NoColumns | HotColdOutcome::Recoverable => ColdAction::HotOnlyIfNoColdFiles,
    }
}

/// Split a `DuckDB` list-of-globs source (`['a', 'b']`, as built by the
/// server's source resolver) into its quoted elements.
///
/// Returns `None` when `source` is not list-shaped (a plain glob), and
/// `Some(vec![])` when it is list-shaped but cannot be parsed — the caller
/// treats that as "assume cold files exist" so a parse gap never turns into
/// a silent cold-data drop.
///
/// Kept as a pure function so the list handling is unit-testable without a
/// live `DuckDB` connection.
fn glob_list_items(source: &str) -> Option<Vec<&str>> {
    let trimmed = source.trim();
    let inner = trimmed.strip_prefix('[')?;
    let Some(inner) = inner.strip_suffix(']') else {
        return Some(Vec::new());
    };

    let mut items = Vec::new();
    let mut rest = inner;
    while let Some(open) = rest.find('\'') {
        let after = &rest[open + 1..];
        // An unterminated quote means we don't understand the source.
        let Some(close) = after.find('\'') else {
            return Some(Vec::new());
        };
        items.push(&after[..close]);
        rest = &after[close + 1..];
    }
    Some(items)
}

/// Whether a `DuckDB` type name (as reported by `DESCRIBE`) is a complex
/// (nested) type — `STRUCT`/`MAP`/`LIST`/array `[]`/`UNION`/`JSON`. These are
/// the types that, when the same column is `VARCHAR` in another file, raise a
/// read-time union conflict. A schema describe that reports one of these may
/// be masking cross-file drift that only surfaces at query time.
///
/// Shared with trawl-server's compaction path (re-exported in `lib.rs`) so the
/// schema describe and compaction's write-time coercion agree on exactly which
/// `DuckDB` types must be coerced to `VARCHAR`.
pub fn is_complex_type(data_type: &str) -> bool {
    let t = data_type.to_ascii_uppercase();
    t.starts_with("STRUCT")
        || t.starts_with("MAP")
        || t.starts_with("LIST")
        || t.starts_with("UNION")
        || t == "JSON"
        || t.ends_with("[]")
}

/// Classify a `DuckDB` type name (as reported by `DESCRIBE`) into its
/// top-level *kind* — the granularity at which `read_parquet(union_by_name)`
/// either reconciles a column or errors.
///
/// Two per-file types with the SAME kind are union-able: `STRUCT(x INTEGER)`
/// and `STRUCT(y INTEGER)` merge to `STRUCT(x INTEGER, y INTEGER)`;
/// `INTEGER[]` and `VARCHAR[]` merge to `VARCHAR[]`. Two types with DIFFERENT
/// kinds are not: a `STRUCT` vs a scalar `VARCHAR`, or a `STRUCT` vs a `LIST`,
/// makes a real query raise a Binder "remap" / `Conversion` error, after which
/// the executor casts the column to `VARCHAR`.
///
/// All scalar types collapse to a single `"scalar"` kind: scalar-vs-scalar
/// drift (e.g. `BIGINT` vs `VARCHAR`) is reconciled by the union to a common
/// type, so it is never flagged irreconcilable here.
fn top_level_kind(data_type: &str) -> &'static str {
    let t = data_type.trim().to_ascii_uppercase();
    if t.starts_with("STRUCT") {
        "struct"
    } else if t.starts_with("MAP") {
        "map"
    } else if t.starts_with("UNION") {
        "union"
    } else if t == "JSON" {
        "json"
    } else if t.starts_with("LIST") || t.ends_with("[]") {
        "list"
    } else {
        "scalar"
    }
}

/// Remap a `DuckDB` binder error about missing columns to `EngineError::Emit`
/// so it surfaces as HTTP 400 instead of 500.
fn remap_binder_error(e: &duckdb::Error) -> EngineError {
    let msg = e.to_string();
    // try to extract the field name from the error message
    let user_msg = if let Some(start) = msg.find('"') {
        if let Some(end) = msg[start + 1..].find('"') {
            let field = &msg[start + 1..start + 1 + end];
            format!("unknown field: {field}")
        } else {
            format!("unknown field in query: {msg}")
        }
    } else {
        format!("unknown field in query: {msg}")
    };
    EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation { message: user_msg })
}

/// Convert trawl-core `SqlValue` params into duckdb `ToSql` trait objects.
fn bind_params(params: &[SqlValue]) -> Vec<Box<dyn duckdb::ToSql>> {
    params
        .iter()
        .map(|val| -> Box<dyn duckdb::ToSql> {
            match val {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
            }
        })
        .collect()
}

/// Extract a `Value` from a duckdb row at the given column index.
///
/// Uses `ValueRef` for type-safe dispatch. Temporal types fall back to
/// string extraction so duckdb handles its own formatting.
///
/// `utc_offset_secs` is applied to timestamp values before civil time
/// formatting. Pass `0` for UTC.
fn extract_value(row: &duckdb::Row<'_>, idx: usize, utc_offset_secs: i32) -> Value {
    match row.get_ref_unwrap(idx) {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Boolean(b),
        ValueRef::TinyInt(i) => Value::Integer(i64::from(i)),
        ValueRef::SmallInt(i) => Value::Integer(i64::from(i)),
        ValueRef::Int(i) => Value::Integer(i64::from(i)),
        ValueRef::BigInt(i) => Value::Integer(i),
        // log data shouldn't exceed i64 range; stringify if it does
        ValueRef::HugeInt(i) => {
            i64::try_from(i).map_or_else(|_| Value::String(i.to_string()), Value::Integer)
        }
        ValueRef::UTinyInt(i) => Value::Integer(i64::from(i)),
        ValueRef::USmallInt(i) => Value::Integer(i64::from(i)),
        ValueRef::UInt(i) => Value::Integer(i64::from(i)),
        ValueRef::UBigInt(i) => {
            i64::try_from(i).map_or_else(|_| Value::String(i.to_string()), Value::Integer)
        }
        ValueRef::Float(f) => Value::Float(f64::from(f)),
        ValueRef::Double(f) => Value::Float(f),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::String(format!("<blob {} bytes>", bytes.len())),
        ValueRef::Timestamp(unit, val) => {
            Value::String(format_timestamp(unit, val, utc_offset_secs))
        }
        ValueRef::Date32(days) => Value::String(format_date(days)),
        ValueRef::Time64(unit, val) => Value::String(format_time(unit, val)),
        // list values from aggregations like LIST(DISTINCT col)
        ValueRef::List(..) => row
            .get::<_, duckdb::types::Value>(idx)
            .map_or(Value::Null, convert_duckdb_value),
        // everything else: try string extraction, fall back to null
        _ => row.get::<_, String>(idx).map_or(Value::Null, Value::String),
    }
}

/// Recursively convert a `duckdb::types::Value` to our `Value`.
fn convert_duckdb_value(v: duckdb::types::Value) -> Value {
    match v {
        duckdb::types::Value::Null => Value::Null,
        duckdb::types::Value::Boolean(b) => Value::Boolean(b),
        duckdb::types::Value::TinyInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::SmallInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::Int(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::BigInt(i) => Value::Integer(i),
        duckdb::types::Value::Float(f) => Value::Float(f64::from(f)),
        duckdb::types::Value::Double(f) => Value::Float(f),
        duckdb::types::Value::Text(s) => Value::String(s),
        duckdb::types::Value::List(elements) => {
            Value::Array(elements.into_iter().map(convert_duckdb_value).collect())
        }
        other => Value::String(format!("{other:?}")),
    }
}

/// Convert a timestamp value to an ISO 8601 string.
///
/// `DuckDB` stores timestamps as integer offsets from the Unix epoch.
/// The `TimeUnit` indicates the resolution.
/// Convert a temporal value to microseconds based on its `TimeUnit`.
const fn to_micros(unit: TimeUnit, val: i64) -> i64 {
    match unit {
        TimeUnit::Second => val * 1_000_000,
        TimeUnit::Millisecond => val * 1_000,
        TimeUnit::Microsecond => val,
        TimeUnit::Nanosecond => val / 1_000,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
fn format_timestamp(unit: TimeUnit, val: i64, utc_offset_secs: i32) -> String {
    let micros = to_micros(unit, val) + i64::from(utc_offset_secs) * 1_000_000;

    let (total_secs, sub_secs) = if micros >= 0 {
        (micros / 1_000_000, (micros % 1_000_000) as u32)
    } else {
        // handle pre-epoch timestamps
        let s = (micros - 999_999) / 1_000_000;
        let us = (micros - s * 1_000_000) as u32;
        (s, us)
    };

    // days since epoch and time-of-day
    let (days, day_secs) = if total_secs >= 0 {
        ((total_secs / 86400) as i32, (total_secs % 86400) as u32)
    } else {
        let d = (total_secs - 86399) / 86400;
        let s = (total_secs - d * 86400) as u32;
        (d as i32, s)
    };

    let (y, m, d) = days_to_ymd(days);
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;

    if sub_secs == 0 {
        format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02}")
    } else {
        // trim trailing zeros from fractional seconds
        let frac = format!("{sub_secs:06}");
        let trimmed = frac.trim_end_matches('0');
        format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02}.{trimmed}")
    }
}

/// Convert a date (days since Unix epoch) to YYYY-MM-DD.
fn format_date(days: i32) -> String {
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert a time value to HH:MM:SS.
fn format_time(unit: TimeUnit, val: i64) -> String {
    let micros = to_micros(unit, val);
    let total_secs = micros / 1_000_000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
///
/// Uses the civil calendar algorithm from Howard Hinnant's `date` library.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
fn days_to_ymd(days: i32) -> (i32, u32, u32) {
    // shift epoch from 0000-03-01 to 1970-01-01
    let z = i64::from(days) + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u32; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use duckdb::Connection;

    use super::{
        ColdAction, Executor, HotColdOutcome, cold_action, error_class, glob_list_items,
        is_complex_type, is_conversion_error, is_union_type_conflict,
    };

    /// Write a one-row parquet file whose `meta` column has the given SQL
    /// type/value expression (e.g. `{'a': 1}` for a `STRUCT`, `'plain'` for a
    /// `VARCHAR`). Mirrors the cold-fixture shape used in `integration.rs`.
    fn write_meta_parquet(conn: &Connection, path: &Path, meta_expr: &str) {
        conn.execute_batch(&format!(
            "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"timestamp\", \
                          'svc' AS service, {meta_expr} AS meta) \
             TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    }

    /// Provoke a live `read_parquet(union_by_name=true)` type conflict by
    /// reading two files where `meta` is `STRUCT` in one and `VARCHAR` in the
    /// other. Returns the raw `DuckDB` error.
    fn read_parquet_remap_error(dir: &Path) -> duckdb::Error {
        let conn = Connection::open_in_memory().unwrap();
        let struct_file = dir.join("struct.parquet");
        let varchar_file = dir.join("varchar.parquet");
        write_meta_parquet(&conn, &struct_file, "{'a': 1}");
        write_meta_parquet(&conn, &varchar_file, "'plain'");
        let glob = format!("{}/*.parquet", dir.display());
        conn.prepare("SELECT * FROM read_parquet(?, union_by_name=true)")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([glob.as_str()])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("STRUCT-vs-VARCHAR read_parquet union must error")
    }

    /// Provoke a live `UNION ALL BY NAME` Conversion error: a `STRUCT` column
    /// unioned with a `VARCHAR` column of the same name.
    fn union_conversion_error() -> duckdb::Error {
        let conn = Connection::open_in_memory().unwrap();
        conn.prepare("SELECT {'a': 1} AS meta UNION ALL BY NAME SELECT 'plain' AS meta")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("STRUCT-vs-VARCHAR UNION ALL BY NAME must error")
    }

    /// Provoke a live "too small to be a Parquet file" corruption error by
    /// pointing `read_parquet` at a file with valid head magic but no trailer.
    fn corruption_error(dir: &Path) -> duckdb::Error {
        let conn = Connection::open_in_memory().unwrap();
        let bad = dir.join("truncated.parquet");
        std::fs::write(&bad, b"PAR1\x00\x00").unwrap();
        let path = format!("{}", bad.display());
        conn.prepare("SELECT * FROM read_parquet(?)")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([path.as_str()])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("truncated parquet must error")
    }

    /// Provoke a benign binder / missing-column error.
    fn benign_binder_error() -> duckdb::Error {
        let conn = Connection::open_in_memory().unwrap();
        conn.prepare("SELECT nonexistent_col FROM (SELECT 1 AS x)")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("missing-column query must error")
    }

    /// Provoke a live *data*-conversion error: a value that cannot be
    /// converted, with no schema disagreement anywhere in sight.
    fn data_conversion_error() -> duckdb::Error {
        let conn = Connection::open_in_memory().unwrap();
        conn.prepare("SELECT CAST(x AS INTEGER) FROM (SELECT unnest(['1', 'abc']) AS x)")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("casting 'abc' to INTEGER must error")
    }

    #[test]
    fn is_conversion_error_matches_real_strings() {
        let dir = tempfile::tempdir().unwrap();

        let remap = read_parquet_remap_error(dir.path());
        assert!(
            is_conversion_error(&remap),
            "read_parquet remap error should classify as a conversion error: {remap}"
        );

        let conversion = union_conversion_error();
        assert!(
            is_conversion_error(&conversion),
            "UNION ALL BY NAME conversion error should classify as a conversion error: {conversion}"
        );

        let corrupt = corruption_error(dir.path());
        assert!(
            !is_conversion_error(&corrupt),
            "corruption error must NOT classify as a conversion error (handled by quarantine): {corrupt}"
        );

        let benign = benign_binder_error();
        assert!(
            !is_conversion_error(&benign),
            "benign missing-column error must NOT classify as a conversion error: {benign}"
        );
    }

    #[test]
    fn genuine_data_conversion_error_is_not_misread_as_a_union_type_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let data = data_conversion_error();

        // The premise: neither the class token nor the message body can tell
        // a value that cannot be converted from a schema that cannot be
        // reconciled. Both are `Conversion`-class, and on the bundled 1.5.5
        // both bodies name a "source column" and talk about casting to a
        // "destination type". Anything keying off the error ALONE misreads
        // this data error as a union type conflict.
        assert_eq!(
            error_class(&data.to_string()),
            Some("Conversion"),
            "a genuine data-conversion error is Conversion-class too: {data}"
        );
        assert!(is_conversion_error(&data));
        assert!(is_conversion_error(&union_conversion_error()));

        // So the classifier requires EVIDENCE — the columns the two sources
        // actually describe differently. A data error has none, and is
        // therefore never misread as a union type conflict (ADR-0008).
        assert!(
            !is_union_type_conflict(&data, &[]),
            "a data-conversion error with no conflicting column must NOT \
             classify as a union type conflict: {data}"
        );

        // The same evidence classifies a real schema conflict as one...
        let conflict = union_conversion_error();
        assert!(
            is_union_type_conflict(&conflict, &["meta".to_string()]),
            "a conversion error over a genuinely drifted column IS a union \
             type conflict: {conflict}"
        );

        // ...and evidence alone is not enough either: an unrelated failure
        // does not become a type conflict just because the schemas drift.
        let corrupt = corruption_error(dir.path());
        assert!(
            !is_union_type_conflict(&corrupt, &["meta".to_string()]),
            "corruption is not a type conflict even with drifted columns: {corrupt}"
        );
    }

    #[test]
    fn hot_cold_conflicts_separate_a_data_error_from_a_schema_conflict() {
        // The evidence `is_union_type_conflict` consumes is not hand-made:
        // prove it is empty for hot/cold sources that agree on every column
        // (the situation a genuine data-conversion error arises in) and
        // non-empty for sources that genuinely drift.
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        setup
            .execute_batch(&format!(
                "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"timestamp\", \
                 'svc' AS service, 'abc' AS duration) TO '{}' (FORMAT PARQUET)",
                dir.path().join("cold.parquet").display()
            ))
            .unwrap();
        let exec = Executor::new().unwrap();
        let source = format!("{}/*.parquet", dir.path().display());

        // `duration` is VARCHAR on both sides — schemas agree.
        let agreed = dir.path().join("agreed.ndjson");
        std::fs::write(
            &agreed,
            "{\"timestamp\":\"2024-01-15T11:00:00Z\",\"service\":\"svc\",\"duration\":\"7\"}\n",
        )
        .unwrap();
        assert!(
            exec.hot_cold_conflicts(&source, agreed.to_str().unwrap())
                .expect("describing matched sources must succeed")
                .is_empty(),
            "sources that agree on every column yield no conflict evidence"
        );

        // `duration` is VARCHAR cold but BIGINT hot — a real schema conflict.
        let drifted = dir.path().join("drifted.ndjson");
        std::fs::write(
            &drifted,
            "{\"timestamp\":\"2024-01-15T11:00:00Z\",\"service\":\"svc\",\"duration\":7}\n",
        )
        .unwrap();
        assert_eq!(
            exec.hot_cold_conflicts(&source, drifted.to_str().unwrap())
                .expect("describing drifted sources must succeed"),
            vec!["duration".to_string()],
            "a column typed differently on each side is the conflict evidence"
        );
    }

    #[test]
    fn conflicting_kind_mixes_all_carry_the_conversion_class() {
        // Compaction's rollup and merge fallbacks trigger on the conversion
        // class alone (there is no second source to describe until the
        // fallback runs), so the class must cover every cross-file mix the
        // old "remap"/"type mismatch" substrings used to catch. Pin the full
        // kind matrix on both the read_parquet and UNION ALL BY NAME paths.
        let mixes = [
            ("{'a': 1}", "'plain'"),
            ("{'a': 1}", "[1, 2]"),
            ("{'a': 1}", "42"),
            ("[1, 2]", "'plain'"),
            ("'plain'", "{'a': 1}"),
        ];
        for (a, b) in mixes {
            let dir = tempfile::tempdir().unwrap();
            let conn = Connection::open_in_memory().unwrap();
            write_meta_parquet(&conn, &dir.path().join("a.parquet"), a);
            write_meta_parquet(&conn, &dir.path().join("b.parquet"), b);
            let glob = format!("{}/*.parquet", dir.path().display());

            let pq = conn
                .prepare("SELECT * FROM read_parquet(?, union_by_name=true)")
                .and_then(|mut stmt| {
                    let mut rows = stmt.query([glob.as_str()])?;
                    while rows.next()?.is_some() {}
                    Ok(())
                })
                .expect_err("an irreconcilable kind mix must error on read_parquet");
            assert!(
                is_conversion_error(&pq),
                "read_parquet mix {a} vs {b} must trigger the cast fallback: {pq}"
            );

            let un = conn
                .prepare(&format!(
                    "SELECT {a} AS meta UNION ALL BY NAME SELECT {b} AS meta"
                ))
                .and_then(|mut stmt| {
                    let mut rows = stmt.query([])?;
                    while rows.next()?.is_some() {}
                    Ok(())
                })
                .expect_err("an irreconcilable kind mix must error on UNION ALL BY NAME");
            assert!(
                is_conversion_error(&un),
                "union mix {a} vs {b} must trigger the cast fallback: {un}"
            );
        }
    }

    #[test]
    fn describe_schema_tolerates_cross_file_drift() {
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        // `meta` is a STRUCT in file A and a VARCHAR in file B — the exact
        // cross-file drift that would 500 a raw union DESCRIBE.
        write_meta_parquet(&setup, &dir.path().join("a.parquet"), "{'a': 1}");
        write_meta_parquet(&setup, &dir.path().join("b.parquet"), "'plain'");

        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());
        let schema = exec
            .describe_schema(&glob)
            .expect("describe_schema must tolerate cross-file drift, not error");

        let meta = schema
            .columns
            .iter()
            .find(|c| c.name == "meta")
            .expect("schema must include the drifted `meta` column");
        assert_eq!(
            meta.data_type, "VARCHAR",
            "drifted column must be reported as VARCHAR (the rollup-convergence type)"
        );
        // Non-drifted columns keep their real types.
        assert!(
            schema.columns.iter().any(|c| c.name == "service"),
            "non-drifted columns must still be present"
        );
        assert_eq!(schema.file_count, 2, "both files should be counted");
    }

    #[test]
    fn describe_schema_keeps_union_able_struct_drift() {
        // `meta` is `STRUCT(x INTEGER)` in file A and `STRUCT(y INTEGER)` in
        // file B — realistic per-batch JSON inference on sparse nested objects.
        // `read_parquet(union_by_name=true)` MERGES these to
        // `STRUCT(x INTEGER, y INTEGER)` and a real query reads them fine, so
        // describe_schema must report the merged STRUCT, NOT collapse to
        // VARCHAR (which would make /api/v1/schema lie about a column queries
        // return as a STRUCT — the inverse of the drift S1 set out to prevent).
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("a.parquet"), "{'x': 1}");
        write_meta_parquet(&setup, &dir.path().join("b.parquet"), "{'y': 2}");

        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());
        let schema = exec
            .describe_schema(&glob)
            .expect("describe_schema must tolerate union-able struct drift");

        let meta = schema
            .columns
            .iter()
            .find(|c| c.name == "meta")
            .expect("schema must include the `meta` column");
        assert!(
            meta.data_type.to_ascii_uppercase().starts_with("STRUCT"),
            "union-able cross-file STRUCT drift must report the merged STRUCT \
             type (a real query reads it as a STRUCT), got `{}`",
            meta.data_type
        );

        // Prove describe matches read time: a SELECT over the same glob must
        // succeed, returning both rows the union merges.
        let result = exec
            .run_query("* | fields meta", &glob, usize::MAX, 0)
            .expect("SELECT meta over the merged-STRUCT glob must succeed");
        assert_eq!(
            result.row_count(),
            2,
            "both rows must survive the merged-STRUCT union read"
        );
    }

    #[test]
    fn run_query_with_hot_returns_hot_on_cold_start() {
        // Cold start: the parquet source matches zero files, so the union
        // raises a "no files" error (NOT a type conflict). The hot-only
        // fallback must return the hot row without erroring and WITHOUT
        // logging a cold-drop (type_conflict is false here).
        let dir = tempfile::tempdir().unwrap();
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"timestamp\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        // Glob that matches no parquet files (cold start).
        let source = format!("{}/nonexistent/*.parquet", dir.path().display());
        let result = exec
            .run_query_with_hot("*", &source, hot.to_str().unwrap(), usize::MAX, 0)
            .expect("cold-start query must return hot rows, not error");
        assert_eq!(
            result.row_count(),
            1,
            "the single hot row must survive the cold-start hot-only fallback"
        );
    }

    #[test]
    fn duckdb_error_classes_parsed_from_live_errors() {
        // The classifier keys off DuckDB's leading "<Class> Error" token, so
        // pin the class each live-provoked error actually carries (verified
        // against the bundled crate — the four ADR-0008 trigger variants all
        // produce this Conversion class, see conversion_error_class below).
        let dir = tempfile::tempdir().unwrap();

        // On the bundled 1.5.5 the per-file remap variant is ALSO a
        // Conversion-class error ("failed to cast column ..."), not the
        // legacy Binder "Struct remap" string — which is why the classifier
        // needs no Binder arm at all.
        let remap_msg = read_parquet_remap_error(dir.path()).to_string();
        assert_eq!(
            error_class(&remap_msg),
            Some("Conversion"),
            "read_parquet remap error carries the Conversion class: {remap_msg}"
        );

        let conversion_msg = union_conversion_error().to_string();
        assert_eq!(
            error_class(&conversion_msg),
            Some("Conversion"),
            "UNION ALL BY NAME conflict carries the Conversion class: {conversion_msg}"
        );

        let corrupt_msg = corruption_error(dir.path()).to_string();
        assert_ne!(
            error_class(&corrupt_msg),
            Some("Conversion"),
            "corruption must not carry the Conversion class: {corrupt_msg}"
        );

        let benign_msg = benign_binder_error().to_string();
        assert_eq!(
            error_class(&benign_msg),
            Some("Binder"),
            "missing-column error carries the Binder class: {benign_msg}"
        );
    }

    /// The four ADR-0008 trigger variants under a hard CAST all produce a
    /// `Conversion Error`-class message — the evidence (verified by execution
    /// against the bundled crate) behind the classifier keying off the class
    /// token. Post-fix these variants never reach a hard CAST, but the
    /// assertion pins the `DuckDB` behavior the design relies on.
    #[test]
    fn trigger_variants_produce_conversion_class_errors() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        let variants = [
            r#""not-a-date""#,
            r#""2026-13-45T99:99:99Z""#,
            r#"{"nested":1}"#,
            "12345",
        ];
        for (i, variant) in variants.iter().enumerate() {
            let f = dir.path().join(format!("trig{i}.ndjson"));
            std::fs::write(&f, format!(r#"{{"timestamp":{variant},"service":"svc"}}"#)).unwrap();
            let err = conn
                .prepare(&format!(
                    "SELECT * REPLACE (CAST(\"timestamp\" AS TIMESTAMP) AS \"timestamp\") \
                     FROM read_json(['{}'], format='newline_delimited', records=true, \
                     auto_detect=true, union_by_name=true, field_appearance_threshold=0, \
                     maximum_depth=2)",
                    f.display()
                ))
                .and_then(|mut stmt| {
                    let mut rows = stmt.query([])?;
                    while rows.next()?.is_some() {}
                    Ok(())
                })
                .expect_err("a hard CAST over a malformed timestamp must error");
            let msg = err.to_string();
            assert_eq!(
                error_class(&msg),
                Some("Conversion"),
                "variant {variant} must carry the Conversion class: {msg}"
            );
        }
    }

    #[test]
    fn cold_action_routes_by_outcome_class() {
        // The outcome policy: hot-only fallback is permitted only when it
        // cannot hide cold data. Columns and Fatal return the outcome; a
        // benign missing-column error goes hot-only; a columnless result and
        // any other failure may go hot-only ONLY if a cold-file presence
        // check proves there is nothing to hide (ADR-0008: a cold-data drop
        // is never silent, and an arbitrary database failure never
        // masquerades as success).
        assert_eq!(
            cold_action(HotColdOutcome::Columns),
            ColdAction::ReturnOutcome,
            "an authoritative columnful result is returned as-is"
        );
        assert_eq!(
            cold_action(HotColdOutcome::Fatal),
            ColdAction::ReturnOutcome,
            "a non-recoverable error is propagated, not masked by a hot-only read"
        );
        assert_eq!(
            cold_action(HotColdOutcome::NoColumns),
            ColdAction::HotOnlyIfNoColdFiles,
            "a columnless result is only a cold start if no cold files exist — \
             a list source reports the same error for one empty element"
        );
        assert_eq!(
            cold_action(HotColdOutcome::BenignBinder),
            ColdAction::HotOnly,
            "a missing-column user error keeps the empty-result UX"
        );
        assert_eq!(
            cold_action(HotColdOutcome::Recoverable),
            ColdAction::HotOnlyIfNoColdFiles,
            "an unexpected failure may go hot-only only when no cold files exist"
        );
    }

    #[test]
    fn unexpected_cold_failure_returns_error_not_hot_only() {
        // An unreadable parquet file (valid magic, truncated body) inside the
        // cold glob, with hot rows present: the query must return an error —
        // never HTTP-200-shaped hot-only success that silently drops the
        // (unreadable but existing) cold data.
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("truncated.parquet");
        std::fs::write(&bad, b"PAR1\x00\x00").unwrap();
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"timestamp\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("{}/*.parquet", dir.path().display());
        let result = exec.run_query_with_hot("*", &source, hot.to_str().unwrap(), usize::MAX, 0);
        assert!(
            result.is_err(),
            "an unreadable cold file must surface as an error, not hot-only success"
        );
    }

    #[test]
    fn missing_column_with_cold_files_returns_empty_not_error() {
        // A query on a nonexistent field with cold files present keeps the
        // current empty-result UX (the BenignBinder carve-out): the binder
        // error is a user error, not a cold-data drop.
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("cold.parquet"), "'plain'");
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"timestamp\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("{}/*.parquet", dir.path().display());
        let result = exec
            .run_query_with_hot(
                "nonexistent_field=value",
                &source,
                hot.to_str().unwrap(),
                usize::MAX,
                0,
            )
            .expect("a missing-column query must stay a benign empty result");
        assert_eq!(
            result.row_count(),
            0,
            "querying a nonexistent field returns empty, not an error"
        );
    }

    #[test]
    fn glob_list_items_splits_list_sources() {
        assert_eq!(glob_list_items("/data/**/*.parquet"), None);
        assert_eq!(
            glob_list_items("['/data/2024-01-15/10/*.parquet']"),
            Some(vec!["/data/2024-01-15/10/*.parquet"])
        );
        assert_eq!(
            glob_list_items(" ['/a/*.parquet', '/b/*.parquet'] "),
            Some(vec!["/a/*.parquet", "/b/*.parquet"])
        );
        // Commas inside a quoted element stay part of that element.
        assert_eq!(
            glob_list_items("['/a,b/*.parquet']"),
            Some(vec!["/a,b/*.parquet"])
        );
        // Unparseable list shapes yield an empty vec → "assume present".
        assert_eq!(glob_list_items("['/a/*.parquet"), Some(Vec::new()));
        assert_eq!(glob_list_items("['/a/*.parquet]"), Some(Vec::new()));
    }

    #[test]
    fn cold_files_present_globs_list_elements() {
        // A list source is not "present by construction": the server emits a
        // list of per-hour globs for essentially every time-filtered query,
        // and an hour directory can exist while holding no parquet yet.
        let dir = tempfile::tempdir().unwrap();
        let empty_hour = dir.path().join("10");
        std::fs::create_dir_all(&empty_hour).unwrap();
        let exec = Executor::new().unwrap();

        let absent = format!("['{}/*.parquet']", empty_hour.display());
        assert!(
            !exec.cold_files_present(&absent),
            "a list whose globs match no file must report no cold files"
        );

        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &empty_hour.join("cold.parquet"), "'plain'");
        assert!(
            exec.cold_files_present(&absent),
            "a list whose globs match a file must report cold files present"
        );
    }

    #[test]
    fn partial_list_source_miss_keeps_cold_rows() {
        // The server emits one glob per hour in the query range and prunes
        // only on parent-directory existence, so a sparse-traffic service
        // routinely gets a list whose elements point at hour directories
        // (created by other services) holding no file of its own. DuckDB
        // raises "No files found that match the pattern" for such an element
        // even when a sibling element has data, and `execute_emitted` maps
        // that to an empty result — so an empty result must NOT be read as
        // "cold start". The cold rows that do exist must still come back.
        let dir = tempfile::tempdir().unwrap();
        let full_hour = dir.path().join("10");
        let empty_hour = dir.path().join("11");
        std::fs::create_dir_all(&full_hour).unwrap();
        std::fs::create_dir_all(&empty_hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &full_hour.join("svc.parquet"), "'plain'");
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"timestamp\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        let result = exec
            .run_query_with_hot("*", &source, hot.to_str().unwrap(), usize::MAX, 0)
            .expect("a partial list-source miss must not fail the query");
        assert_eq!(
            result.row_count(),
            2,
            "the cold row behind the matching glob element must survive an \
             empty sibling element, alongside the hot row"
        );
    }

    #[test]
    fn export_with_hot_falls_back_to_hot_only_for_empty_list_source() {
        // Cold start with a list source (the shape the server builds for a
        // time-filtered query): no parquet exists yet, so the export must
        // fall back to the hot buffer instead of surfacing the cold "No files
        // found" error.
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"timestamp\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        exec.export_parquet_with_hot("*", &source, hot.to_str().unwrap(), &out, 1000)
            .expect("hot-only export must succeed when the cold list matches no file");

        let rows: i64 = exec
            .conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    out.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "the exported parquet must carry the hot row");
    }

    #[test]
    fn export_with_hot_keeps_cold_rows_on_partial_list_miss() {
        // The everyday `service=X last=24h` export shape: one glob per hour in
        // range, one hour directory holding the service's parquet and a
        // sibling hour directory that exists (other services compacted there)
        // without a file of its own. DuckDB rejects the whole list, and the
        // hot-only arm cannot rescue it because the matching sibling makes
        // cold files "present" — so without the prune retry this 500s. Both
        // the cold row and the hot row must land in the exported parquet.
        let dir = tempfile::tempdir().unwrap();
        let full_hour = dir.path().join("10");
        let empty_hour = dir.path().join("11");
        std::fs::create_dir_all(&full_hour).unwrap();
        std::fs::create_dir_all(&empty_hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &full_hour.join("svc.parquet"), "'plain'");
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"timestamp\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        exec.export_parquet_with_hot("*", &source, hot.to_str().unwrap(), &out, 1000)
            .expect("a partial list-source miss must not fail the export");

        let rows: i64 = exec
            .conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    out.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rows, 2,
            "the cold row behind the matching glob element must survive an \
             empty sibling element, alongside the hot row"
        );
    }

    #[test]
    fn is_complex_type_classifies_duckdb_types() {
        // Relocated from trawl-server's compaction.rs when the classifier was
        // consolidated here — both lanes now share this one function.
        assert!(is_complex_type("JSON"));
        assert!(is_complex_type("STRUCT(v BIGINT)"));
        assert!(is_complex_type("MAP(VARCHAR, JSON)"));
        assert!(is_complex_type("UNION(a INTEGER, b VARCHAR)"));
        assert!(is_complex_type("VARCHAR[]"));
        assert!(is_complex_type("BIGINT[]"));
        assert!(!is_complex_type("VARCHAR"));
        assert!(!is_complex_type("BIGINT"));
        assert!(!is_complex_type("TIMESTAMP"));
        assert!(!is_complex_type("DOUBLE"));
    }

    #[test]
    fn insurance_path_reports_reconciled_type_not_first_seen() {
        // Drive the empty-fast_path insurance branch directly (it's unreachable
        // through describe_schema on a DuckDB that tolerates union-able drift).
        // `meta` is BIGINT in the lexically-first file and DOUBLE in the second;
        // union_by_name reconciles to DOUBLE. A first-seen guess would report
        // BIGINT, so asserting DOUBLE proves the hardening.
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("00.parquet"), "CAST(1 AS BIGINT)");
        write_meta_parquet(
            &setup,
            &dir.path().join("01.parquet"),
            "CAST(1.5 AS DOUBLE)",
        );

        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());
        let columns = exec
            .describe_schema_columns_coerced(&glob, Vec::new())
            .expect("insurance-path reconcile must not error on union-able scalar drift");

        let meta = columns
            .iter()
            .find(|c| c.name == "meta")
            .expect("schema must include the drifted `meta` column");
        assert_eq!(
            meta.data_type.to_ascii_uppercase(),
            "DOUBLE",
            "insurance path must report DuckDB's reconciled type (DOUBLE), not \
             the first-seen BIGINT, got `{}`",
            meta.data_type
        );
    }
}
