// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The live driver: one per SSE stream (ADR-0063).
//!
//! The stream's evaluation state ([`LiveState`]) is one owned value that
//! moves into each job and comes back with the job's frames. While a job
//! runs the driver receives nothing, so the bus's broadcast ring is the
//! bounded queue between ingest and the stream, and a stream that falls
//! behind it gets the ring's `lagged` frame.

use std::convert::Infallible;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::Event;
use serde::Serialize;
use tokio::sync::OwnedSemaphorePermit;
use tokio_stream::Stream;
use trawl_core::cancel::Stop;
use trawl_core::context::EvalContext;
use trawl_core::filter::CompiledFilter;
use trawl_core::pin_scope::PinScope;
use trawl_core::stream::{
    LIVE_CHUNK_EVENTS, LiveEnd, LiveError, LiveRun, Snapshot, SnapshotContext, StreamPlan,
};

use crate::bus::{EventSubscriber, IngestBatch, RecvError};

const LIVE_CHUNK_STRING_BYTES: usize = 256 * 1024;

const LIVE_JOB_FRAME_BYTES: usize = 4 * LIVE_CHUNK_STRING_BYTES;

const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub(crate) struct LiveState {
    run: LiveRun,
    output_pins: PinScope,
    /// The stream's SSE slot. It drops with the state, so a job that
    /// outlives its stream keeps the slot until the job exits.
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug)]
pub(crate) struct Live {
    state: LiveState,
    stop: Stop,
    aggregate: bool,
}

impl Live {
    pub(crate) fn new(
        filter: CompiledFilter,
        plan: StreamPlan,
        stop: Stop,
        output_pins: PinScope,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        let aggregate = matches!(plan, StreamPlan::Aggregate { .. });
        Self {
            state: LiveState {
                run: LiveRun::new(filter, plan, stop.clone()),
                output_pins,
                _permit: permit,
            },
            stop,
            aggregate,
        }
    }
}

/// Each event samples its own instant when its job evaluates it (ADR-0017 §3).
pub(crate) fn frames<S: EventSubscriber>(
    subscriber: S,
    live: Live,
    query_id: u64,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    drive(
        subscriber,
        live,
        query_id,
        Jobs::new(EvalContext::capture, LIVE_CHUNK_EVENTS),
    )
}

#[allow(clippy::too_many_lines)]
fn drive<S, C>(
    mut subscriber: S,
    live: Live,
    query_id: u64,
    jobs: Jobs<C>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send
where
    S: EventSubscriber,
    C: Fn() -> EvalContext + Clone + Send + 'static,
{
    let Live {
        mut state,
        stop,
        aggregate,
    } = live;
    async_stream::stream! {
        let _stop = StopOnDrop(stop);
        let mut timer = aggregate.then(|| {
            let mut timer = tokio::time::interval_at(
                tokio::time::Instant::now() + SNAPSHOT_INTERVAL,
                SNAPSHOT_INTERVAL,
            );
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            timer
        });
        // A bus batch a chunk ended inside. Its rest runs before anything
        // else is received, so the timer fires only between bus batches.
        let mut carry: Option<Carry> = None;
        // The events a job handed back at its frame cap, and what ended
        // the gather they came from. They run next, before the carry, and
        // the driver acts on `held` once they have run.
        let mut handed_back: Vec<Piece> = Vec::new();
        let mut held = After::Nothing;
        let mut evaluated = false;

        loop {
            let (work, after) = if !handed_back.is_empty() {
                let after = std::mem::replace(&mut held, After::Nothing);
                (Work::Chunk(std::mem::take(&mut handed_back)), after)
            } else if let Some(start) = carry.take() {
                let (pieces, after) = gather(&mut subscriber, start, &mut carry, jobs.chunk_events);
                (Work::Chunk(pieces), after)
            } else {
                let received = match timer.as_mut() {
                    Some(timer) => tokio::select! {
                        received = subscriber.recv() => Some(received),
                        _ = timer.tick() => None,
                    },
                    None => Some(subscriber.recv().await),
                };
                match received {
                    Some(Ok(batch)) => {
                        let start = Carry { batch, from: 0 };
                        let (pieces, after) = gather(&mut subscriber, start, &mut carry, jobs.chunk_events);
                        (Work::Chunk(pieces), after)
                    }
                    Some(Err(RecvError::Lagged(missed))) => {
                        yield Ok(lagged_frame(query_id, missed));
                        continue;
                    }
                    Some(Err(RecvError::Closed)) => break,
                    None if evaluated => {
                        evaluated = false;
                        (Work::Tick, After::Nothing)
                    }
                    None => continue,
                }
            };
            if let Work::Chunk(pieces) = &work {
                evaluated |= pieces.iter().any(|piece| !piece.events.is_empty());
            }

            let Some((back, out)) = dispatch(state, work, jobs.clone()).await else {
                tracing::error!(
                    event_type = "stream_job_lost",
                    query_id,
                    "ending a live stream: its job ended without an answer"
                );
                break;
            };
            state = back;
            for frame in out.frames {
                yield Ok(frame);
            }
            if out.snapshot_taken
                && let Some(timer) = timer.as_mut()
            {
                timer.reset();
            }
            match out.end {
                None => {}
                Some(LiveEnd::Done) => break,
                Some(LiveEnd::Failed(LiveError::Broken(error))) => {
                    log_missing_prepared_pattern(query_id, &error);
                    break;
                }
                Some(LiveEnd::Failed(LiveError::Cancelled)) => {
                    tracing::debug!(
                        event_type = "stream_cancelled",
                        query_id,
                        "ending a live stream: its run was asked to stop"
                    );
                    break;
                }
            }
            if !out.handed_back.is_empty() {
                handed_back = out.handed_back;
                held = after;
                continue;
            }
            match after {
                After::Nothing => {}
                After::Lagged(missed) => yield Ok(lagged_frame(query_id, missed)),
                After::Closed => break,
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Jobs<C> {
    clock: C,
    chunk_events: usize,
    #[cfg(any(test, feature = "test-support"))]
    in_flight: seam::InFlight,
    #[cfg(any(test, feature = "test-support"))]
    job_times: Option<seam::JobTimes>,
    #[cfg(any(test, feature = "test-support"))]
    job_bytes: Option<seam::JobBytes>,
}

impl<C> Jobs<C> {
    fn new(clock: C, chunk_events: usize) -> Self {
        Self {
            clock,
            chunk_events,
            #[cfg(any(test, feature = "test-support"))]
            in_flight: seam::InFlight::default(),
            #[cfg(any(test, feature = "test-support"))]
            job_times: None,
            #[cfg(any(test, feature = "test-support"))]
            job_bytes: None,
        }
    }
}

/// Run `work` over `state` as one job on the blocking pool, and answer with
/// the state and what the job produced. `None` when the job ended without
/// an answer: it panicked, and its state is gone.
async fn dispatch<C>(state: LiveState, work: Work, jobs: Jobs<C>) -> Option<(LiveState, JobOutput)>
where
    C: Fn() -> EvalContext + Send + 'static,
{
    let (answer, answered) = tokio::sync::oneshot::channel();
    drop(tokio::task::spawn_blocking(move || {
        let mut state = state;
        #[cfg(any(test, feature = "test-support"))]
        let (entered, started) = (jobs.in_flight.enter(), std::time::Instant::now());
        let out = run_job(&mut state, work, &jobs.clock);
        #[cfg(any(test, feature = "test-support"))]
        {
            if let Some(times) = &jobs.job_times {
                times.record(started.elapsed());
            }
            if let Some(bytes) = &jobs.job_bytes {
                bytes.record(out.bytes);
            }
            drop(entered);
        }
        drop(answer.send((state, out)));
    }));
    answered.await.ok()
}

/// A batch whose events have all run is closed before the cap is read again,
/// so its threshold snapshot stays at its end.
fn run_job<C: Fn() -> EvalContext>(state: &mut LiveState, work: Work, clock: &C) -> JobOutput {
    let mut out = JobOutput::default();
    match work {
        Work::Tick => match state.run.snapshot(|| SnapshotContext::new(clock())) {
            Ok(snapshot) => out.push(snapshot.and_then(snapshot_frame)),
            Err(error) => return out.fail(error),
        },
        Work::Chunk(pieces) => {
            let mut pieces = pieces.into_iter();
            while let Some(piece) = pieces.next() {
                for at in piece.events.clone() {
                    if out.bytes >= LIVE_JOB_FRAME_BYTES {
                        let rest = Piece {
                            events: at..piece.events.end,
                            ..piece
                        };
                        out.handed_back = std::iter::once(rest).chain(pieces).collect();
                        return out;
                    }
                    // One event a call, so its rows are frames, and counted,
                    // before the next event runs.
                    let chunk = state
                        .run
                        .accept_chunk(std::iter::once(&piece.batch.events[at]), clock);
                    for row in chunk.rows {
                        out.push(data_frame(row, &state.output_pins));
                    }
                    match chunk.end {
                        None => {}
                        Some(LiveEnd::Done) => {
                            out.end = Some(LiveEnd::Done);
                            return out;
                        }
                        Some(LiveEnd::Failed(error)) => return out.fail(error),
                    }
                }
                if piece.closes_batch {
                    match state.run.end_of_batch(|| SnapshotContext::new(clock())) {
                        Ok(None) => {}
                        Ok(Some(snapshot)) => {
                            out.push(snapshot_frame(snapshot));
                            out.snapshot_taken = true;
                        }
                        Err(error) => return out.fail(error),
                    }
                }
            }
        }
    }
    out
}

#[derive(Debug)]
enum Work {
    Chunk(Vec<Piece>),
    Tick,
}

#[derive(Debug)]
struct Piece {
    batch: Arc<IngestBatch>,
    events: Range<usize>,
    closes_batch: bool,
}

#[derive(Debug)]
struct Carry {
    batch: Arc<IngestBatch>,
    from: usize,
}

/// What ended a gather short of a full chunk. The driver acts on it after
/// the chunk's frames, so a `lagged` frame follows the frames of the
/// events received before it.
#[derive(Debug, PartialEq, Eq)]
enum After {
    Nothing,
    Lagged(u64),
    Closed,
}

#[derive(Debug, Default)]
struct JobOutput {
    frames: Vec<Event>,
    bytes: usize,
    handed_back: Vec<Piece>,
    snapshot_taken: bool,
    end: Option<LiveEnd>,
}

impl JobOutput {
    fn push(&mut self, frame: Option<Frame>) {
        if let Some(Frame { event, bytes }) = frame {
            self.frames.push(event);
            self.bytes = self.bytes.saturating_add(bytes);
        }
    }

    fn fail(mut self, error: LiveError) -> Self {
        if error == LiveError::Cancelled {
            self.frames.clear();
            self.bytes = 0;
        }
        self.end = Some(LiveEnd::Failed(error));
        self
    }
}

/// It never waits for more. A batch the chunk ends inside is left in `carry`,
/// so at most one partly read batch is held.
fn gather<S: EventSubscriber>(
    subscriber: &mut S,
    start: Carry,
    carry: &mut Option<Carry>,
    chunk_events: usize,
) -> (Vec<Piece>, After) {
    let mut pieces = Vec::new();
    let (mut events, mut bytes) = (0, 0_usize);
    let Carry {
        mut batch,
        mut from,
    } = start;
    loop {
        let mut to = from;
        while let Some(event) = batch.events.get(to) {
            if events == chunk_events {
                break;
            }
            let size = string_payload(event);
            if events > 0 && bytes.saturating_add(size) > LIVE_CHUNK_STRING_BYTES {
                break;
            }
            events += 1;
            bytes = bytes.saturating_add(size);
            to += 1;
        }
        let closes_batch = to == batch.events.len();
        if to > from || closes_batch {
            pieces.push(Piece {
                batch: Arc::clone(&batch),
                events: from..to,
                closes_batch,
            });
        }
        if !closes_batch {
            *carry = Some(Carry { batch, from: to });
            return (pieces, After::Nothing);
        }
        if events == chunk_events {
            return (pieces, After::Nothing);
        }
        match subscriber.try_recv() {
            None => return (pieces, After::Nothing),
            Some(Ok(next)) => {
                batch = next;
                from = 0;
            }
            Some(Err(RecvError::Lagged(missed))) => return (pieces, After::Lagged(missed)),
            Some(Err(RecvError::Closed)) => return (pieces, After::Closed),
        }
    }
}

/// The bytes of string an event's values hold, nested ones included: what
/// its regex searches can read.
fn string_payload(event: &serde_json::Map<String, serde_json::Value>) -> usize {
    event.values().map(value_payload).sum()
}

fn value_payload(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    match value {
        Value::String(text) => text.len(),
        Value::Array(items) => items.iter().map(value_payload).sum(),
        Value::Object(fields) => string_payload(fields),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

/// Requests a stream's stop when the stream drops, so a job still running
/// its chunk ends at its next event or regex call.
#[derive(Debug)]
struct StopOnDrop(Stop);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.request();
    }
}

/// The frame that tells a stream it fell behind the bus by `missed`
/// batches. The missed events are in the WAL, and queryable once
/// compacted.
fn lagged_frame(query_id: u64, missed: u64) -> Event {
    tracing::warn!(
        event_type = "stream_lagged",
        query_id,
        missed,
        "stream subscriber fell behind"
    );
    let payload = serde_json::json!({ "missed": missed }).to_string();
    Event::default().event("lagged").data(payload)
}

/// The stream protocol has no terminal error event, so the cause stays in
/// this log. It names the query by id and the cause by the error's fixed
/// sentence, never by the pattern text.
fn log_missing_prepared_pattern(query_id: u64, error: &trawl_core::pattern::PatternError) {
    tracing::error!(
        event_type = "stream_missing_prepared_pattern",
        query_id,
        %error,
        "ending a live stream: a compiled stage has no prepared pattern"
    );
}

/// Each cell is first conformed to the pin `output_pins` holds for its name,
/// so a field the catalog pins BIGINT arrives as the integer a batch query
/// answers for it, not as the text a sender wrote.
fn data_frame(mut row: trawl_core::row::Row, output_pins: &PinScope) -> Option<Frame> {
    if !output_pins.is_empty() {
        for (name, cell) in &mut row {
            if let Some(conformed) = output_pins
                .pin_for(name)
                .and_then(|pin| trawl_core::eval::conformed_cell(cell, pin))
            {
                *cell = conformed;
            }
        }
    }
    let result = trawl_engine::post_process::rows_to_result(std::slice::from_ref(&row), &[]);
    result_frame("data", &result)
}

/// The columns are the snapshot's output schema (the aggregation's declared
/// columns through the post-stages, so they hold with no row).
fn snapshot_frame(snapshot: Snapshot) -> Option<Frame> {
    let schema: Vec<_> = snapshot
        .columns
        .into_iter()
        .map(|name| trawl_core::stream::OutputColumn::new(name, None))
        .collect();
    let frame = trawl_api::stream::SnapshotFrame {
        result: trawl_engine::post_process::rows_to_result(&snapshot.rows, &schema),
        capped: snapshot.capped.into_iter().map(snapshot_cap).collect(),
    };
    result_frame("snapshot", &frame)
}

#[derive(Debug)]
struct Frame {
    event: Event,
    bytes: usize,
}

/// The codec checks every cell against its column's type, and the builder
/// types columns from those very cells, so a refusal is a bug.
fn result_frame(name: &'static str, payload: &impl Serialize) -> Option<Frame> {
    match serde_json::to_string(payload) {
        Ok(json) => Some(Frame {
            bytes: json.len(),
            event: Event::default().event(name).data(json),
        }),
        Err(error) => {
            // serde's wording can quote the cell the codec refused.
            tracing::debug!(
                event_type = "stream_frame_unencodable",
                frame = name,
                %error,
                "dropping a live result frame the result wire refused"
            );
            tracing::error!(
                event_type = "stream_frame_unencodable",
                frame = name,
                error_class = "internal",
                cause_kind = crate::error::CauseKind::Unknown.as_str(),
                "dropping a live result frame the result wire refused"
            );
            None
        }
    }
}

fn snapshot_cap(cap: trawl_core::stream::AggregationCap) -> trawl_api::stream::SnapshotCap {
    use trawl_api::stream::SnapshotCapKind;
    use trawl_core::stream::AggregationCap;
    let kind = match cap {
        AggregationCap::Groups => SnapshotCapKind::Groups,
        AggregationCap::DistinctValues => SnapshotCapKind::DistinctValues,
        AggregationCap::ExactValues => SnapshotCapKind::ExactValues,
    };
    trawl_api::stream::SnapshotCap {
        cap: kind,
        limit: cap.limit() as u64,
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod seam {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use axum::response::sse::Event;
    use tokio::sync::OwnedSemaphorePermit;
    use tokio_stream::Stream;
    use trawl_core::cancel::Stop;
    use trawl_core::context::EvalContext;
    use trawl_core::filter::CompiledFilter;
    use trawl_core::pin_scope::PinScope;
    use trawl_core::stream::StreamPlan;

    use super::{Jobs, Live, drive};
    use crate::bus::EventSubscriber;

    pub fn calibration_frames<S: EventSubscriber>(
        subscriber: S,
        filter: CompiledFilter,
        plan: StreamPlan,
        stop: Stop,
        permit: OwnedSemaphorePermit,
        chunk_events: usize,
        job_times: JobTimes,
    ) -> impl Stream<Item = Result<Event, Infallible>> + Send {
        let live = Live::new(filter, plan, stop, PinScope::unpinned(), permit);
        let mut jobs = Jobs::new(EvalContext::capture, chunk_events);
        jobs.job_times = Some(job_times);
        drive(subscriber, live, 0, jobs)
    }

    #[derive(Debug, Clone, Default)]
    pub struct JobTimes(Arc<Mutex<Vec<Duration>>>);

    impl JobTimes {
        pub(super) fn record(&self, took: Duration) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(took);
        }

        #[must_use]
        pub fn take(&self) -> Vec<Duration> {
            std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct JobBytes(Arc<Mutex<Vec<usize>>>);

    impl JobBytes {
        pub(super) fn record(&self, bytes: usize) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(bytes);
        }

        #[must_use]
        pub fn take(&self) -> Vec<usize> {
            std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    static MOST_JOBS_PER_STREAM: AtomicUsize = AtomicUsize::new(0);

    #[must_use]
    pub fn most_jobs_per_stream() -> usize {
        MOST_JOBS_PER_STREAM.load(Ordering::SeqCst)
    }

    #[derive(Debug, Clone, Default)]
    pub(super) struct InFlight(Arc<AtomicUsize>);

    impl InFlight {
        pub(super) fn enter(&self) -> Entered {
            let now = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            MOST_JOBS_PER_STREAM.fetch_max(now, Ordering::SeqCst);
            Entered(Arc::clone(&self.0))
        }
    }

    #[derive(Debug)]
    pub(super) struct Entered(Arc<AtomicUsize>);

    impl Drop for Entered {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::ops::Range;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::response::IntoResponse as _;
    use axum::response::sse::{Event, Sse};
    use serde_json::json;
    use tokio::sync::{OwnedSemaphorePermit, Semaphore};
    use tokio_stream::{Stream, StreamExt as _};
    use trawl_core::cancel::Stop;
    use trawl_core::context::EvalContext;
    use trawl_core::filter::CompiledFilter;
    use trawl_core::pattern::Preparer;
    use trawl_core::pin_scope::PinScope;
    use trawl_core::schema::FieldTypes;
    use trawl_core::stream::{LIVE_CHUNK_EVENTS, compile_stream_plan_with};

    use super::{
        After, Carry, Jobs, LIVE_CHUNK_STRING_BYTES, LIVE_JOB_FRAME_BYTES, Live, Piece, drive,
        gather, seam, string_payload,
    };
    use crate::bus::{EventSubscriber, IngestBatch, RecvError};

    #[derive(Debug, Default)]
    struct Script(VecDeque<Result<Arc<IngestBatch>, RecvError>>);

    impl EventSubscriber for Script {
        async fn recv(&mut self) -> Result<Arc<IngestBatch>, RecvError> {
            match self.0.pop_front() {
                Some(next) => next,
                None => std::future::pending().await,
            }
        }

        fn try_recv(&mut self) -> Option<Result<Arc<IngestBatch>, RecvError>> {
            self.0.pop_front()
        }
    }

    fn batch(events: impl IntoIterator<Item = serde_json::Value>) -> Arc<IngestBatch> {
        Arc::new(IngestBatch {
            batch_id: "live".into(),
            service: "live".into(),
            events: events
                .into_iter()
                .map(|event| event.as_object().cloned().expect("an object"))
                .collect(),
            byte_size: 0,
        })
    }

    fn requests(ns: Range<usize>) -> Arc<IngestBatch> {
        batch(ns.map(|n| json!({"n": n, "message": format!("request {n}")})))
    }

    fn live(dsl: &str, permit: OwnedSemaphorePermit) -> Live {
        let stop = Stop::new();
        let query = trawl_core::parser::parse(dsl).expect("the query parses");
        let mut preparer = Preparer::new().with_stop(stop.clone());
        let filter = CompiledFilter::compile_with(&query.search, &FieldTypes::new(), &mut preparer)
            .expect("the filter compiles");
        let plan =
            compile_stream_plan_with(&query.pipeline, &PinScope::unpinned(), None, &mut preparer)
                .expect("the plan compiles");
        Live::new(filter, plan, stop, PinScope::unpinned(), permit)
    }

    fn permit() -> OwnedSemaphorePermit {
        Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("a free slot")
    }

    async fn wire(
        stream: impl Stream<Item = Result<Event, Infallible>> + Send + 'static,
    ) -> Vec<(String, String)> {
        let body = Sse::new(stream).into_response().into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .expect("the stream ends");
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8 frames");
        text.split("\n\n")
            .filter_map(|frame| {
                let (mut name, mut data) = (None, None);
                for line in frame.lines() {
                    if let Some(n) = line.strip_prefix("event: ") {
                        name = Some(n.to_owned());
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data = Some(d.to_owned());
                    }
                }
                Some((name?, data?))
            })
            .collect()
    }

    async fn until(label: &str, reached: impl Fn() -> bool) {
        let start = std::time::Instant::now();
        while !reached() {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "never reached: {label}"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[tokio::test]
    async fn a_lag_follows_the_frames_of_the_events_before_it() {
        let k = LIVE_CHUNK_EVENTS + 6;
        let script = Script(VecDeque::from([
            Ok(requests(0..k)),
            Err(RecvError::Lagged(7)),
            Ok(requests(k..k + 2)),
            Err(RecvError::Closed),
        ]));
        let frames = wire(drive(
            script,
            live("message=/request/", permit()),
            1,
            Jobs::new(EvalContext::capture, LIVE_CHUNK_EVENTS),
        ))
        .await;

        let data = |n: usize| ("data".to_owned(), format!("\"request {n}\""));
        let expected: Vec<_> = (0..k)
            .map(data)
            .chain([("lagged".to_owned(), r#"{"missed":7}"#.to_owned())])
            .chain((k..k + 2).map(data))
            .collect();
        let written: Vec<_> = frames.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(frames.len(), expected.len(), "{written:?}");
        for (i, ((name, data), (wanted, needle))) in frames.iter().zip(&expected).enumerate() {
            assert!(
                name == wanted && data.contains(needle.as_str()),
                "frame {i}: {name} {data}, expected {wanted} with {needle}"
            );
        }
    }

    #[tokio::test]
    async fn a_stream_dropped_mid_job_stops_and_releases_its_state_once() {
        let k = 2;
        let slots = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&slots).try_acquire_owned().expect("a slot");
        let sampled = Arc::new(AtomicUsize::new(0));
        let (inside, reached) = std::sync::mpsc::channel();
        let (resume, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(Mutex::new(held));
        // The kth event's instant is sampled inside the job, after its
        // stop check and before its regex call, and holds there.
        let clock = {
            let sampled = Arc::clone(&sampled);
            move || {
                if sampled.fetch_add(1, Ordering::SeqCst) + 1 == k {
                    inside.send(()).expect("the test waits");
                    let held = held.lock().expect("one job at a time");
                    held.recv().expect("the test resumes the job");
                }
                EvalContext::capture()
            }
        };
        let script = Script(VecDeque::from([Ok(requests(0..5))]));
        let stream = drive(
            script,
            live("message=/request/", permit),
            1,
            Jobs::new(clock, LIVE_CHUNK_EVENTS),
        );
        let reader = tokio::spawn(async move {
            let mut stream = std::pin::pin!(stream);
            while stream.next().await.is_some() {}
        });
        tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(20)))
            .await
            .expect("the wait ran")
            .expect("the job reached the kth event");

        reader.abort();
        assert!(reader.await.expect_err("aborted").is_cancelled());
        assert_eq!(
            slots.available_permits(),
            0,
            "the running job keeps the stream's slot"
        );

        resume.send(()).expect("the job waits");
        until("the job exits", || slots.available_permits() == 1).await;
        assert_eq!(
            sampled.load(Ordering::SeqCst),
            k,
            "no event after the kth was sampled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(slots.available_permits(), 1, "the slot came back once");
    }

    #[tokio::test]
    async fn a_job_buffers_at_most_its_frame_cap_and_one_event() {
        let copies: Vec<_> = (0..64).map(|i| format!("a{i}=message")).collect();
        let dsl = format!("* | let {}", copies.join(", "));
        let (events, text) = (200, "x".repeat(1024));
        let input = batch((0..events).map(|n| json!({"n": n, "message": format!("{n:04}{text}")})));
        let payload: usize = input.events.iter().map(string_payload).sum();
        assert!(
            events <= LIVE_CHUNK_EVENTS && payload <= LIVE_CHUNK_STRING_BYTES,
            "one gather takes the whole batch"
        );

        let run = |chunk_events: usize| {
            let bytes = seam::JobBytes::default();
            let mut jobs = Jobs::new(EvalContext::capture, chunk_events);
            jobs.job_bytes = Some(bytes.clone());
            let script = Script(VecDeque::from([
                Ok(Arc::clone(&input)),
                Err(RecvError::Lagged(3)),
                Ok(requests(0..2)),
                Err(RecvError::Closed),
            ]));
            let stream = drive(script, live(&dsl, permit()), 1, jobs);
            async move { (wire(stream).await, bytes.take()) }
        };
        let (one_by_one, singles) = run(1).await;
        let (chunked, jobs) = run(LIVE_CHUNK_EVENTS).await;

        let one_event = singles.iter().copied().max().unwrap_or(0);
        let most = jobs.iter().copied().max().unwrap_or(0);
        assert!(
            singles.iter().sum::<usize>() > 4 * LIVE_JOB_FRAME_BYTES,
            "the stream's output is over the cap many times"
        );
        assert!(
            most <= LIVE_JOB_FRAME_BYTES + one_event,
            "a job buffered {most} bytes, over the cap of {LIVE_JOB_FRAME_BYTES} and one event's {one_event}"
        );
        assert_eq!(
            one_by_one.len(),
            events + 3,
            "every event, the lag, two more"
        );
        assert_eq!(one_by_one[events].0, "lagged");
        assert_eq!(chunked.len(), one_by_one.len());
        assert!(
            chunked == one_by_one,
            "the frames differ from one event a job"
        );
    }

    /// A job whose threshold snapshots take its frames to the cap hands
    /// back what follows the batch it closed: a whole batch, or the part of
    /// one it gathered ahead of the carry. Each snapshot stays at its
    /// batch's end: the stream writes the snapshots it writes at one event
    /// a job.
    #[tokio::test]
    async fn a_job_at_its_frame_cap_hands_back_after_a_closed_batch() {
        let copies: Vec<_> = (0..32).map(|i| format!("a{i}=message")).collect();
        let dsl = format!("* | stats count() by message | let {}", copies.join(", "));
        let text = "x".repeat(256);
        let batches: Vec<_> = (0..4)
            .map(|b| {
                batch((0..100).map(|i| {
                    let n = b * 100 + i;
                    json!({"n": n, "message": format!("{n:04}{text}")})
                }))
            })
            .collect();

        let run = |chunk_events: usize| {
            let bytes = seam::JobBytes::default();
            let mut jobs = Jobs::new(EvalContext::capture, chunk_events);
            jobs.job_bytes = Some(bytes.clone());
            let script = Script(
                batches
                    .iter()
                    .map(|batch| Ok(Arc::clone(batch)))
                    .chain([Err(RecvError::Closed)])
                    .collect(),
            );
            let stream = drive(script, live(&dsl, permit()), 1, jobs);
            async move { (snapshots(wire(stream).await), bytes.take()) }
        };
        let (one_by_one, singles) = run(1).await;
        let (chunked, jobs) = run(LIVE_CHUNK_EVENTS).await;

        assert_eq!(one_by_one.len(), batches.len(), "one snapshot a batch");
        let one_snapshot = singles.iter().copied().max().unwrap_or(0);
        let most = jobs.iter().copied().max().unwrap_or(0);
        assert!(
            most <= LIVE_JOB_FRAME_BYTES + one_snapshot,
            "a job buffered {most} bytes, over the cap of {LIVE_JOB_FRAME_BYTES} and one snapshot's {one_snapshot}"
        );
        assert!(
            chunked == one_by_one,
            "the snapshots differ from one event a job"
        );
    }

    /// Each snapshot frame's rows, sorted: an aggregation emits its groups
    /// in no fixed order.
    fn snapshots(frames: Vec<(String, String)>) -> Vec<serde_json::Value> {
        frames
            .into_iter()
            .map(|(name, data)| {
                assert_eq!(name, "snapshot");
                let mut frame: serde_json::Value = serde_json::from_str(&data).expect("json");
                if let Some(rows) = frame["rows"].as_array_mut() {
                    rows.sort_by_cached_key(ToString::to_string);
                }
                frame
            })
            .collect()
    }

    fn shape(pieces: &[Piece]) -> Vec<(Range<usize>, bool)> {
        pieces
            .iter()
            .map(|piece| (piece.events.clone(), piece.closes_batch))
            .collect()
    }

    #[test]
    fn a_chunk_closes_before_its_string_payload_would_pass_the_cap() {
        let quarter = "x".repeat(LIVE_CHUNK_STRING_BYTES / 4 - 1);
        let over = "x".repeat(LIVE_CHUNK_STRING_BYTES + 1);
        let big = batch((0..5).map(|n| json!({"n": n, "message": quarter})).chain([
            json!({"n": 5, "nested": {"message": [over]}}),
            json!({"n": 6, "message": "small"}),
        ]));
        let mut nothing = Script::default();
        let mut carry = None;
        let mut start = Carry {
            batch: big,
            from: 0,
        };
        let mut chunks = Vec::new();
        loop {
            let (pieces, after) = gather(&mut nothing, start, &mut carry, LIVE_CHUNK_EVENTS);
            assert_eq!(after, After::Nothing);
            chunks.push(shape(&pieces));
            match carry.take() {
                Some(rest) => start = rest,
                None => break,
            }
        }
        assert_eq!(
            chunks,
            [
                vec![(0..4, false)],
                vec![(4..5, false)],
                vec![(5..6, false)],
                vec![(6..7, true)],
            ]
        );
    }

    #[test]
    fn a_chunk_takes_what_has_arrived_up_to_its_event_count() {
        let fill = LIVE_CHUNK_EVENTS - 5;
        let mut script = Script(VecDeque::from([
            Ok(requests(0..3)),
            Ok(requests(0..LIVE_CHUNK_EVENTS)),
            Err(RecvError::Lagged(2)),
        ]));
        let mut carry = None;
        let start = Carry {
            batch: requests(0..2),
            from: 0,
        };
        let (pieces, after) = gather(&mut script, start, &mut carry, LIVE_CHUNK_EVENTS);
        assert_eq!(after, After::Nothing);
        assert_eq!(
            shape(&pieces),
            [(0..2, true), (0..3, true), (0..fill, false)]
        );
        let rest = carry.take().expect("the filled batch is carried");
        assert_eq!(rest.from, fill);

        let (pieces, after) = gather(&mut script, rest, &mut carry, LIVE_CHUNK_EVENTS);
        assert_eq!(shape(&pieces), [(fill..LIVE_CHUNK_EVENTS, true)]);
        assert_eq!(after, After::Lagged(2));
        assert!(carry.is_none());
    }
}
