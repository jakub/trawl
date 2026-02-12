use std::fmt::Write as _;

use crate::ast::FleetDuration;

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
    /// The time filter from the search stage, used by `timechart` auto-bucketing.
    /// Not reset on CTE flush — this is query-wide context.
    pub(crate) time_filter: Option<FleetDuration>,
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
    if !source
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/_.*?{}[]-~".contains(&b))
    {
        return Err(super::EmitError::UnsupportedOperation {
            message: format!("source path contains invalid characters: {source}"),
        });
    }
    Ok(())
}

impl EmitterState {
    pub(crate) fn new(source: &str) -> Result<Self, super::EmitError> {
        validate_source_path(source)?;

        let ext = std::path::Path::new(source)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let reader = if ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("ndjson") {
            format!("read_json_auto('{source}')")
        } else {
            format!("read_parquet('{source}')")
        };
        Ok(Self {
            step: 0,
            source: reader,
            select: Vec::new(),
            where_clauses: Vec::new(),
            group_by: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            has_aggregation: false,
            has_projection: false,
            time_filter: None,
            pivot: None,
            ctes: Vec::new(),
            params: Vec::new(),
        })
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

    /// Produce the final SQL string including any accumulated CTEs.
    pub(crate) fn finalize(&self) -> String {
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

    /// Consume the state and return the accumulated parameters.
    pub(crate) fn into_params(self) -> Vec<SqlValue> {
        self.params
    }
}
