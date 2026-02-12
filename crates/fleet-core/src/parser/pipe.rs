//! Layer 5: pipe stage parsers.
//!
//! Each pipe stage (`stats`, `where`, `sort`, `limit`, `table`) has its own
//! parser function. The pipeline parser chains them with `|` separators.

use chumsky::prelude::*;

use crate::ast::{
    AggExpr, DedupStage, DropStage, ExtractMode, ExtractStage, LetStage, LimitStage, PipeStage,
    PivotStage, RareStage, SortDirection, SortField, SortStage, Spanned, StatsStage, TableStage,
    TimechartStage, TopStage, WhereStage,
};
use crate::parser::expr::expr;
use crate::parser::primitives::{
    ParserExtra, ParserInput, duration, field_name, keyword, raw_quoted_string, spanned, uint,
};

/// Parse an aggregation expression like `count()`, `avg(duration)`,
/// or `count() as total`.
fn agg_expr<'src>() -> impl Parser<'src, ParserInput<'src>, AggExpr, ParserExtra<'src>> + Clone {
    field_name()
        .then_ignore(just('(').padded())
        .then(expr().separated_by(just(',').padded()).collect::<Vec<_>>())
        .then_ignore(just(')').padded())
        .then(keyword("as").padded().ignore_then(field_name()).or_not())
        .map(|((function, args), alias)| AggExpr {
            function,
            args,
            alias,
        })
        .labelled("aggregation expression")
}

/// Parse a `stats` stage: `stats agg(, agg)* by field(, field)*`
fn stats_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("stats")
        .padded()
        .ignore_then(
            agg_expr()
                .separated_by(just(',').padded())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then(
            keyword("by")
                .padded()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').padded())
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|(aggregations, group_by)| {
            PipeStage::Stats(StatsStage {
                aggregations,
                group_by,
            })
        })
        .labelled("stats stage")
}

/// Parse a `where` stage: `where expr`
fn where_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("where")
        .padded()
        .ignore_then(expr())
        .map(|condition| PipeStage::Where(WhereStage { condition }))
        .labelled("where stage")
}

/// Parse a sort field: optional `-` prefix for descending.
fn sort_field<'src>() -> impl Parser<'src, ParserInput<'src>, SortField, ParserExtra<'src>> + Clone
{
    just('-')
        .or_not()
        .then(field_name())
        .map(|(neg, field)| SortField {
            field,
            direction: if neg.is_some() {
                SortDirection::Desc
            } else {
                SortDirection::Asc
            },
        })
        .labelled("sort field")
}

/// Parse a `sort` stage: `sort field(, field)*`
fn sort_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("sort")
        .padded()
        .ignore_then(
            sort_field()
                .separated_by(just(',').padded())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|fields| PipeStage::Sort(SortStage { fields }))
        .labelled("sort stage")
}

/// Parse a `limit` stage: `limit N`
fn limit_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("limit")
        .padded()
        .ignore_then(uint())
        .map(|count| PipeStage::Limit(LimitStage { count }))
        .labelled("limit stage")
}

/// Parse a `table` stage: `table field(, field)*`
fn table_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("table")
        .padded()
        .ignore_then(
            field_name()
                .separated_by(just(',').padded())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|fields| PipeStage::Table(TableStage { fields }))
        .labelled("table stage")
}

/// Parse a `top` stage: `top N field [by field(, field)*]`
fn top_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    keyword("top")
        .padded()
        .ignore_then(uint())
        .then(field_name().padded())
        .then(
            keyword("by")
                .padded()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').padded())
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|((count, field), by)| PipeStage::Top(TopStage { count, field, by }))
        .labelled("top stage")
}

/// Parse a `rare` stage: `rare N field [by field(, field)*]`
fn rare_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("rare")
        .padded()
        .ignore_then(uint())
        .then(field_name().padded())
        .then(
            keyword("by")
                .padded()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').padded())
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|((count, field), by)| PipeStage::Rare(RareStage { count, field, by }))
        .labelled("rare stage")
}

/// Parse a `drop` stage: `drop field(, field)*`
fn drop_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("drop")
        .padded()
        .ignore_then(
            field_name()
                .separated_by(just(',').padded())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|fields| PipeStage::Drop(DropStage { fields }))
        .labelled("drop stage")
}

/// Parse a `let` stage: `let field = expr`
fn let_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    keyword("let")
        .padded()
        .ignore_then(field_name())
        .then_ignore(just('=').padded())
        .then(expr())
        .map(|(field, expr)| PipeStage::Let(LetStage { field, expr }))
        .labelled("let stage")
}

/// Parse an `extract` stage: `extract "pattern" [from field]` or `extract kv [from field]`
fn extract_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    let from_clause = keyword("from").padded().ignore_then(field_name()).or_not();

    let kv_mode = keyword("extract")
        .padded()
        .ignore_then(keyword("kv"))
        .ignore_then(from_clause.clone())
        .map(|source_field| {
            PipeStage::Extract(ExtractStage {
                mode: ExtractMode::KeyValue,
                source_field,
            })
        });

    let regex_mode = keyword("extract")
        .padded()
        .ignore_then(raw_quoted_string())
        .then(from_clause)
        .map(|(pattern, source_field)| {
            PipeStage::Extract(ExtractStage {
                mode: ExtractMode::Regex(pattern),
                source_field,
            })
        });

    choice((kv_mode, regex_mode)).labelled("extract stage")
}

/// Parse a `dedup` stage: `dedup [field(, field)*]`
///
/// Bare `dedup` (no fields) removes exact duplicate rows.
/// With fields, keeps the most recent row per unique field combination.
fn dedup_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("dedup")
        .padded()
        .ignore_then(
            field_name()
                .separated_by(just(',').padded())
                .at_least(1)
                .collect::<Vec<_>>()
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|fields| PipeStage::Dedup(DedupStage { fields }))
        .labelled("dedup stage")
}

/// Parse a `timechart` stage: `timechart [span=DURATION] agg(, agg)* [by field(, field)*]`
fn timechart_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    keyword("timechart")
        .padded()
        .ignore_then(
            keyword("span")
                .then_ignore(just('='))
                .ignore_then(duration())
                .padded()
                .or_not(),
        )
        .then(
            agg_expr()
                .separated_by(just(',').padded())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then(
            keyword("by")
                .padded()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').padded())
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|((span, aggregations), group_by)| {
            PipeStage::Timechart(TimechartStage {
                span,
                aggregations,
                group_by,
            })
        })
        .labelled("timechart stage")
}

/// Parse a `pivot` stage: `pivot agg on field [by field(, field)*]`
fn pivot_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("pivot")
        .padded()
        .ignore_then(agg_expr())
        .then_ignore(keyword("on").padded())
        .then(field_name())
        .then(
            keyword("by")
                .padded()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').padded())
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|((aggregation, on_field), by)| {
            PipeStage::Pivot(PivotStage {
                aggregation,
                on_field,
                by,
            })
        })
        .labelled("pivot stage")
}

/// Parse a single pipe stage.
fn pipe_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    choice((
        stats_stage(),
        timechart_stage(),
        where_stage(),
        sort_stage(),
        limit_stage(),
        let_stage(),
        extract_stage(),
        table_stage(),
        top_stage(),
        rare_stage(),
        dedup_stage(),
        drop_stage(),
        pivot_stage(),
    ))
    .labelled("pipe stage")
}

/// Parse the pipeline: `("|" pipe_stage)*`.
/// Returns a vec of spanned pipe stages.
pub(crate) fn pipeline<'src>()
-> impl Parser<'src, ParserInput<'src>, Vec<Spanned<PipeStage>>, ParserExtra<'src>> + Clone {
    just('|')
        .padded()
        .ignore_then(spanned(pipe_stage()))
        .repeated()
        .collect()
        .labelled("pipeline")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinaryOp, Expr};

    #[test]
    fn test_stats_count_by_host() {
        let input = "| stats count() by host";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Stats(stats) => {
                assert_eq!(stats.aggregations.len(), 1);
                assert_eq!(stats.aggregations[0].function, "count");
                assert_eq!(stats.aggregations[0].args.len(), 0);
                assert_eq!(stats.group_by, vec!["host".to_string()]);
            }
            other => panic!("expected Stats, got {other:?}"),
        }
    }

    #[test]
    fn test_stats_avg_with_alias() {
        let input = "| stats avg(duration) as avg_duration by status";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Stats(stats) => {
                assert_eq!(stats.aggregations[0].function, "avg");
                assert_eq!(stats.aggregations[0].args.len(), 1);
                assert_eq!(
                    stats.aggregations[0].alias,
                    Some("avg_duration".to_string())
                );
            }
            other => panic!("expected Stats, got {other:?}"),
        }
    }

    #[test]
    fn test_where_stage() {
        let input = "| where count > 10";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Where(w) => match &w.condition.node {
                Expr::Binary { op, .. } => assert_eq!(*op, BinaryOp::Gt),
                other => panic!("expected Binary, got {other:?}"),
            },
            other => panic!("expected Where, got {other:?}"),
        }
    }

    #[test]
    fn test_sort_desc() {
        let input = "| sort -count";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Sort(sort) => {
                assert_eq!(sort.fields.len(), 1);
                assert_eq!(sort.fields[0].field, "count");
                assert_eq!(sort.fields[0].direction, SortDirection::Desc);
            }
            other => panic!("expected Sort, got {other:?}"),
        }
    }

    #[test]
    fn test_limit() {
        let input = "| limit 20";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Limit(lim) => assert_eq!(lim.count, 20),
            other => panic!("expected Limit, got {other:?}"),
        }
    }

    #[test]
    fn test_table() {
        let input = "| table status, avg_duration";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Table(t) => {
                assert_eq!(
                    t.fields,
                    vec!["status".to_string(), "avg_duration".to_string()]
                );
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn test_multi_stage_pipeline() {
        let input = "| stats count() by host | where count > 10 | sort -count | limit 20";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 4);
        assert!(matches!(result[0].node, PipeStage::Stats(_)));
        assert!(matches!(result[1].node, PipeStage::Where(_)));
        assert!(matches!(result[2].node, PipeStage::Sort(_)));
        assert!(matches!(result[3].node, PipeStage::Limit(_)));
    }
}
