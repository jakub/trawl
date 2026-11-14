// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Advice on a valid query (ADR-0068): the two shapes, the copy, the cap
//! and the heap walk.

use trawl_core::advice::{AdviceKind, AdviceReport, MAX_ADVICE_FINDINGS, advise};
use trawl_core::ast::{
    Expr, FieldFilter, FilterOp, FilterValue, PipeStage, Query, QuotedSearch, Scalar, SearchStage,
    SearchToken, Spanned, TextSearch, UnaryOp,
};
use trawl_core::parser;

const NEGATED_MESSAGE: &str = "-service=… is a text search, not a field filter: it drops events \
     whose message or _raw contain that text";
const NEGATED_HINT: &str = "to filter the field, write service!=… (keeps events with no service) \
     or NOT service=… (drops them too); to keep the text search and silence this, write NOT \
     \"service=…\"";
const IN_MESSAGE: &str = "in the search stage, in (…) is not a list test: the words before the \
     brackets are text searches, and the brackets hold a search group";
const IN_HINT: &str = "to match a list, write event_type=a,b; in (…) is expression syntax, as in \
     | where event_type in (\"a\", \"b\")";
const NOT_IN_HINT: &str = "to exclude a list, write event_type!=a,b; in (…) is expression \
     syntax, as in | where not event_type in (\"a\", \"b\")";

fn parsed(text: &str) -> Query {
    parser::parse(text).unwrap_or_else(|errors| panic!("{text:?} parses: {errors:?}"))
}

fn advice(text: &str) -> AdviceReport {
    advise(&parsed(text))
}

/// The one finding `text` gives.
fn only(text: &str) -> trawl_core::advice::Advice {
    let report = advice(text);
    assert_eq!(report.omitted, 0, "{text:?}");
    assert_eq!(report.findings.len(), 1, "{text:?}: {report:?}");
    report.findings.into_iter().next().unwrap()
}

#[test]
fn negated_field_shape_is_advised() {
    let finding = only("\"10.244.0.83\" -service=coredns");
    assert_eq!(finding.kind, AdviceKind::NegatedFieldShape);
    assert_eq!(finding.span, 14..30);
    assert_eq!(finding.message, NEGATED_MESSAGE);
    assert_eq!(finding.hint, NEGATED_HINT);

    let finding = only("-status>=500");
    assert_eq!(finding.span, 0..12);
    assert_eq!(
        finding.message,
        "-status>=… is a text search, not a field filter: it drops events whose message or _raw \
         contain that text"
    );
    assert_eq!(
        finding.hint,
        "to filter the field, write NOT status>=…; to keep the text search and silence this, \
         write NOT \"status>=…\""
    );

    let finding = only("-service!=coredns");
    assert_eq!(
        finding.hint,
        "to filter the field, write NOT service!=…; to keep the text search and silence this, \
         write NOT \"service!=…\""
    );

    // The longest operator wins at the split byte, and the value may hold
    // operator bytes of its own.
    for (text, op) in [
        ("-n<5", "<"),
        ("-n<=5", "<="),
        ("-n>5", ">"),
        ("-n>=5", ">="),
        ("-n!=5", "!="),
        ("-n=5", "="),
        ("-n=a=b", "="),
        ("-n<=>", "<="),
    ] {
        let finding = only(text);
        assert_eq!(finding.kind, AdviceKind::NegatedFieldShape, "{text:?}");
        assert!(
            finding.message.starts_with(&format!("-n{op}… ")),
            "{text:?}: {}",
            finding.message
        );
    }

    // Anywhere in the search stage: inside brackets, under NOT, in a
    // second OR group.
    for (text, span) in [
        ("(a OR -service=x)", 6..16),
        ("NOT -service=x", 4..14),
        ("NOT (b -service=x)", 7..17),
        ("a OR -service=x", 5..15),
    ] {
        let finding = only(text);
        assert_eq!(finding.kind, AdviceKind::NegatedFieldShape, "{text:?}");
        assert_eq!(finding.span, span, "{text:?}");
        assert_eq!(finding.message, NEGATED_MESSAGE, "{text:?}");
    }
}

#[test]
fn search_stage_in_is_advised() {
    let finding = only("service=trawld event_type in (a, b)");
    assert_eq!(finding.kind, AdviceKind::SearchStageIn);
    assert_eq!(finding.span, 15..30);
    assert_eq!(finding.message, IN_MESSAGE);
    assert_eq!(finding.hint, IN_HINT);

    for text in ["event_type not in (a, b)", "event_type NOT in (a, b)"] {
        let finding = only(text);
        assert_eq!(finding.kind, AdviceKind::SearchStageIn, "{text:?}");
        assert_eq!(finding.span, 0..text.find('(').unwrap() + 1, "{text:?}");
        assert_eq!(finding.message, IN_MESSAGE, "{text:?}");
        assert_eq!(finding.hint, NOT_IN_HINT, "{text:?}");
        assert!(
            finding
                .hint
                .starts_with("to exclude a list, write event_type!=a,b")
        );
    }

    // `IN`, any case of the lowercase-reading `not`, any group contents.
    for (text, hint) in [
        ("event_type IN (a)", IN_HINT),
        ("event_type Not in (a OR b)", NOT_IN_HINT),
        ("event_type nOT IN (\"a b\" c=d)", NOT_IN_HINT),
        ("event_type NOT IN ((x))", NOT_IN_HINT),
    ] {
        let finding = only(text);
        assert_eq!(finding.kind, AdviceKind::SearchStageIn, "{text:?}");
        assert_eq!(finding.hint, hint, "{text:?}");
    }

    // At any depth, and a run's group is still read.
    let finding = only("a OR (b event_type in (x))");
    assert_eq!(finding.span, 8..23);
    let report = advice("event_type in (-service=x)");
    let kinds: Vec<_> = report
        .findings
        .iter()
        .map(|f| (f.kind, f.span.clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (AdviceKind::SearchStageIn, 0..15),
            (AdviceKind::NegatedFieldShape, 15..25),
        ]
    );
    // A run inside a matched group.
    let report = advice("f not in (g in (x))");
    let kinds: Vec<_> = report
        .findings
        .iter()
        .map(|f| (f.kind, f.span.clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (AdviceKind::SearchStageIn, 0..10),
            (AdviceKind::SearchStageIn, 10..16),
        ]
    );
    // Two runs in one sequence.
    let report = advice("f in (a) g NOT in (b)");
    let spans: Vec<_> = report.findings.iter().map(|f| f.span.clone()).collect();
    assert_eq!(spans, [0..6, 9..19]);
}

#[test]
fn advice_non_triggers_stay_silent() {
    for text in [
        "-debug",
        "-service=",
        "-123=x",
        "-ev-type=x",
        "-\"service=coredns\"",
        "\"-service=coredns\"",
        "service!=coredns",
        "NOT service=coredns",
        "NOT \"service=coredns\"",
        "event_type=a,b",
        "* | where event_type in (\"a\", \"b\")",
        "event_type in a,b",
        "in (a, b)",
        "-event_type in (a)",
        "event_type \"in\" (a)",
        "event_type OR in (a)",
        "event_type In (a)",
        "event_type last=1h in (a)",
        "event_type in last=1h (a)",
    ] {
        let report = advice(text);
        assert_eq!(report, AdviceReport::default(), "{text:?}");
    }
}

/// Field names the copy must spell so that each offered spelling reads as
/// what the copy claims: custom, dotted, `@`-led, grammar and search
/// keywords, expression keywords, and dotted names a keyword leads.
const NAMES: &[&str] = &[
    "service",
    "event_type",
    "_x1",
    "k8s.pod.name",
    "@host",
    "@host.name",
    "last",
    "earliest",
    "latest",
    "OR",
    "or",
    "NOT",
    "not",
    "in",
    "IN",
    "AND",
    "true",
    "null",
    "OR.a",
    "not.a",
    "NOT.a",
    "true.a",
    "last.a",
    "by.x",
];

const SAMPLE: &str = "web01";

/// What an offered spelling claims to be.
#[derive(Debug, PartialEq)]
enum Claim {
    Filter(FilterOp),
    NotFilter(FilterOp),
    NotQuoted,
}

fn filter_op(op: &str) -> FilterOp {
    match op {
        "=" => FilterOp::Eq,
        "!=" => FilterOp::Ne,
        "<" => FilterOp::Lt,
        "<=" => FilterOp::Lte,
        ">" => FilterOp::Gt,
        ">=" => FilterOp::Gte,
        other => panic!("operator {other:?}"),
    }
}

/// The spellings a negated-field-shape hint offers, in order: each word
/// holding `…`, with the `NOT` before it when there is one, and its
/// trailing punctuation cut.
fn offered_spellings(hint: &str) -> Vec<String> {
    let words: Vec<&str> = hint.split(' ').collect();
    let mut spellings = Vec::new();
    for (i, word) in words.iter().enumerate() {
        if !word.contains('…') {
            continue;
        }
        let word = word.trim_end_matches([';', ',']);
        if i > 0 && words[i - 1] == "NOT" {
            spellings.push(format!("NOT {word}"));
        } else {
            spellings.push(word.to_owned());
        }
    }
    spellings
}

/// The one top-level token `text` parses to.
fn single_token(text: &str) -> Query {
    let query = parsed(text);
    assert_eq!(query.search.groups.len(), 1, "{text:?}");
    assert_eq!(query.search.groups[0].len(), 1, "{text:?}");
    query
}

fn assert_field_filter(text: &str, token: &SearchToken, name: &str, op: FilterOp) {
    let SearchToken::FieldFilter(FieldFilter {
        field,
        op: read_op,
        value,
    }) = token
    else {
        panic!("{text:?} is no field filter");
    };
    assert_eq!(field, name, "{text:?}");
    assert_eq!(*read_op, op, "{text:?}");
    assert!(
        matches!(value, FilterValue::Single(Scalar::Exact(v)) if v == SAMPLE),
        "{text:?}"
    );
}

/// Checks the negated-field-shape copy for `-<name><op><sample>`.
fn check_negated_copy(name: &str, op: &str) {
    let term = format!("{name}{op}{SAMPLE}");
    let text = format!("-{term}");
    let finding = only(&text);
    assert_eq!(finding.kind, AdviceKind::NegatedFieldShape, "{text:?}");
    assert!(
        finding.message.starts_with(&format!("-{name}{op}… ")),
        "{text:?}: {}",
        finding.message
    );
    let claims = if op == "=" {
        vec![
            Claim::Filter(FilterOp::Ne),
            Claim::NotFilter(FilterOp::Eq),
            Claim::NotQuoted,
        ]
    } else {
        vec![Claim::NotFilter(filter_op(op)), Claim::NotQuoted]
    };
    let spellings = offered_spellings(&finding.hint);
    assert_eq!(spellings.len(), claims.len(), "{text:?}: {}", finding.hint);
    for (spelling, claim) in spellings.iter().zip(claims) {
        let spelled = spelling.replace('…', SAMPLE);
        let query = single_token(&spelled);
        let token = &query.search.groups[0][0].node;
        match claim {
            Claim::Filter(op) => assert_field_filter(&spelled, token, name, op),
            Claim::NotFilter(op) => {
                let SearchToken::Not(inner) = token else {
                    panic!("{spelled:?} is no NOT");
                };
                assert_field_filter(&spelled, &inner.node, name, op);
            }
            Claim::NotQuoted => {
                let SearchToken::Not(inner) = token else {
                    panic!("{spelled:?} is no NOT");
                };
                let SearchToken::QuotedSearch(QuotedSearch { phrase }) = &inner.node else {
                    panic!("{spelled:?} is no quoted search under NOT");
                };
                assert_eq!(phrase, &term, "{spelled:?}");
            }
        }
        // A spelling the copy offers draws no advice of its own.
        assert_eq!(advice(&spelled), AdviceReport::default(), "{spelled:?}");
    }
}

/// Checks the search-stage `in` copy for `<name> <keyword> (a, b)`, and
/// whether it applied: `false` when the search stage does not read
/// `name` there as a positive text term.
fn check_in_copy(name: &str, keyword: &str, negated: bool) -> bool {
    let text = format!("{name} {keyword} (a, b)");
    let Ok(query) = parser::parse(&text) else {
        return false;
    };
    let field_word = matches!(
        &query.search.groups.first().and_then(|g| g.first()).map(|t| &t.node),
        Some(SearchToken::TextSearch(TextSearch { term, negated: false })) if term == name
    );
    if !field_word {
        return false;
    }
    let finding = only(&text);
    assert_eq!(finding.kind, AdviceKind::SearchStageIn, "{text:?}");
    let (lead, rest) = finding
        .hint
        .split_once("; ")
        .unwrap_or_else(|| panic!("{text:?}: {}", finding.hint));
    let (_, list) = lead
        .rsplit_once(' ')
        .unwrap_or_else(|| panic!("{text:?}: {lead}"));
    let query = single_token(list);
    let SearchToken::FieldFilter(FieldFilter { field, op, value }) =
        &query.search.groups[0][0].node
    else {
        panic!("{list:?} is no field filter");
    };
    assert_eq!(field, name, "{list:?}");
    assert_eq!(
        *op,
        if negated { FilterOp::Ne } else { FilterOp::Eq },
        "{list:?}"
    );
    assert_eq!(
        value,
        &FilterValue::List(vec![
            Scalar::Exact("a".to_owned()),
            Scalar::Exact("b".to_owned())
        ]),
        "{list:?}"
    );
    let (_, stage) = rest
        .split_once("as in ")
        .unwrap_or_else(|| panic!("{text:?}: {rest}"));
    let expression = format!("* {stage}");
    let query = parsed(&expression);
    let [stage] = query.pipeline.as_slice() else {
        panic!("{expression:?} has one stage");
    };
    let PipeStage::Where(where_stage) = &stage.node else {
        panic!("{expression:?} is a where stage");
    };
    let list_test = match &where_stage.condition.node {
        Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } if negated => &operand.node,
        other if !negated => other,
        other => panic!("{expression:?}: {other:?}"),
    };
    let Expr::InList { expr, list } = list_test else {
        panic!("{expression:?} is no in list");
    };
    assert_eq!(expr.node, Expr::FieldRef(name.to_owned()), "{expression:?}");
    assert_eq!(list.len(), 2, "{expression:?}");
    true
}

#[test]
fn advice_copy_spellings_read_as_claimed() {
    let mut in_copy_read = Vec::new();
    for &name in NAMES {
        for op in ["=", "!=", "<", "<=", ">", ">="] {
            check_negated_copy(name, op);
        }
        for (keyword, negated) in [("in", false), ("not in", true), ("NOT in", true)] {
            if check_in_copy(name, keyword, negated) {
                in_copy_read.push((name, keyword));
            }
        }
    }
    // The skips above leave the names that need rendering covered.
    for name in [
        "service", "@host", "last", "true", "not.a", "true.a", "by.x", "in",
    ] {
        for keyword in ["in", "not in", "NOT in"] {
            assert!(
                in_copy_read.contains(&(name, keyword)),
                "the {keyword} copy was never read for {name:?}"
            );
        }
    }
}

#[test]
fn advice_is_capped_in_source_order() {
    let text = vec!["-a=b"; 100].join(" ");
    let report = advice(&text);
    assert_eq!(report.findings.len(), MAX_ADVICE_FINDINGS);
    assert_eq!(report.omitted, 92);
    let spans: Vec<_> = report.findings.iter().map(|f| f.span.clone()).collect();
    let expected: Vec<_> = (0..MAX_ADVICE_FINDINGS).map(|i| i * 5..i * 5 + 4).collect();
    assert_eq!(spans, expected);

    // Mixed kinds, nested and across OR groups, stay in source order.
    let text = "(x -b=1) f in (-c=2) OR NOT -d=3 g NOT in (h)";
    let report = advice(text);
    let found: Vec<_> = report
        .findings
        .iter()
        .map(|f| (f.kind, &text[f.span.clone()]))
        .collect();
    assert_eq!(
        found,
        [
            (AdviceKind::NegatedFieldShape, "-b=1"),
            (AdviceKind::SearchStageIn, "f in ("),
            (AdviceKind::NegatedFieldShape, "-c=2"),
            (AdviceKind::NegatedFieldShape, "-d=3"),
            (AdviceKind::SearchStageIn, "g NOT in ("),
        ]
    );
}

fn leaf(term: &str, negated: bool, span: std::ops::Range<usize>) -> Spanned<SearchToken> {
    Spanned::new(
        SearchToken::TextSearch(TextSearch {
            term: term.to_owned(),
            negated,
        }),
        span,
    )
}

fn query_of(token: Spanned<SearchToken>) -> Query {
    Query {
        search: SearchStage {
            groups: vec![vec![token]],
            time_filter: None,
            earliest: None,
            latest: None,
            hoisted: Vec::new(),
        },
        pipeline: Vec::new(),
    }
}

/// Runs `work` on a thread whose stack a recursive walk of these trees
/// would overflow.
fn on_small_stack(case: &str, work: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(case.to_owned())
        .stack_size(256 * 1024)
        .spawn(work)
        .expect("spawn")
        .join()
        .unwrap_or_else(|_| panic!("{case}: advice walks the tree on the heap"));
}

/// The trees are built in code: the parser refuses their text for its
/// depth. Nothing here clones, compares or debug-prints a deep tree, all
/// of which recurse.
#[test]
fn advice_walks_deep_trees_on_the_heap() {
    const LEVELS: usize = 100_000;

    on_small_stack("brackets", || {
        // The deepest bracket holds `f in (-a=b)`.
        let mut tree = Spanned::new(
            SearchToken::Group(vec![vec![leaf("a=b", true, 7..11)]]),
            5..12,
        );
        tree = Spanned::new(
            SearchToken::Group(vec![vec![
                leaf("f", false, 1..2),
                leaf("in", false, 3..5),
                tree,
            ]]),
            0..13,
        );
        for _ in 0..LEVELS {
            tree = Spanned::new(SearchToken::Group(vec![vec![tree]]), 0..13);
        }
        let report = advise(&query_of(tree));
        let found: Vec<_> = report
            .findings
            .iter()
            .map(|f| (f.kind, f.span.clone()))
            .collect();
        assert_eq!(
            found,
            [
                (AdviceKind::SearchStageIn, 1..6),
                (AdviceKind::NegatedFieldShape, 7..11),
            ]
        );
    });

    on_small_stack("NOT run", || {
        let mut tree = leaf("a=b", true, 4..8);
        for _ in 0..LEVELS {
            tree = Spanned::new(SearchToken::Not(Box::new(tree)), 0..8);
        }
        let report = advise(&query_of(tree));
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].span, 4..8);
    });

    on_small_stack("mixed", || {
        let mut tree = leaf("a=b", true, 0..4);
        for level in 0..LEVELS {
            let node = if level % 2 == 0 {
                SearchToken::Not(Box::new(tree))
            } else {
                SearchToken::Group(vec![
                    vec![leaf("x", false, 0..1), tree],
                    vec![leaf("y", false, 0..1)],
                ])
            };
            tree = Spanned::new(node, 0..4);
        }
        let report = advise(&query_of(tree));
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.omitted, 0);
    });
}
