// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layer 5: pipe stage parsers.
//!
//! Each pipe stage (`stats`, `where`, `sort`, `limit`, `table`) has its own
//! parser function. The pipeline parser chains them with `|` separators.

use chumsky::prelude::*;

use crate::ast::{
    AggExpr, DedupStage, DropStage, EventStatsStage, Expr, ExtractMode, ExtractStage,
    FromSavedStage, LetStage, LimitStage, PipeStage, PivotStage, RareStage, RenameStage,
    SampleMode, SampleStage, SavedRunSelector, SortDirection, SortField, SortStage, Spanned,
    StatsStage, TableStage, TailStage, TimechartStage, TopStage, WhereStage,
};
use crate::parser::expr::expr;
use crate::parser::primitives::{
    ParserExtra, ParserInput, duration, field_name, function_name, keyword, plain_name,
    raw_quoted_string, spanned, uint,
};

/// Parse an aggregation expression like `count()`, `avg(duration)`,
/// or `count() as total`.
fn agg_expr<'src>() -> impl Parser<'src, ParserInput<'src>, AggExpr, ParserExtra<'src>> + Clone {
    function_name()
        .then_ignore(just('(').padded())
        .then(expr().separated_by(just(',').padded()).collect::<Vec<_>>())
        .then_ignore(just(')').padded())
        .then(
            keyword("as")
                .padded()
                .ignore_then(assignment_target())
                .or_not(),
        )
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
        .map(|count| {
            PipeStage::Limit(LimitStage {
                count,
                keyword: "limit",
            })
        })
        .labelled("limit stage")
}

/// Parse a `head` stage: alias for `limit N`.
fn head_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("head")
        .padded()
        .ignore_then(uint())
        .map(|count| {
            PipeStage::Limit(LimitStage {
                count,
                keyword: "head",
            })
        })
        .labelled("head stage")
}

/// Parse a `tail` stage: `tail N` — last N rows.
fn tail_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("tail")
        .padded()
        .ignore_then(uint())
        .map(|count| PipeStage::Tail(TailStage { count }))
        .labelled("tail stage")
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
        .map(|fields| {
            PipeStage::Table(TableStage {
                fields,
                keyword: "table",
            })
        })
        .labelled("table stage")
}

/// Parse a `fields` stage: alias for `table field(, field)*`.
fn fields_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("fields")
        .padded()
        .ignore_then(
            field_name()
                .separated_by(just(',').padded())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|fields| {
            PipeStage::Table(TableStage {
                fields,
                keyword: "fields",
            })
        })
        .labelled("fields stage")
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

/// A field name the pipeline WRITES: a `let`/`eval` target, a `rename`
/// target, or an explicit aggregate alias (`stats`/`eventstats`/
/// `timechart`/`pivot` all mint their output column through `as`).
/// Trawl's `_` namespace is sealed at both doors (ADR-0013 §5) —
/// ingest strips the prefix off an incoming key, so a name the DSL minted
/// there would be a column ingest can never carry.
fn assignment_target<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    field_name().try_map(|name, span| {
        if crate::schema::is_reserved_name(&name) {
            return Err(Rich::custom(
                span,
                format!(
                    "'{name}' is in trawl's reserved namespace — names starting \
                     with '_' are trawl's contract slots and only trawl writes \
                     them; choose a name without the underscore"
                ),
            ));
        }
        Ok(name)
    })
}

/// Refuse a stage whose write targets would land on ONE column.
///
/// `let A = 1, a = 2` names one column twice: `DuckDB` binds identifiers
/// case-insensitively, so the batch projection writes both and dedup-names
/// the loser (`A`, `a_1`) while any in-memory lane keeps whichever it
/// applied last. Neither is what the query asked for, and the two lanes
/// disagree about the row.
///
/// This is a PARSE error rather than a validation one, following the
/// reserved-name-mint precedent (ADR-0013 §5): the assignment list is pure
/// syntax and decidable right here, so every lane inherits the refusal by
/// construction instead of two evaluators each remembering to ask.
fn folded_duplicate_target<'a>(
    targets: impl Iterator<Item = &'a str>,
    stage: &str,
) -> Option<String> {
    let mut seen: Vec<(String, &str)> = Vec::new();
    for name in targets {
        let folded = crate::schema::catalog_key(name);
        if let Some((_, first)) = seen.iter().find(|(key, _)| *key == folded) {
            let both = if *first == name {
                format!("`{name}` twice")
            } else {
                format!("`{first}` and `{name}`, which name one column")
            };
            return Some(format!(
                "{stage} writes {both} — give each target a name of its own"
            ));
        }
        seen.push((folded, name));
    }
    None
}

/// Parse a `let` / `eval` assignment: `field = expr`.
fn let_assignment<'src>()
-> impl Parser<'src, ParserInput<'src>, (String, Spanned<Expr>), ParserExtra<'src>> + Clone {
    assignment_target()
        .then_ignore(just('=').padded())
        .then(expr())
}

/// The assignment list of a `let`/`eval`, with its targets checked to name
/// distinct columns.
fn let_assignments<'src>()
-> impl Parser<'src, ParserInput<'src>, Vec<(String, Spanned<Expr>)>, ParserExtra<'src>> + Clone {
    let_assignment()
        .separated_by(just(',').padded())
        .at_least(1)
        .collect::<Vec<_>>()
        .try_map(|assignments: Vec<(String, Spanned<Expr>)>, span| {
            match folded_duplicate_target(assignments.iter().map(|(name, _)| name.as_str()), "let")
            {
                Some(message) => Err(Rich::custom(span, message)),
                None => Ok(assignments),
            }
        })
}

/// Parse a `let` stage: `let field = expr [, field = expr]*`
fn let_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    keyword("let")
        .padded()
        .ignore_then(let_assignments())
        .map(|assignments| {
            PipeStage::Let(LetStage {
                assignments,
                keyword: "let",
            })
        })
        .labelled("let stage")
}

/// Parse an `eval` stage: alias for `let field = expr [, field = expr]*`.
fn eval_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("eval")
        .padded()
        .ignore_then(let_assignments())
        .map(|assignments| {
            PipeStage::Let(LetStage {
                assignments,
                keyword: "eval",
            })
        })
        .labelled("eval stage")
}

/// Parse an extract/rex stage from a given keyword.
///
/// Both `extract` and `rex` support the same syntax:
/// `KEYWORD "pattern" [from field]` or `KEYWORD kv [sep="X"] [from field]`
fn extract_like_stage<'src>(
    kw: &'static str,
) -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    let from_clause = keyword("from").padded().ignore_then(field_name()).or_not();

    // Optional `sep="X"` clause — a quoted single character.
    let sep_clause = keyword("sep")
        .then(just('='))
        .ignore_then(raw_quoted_string())
        .padded()
        .or_not();

    let kv_mode = keyword(kw)
        .padded()
        .ignore_then(keyword("kv"))
        .padded()
        .ignore_then(sep_clause)
        .then(from_clause.clone())
        .map(move |(sep_str, source_field)| {
            let separator = sep_str
                .and_then(|s| {
                    let mut chars = s.chars();
                    let c = chars.next()?;
                    if chars.next().is_some() {
                        None // multi-char — will fall back to default
                    } else {
                        Some(c)
                    }
                })
                .unwrap_or('=');
            PipeStage::Extract(ExtractStage {
                mode: ExtractMode::KeyValue { separator },
                source_field,
                keyword: kw,
            })
        });

    let regex_mode = keyword(kw)
        .padded()
        .ignore_then(raw_quoted_string())
        .then(from_clause)
        .map(move |(pattern, source_field)| {
            PipeStage::Extract(ExtractStage {
                mode: ExtractMode::Regex(pattern),
                source_field,
                keyword: kw,
            })
        });

    choice((kv_mode, regex_mode)).labelled("extract stage")
}

/// Parse an `extract` stage: `extract "pattern" [from field]` or `extract kv [from field]`
fn extract_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    extract_like_stage("extract")
}

/// Parse a `rex` stage: alias for `extract`.
fn rex_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    extract_like_stage("rex")
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

/// Parse a `rename` stage: `rename old AS new [, old2 AS new2]*`
fn rename_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    let rename_pair = field_name()
        .then_ignore(keyword("as").padded())
        .then(assignment_target());

    keyword("rename")
        .padded()
        .ignore_then(
            rename_pair
                .separated_by(just(',').padded())
                .at_least(1)
                .collect::<Vec<_>>()
                .try_map(|renames: Vec<(String, String)>, span| {
                    match folded_duplicate_target(
                        renames.iter().map(|(_, to)| to.as_str()),
                        "rename",
                    ) {
                        Some(message) => Err(Rich::custom(span, message)),
                        None => Ok(renames),
                    }
                }),
        )
        .map(|renames| PipeStage::Rename(RenameStage { renames }))
        .labelled("rename stage")
}

/// Parse a `sample` stage: `sample 10%` or `sample 1000`.
fn sample_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("sample")
        .padded()
        .ignore_then(uint().then(just('%').or_not()))
        .try_map(|(n, pct), span| {
            if pct.is_some() {
                if n == 0 || n > 100 {
                    return Err(Rich::custom(
                        span,
                        "sample percentage must be between 1 and 100",
                    ));
                }
                Ok(PipeStage::Sample(SampleStage {
                    mode: SampleMode::Percent(n),
                }))
            } else {
                if n == 0 {
                    return Err(Rich::custom(span, "sample count must be at least 1"));
                }
                Ok(PipeStage::Sample(SampleStage {
                    mode: SampleMode::Count(n),
                }))
            }
        })
        .labelled("sample stage")
}

/// Parse an `eventstats` stage: `eventstats agg1(), agg2() [by field1, field2]`.
fn eventstats_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    keyword("eventstats")
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
            PipeStage::EventStats(EventStatsStage {
                aggregations,
                group_by,
            })
        })
        .labelled("eventstats stage")
}

/// Parse a `from saved` stage: `from saved <name> [run=latest|all|N]`
///
/// Name is a bare identifier or a double-quoted string — never backticked: a
/// saved query is not a column (ADR-0013 ruling 7), and the quoted form
/// already covers every name a bare identifier cannot spell. The optional
/// `run=` clause selects which run(s) to load: `latest` (default), `all`, or
/// a numeric ID.
fn from_saved_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    let name = choice((raw_quoted_string(), plain_name())).labelled("saved query name");

    let run_selector = keyword("run")
        .ignore_then(just('='))
        .ignore_then(choice((
            keyword("latest").to(SavedRunSelector::Latest),
            keyword("all").to(SavedRunSelector::All),
            uint().map(|n| SavedRunSelector::Specific(i64::try_from(n).unwrap_or(i64::MAX))),
        )))
        .padded()
        .or_not()
        .map(Option::unwrap_or_default);

    keyword("from")
        .padded()
        .ignore_then(keyword("saved").padded())
        .ignore_then(name)
        .then(run_selector)
        .map(|(name, run)| PipeStage::FromSaved(FromSavedStage { name, run }))
        .labelled("from saved stage")
}

/// Parse a single pipe stage.
fn pipe_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    choice((
        from_saved_stage(),
        stats_stage(),
        eventstats_stage(),
        timechart_stage(),
        where_stage(),
        sort_stage(),
        limit_stage(),
        head_stage(),
        tail_stage(),
        let_stage(),
        eval_stage(),
        extract_stage(),
        rex_stage(),
        table_stage(),
        fields_stage(),
        top_stage(),
        rare_stage(),
        dedup_stage(),
        drop_stage(),
        rename_stage(),
        pivot_stage(),
        sample_stage(),
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
    use crate::ast::{BinaryOp, Expr, SavedRunSelector};

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

    // ── head (alias for limit) ────────────────────────────────────────

    #[test]
    fn test_head() {
        let input = "| head 10";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Limit(lim) => assert_eq!(lim.count, 10),
            other => panic!("expected Limit, got {other:?}"),
        }
    }

    // ── fields (alias for table) ────────────────────────────────────

    #[test]
    fn test_fields() {
        let input = "| fields host, service, level";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Table(t) => {
                assert_eq!(
                    t.fields,
                    vec![
                        "host".to_string(),
                        "service".to_string(),
                        "level".to_string()
                    ]
                );
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    // ── splunk eval alias (alias for let) ───────────────────────────

    #[test]
    fn test_splunk_eval_alias() {
        let input = "| eval msg_len = length(message)";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Let(l) => {
                assert_eq!(l.assignments.len(), 1);
                assert_eq!(l.assignments[0].0, "msg_len");
                match &l.assignments[0].1.node {
                    Expr::FunctionCall { name, args } => {
                        assert_eq!(name, "length");
                        assert_eq!(args.len(), 1);
                    }
                    other => panic!("expected FunctionCall, got {other:?}"),
                }
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    // ── rex (alias for extract) ─────────────────────────────────────

    #[test]
    fn test_rex_regex() {
        let input = r#"| rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message"#;
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Extract(e) => {
                assert!(matches!(e.mode, ExtractMode::Regex(_)));
                assert_eq!(e.source_field, Some("message".to_string()));
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    #[test]
    fn test_rex_kv() {
        let input = "| rex kv from raw";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Extract(e) => {
                assert_eq!(e.mode, ExtractMode::KeyValue { separator: '=' });
                assert_eq!(e.source_field, Some("raw".to_string()));
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    // ── top ─────────────────────────────────────────────────────────────

    #[test]
    fn test_top_basic() {
        let input = "| top 5 host";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Top(t) => {
                assert_eq!(t.count, 5);
                assert_eq!(t.field, "host");
                assert!(t.by.is_empty());
            }
            other => panic!("expected Top, got {other:?}"),
        }
    }

    #[test]
    fn test_top_with_by() {
        let input = "| top 3 host by service";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Top(t) => {
                assert_eq!(t.count, 3);
                assert_eq!(t.field, "host");
                assert_eq!(t.by, vec!["service".to_string()]);
            }
            other => panic!("expected Top, got {other:?}"),
        }
    }

    // ── rare ────────────────────────────────────────────────────────────

    #[test]
    fn test_rare_basic() {
        let input = "| rare 5 status";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Rare(r) => {
                assert_eq!(r.count, 5);
                assert_eq!(r.field, "status");
                assert!(r.by.is_empty());
            }
            other => panic!("expected Rare, got {other:?}"),
        }
    }

    #[test]
    fn test_rare_with_by() {
        let input = "| rare 3 status by service, host";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Rare(r) => {
                assert_eq!(r.count, 3);
                assert_eq!(r.field, "status");
                assert_eq!(r.by, vec!["service".to_string(), "host".to_string()]);
            }
            other => panic!("expected Rare, got {other:?}"),
        }
    }

    // ── drop ────────────────────────────────────────────────────────────

    #[test]
    fn test_drop_single() {
        let input = "| drop host";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Drop(d) => {
                assert_eq!(d.fields, vec!["host".to_string()]);
            }
            other => panic!("expected Drop, got {other:?}"),
        }
    }

    #[test]
    fn test_drop_multiple() {
        let input = "| drop host, status, uri";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Drop(d) => {
                assert_eq!(
                    d.fields,
                    vec!["host".to_string(), "status".to_string(), "uri".to_string()]
                );
            }
            other => panic!("expected Drop, got {other:?}"),
        }
    }

    // ── let ─────────────────────────────────────────────────────────────

    #[test]
    fn test_let_arithmetic() {
        let input = "| let duration_ms = duration * 1000";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Let(l) => {
                assert_eq!(l.assignments.len(), 1);
                assert_eq!(l.assignments[0].0, "duration_ms");
                assert!(matches!(l.assignments[0].1.node, Expr::Binary { .. }));
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    #[test]
    fn test_let_function_call() {
        let input = "| let lower_host = lower(host)";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Let(l) => {
                assert_eq!(l.assignments.len(), 1);
                assert_eq!(l.assignments[0].0, "lower_host");
                match &l.assignments[0].1.node {
                    Expr::FunctionCall { name, args } => {
                        assert_eq!(name, "lower");
                        assert_eq!(args.len(), 1);
                    }
                    other => panic!("expected FunctionCall, got {other:?}"),
                }
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    #[test]
    fn test_let_multi_assignment() {
        let input = "| let a = lower(service), b = length(service)";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Let(l) => {
                assert_eq!(l.assignments.len(), 2);
                assert_eq!(l.assignments[0].0, "a");
                assert_eq!(l.assignments[1].0, "b");
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    // ── extract ─────────────────────────────────────────────────────────

    #[test]
    fn test_extract_regex() {
        let input = r#"| extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message"#;
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Extract(e) => {
                assert!(matches!(e.mode, ExtractMode::Regex(_)));
                assert_eq!(e.source_field, Some("message".to_string()));
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_kv() {
        let input = "| extract kv";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Extract(e) => {
                assert_eq!(e.mode, ExtractMode::KeyValue { separator: '=' });
                assert_eq!(e.source_field, None);
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_kv_from_field() {
        let input = "| extract kv from raw";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Extract(e) => {
                assert_eq!(e.mode, ExtractMode::KeyValue { separator: '=' });
                assert_eq!(e.source_field, Some("raw".to_string()));
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_kv_with_sep() {
        let input = r#"| extract kv sep=":""#;
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Extract(e) => {
                assert_eq!(e.mode, ExtractMode::KeyValue { separator: ':' });
                assert_eq!(e.source_field, None);
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_kv_with_sep_and_from() {
        let input = r#"| extract kv sep=":" from raw"#;
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Extract(e) => {
                assert_eq!(e.mode, ExtractMode::KeyValue { separator: ':' });
                assert_eq!(e.source_field, Some("raw".to_string()));
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    // ── dedup ───────────────────────────────────────────────────────────

    #[test]
    fn test_dedup_bare() {
        let input = "| dedup";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Dedup(d) => {
                assert!(d.fields.is_empty());
            }
            other => panic!("expected Dedup, got {other:?}"),
        }
    }

    #[test]
    fn test_dedup_single_field() {
        let input = "| dedup host";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Dedup(d) => {
                assert_eq!(d.fields, vec!["host".to_string()]);
            }
            other => panic!("expected Dedup, got {other:?}"),
        }
    }

    #[test]
    fn test_dedup_multiple_fields() {
        let input = "| dedup host, service";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Dedup(d) => {
                assert_eq!(d.fields, vec!["host".to_string(), "service".to_string()]);
            }
            other => panic!("expected Dedup, got {other:?}"),
        }
    }

    // ── timechart ───────────────────────────────────────────────────────

    #[test]
    fn test_timechart_with_span() {
        let input = "| timechart span=1h count()";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Timechart(tc) => {
                assert!(tc.span.is_some());
                assert_eq!(tc.span.as_ref().unwrap().quantity, 1);
                assert_eq!(tc.aggregations.len(), 1);
                assert_eq!(tc.aggregations[0].function, "count");
                assert!(tc.group_by.is_empty());
            }
            other => panic!("expected Timechart, got {other:?}"),
        }
    }

    #[test]
    fn test_timechart_with_by() {
        let input = "| timechart count() by service";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Timechart(tc) => {
                assert!(tc.span.is_none());
                assert_eq!(tc.aggregations.len(), 1);
                assert_eq!(tc.group_by, vec!["service".to_string()]);
            }
            other => panic!("expected Timechart, got {other:?}"),
        }
    }

    #[test]
    fn test_timechart_multiple_aggs() {
        let input = "| timechart span=5m count(), avg(duration)";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Timechart(tc) => {
                assert_eq!(tc.aggregations.len(), 2);
                assert_eq!(tc.aggregations[0].function, "count");
                assert_eq!(tc.aggregations[1].function, "avg");
            }
            other => panic!("expected Timechart, got {other:?}"),
        }
    }

    // ── pivot ───────────────────────────────────────────────────────────

    #[test]
    fn test_pivot_basic() {
        let input = "| pivot count() on service";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Pivot(p) => {
                assert_eq!(p.aggregation.function, "count");
                assert_eq!(p.on_field, "service");
                assert!(p.by.is_empty());
            }
            other => panic!("expected Pivot, got {other:?}"),
        }
    }

    #[test]
    fn test_pivot_with_by() {
        let input = "| pivot avg(duration) on service by host";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::Pivot(p) => {
                assert_eq!(p.aggregation.function, "avg");
                assert_eq!(p.on_field, "service");
                assert_eq!(p.by, vec!["host".to_string()]);
            }
            other => panic!("expected Pivot, got {other:?}"),
        }
    }

    // ── tail ────────────────────────────────────────────────────────────

    #[test]
    fn test_tail() {
        let input = "| tail 5";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Tail(t) => assert_eq!(t.count, 5),
            other => panic!("expected Tail, got {other:?}"),
        }
    }

    // ── rename ──────────────────────────────────────────────────────────

    #[test]
    fn test_rename_single() {
        let input = "| rename service as svc";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Rename(r) => {
                assert_eq!(r.renames, vec![("service".to_string(), "svc".to_string())]);
            }
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn test_rename_multiple() {
        let input = "| rename service as svc, host as hostname";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::Rename(r) => {
                assert_eq!(
                    r.renames,
                    vec![
                        ("service".to_string(), "svc".to_string()),
                        ("host".to_string(), "hostname".to_string()),
                    ]
                );
            }
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    // ── from saved ──────────────────────────────────────────────────────

    #[test]
    fn test_from_saved_bare_name() {
        let input = "| from saved daily_errors";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::FromSaved(fs) => {
                assert_eq!(fs.name, "daily_errors");
                assert_eq!(fs.run, SavedRunSelector::Latest);
            }
            other => panic!("expected FromSaved, got {other:?}"),
        }
    }

    #[test]
    fn test_from_saved_quoted_name() {
        let input = r#"| from saved "hourly-error-count""#;
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].node {
            PipeStage::FromSaved(fs) => {
                assert_eq!(fs.name, "hourly-error-count");
                assert_eq!(fs.run, SavedRunSelector::Latest);
            }
            other => panic!("expected FromSaved, got {other:?}"),
        }
    }

    #[test]
    fn test_from_saved_run_latest() {
        let input = "| from saved my_query run=latest";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::FromSaved(fs) => {
                assert_eq!(fs.name, "my_query");
                assert_eq!(fs.run, SavedRunSelector::Latest);
            }
            other => panic!("expected FromSaved, got {other:?}"),
        }
    }

    #[test]
    fn test_from_saved_run_all() {
        let input = "| from saved my_query run=all";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::FromSaved(fs) => {
                assert_eq!(fs.name, "my_query");
                assert_eq!(fs.run, SavedRunSelector::All);
            }
            other => panic!("expected FromSaved, got {other:?}"),
        }
    }

    #[test]
    fn test_from_saved_run_specific() {
        let input = "| from saved daily_rollup run=42";
        let result = pipeline().parse(input).into_result().unwrap();
        match &result[0].node {
            PipeStage::FromSaved(fs) => {
                assert_eq!(fs.name, "daily_rollup");
                assert_eq!(fs.run, SavedRunSelector::Specific(42));
            }
            other => panic!("expected FromSaved, got {other:?}"),
        }
    }

    #[test]
    fn test_from_saved_with_pipeline() {
        let input = "| from saved daily_rollup | where count > 10 | sort -count";
        let result = pipeline().parse(input).into_result().unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(result[0].node, PipeStage::FromSaved(_)));
        assert!(matches!(result[1].node, PipeStage::Where(_)));
        assert!(matches!(result[2].node, PipeStage::Sort(_)));
    }
}
