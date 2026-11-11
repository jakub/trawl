// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The longest deep texts `MAX_QUERY_LEN` admits end in an answer on a
//! tokio worker's 2 MiB stack, never in a stack overflow (#447).
//!
//! Each chain, run and bracket nest the grammar can spell in 64 KiB is
//! refused for its tree depth, and a deep text followed by a grammar
//! error is refused for the grammar error. The grammar builds every one of
//! these trees in full before it is refused, so the parse also drops it on
//! this stack. A chain of `let` aliases is no level deep, so it parses,
//! and the checks after the parse walk it.
//!
//! A parse cancelled after the grammar drops a deep tree too; the parser's
//! own tests cover it
//! (`parser::tests::a_parse_cancelled_after_the_grammar_drops_a_deep_tree`),
//! because only they can request the stop at that point.

use trawl_core::complexity::check_pipeline_complexity;
use trawl_core::emitter::validate_pipeline;
use trawl_core::parser::{MAX_QUERY_LEN, MAX_TREE_DEPTH, parse};

const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;

const DEEP_EXPRESSION: &str = "expression is deeper than 64 levels";
const DEEP_SEARCH: &str = "search is deeper than 64 levels";

/// Names the phase about to run, so a stack overflow, which aborts the
/// process with no unwinding, still says where it died.
fn phase(case: &str, phase: &str) {
    eprintln!("case={case} phase={phase}");
}

/// The text `prefix`, then as many `unit`s as fit `MAX_QUERY_LEN` with
/// `core` and `suffix`, then `core`, then `suffix`. A unit is one level of
/// a run, or one term and its operator in a chain.
fn longest(prefix: &str, unit: &str, core: &str, suffix: &str) -> String {
    let units = (MAX_QUERY_LEN - prefix.len() - core.len() - suffix.len()) / unit.len();
    let text = format!("{prefix}{}{core}{suffix}", unit.repeat(units));
    assert!(
        MAX_QUERY_LEN - text.len() < unit.len(),
        "{prefix}… is longest"
    );
    text
}

/// `(` × n, `core`, `)` × n, as long as `MAX_QUERY_LEN` admits.
fn longest_brackets(core: &str) -> String {
    let n = (MAX_QUERY_LEN - core.len()) / 2;
    let text = format!("{}{core}{}", "(".repeat(n), ")".repeat(n));
    assert!(MAX_QUERY_LEN - text.len() < 2, "brackets around {core}");
    text
}

#[test]
fn the_longest_texts_return_on_a_worker_stack() {
    assert_eq!(MAX_TREE_DEPTH, 64);
    let refused = [
        // `1+` is one term and one level of a chain
        (
            "where chain",
            longest("* | where ", "1+", "1", " > 0"),
            DEEP_EXPRESSION,
        ),
        (
            "let chain",
            longest("* | let x = ", "1+", "1", ""),
            DEEP_EXPRESSION,
        ),
        (
            "stats chain",
            longest("* | stats sum(", "1+", "1", ")"),
            DEEP_EXPRESSION,
        ),
        (
            "chain as in subject",
            longest("* | where ", "1+", "1", " in (1)"),
            DEEP_EXPRESSION,
        ),
        (
            "chain as in element",
            longest("* | where a in (1, ", "1+", "1", ")"),
            DEEP_EXPRESSION,
        ),
        (
            "chain as call argument",
            longest("* | let x = abs(", "1+", "1", ")"),
            DEEP_EXPRESSION,
        ),
        // `-` repeats with no space between
        (
            "minus run",
            longest("* | where ", "-", "1", ""),
            DEEP_EXPRESSION,
        ),
        (
            "not run",
            longest("* | where ", "not ", "a", ""),
            DEEP_EXPRESSION,
        ),
        ("NOT run", longest("", "NOT ", "a", ""), DEEP_SEARCH),
        ("brackets", longest_brackets("a"), DEEP_SEARCH),
    ];
    let chain = vec!["1"; 32_000].join("+");
    let grammar_errors = [
        (
            "deep expression, then a grammar error",
            format!("* | where {chain} > 0 | where"),
        ),
        (
            "deep search, then a grammar error",
            format!("{}a=1{} | where", "(".repeat(30_000), ")".repeat(30_000)),
        ),
    ];
    // a0 = 1, a1 = a0, …: each value one name, so no level deep
    let links: Vec<String> = (1..4_000).map(|i| format!("a{i} = a{}", i - 1)).collect();
    let aliases = format!("* | let a0 = 1, {}", links.join(", "));
    assert!(aliases.len() <= MAX_QUERY_LEN, "{}", aliases.len());

    std::thread::Builder::new()
        .stack_size(TOKIO_WORKER_STACK)
        .spawn(move || {
            for (case, dsl, sentence) in &refused {
                phase(case, "parse");
                let errors = parse(dsl).expect_err(case);
                assert_eq!(errors.len(), 1, "{case}: {errors:?}");
                assert_eq!(errors[0].message, *sentence, "{case}");
            }
            for (case, dsl) in &grammar_errors {
                assert!(dsl.len() <= MAX_QUERY_LEN, "{case}: {}", dsl.len());
                phase(case, "parse");
                let errors = parse(dsl).expect_err(case);
                assert!(
                    errors.iter().all(|e| !e.message.contains("deeper than")),
                    "{case}: the grammar error wins: {errors:?}"
                );
            }

            let case = "4 000-link let alias chain";
            phase(case, "parse");
            let query = parse(&aliases).expect("an alias chain is no level deep");
            phase(case, "check_pipeline_complexity");
            drop(check_pipeline_complexity(&query.pipeline));
            phase(case, "validate_pipeline");
            drop(validate_pipeline(&query.pipeline));
            phase(case, "drop");
            drop(query);
        })
        .expect("spawn")
        .join()
        .expect("every case must return on a 2 MiB stack");
}
