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
use trawl_core::ast::{PipeStage, Query, Spanned};
use trawl_core::emitter::{self, EmittedQuery, SqlValue};
use trawl_core::parser;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::{CanonicalType, FieldTypes};

use crate::error::EngineError;
use crate::value::{Column, QueryResult, SchemaColumn, SchemaResult, Value};

/// Names of the result columns `DuckDB` returned as TIMESTAMP.
type TimestampColumns = Vec<String>;

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
    /// container overlay filesystems), and pins the session time zone
    /// ([`Self::configure`]).
    pub fn new() -> Result<Self, EngineError> {
        let conn = Connection::open_in_memory()?;
        let tmp = std::env::temp_dir();
        conn.execute_batch(&format!(
            "SET temp_directory='{}'",
            tmp.to_string_lossy().replace('\'', "''")
        ))?;
        Self::configure(&conn)?;
        Ok(Self { conn })
    }

    /// Create a new executor sharing the same underlying database.
    ///
    /// The cloned connection benefits from `DuckDB`'s internal metadata
    /// caching (parquet file stats, column statistics) accumulated by
    /// other connections to the same database. A clone shares the
    /// DATABASE, not the session: settings are NOT inherited — it starts
    /// from the process default — so the clone is configured in its own
    /// right (`a_cloned_connection_starts_from_the_process_default_not_the_parent`
    /// in `trawl-engine/tests/duckdb_probe.rs`).
    pub fn try_clone(&self) -> Result<Self, EngineError> {
        let conn = self.conn.try_clone()?;
        Self::configure(&conn)?;
        Ok(Self { conn })
    }

    /// Pin the settings a query's ANSWER depends on — currently the session
    /// time zone, which must be UTC on every connection.
    ///
    /// The bundled `DuckDB` links ICU and defaults `TimeZone` to the HOST
    /// zone (probed with the clone behaviour above), and two things read
    /// it: the hot branch's TIMESTAMP conform
    /// (`trawl_core::conform`, which parses through `TIMESTAMPTZ` so an
    /// offset in the text is applied and a zoneless text is UTC), and
    /// `now()::TIMESTAMP` — the anchor of every `last=Xh` window, compared
    /// against `_time` values ingest canonicalized to UTC. On a host in
    /// `Asia/Kolkata` an unpinned session anchored that window 5h30m into
    /// the future and dropped every fresh event from it.
    fn configure(conn: &Connection) -> Result<(), EngineError> {
        conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)?;
        Ok(())
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
    /// `pins` is the field catalog's full pin snapshot typing the
    /// search-stage comparisons (ADR-0011 slice A). Every caller decides:
    /// the server passes its catalog snapshot; embedded mode passes an
    /// explicit `FieldTypes::new()`, making its documented pin-blindness
    /// visible at the call site.
    ///
    /// `utc_offset_secs` is applied to all timestamp values at format time.
    /// Pass `0` for UTC display.
    pub fn run_query(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<QueryResult, EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let resolved = self.resolve_source(source);
        let emitted = resolved.emit_cold(&ast, pins)?;
        let sql_offset = sql_render_offset(&emitted, utc_offset_secs);
        let outcome = self.execute_emitted_tracked(&emitted, max_rows, sql_offset);
        // Same gate as the hot lanes, one column over: with no hot buffer to
        // fall back to, `HotOnly` is unreachable (see [`cold_action`]) — but a
        // "no files" answer over a source that still reaches files is the
        // silent cold-data drop either way (ADR-0008).
        let (mut result, timestamp_columns) = match self.cold_action_for(
            classify_query_outcome(&outcome),
            &resolved,
            HotLane::Absent,
        ) {
            ColdAction::ColdDataUnread => return Err(EngineError::ColdDataUnread),
            ColdAction::ReturnOutcome | ColdAction::HotOnly => outcome?,
        };
        if !emitted.rust_stages.is_empty() {
            result = crate::post_process::apply_rust_stages(
                result,
                &emitted.rust_stages,
                &emitted.rust_stage_pins,
            )?;
            let tracked = tail_timestamp_scope(&timestamp_columns, &emitted.rust_stages);
            shift_timestamp_columns(&mut result, &tracked, utc_offset_secs);
        }
        if emitted.needs_column_reorder {
            result.reorder_log_columns();
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
    /// not taken as proof of that — it is checked against the evidence
    /// resolution gathered, because a source that still reaches files
    /// answering "no files" is a race, not an empty window (ADR-0008).
    /// `hot_pins` conforms the hot branch (pins ∩ snapshot keys); `pins`
    /// is the full catalog snapshot typing the comparisons — one
    /// interpretation per query (ADR-0011 slice A).
    #[allow(clippy::too_many_arguments)]
    pub fn run_query_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<QueryResult, EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let resolved = self.resolve_source(source);
        let emitted = resolved.emit_union(&ast, hot_source, hot_pins, pins)?;
        let sql_offset = sql_render_offset(&emitted, utc_offset_secs);
        let outcome = self.execute_emitted_tracked(&emitted, max_rows, sql_offset);

        // A hot value disagreeing with a catalog pin is already conformed on
        // the union's hot branch by the emitter (TRY_CAST to NULL), and
        // parquet is write-time conformant — so a type conflict surviving to
        // here means a nonconformant corpus (foreign parquet dropped in
        // post-boot, restore against a stale catalog) and falls through to
        // the outcome policy below as a loud error whenever cold files
        // exist. The read-time coerced retry that used to paper over it is
        // deleted (ADR-0009 slice 2).

        // Classify the outcome, then route on the pure `cold_action`
        // decision so the outcome policy stays unit-testable and identical
        // across the four lanes.
        let (mut result, timestamp_columns) = match self.cold_action_for(
            classify_query_outcome(&outcome),
            &resolved,
            HotLane::Present,
        ) {
            ColdAction::HotOnly => {
                // Hot-only keeps BOTH halves of the interpretation: the same
                // comparison pins, and the same hot-column conformance the
                // union's hot branch applies. Reading the raw ndjson would
                // let `read_json`'s inference type the columns, so
                // `status=200.0` over a VARCHAR-pinned field would match a
                // hot numeric `200` here and stop matching the moment a
                // parquet file appeared.
                let hot_emitted = emitter::emit_hot_only(
                    &ast,
                    hot_source,
                    hot_pins,
                    pins,
                    trawl_core::context::EvalContext::capture(),
                )?;
                match self.execute_emitted_tracked(&hot_emitted, max_rows, sql_offset) {
                    // Hot-only also hit a binder/emit error (e.g. empty ndjson
                    // between compaction cycles). Treat as empty, not error.
                    Err(EngineError::Emit(_)) => (QueryResult::empty(), TimestampColumns::new()),
                    other => other?,
                }
            }
            ColdAction::ColdDataUnread => return Err(EngineError::ColdDataUnread),
            ColdAction::ReturnOutcome => outcome?,
        };
        if !emitted.rust_stages.is_empty() {
            result = crate::post_process::apply_rust_stages(
                result,
                &emitted.rust_stages,
                &emitted.rust_stage_pins,
            )?;
            let tracked = tail_timestamp_scope(&timestamp_columns, &emitted.rust_stages);
            shift_timestamp_columns(&mut result, &tracked, utc_offset_secs);
        }
        if emitted.needs_column_reorder {
            result.reorder_log_columns();
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
        self.execute_emitted_tracked(query, max_rows, utc_offset_secs)
            .map(|(result, _)| result)
    }

    /// [`Self::execute_emitted`], additionally reporting which result
    /// columns `DuckDB` returned as TIMESTAMP.
    ///
    /// Only the Rust-tail lane needs that: it runs over an unshifted
    /// rendering and re-applies the display offset afterwards
    /// ([`shift_timestamp_columns`]).
    fn execute_emitted_tracked(
        &self,
        query: &EmittedQuery,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<(QueryResult, TimestampColumns), EngineError> {
        with_raw_fallback(query, |q| {
            self.execute_emitted_once(q, max_rows, utc_offset_secs)
        })
    }

    fn execute_emitted_once(
        &self,
        query: &EmittedQuery,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<(QueryResult, TimestampColumns), EngineError> {
        let mut stmt = match self.conn.prepare(&query.sql) {
            Ok(s) => s,
            Err(e) if is_no_files_error(&e) => {
                return Ok((QueryResult::empty(), TimestampColumns::new()));
            }
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

        let params = bind_params(&query.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        // start query execution — column metadata is only available
        // after DuckDB resolves table-valued functions like read_parquet()
        let mut result_rows = match stmt.query(param_refs.as_slice()) {
            Ok(r) => r,
            Err(e) if is_no_files_error(&e) => {
                return Ok((QueryResult::empty(), TimestampColumns::new()));
            }
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

        // A column is TIMESTAMP the first time a non-NULL cell of it comes
        // back as one — cheaper than asking for the declared type, and an
        // all-NULL column has nothing to shift anyway. The extra
        // `get_ref_unwrap` costs one lookup per column until it is flagged.
        let mut is_timestamp = vec![false; col_count];

        let mut rows = Vec::new();
        while let Some(row) = result_rows.next()? {
            if rows.len() >= max_rows {
                return Err(EngineError::ResultTooLarge(max_rows));
            }
            let mut cells = Vec::with_capacity(col_count);
            for (i, seen) in is_timestamp.iter_mut().enumerate() {
                if !*seen && matches!(row.get_ref_unwrap(i), ValueRef::Timestamp(..)) {
                    *seen = true;
                }
                cells.push(extract_value(row, i, utc_offset_secs));
            }
            rows.push(cells);
        }

        let timestamp_columns: TimestampColumns = columns
            .iter()
            .zip(&is_timestamp)
            .filter(|&(_, &ts)| ts)
            .map(|(col, _)| col.name.clone())
            .collect();

        Ok((QueryResult { columns, rows }, timestamp_columns))
    }

    /// Resolve a source argument against the files actually on disk — once,
    /// before anything is read (ADR-0008).
    ///
    /// The one door onto [`resolve_list_source`], binding its matcher to this
    /// connection's files.
    ///
    /// The matcher is SPLIT by element shape, for cost: a `glob()` round trip
    /// through `DuckDB` costs roughly a millisecond, and a service-pinned
    /// window is ALL literals (`.../{HH}/nginx.parquet`, one element per
    /// hour), so a 30-day query paid ~700ms just to resolve. A literal element
    /// — no `*`, `?` or `[` — is therefore answered by a single
    /// [`std::fs::metadata`] call instead. Patterns still go through `glob()`.
    ///
    /// The two halves must agree on what "matches" means, because both feed
    /// the same no-silent-cold-drop verdict; `fs_matcher_agrees_with_duckdb_glob_on_literals`
    /// is the drift guard, pinning agreement on the shapes where a filesystem
    /// answer could plausibly diverge from `read_parquet`'s (directory,
    /// symlink, broken symlink, missing path).
    fn resolve_source(&self, source: &str) -> ResolvedSource {
        resolve_list_source(source, |pattern| {
            if has_glob_meta(pattern) {
                self.glob_has_match(pattern)
            } else {
                literal_path_is_file(pattern)
            }
        })
    }

    /// Whether the resolved source has any concrete files behind it.
    ///
    /// Consulted only where the outcome policy's verdict actually depends on
    /// it: a hot-only read — or an empty success — is permitted exactly when
    /// there is no cold data it could
    /// hide. A list source already carries its evidence from resolution, so
    /// this costs nothing and, crucially, never re-globs: a file that matched
    /// at resolution and vanished before the read must surface as
    /// [`EngineError::ColdDataUnread`], not be silently re-resolved away. A
    /// plain glob was never resolved (there is nothing to narrow), so it is
    /// globbed lazily, here and only here. [`Executor::glob_has_match`] errs
    /// on the side of "matches", so an unexpected failure surfaces as an
    /// error rather than degrading to hot-only success.
    fn cold_presence(&self, resolved: &ResolvedSource) -> ColdPresence {
        match resolved.evidence {
            ListEvidence::NotAList => {
                if self.glob_has_match(&resolved.sql) {
                    ColdPresence::Present
                } else {
                    ColdPresence::Absent
                }
            }
            ListEvidence::Matching => ColdPresence::Present,
            ListEvidence::NoneMatching => ColdPresence::Absent,
        }
    }

    /// Route a classified outcome through [`cold_action`], paying the
    /// cold-file presence check only where the table's verdict depends on it.
    fn cold_action_for(
        &self,
        outcome: HotColdOutcome,
        resolved: &ResolvedSource,
        hot: HotLane,
    ) -> ColdAction {
        let cold = if outcome.consults_cold_presence(hot) {
            self.cold_presence(resolved)
        } else {
            // The table answers the same for both values here, so the
            // argument is inert — and the happy path never globs.
            ColdPresence::Absent
        };
        cold_action(outcome, cold, hot)
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

    /// Describe the schema without reading row data — embedded mode's schema
    /// introspection (`trawl schema fields --data`, `trawl query --data`).
    ///
    /// A single `DESCRIBE` over `read_parquet(union_by_name=true)`: `DuckDB`
    /// resolves it without reading rows, and its result is exactly the schema
    /// a *successful* query sees — for all-scalar columns and for union-able
    /// same-kind complex columns (`STRUCT`-vs-`STRUCT`) it reports the MERGED
    /// type, and union-able scalar drift (`BIGINT`-vs-`DOUBLE`) reports the
    /// reconciled type.
    ///
    /// Errors PROPAGATE. The read-time reconcilers that used to degrade an
    /// irreconcilable cross-file mix to `VARCHAR` via per-file describes are
    /// deleted (ADR-0009 slice 2): every trawl-written file conforms to the
    /// field catalog at
    /// write time, so a union conflict can only mean foreign/nonconformant
    /// parquet — and that errors loudly instead of being papered over. The
    /// server's `/api/v1/schema` no longer calls this at all (it reads the
    /// catalog); embedded mode over the user's own parquet is the sole caller.
    pub fn describe_schema(&self, source: &str) -> Result<SchemaResult, EngineError> {
        // Validate source path before interpolation — DuckDB doesn't truly
        // parameterize table-valued function arguments.
        emitter::validate_source_path(source)?;

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
    ///
    /// `max_rows` is a two-mode parameter: a row cap, or `usize::MAX` —
    /// the caller-facing sentinel for an UNBOUNDED export, which emits no
    /// LIMIT at all. Any cap above `DuckDB`'s INT64 LIMIT domain reads as
    /// the sentinel too, since a larger literal is a conversion error.
    /// Both halves are the one decision `export_row_limit` takes, on the
    /// `usize` itself — so the contract holds at every pointer width.
    pub fn export_parquet(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let resolved = self.resolve_source(source);
        let emitted = resolved.emit_cold(&ast, pins)?;
        let outcome = self.export_parquet_from_emitted(&emitted, output_path, max_rows);
        // The same gate the query lanes run, on the same classification. An
        // all-missing source keeps surfacing DuckDB's own "no files" error
        // here — deliberately loud, since there is no empty answer an export
        // could write — but a source that still reaches files must not fail
        // that way (ADR-0008).
        match self.cold_action_for(
            classify_export_outcome(&outcome),
            &resolved,
            HotLane::Absent,
        ) {
            // Discarding `outcome`'s error text is safe by construction:
            // `classify_export_outcome` maps ONLY `is_no_files_error` errors
            // to `NoColumns` (every other `Database` error classifies
            // `Recoverable` and propagates verbatim), so the error dropped
            // here is always the known "no files match" message — which says
            // strictly less than `ColdDataUnread`.
            ColdAction::ColdDataUnread => Err(EngineError::ColdDataUnread),
            ColdAction::ReturnOutcome | ColdAction::HotOnly => outcome,
        }
    }

    /// Export with hot buffer union, falling back to hot-only on cold start.
    ///
    /// Routes through the same resolution and the same outcome gate as
    /// [`Self::run_query_with_hot`]: the hot branch is pin-conformed by the
    /// emitter (a nonconformant corpus errors loudly — see the note in
    /// `run_query_with_hot`), a partial list-source miss is narrowed away
    /// before the read, and a database failure over an existing cold corpus
    /// returns the error instead of silently exporting hot-only data
    /// (ADR-0008).
    ///
    /// `max_rows` is a two-mode parameter: a row cap, or `usize::MAX` —
    /// the caller-facing sentinel for an UNBOUNDED export, which emits no
    /// LIMIT at all. Any cap above `DuckDB`'s INT64 LIMIT domain reads as
    /// the sentinel too, since a larger literal is a conversion error.
    /// Both halves are the one decision `export_row_limit` takes, on the
    /// `usize` itself — so the contract holds at every pointer width.
    #[allow(clippy::too_many_arguments)]
    pub fn export_parquet_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let resolved = self.resolve_source(source);
        let emitted = resolved.emit_union(&ast, hot_source, hot_pins, pins)?;
        let outcome = self.export_parquet_from_emitted(&emitted, output_path, max_rows);

        match self.cold_action_for(
            classify_export_outcome(&outcome),
            &resolved,
            HotLane::Present,
        ) {
            ColdAction::HotOnly => {
                // Hot-only, conformed like the union's hot branch — an
                // export must not write JSON-inferred types where the
                // hot+cold lane would have written the catalog's.
                let hot_emitted = emitter::emit_hot_only(
                    &ast,
                    hot_source,
                    hot_pins,
                    pins,
                    trawl_core::context::EvalContext::capture(),
                )?;
                self.export_parquet_from_emitted(&hot_emitted, output_path, max_rows)
            }
            // Same invariant as `export_parquet`: only an `is_no_files_error`
            // reaches `NoColumns`, so the discarded error text is always that
            // known message and never a distinct diagnosis.
            ColdAction::ColdDataUnread => Err(EngineError::ColdDataUnread),
            ColdAction::ReturnOutcome => outcome,
        }
    }

    /// Internal: stage an emitted query into a temp table and COPY to parquet.
    fn export_parquet_from_emitted(
        &self,
        emitted: &EmittedQuery,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        with_raw_fallback(emitted, |q| {
            self.export_parquet_from_emitted_once(q, output_path, max_rows)
        })
    }

    fn export_parquet_from_emitted_once(
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

        // Create temp table from query results. Which shape the staging
        // SELECT takes is ONE decision, `export_row_limit`, read off the
        // caller's `usize` before any narrowing conversion.
        let create_sql = match export_row_limit(max_rows) {
            Some(max_rows) => format!(
                "CREATE TEMP TABLE __trawl_export AS (SELECT * FROM ({}) LIMIT {max_rows})",
                emitted.sql
            ),
            None => format!(
                "CREATE TEMP TABLE __trawl_export AS (SELECT * FROM ({}))",
                emitted.sql
            ),
        };

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
        // The same INT64 LIMIT domain the export lane clamps against: a
        // larger `usize` is a Conversion Error, not a bigger cap. There is
        // no unbounded shape here — the caller's `max_result_rows` is
        // always a cap — so the ceiling is `i64::MAX`.
        let max_rows = i64::try_from(max_rows).unwrap_or(i64::MAX);
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

/// Retry `attempt` with the query's `_raw`-free SQL when the first try failed
/// to bind a column.
///
/// Text search binds `_raw`, which every trawl-written corpus has but an
/// arbitrary source does not — user-owned parquet read in embedded mode, say.
/// Rather than trust the error message (a missing `_raw` and a genuinely
/// unknown field read the same), this asks for evidence: re-run with the
/// `_raw` side bound to a typed NULL, and take that result only if it binds.
/// If it fails too, the missing column was the user's, so the original error
/// is what they see. The parameter list is identical between the two SQL
/// strings (ADR-0009).
///
/// Costs nothing on the success path, and nothing for queries without a text
/// search — `raw_free_sql` is `None` there.
fn with_raw_fallback<T>(
    query: &EmittedQuery,
    attempt: impl Fn(&EmittedQuery) -> Result<T, EngineError>,
) -> Result<T, EngineError> {
    let err = match attempt(query) {
        Err(e) if is_missing_column_failure(&e) => e,
        outcome => return outcome,
    };
    let Some(raw_free) = &query.raw_free_sql else {
        return Err(err);
    };
    let fallback = EmittedQuery {
        sql: raw_free.clone(),
        raw_free_sql: None,
        ..query.clone()
    };
    attempt(&fallback).map_err(|_| err)
}

/// Whether an engine failure is "a column in the query does not exist in the
/// source" — in either shape it can take: the query path remaps it to
/// [`EngineError::Emit`], the export path surfaces it raw.
///
/// The trigger for the `_raw`-free retry (see [`with_raw_fallback`]), which
/// then decides from evidence — does the raw-free SQL bind? — rather than
/// from which column the message names.
fn is_missing_column_failure(e: &EngineError) -> bool {
    match e {
        EngineError::Emit(_) => true,
        EngineError::Database(db) => is_binder_column_error(db),
        _ => false,
    }
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
/// `conflicting_kind_mixes_all_carry_the_conversion_class`).
///
/// NOTE: since ADR-0009 slice 2 NO production path classifies conversion
/// errors any more — the read-time cast-to-`VARCHAR` fallbacks that consumed
/// this classifier are deleted, because write-time catalog conformance makes
/// a surviving `Conversion` error mean a nonconformant corpus, which errors
/// loudly. The function is retained because the ADR-evidence tests below pin
/// the `DuckDB` behaviour the no-classifier design rests on.
///
/// It is a *necessary but not sufficient* condition for a union type
/// conflict: a genuine data-conversion error (`CAST('abc' AS INTEGER)`)
/// carries the very same class, and no substring of the body separates the
/// two either — both shapes say a value "can't be cast to the destination
/// type" and both name a "source column".
///
/// Deliberately does NOT match corruption ("too small to be a Parquet
/// file") or missing-column binder errors: those are handled by quarantine
/// and the benign-binder carve-out respectively.
pub fn is_conversion_error(e: &duckdb::Error) -> bool {
    error_class(&e.to_string()) == Some("Conversion")
}

/// Classification of a read's outcome, shared by all four entry points.
///
/// Produced by [`classify_query_outcome`] and [`classify_export_outcome`]
/// so no lane holds a private opinion about what a failure means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotColdOutcome {
    /// The read produced columns (rows may be empty) — an authoritative
    /// result; return it as-is. An export's `Ok(())` lands here too.
    Columns,
    /// The read matched no files: on the query lanes `execute_emitted`
    /// mapped "no files match the pattern" to an empty result, on the export
    /// lanes the same error arrives raw. Usually the empty window, but the
    /// resolved source can also have raced a file move, so this is an
    /// observation, not proof: it still needs a cold-file presence check.
    NoColumns,
    /// A user error the hot lanes answer with the established empty-result
    /// UX rather than a failure. What lands here differs by lane: the query
    /// lanes send only a binder error about a missing column (querying a
    /// nonexistent field), while the export lanes ALSO send
    /// [`EngineError::Emit`] refusals (a `rust_stages` pipeline, a non-UTF-8
    /// output path) — main's behavior, kept deliberately. The consequence on
    /// export is that a `HotOnly` verdict retries emit-refused queries
    /// hot-only: a binder-shaped failure can genuinely be rescued by a hot
    /// snapshot that carries the column, while an emitter refusal fails
    /// identically on the retry — futile, harmless, and identical to what
    /// main did.
    BenignBinder,
    /// Any other database failure — hot-only is only provably safe when a
    /// cold-file presence check says no cold files exist.
    Recoverable,
    /// A non-recoverable error (e.g. `ResultTooLarge`, parse) — propagate it.
    Fatal,
}

/// Whether the lane executing a read has a hot buffer to fall back to.
///
/// A property of the ENTRY POINT, not of the data: `run_query` and
/// `export_parquet` have none, so [`cold_action`] never hands them
/// [`ColdAction::HotOnly`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotLane {
    Present,
    Absent,
}

/// What a lane should do with a classified outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdAction {
    /// Return the outcome unchanged (result or error).
    ReturnOutcome,
    /// Read hot-only — no cold data can be hidden by doing so.
    HotOnly,
    /// The read answered "no files" while cold files exist behind the
    /// source. On the query lanes the outcome held is a substituted empty
    /// *success*, so returning it would be exactly the silent cold-data drop
    /// ADR-0008 forbids; on the export lanes it is a raw IO error that says
    /// nothing useful. Both surface [`EngineError::ColdDataUnread`] instead:
    /// explicit, retryable, and never a 200 with rows missing.
    ColdDataUnread,
}

/// Decide the next step for a classified outcome — the ONE outcome policy,
/// consulted by all four entry points (ADR-0008).
///
/// | outcome          | hot=P, cold=P      | hot=P, cold=A | hot=A, cold=P      | hot=A, cold=A |
/// |------------------|--------------------|---------------|--------------------|---------------|
/// | `Columns`        | `Return`           | `Return`      | `Return`           | `Return`      |
/// | `Fatal`          | `Return`           | `Return`      | `Return`           | `Return`      |
/// | `BenignBinder`   | `HotOnly`          | `HotOnly`     | `Return`           | `Return`      |
/// | `NoColumns`      | `ColdDataUnread`   | `HotOnly`     | `ColdDataUnread`   | `Return`      |
/// | `Recoverable`    | `Return`           | `HotOnly`     | `Return`           | `Return`      |
///
/// Two invariants read off it: a hot-only read happens exactly where it
/// cannot hide cold data, and an empty answer is only ever returned when the
/// source provably reaches no files. `cold` is [`ColdPresence`], `hot` is
/// [`HotLane`]; the match is exhaustive with no wildcard arms, so a new
/// variant of either must state its rule or fail to compile.
///
/// Pure, so the policy is unit-testable without provoking every failure
/// class against a live `DuckDB`.
fn cold_action(outcome: HotColdOutcome, cold: ColdPresence, hot: HotLane) -> ColdAction {
    match outcome {
        // An authoritative result and a non-recoverable error are the
        // source's own answer on every lane.
        HotColdOutcome::Columns | HotColdOutcome::Fatal => ColdAction::ReturnOutcome,
        // A missing-column user error keeps the empty-result UX where there
        // is a hot buffer to read it from; without one the error is the UX.
        HotColdOutcome::BenignBinder => match hot {
            HotLane::Present => ColdAction::HotOnly,
            HotLane::Absent => ColdAction::ReturnOutcome,
        },
        HotColdOutcome::NoColumns => match cold {
            ColdPresence::Present => ColdAction::ColdDataUnread,
            ColdPresence::Absent => match hot {
                HotLane::Present => ColdAction::HotOnly,
                HotLane::Absent => ColdAction::ReturnOutcome,
            },
        },
        // An unexpected failure may degrade to hot-only only where there is
        // provably no cold data it would be hiding.
        HotColdOutcome::Recoverable => match (cold, hot) {
            (ColdPresence::Absent, HotLane::Present) => ColdAction::HotOnly,
            (ColdPresence::Absent, HotLane::Absent)
            | (ColdPresence::Present, HotLane::Present | HotLane::Absent) => {
                ColdAction::ReturnOutcome
            }
        },
    }
}

impl HotColdOutcome {
    /// Whether [`cold_action`]'s verdict for this outcome can actually differ
    /// between the two [`ColdPresence`] values — i.e. whether the presence
    /// check is worth paying for.
    ///
    /// Derived FROM the table rather than restated beside it, so it can never
    /// drift out of agreement with it. The happy path (`Columns`) and every
    /// propagated failure (`Fatal`) answer `false`, which is what keeps a
    /// successful query from ever globbing.
    fn consults_cold_presence(self, hot: HotLane) -> bool {
        cold_action(self, ColdPresence::Present, hot)
            != cold_action(self, ColdPresence::Absent, hot)
    }
}

/// Classify a query lane's outcome.
fn classify_query_outcome(
    outcome: &Result<(QueryResult, TimestampColumns), EngineError>,
) -> HotColdOutcome {
    match outcome {
        // Columns present → real result (possibly empty rows).
        Ok((r, _)) if !r.columns.is_empty() => HotColdOutcome::Columns,
        // No columns: `execute_emitted` substituted an empty result for a
        // "no files match the pattern" read error.
        Ok(_) => HotColdOutcome::NoColumns,
        // A binder error the query path remapped to Emit.
        Err(EngineError::Emit(_)) => HotColdOutcome::BenignBinder,
        Err(EngineError::Database(_)) => HotColdOutcome::Recoverable,
        // ResultTooLarge, parse, IO — propagate.
        Err(_) => HotColdOutcome::Fatal,
    }
}

/// Classify an export lane's outcome.
///
/// The export surfaces `DuckDB`'s errors raw where the query path remaps
/// them, so the same three shapes arrive differently: "no files" and the
/// binder error are `Database` here, and `Emit` covers the emitter's own
/// refusals (a `rust_stages` pipeline, a non-UTF-8 path).
fn classify_export_outcome(outcome: &Result<(), EngineError>) -> HotColdOutcome {
    match outcome {
        Ok(()) => HotColdOutcome::Columns,
        Err(EngineError::Database(e)) if is_no_files_error(e) => HotColdOutcome::NoColumns,
        Err(EngineError::Database(e)) if is_binder_column_error(e) => HotColdOutcome::BenignBinder,
        Err(EngineError::Emit(_)) => HotColdOutcome::BenignBinder,
        Err(EngineError::Database(_)) => HotColdOutcome::Recoverable,
        Err(_) => HotColdOutcome::Fatal,
    }
}

/// The LIMIT an export's staging SELECT carries, decided from the caller's
/// `max_rows`.
///
/// `None` is the UNBOUNDED shape — no LIMIT clause at all — and it is the
/// answer for two disjoint reasons. `usize::MAX` is the caller-facing
/// sentinel for an unbounded export; and `DuckDB`'s LIMIT domain is INT64,
/// so every `usize` above `i64::MAX` is a Conversion Error rather than a
/// bigger cap, which the sentinel is not the only way to reach
/// (`max_export_rows` is operator-set).
///
/// The sentinel is tested on the `usize` BEFORE the conversion, because the
/// conversion alone only recognises it where `usize` is wider than `i64`:
/// on a 32-bit target `usize::MAX` is `4_294_967_295` and converts cleanly,
/// which would silently turn the documented unbounded export into a real
/// `LIMIT 4294967295`. Reading the sentinel first is what makes the
/// contract target-independent.
fn export_row_limit(max_rows: usize) -> Option<i64> {
    if max_rows == usize::MAX {
        return None;
    }
    i64::try_from(max_rows).ok()
}

/// What a source's shape — and, for a list, the filesystem underneath it —
/// says about the cold files behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListEvidence {
    /// Not a list: a plain glob, or the `(subquery)` form `from saved`
    /// builds. Nothing was globbed, so presence is unknown until asked.
    NotAList,
    /// A list at least one of whose elements reaches a file — and the
    /// fail-closed home for a list shape [`glob_list_items`] cannot split,
    /// which must never be read as "no cold data".
    Matching,
    /// A list no element of which reaches a file: the genuine empty window.
    NoneMatching,
}

/// Whether the cold side of a read has files behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdPresence {
    Present,
    Absent,
}

/// A source argument resolved against the filesystem — once, before the
/// read, on every lane (ADR-0008).
///
/// `read_parquet` rejects a whole list source when a SINGLE element matches
/// nothing, and the server emits one glob per hour in range, so a
/// sparse-traffic service routinely gets elements pointing at hour
/// directories holding no file of its own. Resolution narrows the list to
/// the elements that reach a file BEFORE the read, rather than retrying
/// after one failed — and the evidence it gathers on the way is what the
/// outcome policy later reads instead of globbing again.
pub(crate) struct ResolvedSource {
    /// The source text to emit — the original for a plain glob, an
    /// all-matching list and an all-missing list; the narrowed list when
    /// some but not all elements match.
    sql: String,
    /// What the resolution learned about the files behind it.
    evidence: ListEvidence,
}

impl ResolvedSource {
    /// Emit the cold-only read over this source.
    ///
    /// The crate's ONE call to [`emitter::emit_with_pins`]: every lane that
    /// reads parquet reads a source that has been resolved.
    fn emit_cold(
        &self,
        query: &Query,
        pins: &FieldTypes,
    ) -> Result<EmittedQuery, emitter::EmitError> {
        emitter::emit_with_pins(
            query,
            &self.sql,
            pins,
            trawl_core::context::EvalContext::capture(),
        )
    }

    /// Emit the hot+cold union read over this source.
    ///
    /// The crate's ONE call to [`emitter::emit_with_hot_source`], for the
    /// same reason as [`Self::emit_cold`]. (`emit_hot_only` stays a free
    /// call — it reads no cold source at all.)
    fn emit_union(
        &self,
        query: &Query,
        hot_source: &str,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
    ) -> Result<EmittedQuery, emitter::EmitError> {
        emitter::emit_with_hot_source(
            query,
            &self.sql,
            hot_source,
            hot_pins,
            pins,
            trawl_core::context::EvalContext::capture(),
        )
    }
}

/// Whether a source element carries a `DuckDB` glob metacharacter — the test
/// that decides whether resolution asks the filesystem or asks `glob()`.
fn has_glob_meta(element: &str) -> bool {
    element.contains(['*', '?', '['])
}

/// Whether a LITERAL path (no glob metacharacters) is a file `read_parquet`
/// could open — the filesystem twin of [`Executor::glob_has_match`].
///
/// `metadata` FOLLOWS symlinks, which is what `glob()` does too: a symlink to
/// a real file matches, a broken one does not. A directory named like a
/// parquet file is not a file, and `glob()` on a literal path does not match
/// one either (probed, not assumed).
///
/// A metadata error that is NOT `NotFound` (a permission or IO fault) RETAINS
/// the element: the element must reach `read_parquet` and fail loudly there,
/// never be dropped into a silently narrower answer.
fn literal_path_is_file(path: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Resolve `source` against a file matcher — the ONE owner of list-source
/// resolution semantics.
///
/// The matcher is injected so the semantics are unit-testable without a live
/// `DuckDB` connection; [`Executor::resolve_source`] binds it to `glob()`.
///
/// List elements are retained VERBATIM in their original order, duplicates
/// included, and never expanded to concrete filenames: the narrowed source
/// must reach exactly the data the original reached, and a glob expanded at
/// resolution time would freeze a file list that compaction is still
/// appending to.
fn resolve_list_source(source: &str, matches: impl Fn(&str) -> bool) -> ResolvedSource {
    let Some(items) = glob_list_items(source) else {
        return ResolvedSource {
            sql: source.to_owned(),
            evidence: ListEvidence::NotAList,
        };
    };
    if items.is_empty() {
        // List-shaped but unparseable: nothing to narrow, and the evidence
        // must not read as "no cold data" (fail closed).
        return ResolvedSource {
            sql: source.to_owned(),
            evidence: ListEvidence::Matching,
        };
    }

    let matching: Vec<&str> = items.iter().copied().filter(|item| matches(item)).collect();
    if matching.is_empty() {
        // The genuine empty window: the original stands, and the read's
        // "no files" answer is the truth.
        return ResolvedSource {
            sql: source.to_owned(),
            evidence: ListEvidence::NoneMatching,
        };
    }
    if matching.len() == items.len() {
        return ResolvedSource {
            sql: source.to_owned(),
            evidence: ListEvidence::Matching,
        };
    }

    let quoted: Vec<String> = matching.iter().map(|item| format!("'{item}'")).collect();
    ResolvedSource {
        sql: format!("[{}]", quoted.join(", ")),
        evidence: ListEvidence::Matching,
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
                // The statement's `now()` anchor (ADR-0017 §3). Bound as
                // MICROSECONDS because that is `DuckDB`'s TIMESTAMP
                // domain and the anchor is truncated to it at capture, so
                // the bound value is exact — never rounded at the wire.
                SqlValue::Timestamp(at) => Box::new(duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    at.and_utc().timestamp_micros(),
                )),
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

/// The offset the SQL result is RENDERED with, given the display offset
/// the caller asked for.
///
/// Zero whenever a Rust tail follows, because that tail is a second
/// evaluator, not a printer: its `| where`/`| let` are pin-aware
/// (ADR-0011 slice A′) and read a TIMESTAMP cell through
/// `compare::conformed_timestamp`, which takes a zoneless text as UTC. A
/// display-shifted rendering handed to it would compare local wall-clock
/// text against a UTC instant and skew every timestamp comparison by the
/// client's offset — silently dropping rows for any non-UTC client. So
/// the tail runs over UTC and the shift is re-applied afterwards
/// ([`shift_timestamp_columns`]), which is the all-SQL path's order too:
/// compare the stored instant, shift last.
fn sql_render_offset(emitted: &EmittedQuery, utc_offset_secs: i32) -> i32 {
    if emitted.rust_stages.is_empty() {
        utc_offset_secs
    } else {
        0
    }
}

/// Follow the SQL result's TIMESTAMP columns through the Rust tail's own
/// rename/alias lineage.
///
/// The tail re-uses the pin-scope walk ([`PinScope::advance`], ADR-0011
/// slice A′) with a scope holding exactly the tracked columns: a
/// `rename`d timestamp column carries its tracking to the new name, a
/// bare-alias `let t2 = _time` copies it, and a computed value or
/// aggregate output — the tail's own, no longer the stored rendering —
/// is killed by the walk and stays as the tail rendered it. Both display
/// lineage and comparison pins follow the SAME stage rules, so they can
/// never disagree about which column is "still `_time`".
fn tail_timestamp_scope(timestamp_columns: &[String], stages: &[Spanned<PipeStage>]) -> PinScope {
    let mut seed = FieldTypes::new();
    for name in timestamp_columns {
        seed.insert(name, CanonicalType::Timestamp);
    }
    let mut scope = PinScope::root(&seed);
    for stage in stages {
        scope.advance(&stage.node);
    }
    scope
}

/// Re-apply the display offset to the TIMESTAMP columns that survived the
/// Rust tail, resolved through the tail's lineage
/// ([`tail_timestamp_scope`]).
///
/// A column the lineage cannot vouch for — an aggregate output, a
/// computed `let` — displays UTC: the honest reading for a value the
/// tail has already transformed, and never a wrong instant. Cells the
/// tail replaced with non-timestamp text are guarded per-cell by
/// [`shift_display_timestamp`]'s parse.
fn shift_timestamp_columns(result: &mut QueryResult, tracked: &PinScope, utc_offset_secs: i32) {
    if utc_offset_secs == 0 || tracked.is_empty() {
        return;
    }
    let targets: Vec<usize> = result
        .columns
        .iter()
        .enumerate()
        .filter(|(_, col)| tracked.pin_for(&col.name).is_some())
        .map(|(idx, _)| idx)
        .collect();
    if targets.is_empty() {
        return;
    }
    for row in &mut result.rows {
        for &idx in &targets {
            if let Some(Value::String(text)) = row.get_mut(idx)
                && let Some(shifted) = shift_display_timestamp(text, utc_offset_secs)
            {
                *text = shifted;
            }
        }
    }
}

/// Re-render [`format_timestamp`]'s UTC output in the display offset.
///
/// `None` when the text is not that rendering — a cell the tail replaced
/// with something else, or a year outside `chrono`'s parse — in which
/// case it is left exactly as it stands.
fn shift_display_timestamp(text: &str, utc_offset_secs: i32) -> Option<String> {
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f").ok()?;
    Some(format_timestamp(
        TimeUnit::Microsecond,
        naive.and_utc().timestamp_micros(),
        utc_offset_secs,
    ))
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
        ColdAction, ColdPresence, EngineError, Executor, FieldTypes, HotColdOutcome, HotLane,
        ListEvidence, cold_action, error_class, export_row_limit, glob_list_items, has_glob_meta,
        is_conversion_error, is_no_files_error, literal_path_is_file, resolve_list_source,
    };

    /// Write a one-row parquet file whose `meta` column has the given SQL
    /// type/value expression (e.g. `{'a': 1}` for a `STRUCT`, `'plain'` for a
    /// `VARCHAR`). Mirrors the cold-fixture shape used in `integration.rs`.
    fn write_meta_parquet(conn: &Connection, path: &Path, meta_expr: &str) {
        conn.execute_batch(&format!(
            "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
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
    fn genuine_data_conversion_error_carries_the_conversion_class() {
        // The premise ADR-0008 rests on: neither the class token nor the
        // message body can tell a value that cannot be converted from a
        // schema that cannot be reconciled. Both are `Conversion`-class, and
        // on the bundled 1.5.5 both bodies name a "source column" and talk
        // about casting to a "destination type". This is exactly why the
        // read path no longer classifies conflicts at all (ADR-0009 slice 2
        // deleted the coerced retry): the catalog enforces conformance at
        // write time and any surviving Conversion error is loud.
        let data = data_conversion_error();
        assert_eq!(
            error_class(&data.to_string()),
            Some("Conversion"),
            "a genuine data-conversion error is Conversion-class too: {data}"
        );
        assert!(is_conversion_error(&data));
        assert!(is_conversion_error(&union_conversion_error()));
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
    fn irreconcilable_drift_describes_raw_and_errors_loudly_at_read() {
        // `meta` is a STRUCT in file A and a VARCHAR in file B — an
        // irreconcilable cross-file mix a real query cannot read. Post-catalog
        // (ADR-0009 slice 2) every trawl-written file conforms at write time,
        // so this shape can only be foreign/nonconformant parquet.
        //
        // Execution-verified on the bundled DuckDB 1.5.5: DESCRIBE never
        // reads rows, so it resolves the union to the complex side WITHOUT
        // erroring — the describe reports what is in the footers, raw. The
        // read-time reconciler that used to degrade the column to VARCHAR is
        // deleted, so the QUERY over the same glob now errors loudly instead
        // of silently succeeding with a coerced VARCHAR column. Pin both
        // halves: describe succeeds (raw), query errors (loud).
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("a.parquet"), "{'a': 1}");
        write_meta_parquet(&setup, &dir.path().join("b.parquet"), "'plain'");

        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());

        let schema = exec
            .describe_schema(&glob)
            .expect("DESCRIBE reads no rows, so it resolves without erroring");
        let meta = schema
            .columns
            .iter()
            .find(|c| c.name == "meta")
            .expect("schema must include the drifted `meta` column");
        assert!(
            meta.data_type.to_ascii_uppercase().starts_with("STRUCT"),
            "describe reports the footer union raw (the complex side), \
             never a coerced VARCHAR; got `{}`",
            meta.data_type
        );

        let result = exec.run_query("* | fields meta", &glob, &FieldTypes::new(), usize::MAX, 0);
        assert!(
            matches!(result, Err(EngineError::Database(ref e)) if is_conversion_error(e)),
            "a query over the irreconcilable mix must error loudly (the \
             coerced VARCHAR retry is deleted); got {result:?}"
        );
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
            .run_query("* | fields meta", &glob, &FieldTypes::new(), usize::MAX, 0)
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
            "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        // Glob that matches no parquet files (cold start).
        let source = format!("{}/nonexistent/*.parquet", dir.path().display());
        let result = exec
            .run_query_with_hot(
                "*",
                &source,
                hot.to_str().unwrap(),
                &FieldTypes::new(),
                &FieldTypes::new(),
                usize::MAX,
                0,
            )
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
    fn cold_action_matrix_is_exhaustive() {
        // The whole outcome policy, cell by cell (ADR-0008). Two invariants
        // read off it: a hot-only read happens exactly where it cannot hide
        // cold data, and an empty answer is returned only when the source
        // provably reaches no files. The `hot=Present` columns are the
        // behaviour the hot+cold lanes have always had; the `hot=Absent`
        // ones extend the same gate to `run_query`/`export_parquet`, which
        // used to have none.
        use ColdAction::{ColdDataUnread as Unread, HotOnly, ReturnOutcome as Return};
        use ColdPresence::{Absent as ColdAbsent, Present as ColdPresent};
        use HotColdOutcome::{BenignBinder, Columns, Fatal, NoColumns, Recoverable};
        use HotLane::{Absent as NoHot, Present as Hot};

        let matrix = [
            // (outcome, cold, hot, action, why)
            (Columns, ColdPresent, Hot, Return, "authoritative result"),
            (Columns, ColdAbsent, Hot, Return, "authoritative result"),
            (Columns, ColdPresent, NoHot, Return, "authoritative result"),
            (Columns, ColdAbsent, NoHot, Return, "authoritative result"),
            (Fatal, ColdPresent, Hot, Return, "never masked"),
            (Fatal, ColdAbsent, Hot, Return, "never masked"),
            (Fatal, ColdPresent, NoHot, Return, "never masked"),
            (Fatal, ColdAbsent, NoHot, Return, "never masked"),
            (BenignBinder, ColdPresent, Hot, HotOnly, "empty-result UX"),
            (BenignBinder, ColdAbsent, Hot, HotOnly, "empty-result UX"),
            (
                BenignBinder,
                ColdPresent,
                NoHot,
                Return,
                "no hot lane to read",
            ),
            (
                BenignBinder,
                ColdAbsent,
                NoHot,
                Return,
                "no hot lane to read",
            ),
            (
                NoColumns,
                ColdPresent,
                Hot,
                Unread,
                "cold data would vanish",
            ),
            (NoColumns, ColdAbsent, Hot, HotOnly, "genuine cold start"),
            (
                NoColumns,
                ColdPresent,
                NoHot,
                Unread,
                "cold data would vanish",
            ),
            (
                NoColumns,
                ColdAbsent,
                NoHot,
                Return,
                "genuinely empty window",
            ),
            (
                Recoverable,
                ColdPresent,
                Hot,
                Return,
                "failure over cold data",
            ),
            (Recoverable, ColdAbsent, Hot, HotOnly, "nothing to hide"),
            (
                Recoverable,
                ColdPresent,
                NoHot,
                Return,
                "failure over cold data",
            ),
            (
                Recoverable,
                ColdAbsent,
                NoHot,
                Return,
                "failure, no hot lane",
            ),
        ];
        for (outcome, cold, hot, expected, why) in matrix {
            assert_eq!(
                cold_action(outcome, cold, hot),
                expected,
                "{outcome:?} × cold={cold:?} × hot={hot:?} must be {expected:?} ({why})"
            );
        }
    }

    #[test]
    fn the_happy_path_never_pays_the_presence_check() {
        // The presence check globs; a successful query must never do that.
        // `consults_cold_presence` is derived from the table, so this asserts
        // the table's own shape rather than a second copy of it.
        for hot in [HotLane::Present, HotLane::Absent] {
            assert!(
                !HotColdOutcome::Columns.consults_cold_presence(hot),
                "a columnful result must not provoke a presence glob"
            );
            assert!(
                !HotColdOutcome::Fatal.consults_cold_presence(hot),
                "a propagated error must not provoke a presence glob"
            );
            assert!(
                !HotColdOutcome::BenignBinder.consults_cold_presence(hot),
                "a missing-column user error is decided without the filesystem"
            );
            assert!(
                HotColdOutcome::NoColumns.consults_cold_presence(hot),
                "a no-files answer is exactly what the presence check settles"
            );
        }
        assert!(
            HotColdOutcome::Recoverable.consults_cold_presence(HotLane::Present),
            "a hot lane may degrade an unexpected failure — only presence says whether"
        );
        assert!(
            !HotColdOutcome::Recoverable.consults_cold_presence(HotLane::Absent),
            "without a hot lane the failure propagates either way"
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
            "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("{}/*.parquet", dir.path().display());
        let result = exec.run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        );
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
            "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("{}/*.parquet", dir.path().display());
        let result = exec
            .run_query_with_hot(
                "nonexistent_field=value",
                &source,
                hot.to_str().unwrap(),
                &FieldTypes::new(),
                &FieldTypes::new(),
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
    fn has_glob_meta_splits_the_matcher() {
        assert!(!has_glob_meta("/data/prod/2024-01-15/10/nginx.parquet"));
        assert!(has_glob_meta("/data/prod/2024-01-15/10/*.parquet"));
        assert!(has_glob_meta("/data/prod/2024-01-15/1?/nginx.parquet"));
        assert!(has_glob_meta("/data/prod/2024-01-15/[01]0/nginx.parquet"));
    }

    #[test]
    #[cfg(unix)]
    fn fs_matcher_agrees_with_duckdb_glob_on_literals() {
        // The drift guard on the literal fast path: "matches" must mean the
        // same thing whether resolution asked the filesystem or `glob()`,
        // because both feed the same no-silent-cold-drop verdict. Pinned on
        // the shapes where the two could plausibly diverge.
        let dir = tempfile::tempdir().unwrap();
        let exec = Executor::new().unwrap();
        let setup = Connection::open_in_memory().unwrap();

        let present = dir.path().join("present.parquet");
        write_meta_parquet(&setup, &present, "'plain'");

        let missing = dir.path().join("missing.parquet");

        // A DIRECTORY named like a parquet file: not something `read_parquet`
        // can open as an element.
        let dir_named_parquet = dir.path().join("dir.parquet");
        std::fs::create_dir(&dir_named_parquet).unwrap();

        // A symlink to a real file, and a broken one.
        let good_link = dir.path().join("good-link.parquet");
        std::os::unix::fs::symlink(&present, &good_link).unwrap();
        let broken_link = dir.path().join("broken-link.parquet");
        std::os::unix::fs::symlink(&missing, &broken_link).unwrap();

        let shapes = [
            (present.clone(), true, "an existing parquet file"),
            (missing.clone(), false, "a missing path"),
            (dir_named_parquet, false, "a directory named like a parquet"),
            (good_link, true, "a symlink to a real file"),
            (broken_link, false, "a broken symlink"),
        ];

        for (path, expected, what) in shapes {
            let path = path.to_str().unwrap();
            assert_eq!(
                literal_path_is_file(path),
                expected,
                "fs matcher disagrees with the pinned answer for {what}"
            );
            assert_eq!(
                exec.glob_has_match(path),
                expected,
                "DuckDB glob() disagrees with the fs matcher for {what}"
            );
        }
    }

    #[test]
    fn literal_path_is_file_retains_on_a_non_notfound_error() {
        // Fail toward RETAINING the element: an unreadable path must reach
        // `read_parquet` and error loudly, never vanish from the list. A
        // component that is a FILE makes the lookup ENOTDIR, not ENOENT.
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        let file = dir.path().join("present.parquet");
        write_meta_parquet(&setup, &file, "'plain'");

        let through_a_file = file.join("nested.parquet");
        let err = std::fs::metadata(&through_a_file).expect_err("must not resolve");
        assert_ne!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "fixture must provoke a non-NotFound error"
        );
        assert!(
            literal_path_is_file(through_a_file.to_str().unwrap()),
            "a non-NotFound metadata error must retain the element"
        );
    }

    #[test]
    fn cold_presence_reads_resolved_list_evidence() {
        // A list source is not "present by construction": the server emits a
        // list of per-hour globs for essentially every time-filtered query,
        // and an hour directory can exist while holding no parquet yet.
        // Resolution is what settles it, and `cold_presence` reads that
        // evidence rather than globbing again.
        let dir = tempfile::tempdir().unwrap();
        let empty_hour = dir.path().join("10");
        std::fs::create_dir_all(&empty_hour).unwrap();
        let exec = Executor::new().unwrap();

        let absent = format!("['{}/*.parquet']", empty_hour.display());
        let resolved = exec.resolve_source(&absent);
        assert_eq!(
            resolved.evidence,
            ListEvidence::NoneMatching,
            "a list whose globs match no file resolves to no evidence of files"
        );
        assert_eq!(
            exec.cold_presence(&resolved),
            ColdPresence::Absent,
            "a list whose globs match no file must report no cold files"
        );

        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &empty_hour.join("cold.parquet"), "'plain'");
        let resolved = exec.resolve_source(&absent);
        assert_eq!(
            resolved.evidence,
            ListEvidence::Matching,
            "a list whose globs match a file resolves to matching evidence"
        );
        assert_eq!(
            exec.cold_presence(&resolved),
            ColdPresence::Present,
            "a list whose globs match a file must report cold files present"
        );
    }

    #[test]
    fn plain_glob_resolves_lazily_and_is_globbed_on_demand() {
        // A plain glob has nothing to narrow, so resolution neither globs nor
        // rewrites it — presence is answered later, and only if the outcome
        // policy asks.
        let dir = tempfile::tempdir().unwrap();
        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());

        let resolved = exec.resolve_source(&glob);
        assert_eq!(resolved.evidence, ListEvidence::NotAList);
        assert_eq!(
            resolved.sql, glob,
            "a plain glob is passed through verbatim"
        );
        assert_eq!(exec.cold_presence(&resolved), ColdPresence::Absent);

        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("cold.parquet"), "'plain'");
        assert_eq!(
            exec.cold_presence(&exec.resolve_source(&glob)),
            ColdPresence::Present
        );
    }

    #[test]
    fn subquery_source_is_never_a_list() {
        // `from saved` builds a `(SELECT ...)` source. It is not list-shaped,
        // so resolution passes it through untouched — no globbing, no
        // rewriting.
        let source = "(SELECT * FROM read_parquet('/data/**/*.parquet'))";
        let resolved = resolve_list_source(source, |_| panic!("must not glob a subquery source"));
        assert_eq!(resolved.evidence, ListEvidence::NotAList);
        assert_eq!(resolved.sql, source);
    }

    #[test]
    fn resolve_list_source_narrows_to_matching_elements() {
        // The everyday `service=X last=Nh` shape: some hour globs reach the
        // service's file, some reach an hour directory another service wrote.
        let source = "['/a/*.parquet', '/b/*.parquet', '/c/*.parquet']";
        let resolved = resolve_list_source(source, |item| item != "/b/*.parquet");
        assert_eq!(resolved.evidence, ListEvidence::Matching);
        assert_eq!(resolved.sql, "['/a/*.parquet', '/c/*.parquet']");
    }

    #[test]
    fn resolve_list_source_preserves_order_and_duplicates() {
        // Elements are kept VERBATIM, in the original order, duplicates
        // included: the narrowed source must reach exactly what the original
        // reached, and never a set of concrete filenames.
        let source = "['/z/*.parquet', '/a/*.parquet', '/z/*.parquet', '/m/*.parquet']";
        let resolved = resolve_list_source(source, |item| item != "/m/*.parquet");
        assert_eq!(
            resolved.sql,
            "['/z/*.parquet', '/a/*.parquet', '/z/*.parquet']"
        );
    }

    #[test]
    fn resolve_list_source_keeps_the_original_when_nothing_is_narrowed() {
        // All matching and none matching both leave the source text alone —
        // only the evidence differs, and only that distinguishes an empty
        // window from a read that must not answer empty.
        let source = "['/a/*.parquet', '/b/*.parquet']";

        let all = resolve_list_source(source, |_| true);
        assert_eq!(all.evidence, ListEvidence::Matching);
        assert_eq!(all.sql, source);

        let none = resolve_list_source(source, |_| false);
        assert_eq!(none.evidence, ListEvidence::NoneMatching);
        assert_eq!(none.sql, source);
    }

    #[test]
    fn resolve_list_source_fails_closed_on_an_unparseable_list() {
        // A list shape the splitter doesn't understand must never read as
        // "no cold data" — that is exactly the silent drop ADR-0008 forbids.
        for source in ["['/a/*.parquet", "['/a/*.parquet]", "[]"] {
            let resolved = resolve_list_source(source, |_| {
                panic!("an unparseable list must not be globbed")
            });
            assert_eq!(
                resolved.evidence,
                ListEvidence::Matching,
                "unparseable list `{source}` must fail closed"
            );
            assert_eq!(resolved.sql, source, "and must be emitted verbatim");
        }
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
            "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        let result = exec
            .run_query_with_hot(
                "*",
                &source,
                hot.to_str().unwrap(),
                &FieldTypes::new(),
                &FieldTypes::new(),
                usize::MAX,
                0,
            )
            .expect("a partial list-source miss must not fail the query");
        assert_eq!(
            result.row_count(),
            2,
            "the cold row behind the matching glob element must survive an \
             empty sibling element, alongside the hot row"
        );
    }

    #[test]
    fn partial_list_source_miss_keeps_cold_rows_without_a_hot_buffer() {
        // The same shape as `partial_list_source_miss_keeps_cold_rows`, one
        // lane over: `run_query` had NEITHER the prune retry nor the outcome
        // gate, so `service=X last=Nh` against an idle install answered 200
        // with zero rows for as long as any hour in range belonged to another
        // service. Resolution is a source property now, so it does not.
        let dir = tempfile::tempdir().unwrap();
        let full_hour = dir.path().join("10");
        let empty_hour = dir.path().join("11");
        std::fs::create_dir_all(&full_hour).unwrap();
        std::fs::create_dir_all(&empty_hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &full_hour.join("svc.parquet"), "'plain'");

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        let result = exec
            .run_query("*", &source, &FieldTypes::new(), usize::MAX, 0)
            .expect("a partial list-source miss must not fail the query");
        assert_eq!(
            result.row_count(),
            1,
            "the cold row behind the matching glob element must survive an \
             empty sibling element, with no hot buffer to rescue it"
        );
    }

    #[test]
    fn export_without_hot_keeps_cold_rows_on_partial_list_miss() {
        // `export_parquet` surfaced DuckDB's "no files" error raw, so the
        // same everyday shape was a 500 on the export lane.
        let dir = tempfile::tempdir().unwrap();
        let full_hour = dir.path().join("10");
        let empty_hour = dir.path().join("11");
        std::fs::create_dir_all(&full_hour).unwrap();
        std::fs::create_dir_all(&empty_hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &full_hour.join("svc.parquet"), "'plain'");
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        exec.export_parquet("*", &source, &FieldTypes::new(), &out, 1000)
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
        assert_eq!(rows, 1, "the surviving cold row must reach the export");
    }

    #[test]
    fn empty_window_without_a_hot_buffer_is_an_empty_success() {
        // The one shape where an empty answer is the truth: the list reaches
        // no file at all. Widening the gate to `run_query` must not turn a
        // genuinely empty window into an error — that would make every query
        // over a quiet time range fail.
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        let result = exec
            .run_query("*", &source, &FieldTypes::new(), usize::MAX, 0)
            .expect("an empty window must be an empty success, not an error");
        assert_eq!(result.row_count(), 0, "an empty window has no rows");
    }

    #[test]
    fn export_without_hot_stays_loud_for_an_empty_window() {
        // An export has no empty answer to write, so the raw "no files" error
        // stands where the query lane returns zero rows. Deliberate, and
        // pinned so the gate's widening does not quietly convert it.
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        let result = exec.export_parquet("*", &source, &FieldTypes::new(), &out, 1000);
        assert!(
            matches!(result, Err(EngineError::Database(ref e)) if is_no_files_error(e)),
            "an export over an empty window keeps DuckDB's own error; got {result:?}"
        );
    }

    #[test]
    fn no_files_outcome_with_cold_data_errors_instead_of_empty_success() {
        // A "no files" read error with cold parquet on disk must NOT surface
        // as the substituted empty success: that would silently drop the
        // entire cold history (ADR-0008). Provoked here the same way it
        // happens in production — the hot snapshot path matches no file while
        // a sibling cold element still does — so the prune retry cannot
        // rescue it and the outcome policy is the last line of defense.
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &hour.join("cold.parquet"), "'plain'");
        let missing_hot = dir.path().join("hot.ndjson"); // never written

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        let result = exec.run_query_with_hot(
            "*",
            &source,
            missing_hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        );
        assert!(
            matches!(result, Err(crate::error::EngineError::ColdDataUnread)),
            "cold data on disk plus a no-files union must be an explicit \
             error, not an empty success; got {result:?}"
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
            "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        exec.export_parquet_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            &out,
            1000,
        )
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
            "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        exec.export_parquet_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            &out,
            1000,
        )
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
    fn describe_schema_reconciles_union_able_scalar_drift() {
        // `meta` is BIGINT in one file and DOUBLE in another — union-able
        // scalar drift `read_parquet(union_by_name)` reconciles to DOUBLE.
        // The single-DESCRIBE path must report the reconciled type, exactly
        // what a real query over the same glob reads.
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
        let schema = exec
            .describe_schema(&glob)
            .expect("union-able scalar drift must describe cleanly");

        let meta = schema
            .columns
            .iter()
            .find(|c| c.name == "meta")
            .expect("schema must include the drifted `meta` column");
        assert_eq!(
            meta.data_type.to_ascii_uppercase(),
            "DOUBLE",
            "describe must report DuckDB's reconciled type (DOUBLE), not \
             the first-seen BIGINT, got `{}`",
            meta.data_type
        );
    }

    /// The export lane's LIMIT shape is decided on the caller's `usize`,
    /// so the unbounded contract holds at every pointer width.
    ///
    /// This asserts the DECISION, not an execution, and that is what makes
    /// it target-independent: `usize::MAX` here IS the target's sentinel,
    /// whatever its width.
    ///
    /// Deciding by conversion alone — `i64::try_from` with the error arm
    /// standing in for "unbounded" — passes on a 64-bit target and FAILS
    /// on a 32-bit one, where `usize::MAX` is `4_294_967_295` and converts
    /// cleanly into a real `LIMIT`.
    #[test]
    fn export_row_limit_sentinel_is_target_independent() {
        assert_eq!(
            export_row_limit(usize::MAX),
            None,
            "`usize::MAX` is the unbounded sentinel on every target"
        );

        // Ordinary caps are unchanged, including the degenerate zero: the
        // bounded arm renders exactly the integer it is handed.
        assert_eq!(export_row_limit(0), Some(0));
        assert_eq!(export_row_limit(2), Some(2));
        assert_eq!(export_row_limit(5_000), Some(5_000));

        // `i64::MAX` is the largest cap DuckDB can name in a LIMIT. Ask
        // only where a `usize` can hold it — on a 32-bit target the value
        // does not exist, and the sentinel correctly answers first.
        if let Ok(top) = usize::try_from(i64::MAX) {
            assert_eq!(export_row_limit(top), Some(i64::MAX));
        }

        // Out of the INT64 domain but NOT the sentinel (`max_export_rows`
        // is operator-set). Only reachable where `usize` is wider than
        // `i64`'s positive range.
        if let Ok(above) = usize::try_from(1_u128 << 63) {
            assert_eq!(export_row_limit(above), None);
        }
    }
}
