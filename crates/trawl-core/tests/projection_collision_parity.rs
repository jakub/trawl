// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Projection-name collisions are one check, stated the same way in both
//! lanes (ADR-0013 ruling 8).
//!
//! A projecting stage's output schema has to be deterministic: two
//! columns of one name is a query whose answer depends on which producer
//! the engine happens to bind. The SQL lane refuses it in
//! `emitter::validate_pipeline`; the stream lane, which never runs that,
//! refuses it in `stream::compile_stream_plan`. Both read the one
//! function, so the sentence a user sees cannot drift between batch and
//! live.

use trawl_core::emitter::validate_pipeline;
use trawl_core::pin_scope::PinScope;
use trawl_core::projection::check_projection;
use trawl_core::stream::compile_stream_plan;

/// Every collision shape, with the two producers the message must name.
const COLLISIONS: &[(&str, &str, &[&str])] = &[
    // stats: a group key against an aggregate's default name
    (
        "* | stats count() by count",
        "count",
        &["group key", "count()"],
    ),
    // stats: two aggregates whose default names coincide
    ("* | stats count(), count()", "count", &["count()"]),
    // stats: explicit aliases colliding after the ASCII fold
    (
        "* | stats count() as total, sum(x) as Total",
        "total",
        &["as total", "as Total"],
    ),
    // stats: an alias against a group key
    (
        "* | stats count() as host by host",
        "host",
        &["group key", "as host"],
    ),
    // timechart: the implicit bucket column against a group key
    (
        "* | timechart span=1h count() by _time",
        "_time",
        &["_time", "group key"],
    ),
    // timechart: an aggregate's default name against a group key
    (
        "* | timechart span=1h count() by count",
        "count",
        &["group key", "count()"],
    ),
    // top/rare: the frequency column the desugar mints
    ("* | top 5 count", "count", &["count"]),
    ("* | rare 5 host by count", "count", &["group key", "count"]),
    // pivot: the group keys, the only static names it projects
    (
        "* | pivot count() on status by host, Host",
        "host",
        &["group key"],
    ),
    // eventstats: duplicate explicit aliases
    (
        "* | eventstats count() as n, sum(x) as N",
        "n",
        &["as n", "as N"],
    ),
];

/// Shapes that must keep working: the check is about duplicate output
/// names, not about names that merely look alike.
const CLEAN: &[&str] = &[
    "* | stats count() by host",
    "* | stats count() as total by count",
    "* | stats count(), avg(dur), max(dur) by host, service",
    "* | timechart span=1h count() by service",
    "* | timechart span=1h count(), avg(dur)",
    "* | top 5 host",
    "* | top 5 host by service",
    "* | rare 5 status by service",
    "* | pivot count() on status by host",
    "* | eventstats count() as n by host",
    // An alias naming a column the rows already carry is the documented
    // `let`-like overwrite, not a collision: `eventstats` projects `*`
    // plus its aliases, so there is exactly one `host` column left.
    "* | eventstats count() as host by host",
    // `by` keys are not aggregate outputs, so a group key may name the
    // field an aggregate reads.
    "* | stats avg(dur) by dur",
];

fn stages(dsl: &str) -> Vec<trawl_core::ast::Spanned<trawl_core::ast::PipeStage>> {
    trawl_core::parser::parse(dsl)
        .unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"))
        .pipeline
}

#[test]
fn both_lanes_refuse_a_collision_with_the_identical_sentence() {
    for (dsl, name, producers) in COLLISIONS {
        let pipeline = stages(dsl);
        let message = check_projection(&pipeline[0].node)
            .expect_err(&format!("{dsl}: the shared check must refuse it"));

        assert!(
            message.contains(&format!("`{name}`")),
            "{dsl}: the message must name the colliding column: {message}"
        );
        for producer in *producers {
            assert!(
                message.contains(producer),
                "{dsl}: the message must name the producer {producer:?}: {message}"
            );
        }

        let sql_err = validate_pipeline(&pipeline)
            .expect_err(&format!("{dsl}: the SQL lane must refuse it"))
            .to_string();
        assert!(
            sql_err.contains(&message),
            "{dsl}: SQL lane sentence drifted: {sql_err}"
        );

        let stream_err = compile_stream_plan(&pipeline, &PinScope::unpinned())
            .expect_err(&format!("{dsl}: the stream lane must refuse it"))
            .to_string();
        assert_eq!(
            stream_err, message,
            "{dsl}: the stream lane states the shared sentence verbatim"
        );
    }
}

/// `eventstats` requires an explicit `as`: the live lane cannot know a
/// row's schema in advance, so an auto-named window column is a schema
/// only the batch engine could predict.
#[test]
fn eventstats_without_an_alias_is_refused_in_both_lanes() {
    let pipeline = stages("* | eventstats count() by host");
    let message = check_projection(&pipeline[0].node).expect_err("missing alias must be refused");
    assert!(
        message.contains("as") && message.contains("eventstats"),
        "the message must demand an explicit `as`: {message}"
    );

    let sql_err = validate_pipeline(&pipeline)
        .expect_err("the SQL lane must refuse it")
        .to_string();
    assert!(sql_err.contains(&message), "{sql_err}");

    let stream_err = compile_stream_plan(&pipeline, &PinScope::unpinned())
        .expect_err("the stream lane must refuse it")
        .to_string();
    assert_eq!(stream_err, message);
}

/// The collision check runs before the stream lane's own
/// unsupported-stage refusals, so a `pivot`/`eventstats` collision gets
/// the shared semantic answer rather than "not supported in streaming
/// mode": the sentence is about the query, not about the transport.
#[test]
fn the_shared_check_precedes_the_stream_lanes_own_refusals() {
    for dsl in [
        "* | pivot count() on status by host, Host",
        "* | eventstats count() as n, sum(x) as N",
    ] {
        let pipeline = stages(dsl);
        let message = check_projection(&pipeline[0].node).expect_err("refused");
        let stream_err = compile_stream_plan(&pipeline, &PinScope::unpinned())
            .expect_err("the stream lane must refuse it")
            .to_string();
        assert_eq!(stream_err, message, "{dsl}");
        assert!(
            !stream_err.contains("streaming mode"),
            "{dsl}: the semantic answer must win: {stream_err}"
        );
    }
}

#[test]
fn clean_projections_stay_clean_in_both_lanes() {
    for dsl in CLEAN {
        let pipeline = stages(dsl);
        for stage in &pipeline {
            assert!(
                check_projection(&stage.node).is_ok(),
                "{dsl}: must not be refused"
            );
        }
        assert!(validate_pipeline(&pipeline).is_ok(), "{dsl}: SQL lane");
        // The stream lane may still refuse the stage (pivot, eventstats,
        // sort), but never for a projection collision.
        if let Err(err) = compile_stream_plan(&pipeline, &PinScope::unpinned()) {
            let text = err.to_string();
            assert!(
                text.contains("not supported in streaming mode"),
                "{dsl}: unexpected stream refusal: {text}"
            );
        }
    }
}
