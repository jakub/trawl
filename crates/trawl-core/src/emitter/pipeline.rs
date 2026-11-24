// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::ast::{EventStatsStage, ExtractMode, PipeStage, SampleMode, SortDirection};

use super::expr::{emit_call_args, emit_expr};
use super::fields::{first_of_each, quote_field};
use super::functions::{default_agg_alias, translate_function};
use super::state::{EmitterState, FlushCondition, Use};
use super::validate::batch_extract;
use super::{EmitError, SqlValue};
use crate::schema::catalog_key;

const COLUMN: Use<'static> = Use::Column { alias: None };

/// `translate` maps character by character and leaves everything outside
/// `A-Z` alone, so it reproduces `DuckDB`'s own identifier folding exactly
/// (`CAFÉ` → `cafÉ`), which `lower()` does not.
const ASCII_FOLD_SQL: &str =
    "translate(c, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz')";

pub(crate) fn process_stage(pipe: &PipeStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    match pipe {
        PipeStage::Stats(s) => process_stats(s, ctx),
        PipeStage::Where(w) => process_where(w, ctx),
        PipeStage::Sort(s) => process_sort(s, ctx),
        PipeStage::Limit(l) => {
            process_limit(l, ctx);
            Ok(())
        }
        PipeStage::Table(t) => process_table(t, ctx),
        PipeStage::Top(t) => process_top(t, ctx),
        PipeStage::Rare(r) => process_rare(r, ctx),
        PipeStage::Drop(d) => {
            process_drop(d, ctx);
            Ok(())
        }
        PipeStage::Let(l) => process_let(l, ctx),
        PipeStage::Extract(e) => process_extract(e, ctx),
        PipeStage::Dedup(d) => process_dedup(d, ctx),
        PipeStage::Timechart(t) => process_timechart(t, ctx),
        PipeStage::Pivot(p) => process_pivot(p, ctx),
        PipeStage::Tail(t) => process_tail(t, ctx),
        PipeStage::Rename(r) => process_rename(r, ctx),
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
    // A pending ORDER BY or LIMIT belongs to the aggregation's input, and
    // SQL would apply both to its output, so they move into the CTE first,
    // whether or not anything else modified the state.
    ctx.flush_if(FlushCondition::IfModifiedOrderedOrLimited);

    // The aggregation expressions first, through the ordering guard (see
    // [`EmitterState::emit_ordered_select`]). The group-by items have to come
    // second because the guard may flush to a CTE, which clears `group_by`.
    let agg_items = ctx.emit_ordered_select(|ctx| {
        let mut items = Vec::with_capacity(agg_stage.aggregations.len());
        for agg in &agg_stage.aggregations {
            let arg_strings = emit_call_args(&agg.function, &agg.args, ctx)?;

            let sql_func = translate_function(&agg.function, &arg_strings, ctx)?;

            let alias = match &agg.alias {
                Some(a) => quote_field(a),
                None => default_agg_alias(agg),
            };

            items.push(format!("{sql_func} AS {alias}"));
        }
        Ok(items)
    })?;

    let mut select_items = Vec::new();

    for field in &agg_stage.group_by {
        select_items.push(ctx.read_field(field, COLUMN)?);
        let key = ctx.read_field(field, Use::Operand)?;
        ctx.group_by.push(key);
    }
    select_items.extend(agg_items);

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

fn process_sort(
    sort_stage: &crate::ast::SortStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::IfModifiedOrOrdered);

    for field in &sort_stage.fields {
        let key = ctx.read_field(&field.field, Use::Sort)?;
        let dir = match field.direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        ctx.order_by.push(format!("{key} {dir}"));
    }
    Ok(())
}

fn process_limit(limit_stage: &crate::ast::LimitStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModifiedOrLimited);

    ctx.limit = Some(limit_stage.count);
}

fn process_table(
    table_stage: &crate::ast::TableStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::IfModified);

    ctx.select = first_of_each(table_stage.fields.iter().map(String::as_str))
        .into_iter()
        .map(|f| ctx.read_field(f, COLUMN))
        .collect::<Result<_, _>>()?;
    ctx.has_projection = true;
    ctx.had_explicit_columns = true;
    Ok(())
}

fn process_top(top: &crate::ast::TopStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    process_frequency(top.count, &top.field, &top.by, "DESC", ctx)
}

fn process_rare(rare: &crate::ast::RareStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    process_frequency(rare.count, &rare.field, &rare.by, "ASC", ctx)
}

fn process_frequency(
    count: u64,
    field: &str,
    by: &[String],
    sort_dir: &str,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    // `top`/`rare` desugar to an aggregation, so they take the same
    // placement rule `stats`/`timechart` do.
    ctx.flush_if(FlushCondition::IfModifiedOrderedOrLimited);

    let mut select_items = Vec::with_capacity(by.len() + 2);
    let mut group_items = Vec::with_capacity(by.len() + 1);
    for key in std::iter::once(field).chain(by.iter().map(String::as_str)) {
        select_items.push(ctx.read_field(key, COLUMN)?);
        group_items.push(ctx.read_field(key, Use::Operand)?);
    }

    select_items.push("COUNT(*) AS \"count\"".to_string());

    ctx.select = select_items;
    ctx.group_by = group_items;
    ctx.has_aggregation = true;
    ctx.had_explicit_columns = true;

    ctx.flush_to_cte();
    ctx.order_by.push(format!("\"count\" {sort_dir}"));
    ctx.limit = Some(count);
    Ok(())
}

fn process_drop(drop_stage: &crate::ast::DropStage, ctx: &mut EmitterState) {
    ctx.flush_if(FlushCondition::IfModified);

    ctx.select = vec![columns_excluding(&drop_stage.fields)];
    ctx.has_projection = true;
}

/// Unlike `* EXCLUDE (...)` the lambda tolerates a name no incoming
/// column carries. It folds both sides because `DuckDB` binds identifiers
/// case-insensitively and [`crate::projection::check_projection`] folds
/// every output name: a raw `c NOT IN ('Host')` would leave an incoming
/// `host` in place beside the new `Host`.
fn columns_excluding(names: &[String]) -> String {
    let list = names
        .iter()
        .map(|n| format!("'{}'", catalog_key(n).replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("COLUMNS(c -> {ASCII_FOLD_SQL} NOT IN ({list}))")
}

fn process_let(let_stage: &crate::ast::LetStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    // always flush: the computed expression may add ? params to SELECT,
    // which must not interleave with WHERE params from prior stages
    ctx.flush_if(FlushCondition::Always);

    ctx.begin_let(
        let_stage
            .assignments
            .iter()
            .map(|(target, _)| target.as_str()),
    );
    let mut targets = Vec::new();
    let mut computed = Vec::new();
    for (field, expr) in &let_stage.assignments {
        let expr_sql = emit_expr(expr, ctx)?;
        ctx.assigned(field, &expr.node);
        let alias = quote_field(field);
        targets.push(field.clone());
        computed.push(format!("({expr_sql}) AS {alias}"));
    }
    ctx.end_let();

    let mut items = vec![columns_excluding(&targets)];
    items.extend(computed);
    ctx.select = items;
    ctx.has_projection = true;

    Ok(())
}

const EXTRACT_STRUCT: &str = "\"_extract\"";

fn process_extract(
    extract: &crate::ast::ExtractStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    // always flush: regexp_extract params in SELECT must not interleave
    // with WHERE params from prior stages (DuckDB binds ? left to right)
    ctx.flush_if(FlushCondition::Always);

    let source = ctx.read_field(
        extract
            .source_field
            .as_deref()
            .unwrap_or(crate::schema::MESSAGE),
        Use::Operand,
    )?;

    match &extract.mode {
        ExtractMode::Regex(pattern) => {
            let batch = batch_extract(pattern)?;

            // DuckDB's struct form wants its pattern constant at bind, and
            // a `?` is none when the statement prepares. A bound names
            // list defers the call's bind until the values are bound, and
            // by then the pattern is a constant
            // (trawl-engine's tests/extract_struct.rs).
            let placeholder = ctx.push_extract_regex_param(&batch.regex)?;
            let fields: Vec<String> = (1..=batch.captures.len())
                .map(|k| format!("g{k}"))
                .collect();
            let names = ctx.push_param(SqlValue::String(format!("[{}]", fields.join(", "))));

            let owned: Vec<String> = batch.captures.iter().map(|c| c.name.clone()).collect();
            ctx.select = vec![
                columns_excluding(&owned),
                format!(
                    "regexp_extract({source}, {placeholder}, {names}::VARCHAR[]) AS {EXTRACT_STRUCT}"
                ),
            ];
            ctx.has_projection = true;

            // The struct is computed once a row in its own stage, and
            // each field read here: nullif turns a group that took no part
            // in the match, and every group of a row that does not match,
            // from '' into NULL.
            ctx.flush_to_cte();
            let mut select_items = vec![format!("* EXCLUDE ({EXTRACT_STRUCT})")];
            for capture in &batch.captures {
                select_items.push(format!(
                    "nullif(struct_extract({EXTRACT_STRUCT}, 'g{}'), '') AS {}",
                    capture.index,
                    quote_field(&capture.name)
                ));
            }
            ctx.select = select_items;
            ctx.has_projection = true;
        }
        ExtractMode::KeyValue { .. } => {
            return Err(EmitError::UnsupportedOperation {
                message: "extract kv reached SQL emitter (should have been split to rust_stages)"
                    .to_string(),
            });
        }
    }

    Ok(())
}

fn process_dedup(dedup: &crate::ast::DedupStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::Always);

    if dedup.fields.is_empty() {
        ctx.select = vec!["DISTINCT *".to_string()];
        ctx.has_projection = true;
    } else {
        let partition_fields: Vec<String> = dedup
            .fields
            .iter()
            .map(|f| ctx.read_field(f, Use::Operand))
            .collect::<Result<_, _>>()?;
        let partition_clause = partition_fields.join(", ");
        let time = ctx.read_field(crate::schema::TIME, Use::Operand)?;

        ctx.select = vec![
            "*".to_string(),
            format!(
                "ROW_NUMBER() OVER (PARTITION BY {partition_clause} ORDER BY \
                 TRY_CAST({time} AS TIMESTAMP) DESC) AS \"_rn\""
            ),
        ];
        ctx.has_projection = true;

        ctx.flush_to_cte();

        ctx.push_where("\"_rn\" = 1".to_string());
        ctx.select = vec!["* EXCLUDE (\"_rn\")".to_string()];
        ctx.has_projection = true;
    }
    Ok(())
}

fn process_timechart(
    tc: &crate::ast::TimechartStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::IfModifiedOrderedOrLimited);

    let interval = crate::timechart::resolve_span(tc.span, ctx.time_filter).to_interval_string();

    let bucket_source = match tc.on.as_deref() {
        Some(col) if !crate::schema::is_event_time(col) => {
            // Read first, so the probe asks about the column this stage
            // reads: an absent one probes as its NULL, which an unpinned
            // field types as no timestamp and the probe refuses.
            let column = ctx.read_field(col, Use::Operand)?;
            // Taken here, and only here: the relation this stage reads
            // is the state as it stands at this moment.
            let probe = crate::emitter::TimechartInputCheck {
                column: col.to_string(),
                sql: ctx.stage_input_probe(&column),
                params: ctx.params_so_far(),
            };
            ctx.timechart_input_checks.push(probe);
            column
        }
        None | Some(_) => format!(
            "TRY_CAST({} AS TIMESTAMP)",
            ctx.read_field(crate::schema::TIME, Use::Operand)?
        ),
    };

    // The bucket is aliased AS "_time", which is also the name of the
    // physical event-time column it buckets. GROUP BY / ORDER BY must
    // therefore reference the full expression, not the name: a bare
    // "_time" would bind to the source column and silently break the
    // aggregation (one group per input row).
    let bucket = format!("time_bucket(INTERVAL '{interval}', {bucket_source})");

    // Same ordering guard as `stats`, for the same reason: the
    // bucket/group items are assembled after the guard has had its chance
    // to flush (which would otherwise clear `group_by` and `order_by`).
    let agg_items = ctx.emit_ordered_select(|ctx| {
        let mut items = Vec::with_capacity(tc.aggregations.len());
        for agg in &tc.aggregations {
            let arg_strings = emit_call_args(&agg.function, &agg.args, ctx)?;

            let sql_func = translate_function(&agg.function, &arg_strings, ctx)?;

            let alias = match &agg.alias {
                Some(a) => quote_field(a),
                None => default_agg_alias(agg),
            };

            items.push(format!("{sql_func} AS {alias}"));
        }
        Ok(items)
    })?;

    let mut select_items = vec![format!("{bucket} AS \"_time\"")];
    let mut group_items = vec![bucket.clone()];

    for field in &tc.group_by {
        select_items.push(ctx.read_field(field, COLUMN)?);
        group_items.push(ctx.read_field(field, Use::Operand)?);
    }
    select_items.extend(agg_items);

    ctx.select = select_items;
    ctx.group_by = group_items;
    ctx.order_by.push(format!("{bucket} ASC"));
    ctx.has_aggregation = true;
    ctx.had_explicit_columns = true;

    Ok(())
}

fn process_tail(tail: &crate::ast::TailStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::IfModifiedOrLimited);

    if ctx.order_by.is_empty() {
        let time = ctx.read_field(crate::schema::TIME, Use::Sort)?;
        ctx.order_by.push(format!("{time} DESC"));
    }
    ctx.limit = Some(tail.count);
    Ok(())
}

fn process_rename(
    rename: &crate::ast::RenameStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
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
        .map(|(old, new)| ctx.read_field(old, Use::Column { alias: Some(new) }))
        .collect::<Result<_, _>>()?;

    ctx.select = vec![columns_excluding(&excluded), aliases.join(", ")];
    ctx.has_projection = true;
    Ok(())
}

fn process_pivot(pivot: &crate::ast::PivotStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    ctx.flush_to_cte();

    // narrow source to only columns needed by PIVOT — without explicit GROUP BY,
    // DuckDB treats ALL non-ON columns as implicit row identifiers. An
    // absent input is a NULL column here, so the PIVOT names exist. Each
    // is named once: a repeat would be a `host_1` row identifier.
    let args = pivot
        .aggregation
        .args
        .iter()
        .filter_map(|arg| match &arg.node {
            crate::ast::Expr::FieldRef(name) => Some(name.as_str()),
            _ => None,
        });
    let names = std::iter::once(pivot.on_field.as_str())
        .chain(pivot.by.iter().map(String::as_str))
        .chain(args);
    ctx.select = first_of_each(names)
        .into_iter()
        .map(|name| ctx.read_field(name, COLUMN))
        .collect::<Result<_, _>>()?;
    ctx.has_projection = true;
    ctx.flush_to_cte();

    let arg_strings = emit_call_args(&pivot.aggregation.function, &pivot.aggregation.args, ctx)?;

    let agg_sql = translate_function(&pivot.aggregation.function, &arg_strings, ctx)?;

    ctx.set_pivot(
        agg_sql,
        pivot.on_field.clone(),
        pivot.by.clone(),
        crate::complexity::pivot_aggregate_regex(pivot),
    );
    ctx.had_explicit_columns = true;

    Ok(())
}

fn process_sample(mode: &SampleMode, ctx: &mut EmitterState) {
    let clause = match mode {
        SampleMode::Percent(p) => format!("USING SAMPLE {p} PERCENT (bernoulli)"),
        SampleMode::Count(n) => format!("USING SAMPLE {n} ROWS (reservoir)"),
    };
    ctx.sample_boundary(clause);
}

fn process_eventstats(stage: &EventStatsStage, ctx: &mut EmitterState) -> Result<(), EmitError> {
    ctx.flush_if(FlushCondition::Always);

    let partition = if stage.group_by.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = stage
            .group_by
            .iter()
            .map(|f| ctx.read_field(f, Use::Operand))
            .collect::<Result<_, _>>()?;
        format!("PARTITION BY {}", parts.join(", "))
    };

    let mut targets = Vec::new();
    let mut computed = Vec::new();
    for agg in &stage.aggregations {
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

        let arg_strings = emit_call_args(&agg.function, &agg.args, ctx)?;
        let sql_func = translate_function(&agg.function, &arg_strings, ctx)?;
        let name = crate::projection::agg_output_name(agg);
        computed.push(format!(
            "{sql_func} OVER ({partition}) AS {}",
            quote_field(&name)
        ));
        targets.push(name);
    }

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
