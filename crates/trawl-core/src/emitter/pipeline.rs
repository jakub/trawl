// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::ast::{
    EventStatsStage, ExtractMode, PipeStage, SampleMode, SortDirection, TrawlDuration,
};

use super::EmitError;
use super::SqlValue;
use super::expr::{emit_call_args, emit_expr};
use super::fields::quote_field;
use super::functions::{default_agg_alias, translate_function};
use super::state::{EmitterState, FlushCondition};
use crate::schema::catalog_key;

/// `DuckDB` SQL that ASCII-folds the `COLUMNS` lambda's column name `c` —
/// the Rust-side [`catalog_key`] rendered as an expression.
///
/// `translate` maps character by character and leaves everything outside
/// `A-Z` alone, so it reproduces `DuckDB`'s own identifier folding exactly
/// (`CAFÉ` → `cafÉ`), which `lower()` does not.
const ASCII_FOLD_SQL: &str =
    "translate(c, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz')";

/// Process a single pipe stage, mutating the emitter state.
pub(crate) fn process_stage(pipe: &PipeStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    match pipe {
        PipeStage::Stats(s) => process_stats(s, ctx),
        PipeStage::Where(w) => process_where(w, ctx),
        PipeStage::Sort(s) => {
            process_sort(s, ctx);
            Ok(())
        }
        PipeStage::Limit(l) => {
            process_limit(l, ctx);
            Ok(())
        }
        PipeStage::Table(t) => {
            process_table(t, ctx);
            Ok(())
        }
        PipeStage::Top(t) => {
            process_top(t, ctx);
            Ok(())
        }
        PipeStage::Rare(r) => {
            process_rare(r, ctx);
            Ok(())
        }
        PipeStage::Drop(d) => {
            process_drop(d, ctx);
            Ok(())
        }
        PipeStage::Let(l) => process_let(l, ctx),
        PipeStage::Extract(e) => process_extract(e, ctx),
        PipeStage::Dedup(d) => {
            process_dedup(d, ctx);
            Ok(())
        }
        PipeStage::Timechart(t) => process_timechart(t, ctx),
        PipeStage::Pivot(p) => process_pivot(p, ctx),
        PipeStage::Tail(t) => {
            process_tail(t, ctx);
            Ok(())
        }
        PipeStage::Rename(r) => {
            process_rename(r, ctx);
            Ok(())
        }
        PipeStage::Sample(s) => {
            process_sample(&s.mode, ctx);
            Ok(())
        }
        PipeStage::EventStats(es) => process_eventstats(es, ctx),
        PipeStage::FromSaved(_) => Err(EmitError::UnsupportedOperation {
            message: "'from saved' must be resolved before SQL emission".to_string(),
        }),
    }
}

fn process_stats(
    agg_stage: &crate::ast::StatsStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::IfModified);

    let mut select_items = Vec::new();

    // group-by fields go first in SELECT and GROUP BY
    for field in &agg_stage.group_by {
        let quoted = quote_field(field);
        select_items.push(quoted.clone());
        ctx.group_by.push(quoted);
    }

    // aggregation expressions
    for agg in &agg_stage.aggregations {
        // The SAME argument walk the expression lane uses: an
        // aggregation position is still a call, and its per-position
        // literal rules (`sev()`'s dialect) apply there too.
        let arg_strings = emit_call_args(&agg.function, &agg.args, ctx)?;

        let sql_func = translate_function(&agg.function, &arg_strings)?;

        // determine alias
        let alias = match &agg.alias {
            Some(a) => quote_field(a),
            None => default_agg_alias(agg),
        };

        select_items.push(format!("{sql_func} AS {alias}"));
    }

    ctx.select = select_items;
    ctx.has_aggregation = true;
    ctx.had_explicit_columns = true;

    Ok(())
}

fn process_where(
    where_stage: &crate::ast::WhereStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::IfModified);

    let clause = emit_expr(&where_stage.condition, ctx)?;
    ctx.push_where(clause);

    Ok(())
}

fn process_sort(sort_stage: &crate::ast::SortStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModifiedOrOrdered);

    for field in &sort_stage.fields {
        let quoted = quote_field(&field.field);
        let dir = match field.direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        ctx.order_by.push(format!("{quoted} {dir}"));
    }
}

fn process_limit(limit_stage: &crate::ast::LimitStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModifiedOrLimited);

    ctx.limit = Some(limit_stage.count);
}

fn process_table(table_stage: &crate::ast::TableStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModified);

    ctx.select = table_stage.fields.iter().map(|f| quote_field(f)).collect();
    ctx.has_projection = true;
    ctx.had_explicit_columns = true;
}

/// Desugar `top N field` → stats `count()` by field | sort -count | limit N.
fn process_top(top: &crate::ast::TopStage, ctx: &mut EmitterState) {
    process_frequency(top.count, &top.field, &top.by, "DESC", ctx);
}

/// Desugar `rare N field` → stats `count()` by field | sort count | limit N.
fn process_rare(rare: &crate::ast::RareStage, ctx: &mut EmitterState) {
    process_frequency(rare.count, &rare.field, &rare.by, "ASC", ctx);
}

/// Shared logic for `top` and `rare` — frequency analysis desugaring.
fn process_frequency(
    count: u64,
    field: &str,
    by: &[String],
    sort_dir: &str,
    ctx: &mut EmitterState,
) {
    ctx.flush_if(FlushCondition::IfModified);

    let field_quoted = quote_field(field);
    let mut select_items = vec![field_quoted.clone()];
    let mut group_items = vec![field_quoted];

    for by_field in by {
        let q = quote_field(by_field);
        select_items.push(q.clone());
        group_items.push(q);
    }

    select_items.push("COUNT(*) AS \"count\"".to_string());

    ctx.select = select_items;
    ctx.group_by = group_items;
    ctx.has_aggregation = true;
    ctx.had_explicit_columns = true;

    // flush aggregation to CTE, then sort+limit on the result
    ctx.flush_to_cte();
    ctx.order_by.push(format!("\"count\" {sort_dir}"));
    ctx.limit = Some(count);
}

fn process_drop(drop_stage: &crate::ast::DropStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModified);

    let excluded: Vec<String> = drop_stage.fields.iter().map(|f| quote_field(f)).collect();
    ctx.select = vec![format!("* EXCLUDE ({})", excluded.join(", "))];
    ctx.has_projection = true;
}

/// The wildcard half of an OVERWRITING projection: every incoming column
/// except the ones the stage is about to project under its own aliases.
///
/// Unlike `* EXCLUDE (...)` the lambda tolerates a name no incoming
/// column carries — crucial for `let a = expr` where `a` is brand new.
/// It folds BOTH sides because `DuckDB` binds identifiers
/// case-insensitively and [`crate::projection::check_projection`] folds
/// every output name: a raw `c NOT IN ('Host')` would leave an incoming
/// `host` in place beside the new `Host` and hand back two columns of one
/// folded name.
///
/// The fold is ASCII-ONLY on both sides — [`ASCII_FOLD_SQL`] on the column
/// name, [`catalog_key`] on the literal — because that is the fold
/// `DuckDB`'s identifier equality performs. `lower()` folds Unicode too,
/// so over a corpus carrying `ü` the backtickable target `` `Ü` `` would
/// have deleted the `ü` column the query never named, while `DuckDB`
/// itself keeps the two apart.
fn columns_excluding(names: &[String]) -> String {
    let list = names
        .iter()
        // Escape single quotes for the COLUMNS lambda string comparison.
        .map(|n| format!("'{}'", catalog_key(n).replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("COLUMNS(c -> {ASCII_FOLD_SQL} NOT IN ({list}))")
}

fn process_let(let_stage: &crate::ast::LetStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    // always flush: the computed expression may add ? params to SELECT,
    // which must not interleave with WHERE params from prior stages
    ctx.flush_if(FlushCondition::Always);

    let mut targets = Vec::new();
    let mut computed = Vec::new();
    for (field, expr) in &let_stage.assignments {
        let expr_sql = emit_expr(expr, ctx)?;
        let alias = quote_field(field);
        targets.push(field.clone());
        computed.push(format!("({expr_sql}) AS {alias}"));
    }

    let mut items = vec![columns_excluding(&targets)];
    items.extend(computed);
    ctx.select = items;
    ctx.has_projection = true;

    Ok(())
}

fn process_extract(
    extract: &crate::ast::ExtractStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    // always flush: regexp_extract params in SELECT must not interleave
    // with WHERE params from prior stages (DuckDB binds ? left to right)
    ctx.flush_if(FlushCondition::Always);

    let source = match &extract.source_field {
        Some(f) => quote_field(f),
        None => quote_field("message"),
    };

    match &extract.mode {
        ExtractMode::Regex(pattern) => {
            let re = regex::Regex::new(pattern).map_err(|e| EmitError::UnsupportedOperation {
                message: format!("invalid regex in extract: {e}"),
            })?;

            let group_names: Vec<&str> = re.capture_names().flatten().collect();

            if group_names.is_empty() {
                return Err(EmitError::UnsupportedOperation {
                    message: "extract regex must contain at least one named capture group \
                              (?P<name>...)"
                        .to_string(),
                });
            }

            // A projection write owns its folded name. Exclude any input
            // spelling of every capture before adding the capture aliases.
            let owned: Vec<String> = group_names.iter().map(|name| (*name).to_string()).collect();
            let mut select_items = vec![columns_excluding(&owned)];
            for (i, name) in group_names.iter().enumerate() {
                let group_idx = i + 1;
                let alias = quote_field(name);
                // Each regexp_extract() call needs its own ? placeholder —
                // DuckDB binds ? sequentially, so N groups need N params.
                // nullif wraps the result so non-matches return NULL instead of ''.
                let placeholder = ctx.push_param(SqlValue::String(pattern.clone()));
                select_items.push(format!(
                    "nullif(regexp_extract({source}, {placeholder}, {group_idx}), '') AS {alias}"
                ));
            }

            ctx.select = select_items;
            ctx.has_projection = true;
        }
        ExtractMode::KeyValue { .. } => {
            // kv extraction can't be expressed as SQL (dynamic columns).
            // The emitter splits the pipeline at this point — this branch
            // should never be reached because emit_from_state collects kv
            // and all subsequent stages into rust_stages.
            return Err(EmitError::UnsupportedOperation {
                message: "extract kv reached SQL emitter (should have been split to rust_stages)"
                    .to_string(),
            });
        }
    }

    Ok(())
}

fn process_dedup(dedup: &crate::ast::DedupStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::Always);

    if dedup.fields.is_empty() {
        // bare `dedup` — exact row deduplication
        ctx.select = vec!["DISTINCT *".to_string()];
        ctx.has_projection = true;
    } else {
        // `dedup field, ...` — keep most recent row per unique field combination
        let partition_fields: Vec<String> = dedup.fields.iter().map(|f| quote_field(f)).collect();
        let partition_clause = partition_fields.join(", ");

        ctx.select = vec![
            "*".to_string(),
            format!(
                "ROW_NUMBER() OVER (PARTITION BY {partition_clause} ORDER BY \
                 TRY_CAST(\"_time\" AS TIMESTAMP) DESC) AS \"_rn\""
            ),
        ];
        ctx.has_projection = true;

        // flush the window function CTE
        ctx.flush_to_cte();

        // filter to keep only the first row per partition, drop _rn
        ctx.push_where("\"_rn\" = 1".to_string());
        ctx.select = vec!["* EXCLUDE (\"_rn\")".to_string()];
        ctx.has_projection = true;
    }
}

fn process_timechart(
    tc: &crate::ast::TimechartStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::IfModified);

    let interval = match &tc.span {
        Some(d) => d.to_interval_string(),
        None => auto_bucket_interval(ctx.time_filter.as_ref()),
    };

    // The bucket is aliased AS "_time", which is ALSO the name of the
    // physical event-time column it buckets. GROUP BY / ORDER BY must
    // therefore reference the full expression, not the name — a bare
    // "_time" would bind to the source column and silently break the
    // aggregation (one group per input row).
    let bucket = format!("time_bucket(INTERVAL '{interval}', TRY_CAST(\"_time\" AS TIMESTAMP))");

    let mut select_items = vec![format!("{bucket} AS \"_time\"")];
    let mut group_items = vec![bucket.clone()];

    for field in &tc.group_by {
        let q = quote_field(field);
        select_items.push(q.clone());
        group_items.push(q);
    }

    for agg in &tc.aggregations {
        // The SAME argument walk the expression lane uses: an
        // aggregation position is still a call, and its per-position
        // literal rules (`sev()`'s dialect) apply there too.
        let arg_strings = emit_call_args(&agg.function, &agg.args, ctx)?;

        let sql_func = translate_function(&agg.function, &arg_strings)?;

        let alias = match &agg.alias {
            Some(a) => quote_field(a),
            None => default_agg_alias(agg),
        };

        select_items.push(format!("{sql_func} AS {alias}"));
    }

    ctx.select = select_items;
    ctx.group_by = group_items;
    ctx.order_by.push(format!("{bucket} ASC"));
    ctx.has_aggregation = true;
    ctx.had_explicit_columns = true;

    Ok(())
}

/// Auto-bucketing heuristic: map time filter duration to a reasonable bucket span.
fn auto_bucket_interval(time_filter: Option<&crate::ast::TrawlDuration>) -> String {
    let seconds = time_filter.map_or(3600, TrawlDuration::to_seconds);

    if seconds <= 3600 {
        "1 minutes"
    } else if seconds <= 21_600 {
        "5 minutes"
    } else if seconds <= 86_400 {
        "15 minutes"
    } else if seconds <= 604_800 {
        "1 hours"
    } else if seconds <= 2_592_000 {
        "6 hours"
    } else {
        "1 days"
    }
    .to_string()
}

fn process_tail(tail: &crate::ast::TailStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModifiedOrLimited);

    // if no explicit sort exists, default to _time DESC so "tail"
    // means "last N chronologically". otherwise, respect the existing
    // sort order and just limit.
    if ctx.order_by.is_empty() {
        ctx.order_by.push("\"_time\" DESC".to_string());
    }
    ctx.limit = Some(tail.count);
}

fn process_rename(rename: &crate::ast::RenameStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModified);

    // Exclude both sources and targets through the folded COLUMNS predicate:
    // `rename a as B` over an existing `b` must leave one column, not a
    // DuckDB-deduplicated pair whose later binding differs from the live row.
    let excluded: Vec<String> = rename
        .renames
        .iter()
        .flat_map(|(old, new)| [old.clone(), new.clone()])
        .collect();
    let aliases: Vec<String> = rename
        .renames
        .iter()
        .map(|(old, new)| format!("{} AS {}", quote_field(old), quote_field(new)))
        .collect();

    ctx.select = vec![columns_excluding(&excluded), aliases.join(", ")];
    ctx.has_projection = true;
}

fn process_pivot(pivot: &crate::ast::PivotStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    ctx.flush_to_cte();

    // narrow source to only columns needed by PIVOT — without explicit GROUP BY,
    // DuckDB treats ALL non-ON columns as implicit row identifiers
    let mut needed = vec![quote_field(&pivot.on_field)];
    for f in &pivot.by {
        needed.push(quote_field(f));
    }
    for arg in &pivot.aggregation.args {
        if let crate::ast::Expr::FieldRef(name) = &arg.node {
            needed.push(quote_field(name));
        }
    }
    ctx.select = needed;
    ctx.has_projection = true;
    ctx.flush_to_cte();

    let arg_strings = emit_call_args(&pivot.aggregation.function, &pivot.aggregation.args, ctx)?;

    let agg_sql = translate_function(&pivot.aggregation.function, &arg_strings)?;

    ctx.set_pivot(agg_sql, pivot.on_field.clone(), pivot.by.clone());
    ctx.had_explicit_columns = true;

    Ok(())
}

fn process_sample(mode: &SampleMode, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModified);
    let clause = match mode {
        SampleMode::Percent(p) => format!("USING SAMPLE {p} PERCENT (bernoulli)"),
        SampleMode::Count(n) => format!("USING SAMPLE {n} ROWS (reservoir)"),
    };
    ctx.sample = Some(clause);
}

fn process_eventstats(stage: &EventStatsStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::Always);

    let partition = if stage.group_by.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = stage.group_by.iter().map(|f| quote_field(f)).collect();
        format!("PARTITION BY {}", parts.join(", "))
    };

    let mut targets = Vec::new();
    let mut computed = Vec::new();
    for agg in &stage.aggregations {
        // Reject functions not supported as window functions in DuckDB.
        if matches!(
            agg.function.as_str(),
            "dc" | "distinct_count" | "values" | "list"
        ) {
            return Err(EmitError::UnsupportedOperation {
                message: format!(
                    "{}() is not supported in eventstats (window functions do not support DISTINCT)",
                    agg.function
                ),
            });
        }

        // The SAME argument walk the expression lane uses: an
        // aggregation position is still a call, and its per-position
        // literal rules (`sev()`'s dialect) apply there too.
        let arg_strings = emit_call_args(&agg.function, &agg.args, ctx)?;
        let sql_func = translate_function(&agg.function, &arg_strings)?;
        // `eventstats` demands an explicit `as` (ADR-0013 ruling 8,
        // checked in `projection::check_projection`); the alias-less
        // name is defensive for a hand-built AST that skipped validation.
        let name = crate::projection::agg_output_name(agg);
        computed.push(format!(
            "{sql_func} OVER ({partition}) AS {}",
            quote_field(&name)
        ));
        targets.push(name);
    }

    // An alias naming an incoming column OVERWRITES it (documented,
    // `let`-like), so the wildcard must not also emit the original —
    // through the same case-folding lambda `let` uses.
    let mut items = if targets.is_empty() {
        vec!["*".to_string()]
    } else {
        vec![columns_excluding(&targets)]
    };
    items.extend(computed);

    ctx.select = items;
    ctx.has_projection = true;
    Ok(())
}
