// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The live lane's sampling boundaries (ADR-0017 §3), under a FAKE
//! clock.
//!
//! Two boundaries, and they are not the same boundary:
//!
//! - **pass-through**: ONE instant per EVENT. A subscription's clock
//!   advances between rows while each row is internally frozen — every
//!   `now()` a row reads, in the search-stage window and in every stage
//!   after it, is one value.
//! - **aggregate snapshots**: ONE instant per emitted SNAPSHOT, for all
//!   its post-stage rows. The events that FED that snapshot each sampled
//!   their own instant, possibly much earlier.
//!
//! Nothing here reads a clock: every case names its instants, so a
//! failure is a semantic change and never a race. That is also the
//! property under test — an evaluation path that reached for
//! `Utc::now()` could not be pinned like this at all.

use serde_json::{Map, Value, json};
use trawl_core::context::EvalContext;
use trawl_core::filter::CompiledFilter;
use trawl_core::pin_scope::PinScope;
use trawl_core::row::{self, Row};
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{
    self, CompiledAggregation, CompiledStage, LiveOutcome, SnapshotContext, StageResult,
    StreamPlan, compile_stream_plan,
};

/// An instant, named rather than sampled.
fn at(text: &str) -> EvalContext {
    EvalContext::at(
        chrono::DateTime::parse_from_rfc3339(text)
            .expect("literal is RFC 3339")
            .with_timezone(&chrono::Utc),
    )
}

/// The text a cell holding that instant renders as — the same renderer
/// the wire and a group key go through, so "byte-identical" is what is
/// actually asserted.
fn now_text(ctx: &EvalContext) -> String {
    row::cell_text(&ctx.now_value())
}

fn event(value: &Value) -> Map<String, Value> {
    value.as_object().expect("an object").clone()
}

fn cell(row: &Row, key: &str) -> String {
    row::cell_text(row.get(key).unwrap_or_else(|| panic!("no cell {key:?}")))
}

/// Compile a whole DSL the way `stream_query` does: one snapshot roots
/// the filter and the plan.
fn compile(dsl: &str) -> (CompiledFilter, StreamPlan) {
    let query = trawl_core::parser::parse(dsl).expect("dsl parses");
    let filter =
        CompiledFilter::compile(&query.search, &FieldTypes::new()).expect("filter compiles");
    let plan = compile_stream_plan(&query.pipeline, &PinScope::unpinned()).expect("plan compiles");
    (filter, plan)
}

fn pass_through(dsl: &str) -> (CompiledFilter, Vec<CompiledStage>) {
    match compile(dsl) {
        (filter, StreamPlan::PassThrough(stages)) => (filter, stages),
        _ => panic!("{dsl} must compile to a pass-through plan"),
    }
}

type AggregatePlan = (
    CompiledFilter,
    Vec<CompiledStage>,
    CompiledAggregation,
    Vec<CompiledStage>,
);

fn aggregate(dsl: &str) -> AggregatePlan {
    match compile(dsl) {
        (
            filter,
            StreamPlan::Aggregate {
                pre_stages,
                aggregation,
                post_stages,
            },
        ) => (filter, pre_stages, aggregation, post_stages),
        _ => panic!("{dsl} must compile to an aggregate plan"),
    }
}

// ── pass-through: one instant per event ────────────────────────────

/// AC2. Every `now()` one row reads is ONE value — across the SIBLING
/// assignments of a single `let`, and across a LATER stage — while the
/// subscription's clock advances between rows.
///
/// The `| where echo == third` is the load-bearing half: if the two
/// stages sampled separately, the row would not be emitted at all.
#[test]
fn one_event_freezes_now_across_siblings_and_a_later_stage() {
    let (filter, mut stages) = pass_through(
        "* | let echo = now(), echo2 = now() | let third = now() | where echo == third",
    );

    let first = at("2026-08-24T12:00:00.500000Z");
    let second = at("2026-08-24T12:00:41.250000Z");
    let ev = event(&json!({"host": "web-1", "message": "hello"}));

    let LiveOutcome::Emit(row) = stream::accept_event(&filter, &mut stages, &ev, &first) else {
        panic!("the row must survive: its two stages must read ONE instant");
    };
    assert_eq!(cell(&row, "echo"), now_text(&first));
    assert_eq!(
        cell(&row, "echo2"),
        cell(&row, "echo"),
        "sibling assignments of one `let` read one instant"
    );
    assert_eq!(
        cell(&row, "third"),
        cell(&row, "echo"),
        "a later stage reads the SAME instant, not a fresher one"
    );

    let LiveOutcome::Emit(next) = stream::accept_event(&filter, &mut stages, &ev, &second) else {
        panic!("the second row must survive too");
    };
    assert_eq!(cell(&next, "echo"), now_text(&second));
    assert_ne!(
        cell(&next, "echo"),
        cell(&row, "echo"),
        "the subscription's clock ADVANCES between rows — per-batch \
         sampling would have frozen both rows at one instant"
    );
}

/// AC2, the defect itself: the search-stage window and the pipeline are
/// one `now()` reader, not two.
///
/// The event is crafted so its fate DISAGREES between two instants 90
/// seconds apart: at `admits`, a 60-second window admits it and
/// `_time >= now()` holds exactly; at `rejects`, the window has moved
/// past it and the comparison is false. Under two clocks — the shape
/// the SSE loop had, a per-batch sample for the filter and a per-event
/// one for the stages — the row is admitted by one and dropped by the
/// other, in either direction. The door answers once.
#[test]
fn the_filter_and_the_stages_cannot_read_different_clocks() {
    let (filter, mut stages) = pass_through("last=60s | where _time >= now()");
    let admits = at("2026-08-24T12:00:00Z");
    let rejects = at("2026-08-24T12:01:30Z");
    let ev = event(&json!({"_time": "2026-08-24T12:00:00Z", "message": "hello"}));

    // The split, simulated by hand — the API no longer permits it.
    // Stale filter sample, fresh stage sample: the window admits the
    // event and the stage then silently drops it.
    assert!(
        filter.matches_at(&ev, &admits),
        "the window admits it at the earlier instant"
    );
    let mut row = row::from_json(&ev);
    let survives_fresh = stages
        .iter_mut()
        .all(|stage| stream::apply_stage(stage, &mut row, &rejects) == StageResult::Pass);
    assert!(
        !survives_fresh,
        "a fresher stage clock drops the row the window admitted"
    );

    // And the reverse: the window rejects an event the stage would have
    // kept.
    assert!(!filter.matches_at(&ev, &rejects));
    let mut row = row::from_json(&ev);
    let survives_stale = stages
        .iter_mut()
        .all(|stage| stream::apply_stage(stage, &mut row, &admits) == StageResult::Pass);
    assert!(survives_stale, "the stage keeps it at the earlier instant");

    // One context per event: one answer, whichever instant it is.
    assert!(
        matches!(
            stream::accept_event(&filter, &mut stages, &ev, &admits),
            LiveOutcome::Emit(_)
        ),
        "admitted by the window AND kept by the stage"
    );
    assert_eq!(
        stream::accept_event(&filter, &mut stages, &ev, &rejects),
        LiveOutcome::Filtered,
        "rejected by the window, and never half-processed"
    );
}

/// The door's `Done` is what ends a subscription, and it does not emit
/// the event that produced it — the handler stops at the first one.
///
/// It also does not UN-say it. The subscription's events arrive in bus
/// batches, and the handler's `break` only covers the batch in hand; a
/// door that answered `Done` once and then went back to emitting would
/// put the next batch's events on the wire past `limit N`. The extra
/// calls below are that next batch.
#[test]
fn a_limit_reports_done_rather_than_emitting() {
    let (filter, mut stages) = pass_through("* | limit 1");
    let ctx = at("2026-08-24T12:00:00Z");
    let ev = event(&json!({"message": "hello"}));

    assert!(matches!(
        stream::accept_event(&filter, &mut stages, &ev, &ctx),
        LiveOutcome::Emit(_)
    ));
    for attempt in 0..4 {
        assert_eq!(
            stream::accept_event(&filter, &mut stages, &ev, &ctx),
            LiveOutcome::Done,
            "call {attempt} after the limit must stay Done, never Emit"
        );
    }
}

/// The aggregate lane's pre-stages honour a `limit` too: exactly N
/// events reach the accumulators.
///
/// This lane never stops asking — an aggregate subscription keeps
/// running so its snapshots stay current — so a `Done` that fired once
/// and then lapsed let every later event feed the accumulators, and the
/// snapshot counted them.
#[test]
fn a_pre_stage_limit_caps_what_reaches_the_accumulators() {
    let (filter, mut pre_stages, mut aggregation, mut post_stages) =
        aggregate("* | limit 1 | stats count()");
    let ctx = at("2026-08-24T12:00:00Z");

    let mut admitted = 0;
    for i in 0..5 {
        let ev = event(&json!({"message": format!("event-{i}")}));
        if stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            &ctx,
        ) {
            admitted += 1;
        }
    }
    assert_eq!(admitted, 1, "`limit 1` admits one event, then none");

    let (_, rows) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(at("2026-08-24T12:05:00Z")),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        cell(&rows[0], "count"),
        "1",
        "the snapshot counts the events the limit ADMITTED: {rows:?}"
    );
}

/// A `limit` after the aggregation caps the SNAPSHOT's rows.
///
/// Three groups, `limit 1`: the snapshot carries one row. A lapsing
/// `Done` dropped the second row and then let the third through, which
/// is both over the limit and a silently arbitrary row set.
#[test]
fn a_post_stage_limit_caps_a_snapshots_rows() {
    let (filter, mut pre_stages, mut aggregation, mut post_stages) =
        aggregate("* | stats count() by host | limit 1");
    let ctx = at("2026-08-24T12:00:00Z");

    for host in ["web-1", "web-2", "web-3"] {
        let ev = event(&json!({"host": host, "message": "hello"}));
        assert!(stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            &ctx,
        ));
    }

    let (_, rows) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(at("2026-08-24T12:05:00Z")),
    );
    assert_eq!(rows.len(), 1, "one row, not one DROPPED row: {rows:?}");
}

// ── aggregates: one instant per emitted snapshot ───────────────────

/// AC3. Every post-stage row of ONE snapshot reads ONE instant — the
/// snapshot's, not any feeding event's — and the next snapshot reads the
/// next one.
///
/// Two groups, so "one instant per snapshot" is discriminable from "one
/// instant per row"; four feeding events at four distinct instants, so
/// a leak from the feed lane would be visible; and
/// `| where seen == seen2` drops the row outright if a snapshot's own
/// two reads ever disagree.
#[test]
fn a_snapshot_stamps_one_instant_on_every_group_row() {
    let (filter, mut pre_stages, mut aggregation, mut post_stages) = aggregate(
        "* | stats count() by host | let seen = now(), seen2 = now() | where seen == seen2",
    );

    let feeds = [
        (at("2026-08-24T11:00:00Z"), "web-1"),
        (at("2026-08-24T11:00:10Z"), "web-1"),
        (at("2026-08-24T11:00:20Z"), "web-2"),
        (at("2026-08-24T11:00:30Z"), "web-2"),
    ];
    for (ctx, host) in &feeds {
        let ev = event(&json!({"host": host, "message": "hello"}));
        assert!(stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            ctx,
        ));
    }

    let first = at("2026-08-24T11:05:00.000001Z");
    let (columns, rows) =
        stream::emit_snapshot(&aggregation, &mut post_stages, &SnapshotContext::new(first));
    assert!(columns.contains(&"host".to_owned()));
    assert_eq!(rows.len(), 2, "one row per group: {rows:?}");
    for row in &rows {
        assert_eq!(
            cell(row, "seen"),
            now_text(&first),
            "every group row carries the SNAPSHOT's instant"
        );
        assert_eq!(cell(row, "seen2"), cell(row, "seen"));
        for (ctx, _) in &feeds {
            assert_ne!(
                cell(row, "seen"),
                now_text(ctx),
                "a feeding event's instant must not reach the snapshot"
            );
        }
    }

    // A second snapshot over the same accumulators reads the NEXT
    // instant: the boundary is the emission, not the aggregation.
    let second = at("2026-08-24T11:05:30Z");
    let (_, rows) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(second),
    );
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(cell(row, "seen"), now_text(&second));
    }
}

/// The same for a `timechart` snapshot: the span buckets come from the
/// events, the `now()` comes from the emission.
#[test]
fn a_timechart_snapshot_stamps_one_instant_too() {
    let (filter, mut pre_stages, mut aggregation, mut post_stages) =
        aggregate("* | timechart span=1m count() | let seen = now()");

    for (ctx, time) in [
        (at("2026-08-24T11:00:00Z"), "2026-08-24T09:00:10Z"),
        (at("2026-08-24T11:00:05Z"), "2026-08-24T09:01:10Z"),
    ] {
        let ev = event(&json!({"_time": time, "message": "hello"}));
        assert!(stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            &ctx,
        ));
    }

    let snapshot = at("2026-08-24T11:00:09.750000Z");
    let (_, rows) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(snapshot),
    );
    assert_eq!(rows.len(), 2, "two span buckets: {rows:?}");
    for row in &rows {
        assert_eq!(cell(row, "seen"), now_text(&snapshot));
    }
    let buckets: Vec<String> = rows.iter().map(|row| cell(row, "_time")).collect();
    assert_eq!(
        buckets,
        vec![
            "2026-08-24T09:00:00+00:00".to_owned(),
            "2026-08-24T09:01:00+00:00".to_owned()
        ],
        "the buckets are the EVENTS' own hours, hours before the snapshot"
    );
}

/// The snapshot instant is the caller's sample point, taken before the
/// rows are: a post-stage that drops a group does not move it for the
/// survivors.
#[test]
fn a_dropped_group_does_not_move_the_survivors_instant() {
    let (filter, mut pre_stages, mut aggregation, mut post_stages) =
        aggregate("* | stats count() by host | let seen = now() | where count > 1");

    let ctx = at("2026-08-24T11:00:00Z");
    for host in ["web-1", "web-1", "web-2"] {
        let ev = event(&json!({"host": host, "message": "hello"}));
        assert!(stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            &ctx,
        ));
    }

    let snapshot = at("2026-08-24T11:09:09Z");
    let (_, rows) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(snapshot),
    );
    assert_eq!(rows.len(), 1, "web-2's single event is dropped: {rows:?}");
    assert_eq!(cell(&rows[0], "seen"), now_text(&snapshot));
}

/// The other half of ADR-0017 §3's aggregate rule: the events that FEED
/// a snapshot sample PER EVENT, exactly as pass-through does.
///
/// Grouping on the pre-stage `now()` makes that visible in the snapshot:
/// two events under one instant are one group, a third under another
/// instant is a second group. A per-snapshot feed instant would give one
/// group of three.
#[test]
fn pre_stage_rows_sample_per_event() {
    let (filter, mut pre_stages, mut aggregation, mut post_stages) =
        aggregate("* | let seen = now() | stats count() by seen");

    let early = at("2026-08-24T11:00:00Z");
    let late = at("2026-08-24T11:00:01Z");
    for ctx in [&early, &early, &late] {
        let ev = event(&json!({"message": "hello"}));
        assert!(stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            ctx,
        ));
    }

    let (_, rows) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(at("2026-08-24T11:30:00Z")),
    );
    let mut seen: Vec<(String, String)> = rows
        .iter()
        .map(|row| (cell(row, "seen"), cell(row, "count")))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            (now_text(&early), "2".to_owned()),
            (now_text(&late), "1".to_owned()),
        ],
        "two feed instants, two groups"
    );
}

// ── the timechart fallback reads the EVENT's context ───────────────

/// An event with no `_time` buckets at ITS OWN context's instant.
///
/// The instants here are in 2020, so a second clock read — the
/// `Utc::now()` this fallback used to take — would land the event six
/// years away from the bucket asserted. The pair straddles a span
/// boundary by one second, which is the finer failure the coarse one
/// hides: the bucket a bucketless event lands in is decided by the
/// instant its own filter and stages read, not by a later one.
#[test]
fn a_bucketless_event_buckets_at_its_own_contexts_instant() {
    let (filter, mut pre_stages, mut aggregation, mut post_stages) =
        aggregate("* | timechart span=1m count()");

    for ctx in [
        at("2020-03-04T05:05:59.999999Z"),
        at("2020-03-04T05:06:00Z"),
    ] {
        let ev = event(&json!({"message": "hello"}));
        assert!(stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            &ctx,
        ));
    }

    let (_, rows) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(at("2026-08-24T11:00:00Z")),
    );
    let buckets: Vec<String> = rows.iter().map(|row| cell(row, "_time")).collect();
    assert_eq!(
        buckets,
        vec![
            "2020-03-04T05:05:00+00:00".to_owned(),
            "2020-03-04T05:06:00+00:00".to_owned()
        ],
        "one bucket each side of the boundary, from the EVENTS' contexts \
         — never the snapshot's, and never a fresh clock's"
    );
}

/// A post-stage `limit` caps EVERY snapshot, not the plan's lifetime.
///
/// ADR-0001 makes batch the contract and streaming the mirror, and an
/// emitted snapshot is the live rendering of the batch result set: what
/// `| stats count() by host | limit 1` caps there is one result set, so
/// here it caps each snapshot. Spending the limit once would leave the
/// stream permanently silent while the aggregation kept evolving —
/// a mirror of nothing.
///
/// The asymmetry with a PRE-aggregation `limit`, which stays sticky for
/// the subscription's life, is the batch-mirroring one: that end caps
/// the INPUT set, and batch reads its input once.
#[test]
fn a_post_stage_limit_is_re_armed_for_each_snapshot() {
    let (filter, mut pre_stages, mut aggregation, mut post_stages) =
        aggregate("* | stats count() by host | limit 1");
    let ctx = at("2026-08-24T12:00:00Z");

    for host in ["web-1", "web-2", "web-3"] {
        let ev = event(&json!({"host": host, "message": "hello"}));
        assert!(stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            &ctx,
        ));
    }

    let (_, first) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(at("2026-08-24T12:05:00Z")),
    );
    assert_eq!(first.len(), 1, "the first snapshot carries its one row");
    let (_, second) = stream::emit_snapshot(
        &aggregation,
        &mut post_stages,
        &SnapshotContext::new(at("2026-08-24T12:05:30Z")),
    );
    assert_eq!(second.len(), 1, "and so does the next one: {second:?}");

    // A pre-stage limit is the other rule, in the same plan shape: it
    // caps the input once and stays spent.
    let (filter, mut pre_stages, mut aggregation, mut post_stages) =
        aggregate("* | limit 1 | stats count()");
    for i in 0..3 {
        let ev = event(&json!({"message": format!("event-{i}")}));
        let admitted = stream::accept_event_into_aggregate(
            &filter,
            &mut pre_stages,
            &mut aggregation,
            &ev,
            &ctx,
        );
        assert_eq!(admitted, i == 0, "only the first event feeds");
    }
    for snapshot in 0..2 {
        let (_, rows) = stream::emit_snapshot(
            &aggregation,
            &mut post_stages,
            &SnapshotContext::new(at("2026-08-24T12:06:00Z")),
        );
        assert_eq!(
            cell(&rows[0], "count"),
            "1",
            "snapshot {snapshot} counts the one admitted event"
        );
    }
}
