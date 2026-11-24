// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layer 5: pipe stage parsers.

use chumsky::prelude::*;

use crate::ast::{
    AggExpr, DedupStage, DropStage, EventStatsStage, Expr, ExtractMode, ExtractStage,
    FromSavedStage, LetStage, LimitStage, PipeStage, PivotStage, RareStage, RenameStage,
    SampleMode, SampleStage, SavedRunSelector, SortDirection, SortField, SortStage, Spanned,
    StatsStage, TableStage, TailStage, TimechartStage, TopStage, WhereStage,
};
use crate::parser::comment::Spaced;
use crate::parser::expr::expr;
use crate::parser::primitives::{
    ParserExtra, ParserInput, check_regex_body, duration, field_name, keyword, plain_name,
    quoted_string, raw_quoted_string, spanned, uint,
};

fn agg_expr<'src>() -> impl Parser<'src, ParserInput<'src>, AggExpr, ParserExtra<'src>> + Clone {
    plain_name()
        .then_ignore(just('(').spaced())
        .then(expr().separated_by(just(',').spaced()).collect::<Vec<_>>())
        .then_ignore(just(')').spaced())
        .then(
            keyword("as")
                .spaced()
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

fn stats_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("stats")
        .spaced()
        .ignore_then(
            agg_expr()
                .separated_by(just(',').spaced())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then(
            keyword("by")
                .spaced()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').spaced())
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

fn where_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("where")
        .spaced()
        .ignore_then(expr())
        .map(|condition| PipeStage::Where(WhereStage { condition }))
        .labelled("where stage")
}

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

fn sort_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("sort")
        .spaced()
        .ignore_then(
            sort_field()
                .separated_by(just(',').spaced())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|fields| PipeStage::Sort(SortStage { fields }))
        .labelled("sort stage")
}

fn limit_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("limit")
        .spaced()
        .ignore_then(uint())
        .map(|count| {
            PipeStage::Limit(LimitStage {
                count,
                keyword: "limit",
            })
        })
        .labelled("limit stage")
}

fn head_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("head")
        .spaced()
        .ignore_then(uint())
        .map(|count| {
            PipeStage::Limit(LimitStage {
                count,
                keyword: "head",
            })
        })
        .labelled("head stage")
}

fn tail_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("tail")
        .spaced()
        .ignore_then(uint())
        .map(|count| PipeStage::Tail(TailStage { count }))
        .labelled("tail stage")
}

fn table_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("table")
        .spaced()
        .ignore_then(
            field_name()
                .separated_by(just(',').spaced())
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

fn fields_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("fields")
        .spaced()
        .ignore_then(
            field_name()
                .separated_by(just(',').spaced())
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

fn top_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    keyword("top")
        .spaced()
        .ignore_then(uint())
        .then(field_name().spaced())
        .then(
            keyword("by")
                .spaced()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').spaced())
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|((count, field), by)| PipeStage::Top(TopStage { count, field, by }))
        .labelled("top stage")
}

fn rare_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("rare")
        .spaced()
        .ignore_then(uint())
        .then(field_name().spaced())
        .then(
            keyword("by")
                .spaced()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').spaced())
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|((count, field), by)| PipeStage::Rare(RareStage { count, field, by }))
        .labelled("rare stage")
}

fn drop_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("drop")
        .spaced()
        .ignore_then(
            field_name()
                .separated_by(just(',').spaced())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|fields| PipeStage::Drop(DropStage { fields }))
        .labelled("drop stage")
}

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

fn let_assignment<'src>()
-> impl Parser<'src, ParserInput<'src>, (String, Spanned<Expr>), ParserExtra<'src>> + Clone {
    assignment_target()
        .then_ignore(just('=').spaced())
        .then(expr())
}

fn let_assignments<'src>(
    stage: &'static str,
) -> impl Parser<'src, ParserInput<'src>, Vec<(String, Spanned<Expr>)>, ParserExtra<'src>> + Clone {
    let_assignment()
        .separated_by(just(',').spaced())
        .at_least(1)
        .collect::<Vec<_>>()
        .try_map(move |assignments: Vec<(String, Spanned<Expr>)>, span| {
            match crate::schema::duplicate_target_message(
                assignments.iter().map(|(name, _)| name.as_str()),
                stage,
            ) {
                Some(message) => Err(Rich::custom(span, message)),
                None => Ok(assignments),
            }
        })
}

fn let_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    keyword("let")
        .spaced()
        .ignore_then(let_assignments("let"))
        .map(|assignments| {
            PipeStage::Let(LetStage {
                assignments,
                keyword: "let",
            })
        })
        .labelled("let stage")
}

fn eval_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("eval")
        .spaced()
        .ignore_then(let_assignments("eval"))
        .map(|assignments| {
            PipeStage::Let(LetStage {
                assignments,
                keyword: "eval",
            })
        })
        .labelled("eval stage")
}

fn extract_like_stage<'src>(
    kw: &'static str,
) -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    let from_clause = keyword("from").spaced().ignore_then(field_name()).or_not();

    let sep_clause = keyword("sep")
        .then(just('='))
        .ignore_then(raw_quoted_string())
        .spaced()
        .or_not();

    let kv_mode = keyword(kw)
        .spaced()
        .ignore_then(keyword("kv"))
        .spaced()
        .ignore_then(sep_clause)
        .then(from_clause.clone())
        .map(move |(sep_str, source_field)| {
            let separator = sep_str
                .and_then(|s| {
                    let mut chars = s.chars();
                    let c = chars.next()?;
                    if chars.next().is_some() {
                        None
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

    // The body gets the checks a `/body/` regex gets, over the quoted
    // string as written: a raw string decodes nothing, so its length is
    // the bytes between the quotes.
    let body = raw_quoted_string().validate(|body: String, extra, emitter| {
        check_regex_body(&body, extra.span(), extra.state(), emitter);
        body
    });
    let regex_mode = keyword(kw)
        .spaced()
        .ignore_then(body)
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

fn extract_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    extract_like_stage("extract")
}

fn rex_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    extract_like_stage("rex")
}

fn dedup_stage<'src>() -> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone
{
    keyword("dedup")
        .spaced()
        .ignore_then(
            field_name()
                .separated_by(just(',').spaced())
                .at_least(1)
                .collect::<Vec<_>>()
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|fields| PipeStage::Dedup(DedupStage { fields }))
        .labelled("dedup stage")
}

fn timechart_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    keyword("timechart")
        .spaced()
        // `.spaced()` wraps the whole clause, not just the keyword:
        // `field_name()` consumes no padding of its own, and what follows
        // it here — the optional `span=`, or the first aggregate — is the
        // one position in this stage that is not introduced by a padded
        // keyword.
        .ignore_then(
            keyword("on")
                .spaced()
                .ignore_then(field_name())
                .spaced()
                .or_not(),
        )
        .then(
            keyword("span")
                .then_ignore(just('='))
                .ignore_then(duration())
                .spaced()
                .or_not(),
        )
        .then(
            agg_expr()
                .separated_by(just(',').spaced())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then(
            keyword("by")
                .spaced()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').spaced())
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not()
                .map(Option::unwrap_or_default),
        )
        .map(|(((on, span), aggregations), group_by)| {
            PipeStage::Timechart(TimechartStage {
                on,
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
        .spaced()
        .ignore_then(agg_expr())
        .then_ignore(keyword("on").spaced())
        .then(field_name())
        .then(
            keyword("by")
                .spaced()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').spaced())
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
        .then_ignore(keyword("as").spaced())
        .then(assignment_target());

    keyword("rename")
        .spaced()
        .ignore_then(
            rename_pair
                .separated_by(just(',').spaced())
                .at_least(1)
                .collect::<Vec<_>>()
                .try_map(|renames: Vec<(String, String)>, span| {
                    match crate::schema::duplicate_target_message(
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
        .spaced()
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
        .spaced()
        .ignore_then(
            agg_expr()
                .separated_by(just(',').spaced())
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then(
            keyword("by")
                .spaced()
                .ignore_then(
                    field_name()
                        .separated_by(just(',').spaced())
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

fn from_saved_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, PipeStage, ParserExtra<'src>> + Clone {
    let name = choice((quoted_string(), plain_name())).labelled("saved query name");

    let run_selector = keyword("run")
        .ignore_then(just('='))
        .ignore_then(choice((
            keyword("latest").to(SavedRunSelector::Latest),
            keyword("all").to(SavedRunSelector::All),
            uint().map(|n| SavedRunSelector::Specific(i64::try_from(n).unwrap_or(i64::MAX))),
        )))
        .spaced()
        .or_not()
        .map(Option::unwrap_or_default);

    keyword("from")
        .spaced()
        .ignore_then(keyword("saved").spaced())
        .ignore_then(name)
        .then(run_selector)
        .map(|(name, run)| PipeStage::FromSaved(FromSavedStage { name, run }))
        .labelled("from saved stage")
}

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

pub(crate) fn pipeline<'src>()
-> impl Parser<'src, ParserInput<'src>, Vec<Spanned<PipeStage>>, ParserExtra<'src>> + Clone {
    just('|')
        .spaced()
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

    #[test]
    fn backticks_are_accepted_in_every_field_position() {
        let cases: &[(&str, &str)] = &[
            ("| table `request id`, `http-status`", "request id"),
            ("| fields `request id`", "request id"),
            ("| stats count() by `where`", "where"),
            ("| stats avg(`response time`)", "response time"),
            ("| stats count() as `total count`", "total count"),
            (
                "| timechart span=1h count() by `http-status`",
                "http-status",
            ),
            (
                "| eventstats count() as `n rows` by `http-status`",
                "n rows",
            ),
            ("| pivot count() on `http-status`", "http-status"),
            ("| sort -`http-status`", "http-status"),
            ("| drop `request id`", "request id"),
            ("| dedup `request id`", "request id"),
            ("| top 5 `http-status` by `request id`", "http-status"),
            ("| rare 5 `http-status`", "http-status"),
            ("| rename `request id` as `req id`", "request id"),
            ("| let `req id` = 1", "req id"),
            ("| where `http-status` > 400", "http-status"),
            (r#"| extract "(?P<ip>.)" from `raw body`"#, "raw body"),
        ];
        for (dsl, needle) in cases {
            let stages = pipeline()
                .parse(dsl)
                .into_result()
                .unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            let printed = format!("{:?}", stages[0].node);
            assert!(
                printed.contains(needle),
                "{dsl}: expected {needle:?} in the AST, got {printed}"
            );
        }
    }

    #[test]
    fn backticks_do_not_unseal_the_reserved_namespace() {
        for dsl in [
            "| let `_foo` = 1",
            "| eval `_severity` = 17",
            "| rename x as `_foo`",
            "| stats count() as `_total`",
            "| timechart span=1h count() as `_time`",
        ] {
            assert!(
                pipeline().parse(dsl).into_result().is_err(),
                "{dsl} must be a parse error"
            );
        }
    }

    #[test]
    fn aggregate_function_names_take_no_backticks() {
        assert!(pipeline().parse("| stats `count`()").into_result().is_err());
        assert!(
            pipeline()
                .parse("| stats `avg`(duration)")
                .into_result()
                .is_err()
        );
    }

    #[test]
    fn saved_query_names_take_no_backticks() {
        assert!(
            pipeline()
                .parse("| from saved `daily errors`")
                .into_result()
                .is_err()
        );
    }

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

    #[test]
    fn projection_assignment_targets_must_be_folded_distinct() {
        for input in [
            "| let A = 1, a = 2",
            "| eval A = 1, a = 2",
            "| rename a as X, b as x",
        ] {
            let errors = pipeline().parse(input).into_errors();
            assert_eq!(errors.len(), 1, "{input}: {errors:?}");
            assert!(
                errors[0].to_string().contains("which name one column"),
                "{input}: {}",
                errors[0]
            );
        }
    }
}
