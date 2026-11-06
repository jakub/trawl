// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The live driver: one per SSE stream (ADR-0063).
//!
//! A live stream evaluates every event it receives, and that evaluation
//! runs regexes. The driver keeps it off the reactor. It awaits the bus,
//! and for an aggregate plan the snapshot timer, on the reactor, and hands
//! the events to the blocking pool in bounded chunks, one job at a time.
//!
//! The stream's evaluation state ([`LiveState`]) is one owned value that
//! moves into each job and comes back with the job's frames. While a job
//! runs the driver receives nothing, so the bus's broadcast ring is the
//! bounded queue between ingest and the stream, and a stream that falls
//! behind it gets the ring's `lagged` frame.
//!
//! A chunk bounds a job's input, and [`LIVE_JOB_FRAME_BYTES`] its output:
//! a job takes no event once its frames reach that, and hands the rest of
//! its chunk back. They run next, before anything else is received.
//!
//! A stream that goes away mid-job requests its run's stop, and the job
//! ends at its next event or regex call. Its state then drops inside the
//! job, which keeps the stream's SSE slot and its regex reservation until
//! it exits and releases each once.

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

/// The most string payload one chunk carries, in bytes. A chunk closes
/// before an event that would take it past this, and an event larger than
/// this runs alone.
const LIVE_CHUNK_STRING_BYTES: usize = 256 * 1024;

/// The most frame data one job buffers, in bytes: four input caps. A
/// stage can make an event's frames far larger than the event, so the
/// input caps do not bound them. A job takes no event once its frames
/// reach this and hands the rest back to the driver, so it holds at most
/// this and one event's frames, or one snapshot's. Its first event always
/// runs, so an event whose own frames are over this runs alone.
const LIVE_JOB_FRAME_BYTES: usize = 4 * LIVE_CHUNK_STRING_BYTES;

/// How often an aggregate stream takes a snapshot, when an event fed its
/// aggregation since the last one. [`trawl_core::stream::SNAPSHOT_EVENT_THRESHOLD`]
/// events at the end of a bus batch take one sooner.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(500);

/// What one live stream evaluates with. It moves into each job and back
/// out with the job's output, so a stream has one job at a time.
#[derive(Debug)]
pub(crate) struct LiveState {
    run: LiveRun,
    output_pins: PinScope,
    /// The stream's SSE slot. It drops with the state, so a job that
    /// outlives its stream keeps the slot until the job exits.
    _permit: OwnedSemaphorePermit,
}

/// A live stream ready to drive: its state, the stop its patterns were
/// prepared under, and whether its plan aggregates.
#[derive(Debug)]
pub(crate) struct Live {
    state: LiveState,
    stop: Stop,
    aggregate: bool,
}

impl Live {
    /// A stream of `filter` and `plan`, whose patterns were prepared under
    /// `stop`, typing its rows under `output_pins` and holding `permit`.
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

/// The SSE frames of `live` over the batches `subscriber` receives. Each
/// event samples its own instant when its job evaluates it (ADR-0017 §3).
/// `query_id` names the stream in its logs.
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

/// [`frames`], its jobs run as `jobs` says.
#[allow(clippy::too_many_lines)] // the driver's one loop, its steps in order
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
        // Dropped with the stream, mid-job included, it stops the run.
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
        // Whether a chunk has evaluated an event since the timer's last
        // job: with none, the aggregation has nothing new to snapshot.
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

/// What each of a stream's jobs runs with, besides its state.
#[derive(Debug, Clone)]
struct Jobs<C> {
    /// Samples each event's and each snapshot's instant.
    clock: C,
    /// The most events one job evaluates.
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
///
/// The job owns the state while it runs. Nobody waits for the answer once
/// the stream has gone away, so the state drops inside the job, which has
/// stopped by then at its next event or regex call.
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
        // With no stream waiting, the state and the frames drop here.
        drop(answer.send((state, out)));
    }));
    answered.await.ok()
}

/// One job's work over `state`.
///
/// A chunk takes no event once its frames reach [`LIVE_JOB_FRAME_BYTES`],
/// and hands the events it did not take back to the driver in
/// [`JobOutput::handed_back`]. A batch whose events have all run is closed
/// before the cap is read again, so its threshold snapshot stays at its end.
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

/// One job's work.
#[derive(Debug)]
enum Work {
    /// Evaluate these events, in bus order.
    Chunk(Vec<Piece>),
    /// The snapshot timer fired.
    Tick,
}

/// The events of one bus batch that one job evaluates.
#[derive(Debug)]
struct Piece {
    batch: Arc<IngestBatch>,
    events: Range<usize>,
    /// The piece ends with the batch's last event, so its job closes the
    /// batch after it ([`LiveRun::end_of_batch`]).
    closes_batch: bool,
}

/// A bus batch whose events from `from` on are still to run.
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

/// What one job produced.
#[derive(Debug, Default)]
struct JobOutput {
    frames: Vec<Event>,
    /// The data bytes of `frames`, which [`LIVE_JOB_FRAME_BYTES`] caps.
    bytes: usize,
    /// The events the job did not take once its frames reached the cap,
    /// in bus order. They run next.
    handed_back: Vec<Piece>,
    /// A bus batch's end took a snapshot, so the timer starts over.
    snapshot_taken: bool,
    /// Why the run ended, if it did.
    end: Option<LiveEnd>,
}

impl JobOutput {
    fn push(&mut self, frame: Option<Frame>) {
        if let Some(Frame { event, bytes }) = frame {
            self.frames.push(event);
            self.bytes = self.bytes.saturating_add(bytes);
        }
    }

    /// End the job with `error`. A cancelled run is discarded, and the
    /// frames it produced with it.
    fn fail(mut self, error: LiveError) -> Self {
        if error == LiveError::Cancelled {
            self.frames.clear();
            self.bytes = 0;
        }
        self.end = Some(LiveEnd::Failed(error));
        self
    }
}

/// Gather one chunk from `start`: the events that have already arrived, up
/// to `chunk_events` and [`LIVE_CHUNK_STRING_BYTES`]. It never waits
/// for more. A batch the chunk ends inside is left in `carry`, so at most
/// one partly read batch is held.
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
            // The cap bounds batching, not event size: an event cannot be
            // split, so the first event of a chunk is taken even over the
            // cap, and runs alone (R14).
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

/// Log why a live stream ends early: a stage of its plan has no prepared
/// pattern for an operand (ADR-0063), which is a broken plan, not a
/// non-match. The stream protocol has no terminal error event, so the
/// stream ends the way a spent `limit` ends it, and the cause stays in
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

/// An SSE `data` event for one event a pass-through stream emits: a
/// one-row typed result.
///
/// Each cell is first conformed to the pin `output_pins` holds for its
/// name ([`trawl_core::eval::conformed_cell`]), the reading a pinned
/// `| where` takes, so a field the catalog pins BIGINT arrives as the
/// integer a batch query answers for it, not as the text a sender wrote. A
/// cell with no reading keeps its own kind, and a field a stage derived
/// has no pin in the scope, so its own cell types it.
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

/// An SSE snapshot event for an aggregate snapshot, its post-stages
/// already applied.
///
/// The payload is a [`trawl_api::stream::SnapshotFrame`]: the rows as a
/// typed result, its columns the snapshot's output schema (the
/// aggregation's declared columns through the post-stages, so they hold
/// with no row) and typed by the cells the aggregation emitted, plus
/// `capped`, one `{cap, limit}` per cap that dropped input since the
/// stream started, on this and every later snapshot, omitted while nothing
/// has been dropped (ADR-0001). An event dropped at a cap still counts
/// toward the next snapshot, so the scheduled snapshot discloses a cap
/// even when no further input arrives.
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

/// One frame a job produced, and the bytes of its data.
#[derive(Debug)]
struct Frame {
    event: Event,
    bytes: usize,
}

/// One result frame as an SSE event named `name`. The codec checks every
/// cell against its column's type, and the builder types columns from
/// those very cells, so a refusal is a bug: it is logged and the frame
/// dropped rather than sent half-written.
fn result_frame(name: &'static str, payload: &impl Serialize) -> Option<Frame> {
    match serde_json::to_string(payload) {
        Ok(json) => Some(Frame {
            bytes: json.len(),
            event: Event::default().event(name).data(json),
        }),
        Err(error) => {
            tracing::error!(
                event_type = "stream_frame_unencodable",
                frame = name,
                %error,
                "dropping a live result frame the result wire refused"
            );
            None
        }
    }
}

/// One dropped-input cap as the snapshot wire spells it.
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

/// The test seams: each stream's jobs in flight, the frame bytes each job
/// buffered, and the driver at a chosen chunk size for the chunk
/// calibration.
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

    /// The SSE frames of a live stream of `filter` and `plan`, whose
    /// patterns were prepared under `stop`, driven as [`super::frames`]
    /// drives them but with at most `chunk_events` events a job. Each
    /// job's duration goes to `job_times`, and the stream holds `permit`
    /// as its SSE slot. The chunk calibration of issue #336 runs it.
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

    /// The durations of a stream's jobs, in the order they ended.
    #[derive(Debug, Clone, Default)]
    pub struct JobTimes(Arc<Mutex<Vec<Duration>>>);

    impl JobTimes {
        pub(super) fn record(&self, took: Duration) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(took);
        }

        /// Every duration recorded since the last take.
        #[must_use]
        pub fn take(&self) -> Vec<Duration> {
            std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    /// The frame data each of a stream's jobs buffered, in bytes, in the
    /// order the jobs ended.
    #[derive(Debug, Clone, Default)]
    pub struct JobBytes(Arc<Mutex<Vec<usize>>>);

    impl JobBytes {
        pub(super) fn record(&self, bytes: usize) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(bytes);
        }

        /// Every job's bytes recorded since the last take.
        #[must_use]
        pub fn take(&self) -> Vec<usize> {
            std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    /// The most jobs any one stream has had in flight at once.
    static MOST_JOBS_PER_STREAM: AtomicUsize = AtomicUsize::new(0);

    /// The most jobs any one stream of this process has had in flight at
    /// once. A driver that waits for each job before it starts the next
    /// keeps this at 1 once a job has run.
    #[must_use]
    pub fn most_jobs_per_stream() -> usize {
        MOST_JOBS_PER_STREAM.load(Ordering::SeqCst)
    }

    /// One stream's jobs in flight.
    #[derive(Debug, Clone, Default)]
    pub(super) struct InFlight(Arc<AtomicUsize>);

    impl InFlight {
        /// Count a job in until the guard drops.
        pub(super) fn enter(&self) -> Entered {
            let now = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            MOST_JOBS_PER_STREAM.fetch_max(now, Ordering::SeqCst);
            Entered(Arc::clone(&self.0))
        }
    }

    /// One job counted in.
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

    /// A subscriber that answers its script in order, then waits forever.
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

    /// Events `request {n}`, for each `n` of `ns`.
    fn requests(ns: Range<usize>) -> Arc<IngestBatch> {
        batch(ns.map(|n| json!({"n": n, "message": format!("request {n}")})))
    }

    /// `dsl` set up for a live stream as its setup prepares it: one ledger
    /// under one stop, the run's.
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

    /// Every frame `stream` writes until it ends, as its event name and
    /// its data, read off the SSE body.
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

    /// Wait until `reached`, at most 20 s.
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

    /// A lag the bus reports after k events ends the gather, and its
    /// `lagged` frame follows the frames of those k events, the rest of a
    /// split batch included. The events received after it follow it.
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

    /// A stream dropped while its job runs requests the run's stop. The job
    /// evaluates no event after the one it was in, and keeps the stream's
    /// SSE slot until it exits. Its state drops there, and the slot comes
    /// back once.
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

    /// A stage that multiplies each event's output cannot make one job
    /// buffer more than [`LIVE_JOB_FRAME_BYTES`] and one event's frames,
    /// though one gather takes every event. The stream writes what it
    /// writes at one event a job, the `lagged` frame after the frames of
    /// the events before it.
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

    /// A chunk closes before an event that would take its string payload
    /// past [`LIVE_CHUNK_STRING_BYTES`], and an event over the cap runs
    /// alone. The rest of the batch is carried to the next chunk.
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

    /// A chunk takes the batches that have already arrived, up to
    /// [`LIVE_CHUNK_EVENTS`] events, each closing its batch, and carries
    /// the rest of the batch it fills inside. A lag ends the gather.
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
