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
    /// Handles cold-start gracefully: when no parquet files exist yet
    /// (empty columns = no data source), falls back to querying just the
    /// hot buffer so events ingested before the first compaction are visible.
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
        // would otherwise fall through to the hot-only path below and
        // silently drop every cold row. Detect the conflicting columns and
        // retry the union with them coerced to VARCHAR on both sides,
        // preserving hot AND cold data. A false positive (no conflicts found)
        // or a describe failure degrades to the hot-only path below — but if
        // a conflict was detected and the coerced retry still failed, that
        // degradation drops cold/parquet rows and is now logged at warn
        // (`event_type = "query_cold_drop"`), distinct from a legitimate
        // cold-start hot-only path (where `type_conflict` is false).
        let type_conflict = matches!(
            &outcome,
            Err(EngineError::Database(e)) if is_union_type_conflict(e)
        );
        if type_conflict
            && let Ok(cols) = self.hot_cold_conflicts(source, hot_source)
            && !cols.is_empty()
        {
            let coerced = emitter::emit_with_hot_source_coerced(&ast, source, hot_source, &cols)?;
            outcome = self.execute_emitted(&coerced, max_rows, utc_offset_secs);
        }

        let mut result = match &outcome {
            // Columns present → real result (possibly empty rows). Return as-is.
            Ok(r) if !r.columns.is_empty() => outcome?,
            // No columns (no parquet source files), database error (UNION
            // fails on missing source), or binder error remapped to Emit
            // (column not found in empty parquet) → fall back to hot-only.
            // ResultTooLarge is excluded: the query worked, just too many rows.
            Ok(_) | Err(EngineError::Database(_) | EngineError::Emit(_)) => {
                if type_conflict {
                    // A hot/cold column type conflict was detected but the
                    // coerced retry did not resolve it (conflict-detection
                    // failed, found no columns, or the retry itself errored).
                    // We are about to return HOT-ONLY results, silently
                    // omitting all cold/parquet rows.
                    tracing::warn!(
                        event_type = "query_cold_drop",
                        "hot/cold type conflict survived coercion; returning hot-only results, cold/parquet rows dropped"
                    );
                }
                let hot_emitted = emitter::emit(&ast, hot_source)?;
                match self.execute_emitted(&hot_emitted, max_rows, utc_offset_secs) {
                    // Hot-only also hit a binder/emit error (e.g. empty ndjson
                    // between compaction cycles). Treat as empty, not error.
                    Err(EngineError::Emit(_)) => QueryResult::empty(),
                    other => other?,
                }
            }
            Err(_) => outcome?,
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
            "SELECT * REPLACE (CAST(\"timestamp\" AS TIMESTAMP) AS \"timestamp\") FROM {hot_reader}"
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
            // Insurance: some paths/versions raise the conflict at describe. We
            // have no fast-path baseline here, so reconcile from scratch.
            Err(EngineError::Database(e)) if is_union_type_conflict(&e) => {
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
    /// column is reported `VARCHAR` and reconcilable columns keep their single
    /// observed type. Column order follows the fast path when present, else
    /// first-seen per-file order, so the result is deterministic.
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

        // Insurance path: no fast-path baseline. Report VARCHAR for
        // irreconcilable columns, else the single observed type.
        Ok(col_types
            .into_iter()
            .map(|(name, types)| {
                let data_type = if is_irreconcilable(&types) {
                    "VARCHAR".to_owned()
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

    /// Extract per-column statistics from parquet row group metadata.
    ///
    /// Queries `DuckDB`'s `parquet_metadata()` table function which reads only
    /// file footers — no row data is touched. Returns aggregate stats per
    /// column: total values, null count, min/max, and compressed size.
    pub fn parquet_column_stats(
        &self,
        source: &str,
    ) -> Result<Vec<crate::value::ParquetColumnStats>, EngineError> {
        emitter::validate_source_path(source)?;

        let sql = r"
            SELECT
                path_in_schema AS column_name,
                SUM(num_values)::BIGINT AS total_count,
                SUM(stats_null_count)::BIGINT AS null_count,
                MIN(stats_min_value) AS min_value,
                MAX(stats_max_value) AS max_value,
                SUM(total_compressed_size)::BIGINT AS compressed_bytes
            FROM parquet_metadata(?)
            GROUP BY path_in_schema
            ORDER BY path_in_schema
        ";

        let mut stmt = match self.conn.prepare(sql) {
            Ok(s) => s,
            Err(e) if is_no_files_error(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut rows = match stmt.query([source]) {
            Ok(r) => r,
            Err(e) if is_no_files_error(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut stats = Vec::new();
        while let Some(row) = rows.next()? {
            let column_name: String = row.get(0)?;
            let total_count: i64 = row.get(1)?;
            let null_count: i64 = row.get(2)?;
            let min_value: Option<String> = row.get(3).ok();
            let max_value: Option<String> = row.get(4).ok();
            let compressed_bytes: i64 = row.get(5)?;

            stats.push(crate::value::ParquetColumnStats {
                column_name,
                total_count: u64::try_from(total_count).unwrap_or(0),
                null_count: u64::try_from(null_count).unwrap_or(0),
                min_value,
                max_value,
                compressed_bytes: u64::try_from(compressed_bytes).unwrap_or(0),
            });
        }

        Ok(stats)
    }

    /// Sum of `num_rows` from parquet file metadata for total event count.
    ///
    /// Uses `parquet_file_metadata()` which reads only file-level metadata
    /// (not row groups), making it very fast.
    pub fn parquet_row_counts(&self, source: &str) -> Result<u64, EngineError> {
        emitter::validate_source_path(source)?;

        let sql = "SELECT COALESCE(SUM(num_rows)::BIGINT, 0) FROM parquet_file_metadata(?)";
        let count: i64 = match self.conn.query_row(sql, [source], |row| row.get(0)) {
            Ok(c) => c,
            Err(e) if is_no_files_error(&e) => return Ok(0),
            Err(e) => return Err(e.into()),
        };

        Ok(u64::try_from(count).unwrap_or(0))
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
        match self.export_parquet_from_emitted(&emitted, output_path, max_rows) {
            // No columns / no parquet files → fall back to hot-only.
            Err(EngineError::Database(_) | EngineError::Emit(_)) => {
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

/// Check if a `DuckDB` error is a column type conflict raised when a
/// `UNION ALL BY NAME` (or `read_parquet(..., union_by_name=true)`) cannot
/// reconcile a column's type across sources — e.g. JSON/STRUCT vs `VARCHAR`.
///
/// Matches two real `DuckDB` 1.4.x strings:
/// - the union path raises a `"Conversion"` error;
/// - the `read_parquet(union_by_name)` path raises a Binder
///   `"Struct remap can only remap nested types, not 'VARCHAR'"` error
///   (the `"remap"` substring), plus the generic `"type mismatch"`.
///
/// Deliberately does NOT match `"too small to be a Parquet file"`: that is a
/// corruption error, handled by quarantining the file, not by the cast
/// fallback. False positives are harmless — the caller retries conflict
/// detection, finds none, and falls through unchanged.
pub fn is_union_type_conflict(e: &duckdb::Error) -> bool {
    let msg = e.to_string();
    msg.contains("Conversion") || msg.contains("remap") || msg.contains("type mismatch")
}

/// Whether a `DuckDB` type name (as reported by `DESCRIBE`) is a complex
/// (nested) type — `STRUCT`/`MAP`/`LIST`/array `[]`/`UNION`/`JSON`. These are
/// the types that, when the same column is `VARCHAR` in another file, raise a
/// read-time union conflict. A schema describe that reports one of these may
/// be masking cross-file drift that only surfaces at query time.
fn is_complex_type(data_type: &str) -> bool {
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

    use super::{Executor, is_union_type_conflict};

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

    #[test]
    fn is_union_type_conflict_matches_real_strings() {
        let dir = tempfile::tempdir().unwrap();

        let remap = read_parquet_remap_error(dir.path());
        assert!(
            is_union_type_conflict(&remap),
            "read_parquet remap error should classify as a union type conflict: {remap}"
        );

        let conversion = union_conversion_error();
        assert!(
            is_union_type_conflict(&conversion),
            "UNION ALL BY NAME conversion error should classify as a union type conflict: {conversion}"
        );

        let corrupt = corruption_error(dir.path());
        assert!(
            !is_union_type_conflict(&corrupt),
            "corruption error must NOT classify as a type conflict (handled by quarantine): {corrupt}"
        );

        let benign = benign_binder_error();
        assert!(
            !is_union_type_conflict(&benign),
            "benign missing-column error must NOT classify as a type conflict: {benign}"
        );
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
}
