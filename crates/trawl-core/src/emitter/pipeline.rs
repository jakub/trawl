use crate::ast::{ExtractMode, FleetDuration, PipeStage, SortDirection};

use super::EmitError;
use super::SqlValue;
use super::expr::emit_expr;
use super::fields::quote_field;
use super::functions::{default_agg_alias, translate_function};
use super::state::{EmitterState, FlushCondition};

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
        let arg_strings: Vec<String> = agg
            .args
            .iter()
            .map(|a| emit_expr(a, ctx))
            .collect::<Result<_, _>>()?;

        let sql_func = translate_function(&agg.function, &arg_strings)?;

        // determine alias
        let first_arg_name = extract_field_name(agg.args.first());
        let alias = match &agg.alias {
            Some(a) => quote_field(a),
            None => default_agg_alias(&agg.function, first_arg_name.as_deref()),
        };

        select_items.push(format!("{sql_func} AS {alias}"));
    }

    ctx.select = select_items;
    ctx.has_aggregation = true;

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
}

/// Try to extract a field name from the first arg of an aggregation.
///
/// Recurses through wrapping expressions (function calls, binary ops, unary ops)
/// to find the innermost field reference. This lets `avg(tonumber(rssi) * -1)`
/// alias to `avg_rssi` instead of just `avg`.
fn extract_field_name(arg: Option<&crate::ast::Spanned<crate::ast::Expr>>) -> Option<String> {
    use crate::ast::Expr;
    let expr = &arg?.node;
    match expr {
        Expr::FieldRef(name) => Some(name.clone()),
        Expr::FunctionCall { args, .. } => extract_field_name(args.first()),
        Expr::Binary { lhs, .. } => extract_field_name(Some(lhs)),
        Expr::Unary { operand, .. } => extract_field_name(Some(operand)),
        _ => None,
    }
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

fn process_let(let_stage: &crate::ast::LetStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    // always flush: the computed expression may add ? params to SELECT,
    // which must not interleave with WHERE params from prior stages
    ctx.flush_if(FlushCondition::Always);

    let mut not_in_values = Vec::new();
    let mut computed = Vec::new();
    for (field, expr) in &let_stage.assignments {
        let expr_sql = emit_expr(expr, ctx)?;
        let alias = quote_field(field);
        // Escape single quotes for the COLUMNS lambda string comparison.
        let escaped = field.replace('\'', "''");
        not_in_values.push(format!("'{escaped}'"));
        computed.push(format!("({expr_sql}) AS {alias}"));
    }

    // Use COLUMNS lambda to filter out columns being overridden. Unlike
    // `* EXCLUDE (...)`, this tolerates missing columns — crucial for
    // `let a = expr` when `a` is a new computed field, not an override.
    let not_in_list = not_in_values.join(", ");
    let mut items = vec![format!("COLUMNS(c -> c NOT IN ({not_in_list}))")];
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

            let mut select_items = vec!["*".to_string()];
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
                 CAST(\"timestamp\" AS TIMESTAMP) DESC) AS \"_rn\""
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

    let bucket_expr = format!(
        "time_bucket(INTERVAL '{interval}', CAST(\"timestamp\" AS TIMESTAMP)) AS \"_time\""
    );

    let mut select_items = vec![bucket_expr];
    let mut group_items = vec!["\"_time\"".to_string()];

    for field in &tc.group_by {
        let q = quote_field(field);
        select_items.push(q.clone());
        group_items.push(q);
    }

    for agg in &tc.aggregations {
        let arg_strings: Vec<String> = agg
            .args
            .iter()
            .map(|a| emit_expr(a, ctx))
            .collect::<Result<_, _>>()?;

        let sql_func = translate_function(&agg.function, &arg_strings)?;

        let first_arg_name = extract_field_name(agg.args.first());
        let alias = match &agg.alias {
            Some(a) => quote_field(a),
            None => default_agg_alias(&agg.function, first_arg_name.as_deref()),
        };

        select_items.push(format!("{sql_func} AS {alias}"));
    }

    ctx.select = select_items;
    ctx.group_by = group_items;
    ctx.order_by.push("\"_time\" ASC".to_string());
    ctx.has_aggregation = true;

    Ok(())
}

/// Auto-bucketing heuristic: map time filter duration to a reasonable bucket span.
fn auto_bucket_interval(time_filter: Option<&crate::ast::FleetDuration>) -> String {
    let seconds = time_filter.map_or(3600, FleetDuration::to_seconds);

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
        "1 hours"
    }
    .to_string()
}

fn process_tail(tail: &crate::ast::TailStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModifiedOrLimited);

    // if no explicit sort exists, default to timestamp DESC so "tail"
    // means "last N chronologically". otherwise, respect the existing
    // sort order and just limit.
    if ctx.order_by.is_empty() {
        ctx.order_by.push("\"timestamp\" DESC".to_string());
    }
    ctx.limit = Some(tail.count);
}

fn process_rename(rename: &crate::ast::RenameStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModified);

    let excluded: Vec<String> = rename
        .renames
        .iter()
        .map(|(old, _)| quote_field(old))
        .collect();
    let aliases: Vec<String> = rename
        .renames
        .iter()
        .map(|(old, new)| format!("{} AS {}", quote_field(old), quote_field(new)))
        .collect();

    ctx.select = vec![
        format!("* EXCLUDE ({})", excluded.join(", ")),
        aliases.join(", "),
    ];
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

    let arg_strings: Vec<String> = pivot
        .aggregation
        .args
        .iter()
        .map(|a| emit_expr(a, ctx))
        .collect::<Result<_, _>>()?;

    let agg_sql = translate_function(&pivot.aggregation.function, &arg_strings)?;

    ctx.set_pivot(agg_sql, pivot.on_field.clone(), pivot.by.clone());

    Ok(())
}
