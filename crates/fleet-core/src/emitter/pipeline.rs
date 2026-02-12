use crate::ast::{PipeStage, SortDirection};

use super::EmitError;
use super::expr::emit_expr;
use super::fields::quote_field;
use super::functions::{default_agg_alias, translate_function};
use super::state::EmitterState;

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
    }
}

fn process_stats(
    agg_stage: &crate::ast::StatsStage,
    ctx: &mut EmitterState,
) -> Result<(), EmitError> {
    // flush if a prior aggregation or projection would be clobbered
    if ctx.has_aggregation || ctx.has_projection {
        ctx.flush_to_cte();
    }

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
    // flush if prior aggregation or projection so WHERE applies to CTE output
    if ctx.has_aggregation || ctx.has_projection {
        ctx.flush_to_cte();
    }

    let clause = emit_expr(&where_stage.condition, ctx)?;
    ctx.push_where(clause);

    Ok(())
}

fn process_sort(sort_stage: &crate::ast::SortStage, ctx: &mut EmitterState) {
    // flush if prior aggregation, projection, or existing order
    if ctx.has_aggregation || ctx.has_projection || !ctx.order_by.is_empty() {
        ctx.flush_to_cte();
    }

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
    // flush if prior aggregation, projection, or existing limit
    if ctx.has_aggregation || ctx.has_projection || ctx.limit.is_some() {
        ctx.flush_to_cte();
    }

    ctx.limit = Some(limit_stage.count);
}

fn process_table(table_stage: &crate::ast::TableStage, ctx: &mut EmitterState) {
    if ctx.has_projection {
        ctx.flush_to_cte();
    }

    ctx.select = table_stage.fields.iter().map(|f| quote_field(f)).collect();
    ctx.has_projection = true;
}

/// Try to extract a bare field name from the first arg of an aggregation.
fn extract_field_name(arg: Option<&crate::ast::Spanned<crate::ast::Expr>>) -> Option<String> {
    match arg {
        Some(spanned) => match &spanned.node {
            crate::ast::Expr::FieldRef(name) => Some(name.clone()),
            _ => None,
        },
        None => None,
    }
}
