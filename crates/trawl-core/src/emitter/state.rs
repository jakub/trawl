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
    if source.starts_with('[') {
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
            Ok(format!(
                "read_json('{source}', format='newline_delimited', records=true, \
                 auto_detect=true, field_appearance_threshold=0)"
            ))
        } else {
            Ok(format!("read_parquet('{source}', union_by_name=true)"))
        }
    }
}

impl EmitterState {
    pub(crate) fn new(source: &str) -> Result<Self, super::EmitError> {
        let reader = build_reader(source)?;
        Ok(Self::with_source(reader))
    }

    /// Construct with a composite source that unions parquet with hot buffer ndjson.
    ///
    /// The hot source is read via `read_json` with explicit format parameters
    /// and a CAST on the timestamp column to match parquet's TIMESTAMP type.
    /// `field_appearance_threshold=0` prevents `DuckDB` from collapsing
    /// heterogeneous-schema events into a single MAP column.
    pub(crate) fn with_hot_source(primary: &str, hot: &str) -> Result<Self, super::EmitError> {
        validate_source_path(hot)?;
        let primary_reader = build_reader(primary)?;
        let composite = format!(
            "(SELECT * FROM {primary_reader} UNION ALL BY NAME \
             SELECT * REPLACE (CAST(\"timestamp\" AS TIMESTAMP) AS \"timestamp\") \
             FROM read_json('{hot}', format='newline_delimited', records=true, \
             auto_detect=true, field_appearance_threshold=0))"
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
    /// Swaps the WHERE buffer, runs the closure, then restores the original.
    /// Parameters pushed during the closure are kept (ordering is preserved).
    /// Used by multi-group (OR) search emission to collect per-group clauses.
    pub(crate) fn collect_where_clauses(&mut self, f: impl FnOnce(&mut Self)) -> Vec<String> {
        let original = std::mem::take(&mut self.where_clauses);
        f(self);
        std::mem::replace(&mut self.where_clauses, original)
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
