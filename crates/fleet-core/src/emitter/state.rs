use std::fmt::Write as _;

use super::SqlValue;

/// A single CTE in the emitted SQL.
struct Cte {
    name: String,
    sql: String,
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
    ctes: Vec<Cte>,
    params: Vec<SqlValue>,
}

impl EmitterState {
    pub(crate) fn new(source: &str) -> Self {
        Self {
            step: 0,
            source: format!("read_parquet('{source}')"),
            select: Vec::new(),
            where_clauses: Vec::new(),
            group_by: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            has_aggregation: false,
            has_projection: false,
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

    /// Produce the final SQL string including any accumulated CTEs.
    pub(crate) fn finalize(&self) -> String {
        if self.ctes.is_empty() {
            return self.build_select();
        }

        let mut sql = String::from("WITH ");
        for (i, cte) in self.ctes.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&cte.name);
            sql.push_str(" AS (\n");
            // indent the CTE body
            for line in cte.sql.lines() {
                sql.push_str("  ");
                sql.push_str(line);
                sql.push('\n');
            }
            sql.push(')');
        }
        sql.push('\n');
        sql.push_str(&self.build_select());
        sql
    }

    /// Consume the state and return the accumulated parameters.
    pub(crate) fn into_params(self) -> Vec<SqlValue> {
        self.params
    }
}
