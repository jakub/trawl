// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Write as _;

use crate::ast::TrawlDuration;

use super::SqlValue;

/// A single CTE in the emitted SQL.
struct Cte {
    name: String,
    sql: String,
}

/// `DuckDB` PIVOT specification — set when `pivot` is the terminal stage.
pub(crate) struct PivotSpec {
    pub agg_sql: String,
    pub on_field: String,
    pub by_fields: Vec<String>,
}

/// When to flush accumulated state to a CTE before processing a new stage.
#[derive(Clone, Copy)]
pub(crate) enum FlushCondition {
    /// Flush if any prior aggregation or projection would be clobbered.
    IfModified,
    /// Flush if modified OR if there's an existing ORDER BY.
    IfModifiedOrOrdered,
    /// Flush if modified OR if there's an existing LIMIT.
    IfModifiedOrLimited,
    /// Always flush (stage requires clean state for param ordering).
    Always,
}

/// Accumulates SQL clauses as the emitter walks the AST.
///
/// The emitter pushes clauses into the state, and flushes to CTEs when
/// pipe stages are incompatible (e.g. `where` after `stats`).
pub(crate) struct EmitterState {
    step: usize,
    source: String,
    pub(crate) select: Vec<String>,
    pub(crate) where_clauses: Vec<String>,
    pub(crate) group_by: Vec<String>,
    pub(crate) order_by: Vec<String>,
    pub(crate) limit: Option<u64>,
    pub(crate) has_aggregation: bool,
    pub(crate) has_projection: bool,
    /// Persistent flag: set when any stage defines a complete output column set
    /// (table/fields, stats, top, rare, timechart, pivot). Unlike `has_aggregation`
    /// and `has_projection`, this is NOT reset on CTE flush — it tracks whether
    /// the pipeline as a whole produced an explicit schema.
    pub(crate) had_explicit_columns: bool,
    /// The time filter from the search stage, used by `timechart` auto-bucketing.
    /// Not reset on CTE flush — this is query-wide context.
    pub(crate) time_filter: Option<TrawlDuration>,
    /// `USING SAMPLE` clause set by `sample` stage.
    pub(crate) sample: Option<String>,
    /// Set by `pivot` stage — overrides normal `build_select()` in `finalize()`.
    pivot: Option<PivotSpec>,
    ctes: Vec<Cte>,
    params: Vec<SqlValue>,
    /// How text search binds `_raw` in this pass (see [`RawBinding`]).
    raw_binding: RawBinding,
}

/// How the `_raw` column is bound by text search in one emission pass.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawBinding {
    /// `_raw` may be bound; no text search has done so yet.
    Available,
    /// A text-search predicate bound `_raw`, so this query needs a raw-free
    /// variant for sources that lack the column.
    Bound,
    /// The raw-free pass: text search substitutes a typed NULL for `_raw`.
    Suppressed,
}

/// Validate a source path for use in `DuckDB` table-valued functions.
///
/// `DuckDB`'s `read_parquet()`/`read_json_auto()` don't support parameterized
/// paths, so the path must be sanitized before interpolation into SQL.
pub fn validate_source_path(source: &str) -> Result<(), super::EmitError> {
    if source.is_empty() {
        return Err(super::EmitError::UnsupportedOperation {
            message: "source path cannot be empty".to_string(),
        });
    }
    if !source
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/_.*?{}[]-".contains(&b))
    {
        return Err(super::EmitError::UnsupportedOperation {
            message: format!("source path contains invalid characters: {source}"),
        });
    }
    // reject path traversal via .. components
    if source.split('/').any(|component| component == "..") {
        return Err(super::EmitError::UnsupportedOperation {
            message: format!("source path contains path traversal: {source}"),
        });
    }
    Ok(())
}

/// Validate a list-format source for `read_parquet()`.
///
/// Expected format: `['path1', 'path2', ...]`. Each individual path
/// inside the list must pass [`validate_source_path`].
fn validate_source_list(source: &str) -> Result<(), super::EmitError> {
    let inner = source
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| super::EmitError::UnsupportedOperation {
            message: "invalid source list format".to_string(),
        })?;

    for segment in inner.split(',') {
        let trimmed = segment.trim();
        let path = trimmed
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .ok_or_else(|| super::EmitError::UnsupportedOperation {
                message: format!("invalid source list element: {trimmed}"),
            })?;
        validate_source_path(path)?;
    }

    Ok(())
}

/// Build a `DuckDB` reader expression from a source path.
///
/// Handles three source formats:
/// - List: `['path1', 'path2']` → `read_parquet([...], union_by_name=true)`
/// - JSON/ndjson file: `*.json` or `*.ndjson` → `read_json(...)`
/// - Parquet glob: everything else → `read_parquet('...', union_by_name=true)`
fn build_reader(source: &str) -> Result<String, super::EmitError> {
    if source.starts_with('(') {
        // Raw SQL subquery — used by `from saved` resolution for run=all
        // UNION ALL sources with injected synthetic columns.
        Ok(source.to_string())
    } else if source.starts_with('[') {
        validate_source_list(source)?;
        Ok(format!("read_parquet({source}, union_by_name=true)"))
    } else {
        validate_source_path(source)?;
        let ext = std::path::Path::new(source)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if ext.eq_ignore_ascii_case("ndjson") || ext.eq_ignore_ascii_case("json") {
            // Explicit format params prevent DuckDB from inferring the ndjson
            // as a single JSON column. field_appearance_threshold=0 is critical:
            // the default (0.1) causes DuckDB to fall back to MAP(VARCHAR, JSON)
            // when events have heterogeneous schemas where most fields appear in
            // less than 10% of records (e.g. internal telemetry with 64 keys
            // mixed with external events that only have 5 keys).
            // Schema detection stays on DuckDB's bounded default sample — see
            // `hot_reader` below for why whole-file detection is not the way to
            // keep sparse columns alive.
            Ok(format!(
                "read_json('{source}', format='newline_delimited', records=true, \
                 auto_detect=true, field_appearance_threshold=0)"
            ))
        } else {
            Ok(format!("read_parquet('{source}', union_by_name=true)"))
        }
    }
}

/// Build the `read_json` reader expression for a hot-buffer ndjson snapshot.
///
/// `field_appearance_threshold=0` prevents `DuckDB` from collapsing
/// heterogeneous-schema events into a single MAP column.
///
/// Schema detection deliberately keeps `DuckDB`'s bounded default sample.
/// A sparse column — `_repairs`, which only repaired events carry
/// (ADR-0008) — first appearing past that prefix throws an `unknown key`
/// error for the whole query, but `sample_size=-1` is the wrong cure: it
/// re-parses the entire snapshot on *every* query and SSE poll (~2.7x the
/// read cost, and it grows with the buffer). The snapshot writer instead
/// hoists one event per novel key to the front of the file, so the whole key
/// set is inside the prefix (see `HotBuffer::build_snapshot`).
fn hot_reader(hot: &str) -> Result<String, super::EmitError> {
    validate_source_path(hot)?;
    Ok(format!(
        "read_json('{hot}', format='newline_delimited', records=true, \
         auto_detect=true, field_appearance_threshold=0)"
    ))
}

/// Conform expression for one pinned field on the hot branch, applied
/// UNTYPED — the emitter has no `DESCRIBE`, so the expression must be valid
/// whatever type `read_json` inferred for the column.
///
/// Typed pins (`BIGINT`/`DOUBLE`/`TIMESTAMP`/`BOOLEAN`) use `TRY_CAST`:
/// a nonconforming hot value degrades to NULL instead of throwing the
/// hot+cold union (ADR-0008). The VARCHAR pin uses
/// `json_extract_string(to_json(x), '$')`, which yields UNQUOTED strings
/// over every inference class the snapshot can produce (VARCHAR, JSON from
/// mixed values, BIGINT) — probed by execution in
/// `trawl-engine/tests/duckdb_probe.rs`; a plain `CAST(x AS VARCHAR)` on a
/// JSON-inferred column would keep the quotes.
fn conform_untyped(quoted: &str, pin: crate::schema::CanonicalType) -> String {
    use crate::schema::CanonicalType;
    match pin {
        CanonicalType::Varchar => format!("json_extract_string(to_json({quoted}), '$')"),
        typed => format!("TRY_CAST({quoted} AS {})", typed.as_duckdb()),
    }
}

/// Fold catalog pins onto the quoted identifiers they actually name,
/// yielding at most one `REPLACE` entry per hot column.
///
/// Catalog field names are case-SENSITIVE (a postgres TEXT primary key, fed
/// straight from client JSON keys, never case-normalised at ingest) while
/// `DuckDB` identifiers are case-INSENSITIVE. So `Status` from one service
/// and `status` from another are two pins naming ONE hot column, and a
/// `REPLACE` list carrying both is `Parser Error: Duplicate entry "status"`
/// — which, with any cold parquet present, fails EVERY query and SSE poll
/// for as long as such events sit in the buffer. The same collision reaches
/// here through [`super::fields::quote_field`]'s alias mapping, so the fold
/// keys on the mapped, quoted name rather than the catalog's.
///
/// `DuckDB`'s identifier comparison is ASCII-only (`café` and `CAFÉ` stay
/// distinct columns — probed in `trawl-engine/tests/duckdb_probe.rs`), so
/// the fold uses ASCII case only and collapses exactly what `DuckDB` would.
///
/// Colliding pins that agree on a type keep it; pins that disagree degrade
/// to `VARCHAR`, the lossless conform ([`conform_untyped`] stringifies every
/// inference class) — picking either typed pin instead would `TRY_CAST` the
/// other variant's values to NULL. Pins colliding with a `TIMESTAMP_COLUMNS`
/// entry drop out entirely: those already carry an unconditional `TRY_CAST`.
///
/// This is the last line of defence, not the policy: a caller that knows the
/// hot source's key set drops colliding pins BEFORE emit (the server's
/// `FieldCatalog::intersect`), because a snapshot carrying both spellings is
/// read as `x` + `x_1` and any surviving entry would conform whichever
/// column `DuckDB` binds first — not necessarily the pinned field's own.
/// The fold keeps catalog-less and key-set-less callers out of the parse
/// error regardless.
///
/// Returned in identifier order, so the emitted SQL stays deterministic.
fn fold_case_variants(
    pins: &crate::schema::FieldTypes,
) -> Vec<(String, crate::schema::CanonicalType)> {
    use std::collections::BTreeMap;

    let timestamps: Vec<String> = crate::schema::TIMESTAMP_COLUMNS
        .iter()
        .map(|col| super::fields::quote_field(col).to_ascii_uppercase())
        .collect();

    let mut folded: BTreeMap<String, (String, crate::schema::CanonicalType)> = BTreeMap::new();
    for (field, ty) in pins.iter() {
        let quoted = super::fields::quote_field(field);
        let key = quoted.to_ascii_uppercase();
        if timestamps.contains(&key) {
            continue;
        }
        folded
            .entry(key)
            .and_modify(|slot| {
                if slot.1 != ty {
                    slot.1 = crate::schema::CanonicalType::Varchar;
                }
            })
            .or_insert((quoted, ty));
    }
    folded.into_values().collect()
}

/// Public accessor for the parquet/list source reader expression, so the
/// engine can `DESCRIBE` the same cold source the emitter reads from.
pub fn source_reader(source: &str) -> Result<String, super::EmitError> {
    build_reader(source)
}

/// Public accessor for the hot-buffer reader expression (see [`hot_reader`]).
pub fn hot_source_reader(hot: &str) -> Result<String, super::EmitError> {
    hot_reader(hot)
}

impl EmitterState {
    pub(crate) fn new(source: &str) -> Result<Self, super::EmitError> {
        let reader = build_reader(source)?;
        Ok(Self::with_source(reader))
    }

    /// Construct with a composite source that unions parquet with hot buffer ndjson.
    ///
    /// The hot source is read via `read_json` with explicit format parameters
    /// (`field_appearance_threshold=0` prevents `DuckDB` from collapsing
    /// heterogeneous-schema events into a single MAP column) and a
    /// `REPLACE` list that conforms the hot branch to the catalog:
    ///
    /// - both envelope TIMESTAMP columns get their unconditional `TRY_CAST`s
    ///   (ADR-0008 — survives empty `pins`, so catalog-less callers keep the
    ///   timestamp guarantee), and
    /// - every pinned field (excluding the timestamp columns, already
    ///   handled) gets its [`conform_untyped`] expression, so a hot value
    ///   that disagrees with the write-time pin degrades to NULL instead of
    ///   throwing the union.
    ///
    /// Pins are folded onto the identifiers they actually name first — see
    /// [`fold_case_variants`], without which two case-variant pins are a
    /// hard parse error on every query.
    ///
    /// The cold branch is deliberately plain: parquet is write-time
    /// conformant (ADR-0009 slice 2), and a defensive cold cast would mask
    /// a real invariant breach.
    pub(crate) fn with_hot_source(
        primary: &str,
        hot: &str,
        pins: &crate::schema::FieldTypes,
    ) -> Result<Self, super::EmitError> {
        let primary_reader = build_reader(primary)?;
        let hot_reader = hot_reader(hot)?;

        let mut parts = Vec::with_capacity(crate::schema::TIMESTAMP_COLUMNS.len() + pins.len());
        for col in crate::schema::TIMESTAMP_COLUMNS {
            parts.push(format!("TRY_CAST(\"{col}\" AS TIMESTAMP) AS \"{col}\""));
        }
        for (quoted, ty) in fold_case_variants(pins) {
            parts.push(format!("{} AS {quoted}", conform_untyped(&quoted, ty)));
        }
        let hot_replace = parts.join(", ");

        let composite = format!(
            "(SELECT * FROM {primary_reader} UNION ALL BY NAME \
             SELECT * REPLACE ({hot_replace}) FROM {hot_reader})"
        );
        Ok(Self::with_source(composite))
    }

    fn with_source(source: String) -> Self {
        Self {
            step: 0,
            source,
            select: Vec::new(),
            where_clauses: Vec::new(),
            group_by: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            has_aggregation: false,
            has_projection: false,
            had_explicit_columns: false,
            time_filter: None,
            sample: None,
            pivot: None,
            ctes: Vec::new(),
            params: Vec::new(),
            raw_binding: RawBinding::Available,
        }
    }

    /// Emit the raw-free variant of this query: text search binds a typed
    /// NULL instead of the `_raw` column.
    pub(crate) fn without_raw_column(mut self) -> Self {
        self.raw_binding = RawBinding::Suppressed;
        self
    }

    /// Whether text search bound the `_raw` column during this pass.
    pub(crate) fn bound_raw_column(&self) -> bool {
        self.raw_binding == RawBinding::Bound
    }

    /// The expression text search uses for the `_raw` side of its predicate.
    ///
    /// `_raw` is a server guarantee, not a guarantee of every source a query
    /// can be pointed at: user-owned parquet read in embedded mode has no
    /// such column, and binding it there fails the whole query. The raw-free
    /// pass substitutes a typed NULL, which under the predicate's
    /// three-valued logic degrades bare-word search to `message` alone —
    /// exactly what the in-memory filter does for an event without `_raw`
    /// (ADR-0009: bare search covers `_raw` *where present*).
    pub(crate) fn raw_column(&mut self) -> &'static str {
        match self.raw_binding {
            RawBinding::Suppressed => "NULL::VARCHAR",
            RawBinding::Available | RawBinding::Bound => {
                self.raw_binding = RawBinding::Bound;
                "\"_raw\""
            }
        }
    }

    /// Push a parameter value and return the `?` placeholder string.
    pub(crate) fn push_param(&mut self, val: SqlValue) -> String {
        self.params.push(val);
        "?".to_string()
    }

    /// Append a WHERE clause fragment (will be AND-joined with others).
    pub(crate) fn push_where(&mut self, clause: String) {
        self.where_clauses.push(clause);
    }

    /// Run a closure that may push WHERE clauses, capturing them separately.
    ///
    /// Swaps the WHERE buffer, runs the closure, then restores the original —
    /// the restore happens even when the closure fails, so a failed emission
    /// never leaks half-built clauses into the caller's buffer. Parameters
    /// pushed during the closure are kept (ordering is preserved). Used by
    /// multi-group (OR) search emission to collect per-group clauses.
    pub(crate) fn collect_where_clauses(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<(), super::EmitError>,
    ) -> Result<Vec<String>, super::EmitError> {
        let original = std::mem::take(&mut self.where_clauses);
        let outcome = f(self);
        let collected = std::mem::replace(&mut self.where_clauses, original);
        outcome.map(|()| collected)
    }

    /// Conditionally flush the current state to a CTE based on the given condition.
    pub(crate) fn flush_if(&mut self, condition: FlushCondition) {
        let should_flush = match condition {
            FlushCondition::IfModified => self.has_aggregation || self.has_projection,
            FlushCondition::IfModifiedOrOrdered => {
                self.has_aggregation || self.has_projection || !self.order_by.is_empty()
            }
            FlushCondition::IfModifiedOrLimited => {
                self.has_aggregation || self.has_projection || self.limit.is_some()
            }
            FlushCondition::Always => true,
        };
        if should_flush {
            self.flush_to_cte();
        }
    }

    /// Flush the current state into a CTE and reset for the next stage.
    pub(crate) fn flush_to_cte(&mut self) {
        let cte_name = format!("_s{}", self.step);
        let sql = self.build_select();

        self.ctes.push(Cte {
            name: cte_name.clone(),
            sql,
        });

        // reset for next stage — the new source is the CTE we just created
        self.step += 1;
        self.source = format!("\"{cte_name}\"");
        self.select.clear();
        self.where_clauses.clear();
        self.group_by.clear();
        self.order_by.clear();
        self.limit = None;
        self.has_aggregation = false;
        self.has_projection = false;
        self.sample = None;
    }

    /// Build a SELECT statement from the current accumulated state.
    fn build_select(&self) -> String {
        let mut sql = String::new();

        // SELECT
        if self.select.is_empty() {
            sql.push_str("SELECT *");
        } else {
            sql.push_str("SELECT ");
            sql.push_str(&self.select.join(", "));
        }

        // FROM
        sql.push_str("\nFROM ");
        sql.push_str(&self.source);

        // USING SAMPLE (between FROM and WHERE)
        if let Some(ref sample) = self.sample {
            sql.push('\n');
            sql.push_str(sample);
        }

        // WHERE
        if !self.where_clauses.is_empty() {
            sql.push_str("\nWHERE ");
            sql.push_str(&self.where_clauses.join(" AND "));
        }

        // GROUP BY
        if !self.group_by.is_empty() {
            sql.push_str("\nGROUP BY ");
            sql.push_str(&self.group_by.join(", "));
        }

        // ORDER BY
        if !self.order_by.is_empty() {
            sql.push_str("\nORDER BY ");
            sql.push_str(&self.order_by.join(", "));
        }

        // LIMIT
        if let Some(limit) = self.limit {
            let _ = write!(sql, "\nLIMIT {limit}");
        }

        sql
    }

    /// Set pivot mode — overrides normal SELECT in finalize.
    pub(crate) fn set_pivot(&mut self, agg_sql: String, on_field: String, by_fields: Vec<String>) {
        self.pivot = Some(PivotSpec {
            agg_sql,
            on_field,
            by_fields,
        });
    }

    /// Whether the state currently has a pending pivot.
    pub(crate) fn has_pivot(&self) -> bool {
        self.pivot.is_some()
    }

    /// Flush a pending PIVOT to a CTE so that downstream stages can operate
    /// on the pivot-generated columns.
    ///
    /// `DuckDB` doesn't support parameterized PIVOT, so we inline all
    /// accumulated `?` placeholders across all CTEs and the PIVOT body,
    /// then clear the param list. Subsequent stages can add fresh `?`
    /// params as normal.
    pub(crate) fn flush_pivot_to_cte(&mut self) {
        let Some(pivot) = self.pivot.take() else {
            return;
        };

        let pivot_sql = self.build_pivot(&pivot);

        // Inline params sequentially across all CTEs + the pivot body.
        // DuckDB binds ? left-to-right across the entire statement, so
        // we must process them in the same order.
        let mut param_idx = 0;
        for cte in &mut self.ctes {
            let (inlined, consumed) =
                Self::inline_params_counted(&cte.sql, &self.params, param_idx);
            cte.sql = inlined;
            param_idx = consumed;
        }
        let (pivot_inlined, _) = Self::inline_params_counted(&pivot_sql, &self.params, param_idx);
        self.params.clear();

        // Push the pivot as a new CTE.
        let cte_name = format!("_s{}", self.step);
        self.ctes.push(Cte {
            name: cte_name.clone(),
            sql: pivot_inlined,
        });

        // Reset for downstream stages.
        self.step += 1;
        self.source = format!("\"{cte_name}\"");
        self.select.clear();
        self.where_clauses.clear();
        self.group_by.clear();
        self.order_by.clear();
        self.limit = None;
        self.has_aggregation = false;
        self.has_projection = false;
    }

    /// Produce the final SQL string including any accumulated CTEs.
    ///
    /// For PIVOT queries, inlines all `?` params directly into the SQL
    /// and clears the param list — `DuckDB` doesn't support parameterized
    /// PIVOT statements.
    pub(crate) fn finalize(&mut self) -> String {
        let body = if let Some(pivot) = &self.pivot {
            self.build_pivot(pivot)
        } else {
            self.build_select()
        };

        if self.ctes.is_empty() {
            return body;
        }

        let mut sql = String::from("WITH ");
        for (i, cte) in self.ctes.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&cte.name);
            sql.push_str(" AS (\n");
            for line in cte.sql.lines() {
                sql.push_str("  ");
                sql.push_str(line);
                sql.push('\n');
            }
            sql.push(')');
        }
        sql.push('\n');
        sql.push_str(&body);

        if self.pivot.is_some() {
            let inlined = Self::inline_params(&sql, &self.params);
            self.params.clear();
            return inlined;
        }

        sql
    }

    /// Build a `DuckDB` PIVOT statement from the current source.
    fn build_pivot(&self, pivot: &PivotSpec) -> String {
        let on_field = super::fields::quote_field(&pivot.on_field);
        let mut sql = format!(
            "PIVOT {}\nON {on_field}\nUSING {}",
            self.source, pivot.agg_sql
        );

        if !pivot.by_fields.is_empty() {
            let quoted: Vec<String> = pivot
                .by_fields
                .iter()
                .map(|f| super::fields::quote_field(f))
                .collect();
            let _ = write!(sql, "\nGROUP BY {}", quoted.join(", "));
        }

        sql
    }

    /// Replace `?` placeholders with literal values for engines that
    /// don't support parameterized queries (e.g. `DuckDB` PIVOT).
    fn inline_params(sql: &str, params: &[SqlValue]) -> String {
        Self::inline_params_counted(sql, params, 0).0
    }

    /// Replace `?` placeholders starting from `start_idx` in the params slice.
    /// Returns the inlined SQL and the next param index (for chaining across
    /// multiple SQL fragments).
    fn inline_params_counted(sql: &str, params: &[SqlValue], start_idx: usize) -> (String, usize) {
        let mut result = String::with_capacity(sql.len());
        let mut param_idx = start_idx;
        for ch in sql.chars() {
            if ch == '?' && param_idx < params.len() {
                match &params[param_idx] {
                    SqlValue::String(s) => {
                        result.push('\'');
                        // escape single quotes by doubling them
                        result.push_str(&s.replace('\'', "''"));
                        result.push('\'');
                    }
                    SqlValue::Int(i) => {
                        let _ = write!(result, "{i}");
                    }
                    SqlValue::Float(f) => {
                        let _ = write!(result, "{f}");
                    }
                    SqlValue::Bool(b) => {
                        result.push_str(if *b { "TRUE" } else { "FALSE" });
                    }
                }
                param_idx += 1;
            } else {
                result.push(ch);
            }
        }
        (result, param_idx)
    }

    /// Consume the state and return the accumulated parameters.
    pub(crate) fn into_params(self) -> Vec<SqlValue> {
        self.params
    }

    /// Whether the result columns should be reordered to put well-known fields first.
    ///
    /// Returns `true` when no stage in the pipeline defined a complete output
    /// column set (table/fields, stats, top, rare, timechart, pivot).
    pub(crate) fn needs_column_reorder(&self) -> bool {
        !self.had_explicit_columns
    }
}
