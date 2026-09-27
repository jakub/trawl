// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One query's timing account and the two events that report it
//! (ADR-0046).
//!
//! A [`QueryTiming`] is created once a DSL request is valid, before its
//! `dsl_check`, and every layer that works on the query books its phases
//! on the account's [`PhaseClock`]. Exactly one side writes the final
//! `query_timing` event: [`QueryTiming::emit_complete`] and
//! [`QueryTiming::emit_partial`] share one latch, and a handler's
//! [`TimingGuard`] writes `outcome=abandoned` if the request is dropped
//! before anybody else wrote it. When work outlives its request,
//! [`emit_reclaimed`] carries the worker's final totals on
//! `query_permit_reclaimed`.
//!
//! Both events are root events (`parent: None`) with explicit fields,
//! so nothing is inherited from the `http_request` span, which carries
//! the raw path and user agent (ADR-0040). They carry numbers, closed
//! enums and ids: no user, role, DSL, SQL, literal, path or error text.
//! A phase value is an `Option<u64>` recorded through tracing's own
//! `Option` value, so an absent phase is an absent field and a present
//! one is an integer.
//!
//! Lock order: the account's lock and the clock's are leaves. Never take
//! either while holding the pool registry lock; read what the registry
//! says, release it, then write the account.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use trawl_engine::error::EngineError;
use trawl_engine::timing::{PhaseClock, PhaseTotals, QueryPhase};

use crate::error::ServerError;
use crate::pool::WorkKind;

/// The id that joins a query's timing to the rest of its story and
/// survives a restart, unlike `query_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Correlation {
    /// HTTP work: the request's ULID (ADR-0040).
    Request(String),
    /// A scheduled or manual run: the run record's id.
    Run(i64),
}

impl Correlation {
    fn request_id(&self) -> Option<&str> {
        match self {
            Self::Request(id) => Some(id),
            Self::Run(_) => None,
        }
    }

    fn run_id(&self) -> Option<i64> {
        match self {
            Self::Run(id) => Some(*id),
            Self::Request(_) => None,
        }
    }
}

/// An export's body format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    Parquet,
}

impl ExportFormat {
    /// The closed value the `format` field carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Parquet => "parquet",
        }
    }
}

/// What kind of DSL work the account times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingKind {
    /// `POST /api/v1/query`.
    Query,
    /// A `| from saved` query. Set only once saved resolution resolved.
    FromSaved,
    /// `POST /api/v1/export`, with its format.
    Export(ExportFormat),
    /// A scheduled or manual report run.
    Scheduled,
}

impl TimingKind {
    /// The closed value the `kind` field carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::FromSaved => "from_saved",
            Self::Export(_) => "export",
            Self::Scheduled => "scheduled",
        }
    }

    /// The `format` field: present for exports only.
    #[must_use]
    pub const fn format(self) -> Option<&'static str> {
        match self {
            Self::Export(format) => Some(format.as_str()),
            Self::Query | Self::FromSaved | Self::Scheduled => None,
        }
    }
}

/// How the query's execution ended, as the request saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// Failed; carries [`ServerError::error_class`], never the text.
    Error(&'static str),
    /// Refused before its work started, for want of capacity. Named by
    /// the pool where it makes the refusal, which knows it structurally.
    CapacityRefused,
    Timeout,
    Abandoned,
}

impl Outcome {
    /// The outcome of a failed execution: the one mapping from an error
    /// to an outcome. A capacity refusal is not derived here; the pool
    /// names it with [`Outcome::CapacityRefused`] where it refuses,
    /// rather than reading it back out of a 503's message.
    #[must_use]
    pub fn of_error(error: &ServerError) -> Self {
        match error {
            ServerError::Timeout => Self::Timeout,
            other => Self::Error(other.error_class()),
        }
    }

    /// The closed value the `outcome` field carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error(_) => "error",
            Self::CapacityRefused => "capacity_refused",
            Self::Timeout => "timeout",
            Self::Abandoned => "abandoned",
        }
    }

    /// The `error_class` field: present on `error` only.
    #[must_use]
    pub const fn error_class(self) -> Option<&'static str> {
        match self {
            Self::Error(class) => Some(class),
            Self::Success | Self::CapacityRefused | Self::Timeout | Self::Abandoned => None,
        }
    }
}

/// How the physical work behind a retained permit ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalOutcome {
    Completed,
    Failed,
    Cancelled,
    Panicked,
    /// The permit was retained and the work never started.
    NotStarted,
}

impl PhysicalOutcome {
    /// How a worker's result ended.
    #[must_use]
    pub fn of_result<T>(result: &Result<T, ServerError>) -> Self {
        match result {
            Ok(_) => Self::Completed,
            Err(ServerError::Engine(EngineError::Cancelled)) => Self::Cancelled,
            Err(ServerError::Panicked(_)) => Self::Panicked,
            Err(_) => Self::Failed,
        }
    }

    /// The closed value the `physical_outcome` field carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Panicked => "panicked",
            Self::NotStarted => "not_started",
        }
    }
}

/// Whole microseconds, floored, saturating.
fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The mutable half of an account.
#[derive(Debug)]
struct Account {
    kind: TimingKind,
    /// Latched by whoever writes the final `query_timing`.
    emitted: bool,
    /// The pool has taken over writing the account, so a dropped
    /// handler guard leaves it alone.
    pool_owns_emit: bool,
    work_started: bool,
}

#[derive(Debug)]
struct Inner {
    clock: PhaseClock,
    origin: Instant,
    query_id: u64,
    correlation: Correlation,
    account: Mutex<Account>,
}

/// One DSL query's timing account. Clones share it.
#[derive(Debug, Clone)]
pub struct QueryTiming(Arc<Inner>);

impl QueryTiming {
    /// Open an account whose observed window starts at `origin`: the
    /// instant the response's execution record measures from.
    #[must_use]
    pub fn new(origin: Instant, query_id: u64, correlation: Correlation, kind: TimingKind) -> Self {
        Self(Arc::new(Inner {
            clock: PhaseClock::new(),
            origin,
            query_id,
            correlation,
            account: Mutex::new(Account {
                kind,
                emitted: false,
                pool_owns_emit: false,
                work_started: false,
            }),
        }))
    }

    /// The account, recovering it from a poisoned lock: every write under
    /// it is a plain field store.
    fn account(&self) -> MutexGuard<'_, Account> {
        self.0
            .account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The clock every layer books this query's phases on.
    #[must_use]
    pub fn clock(&self) -> &PhaseClock {
        &self.0.clock
    }

    #[must_use]
    pub fn query_id(&self) -> u64 {
        self.0.query_id
    }

    #[must_use]
    pub fn origin(&self) -> Instant {
        self.0.origin
    }

    #[must_use]
    pub fn correlation(&self) -> &Correlation {
        &self.0.correlation
    }

    #[must_use]
    pub fn kind(&self) -> TimingKind {
        self.account().kind
    }

    /// Re-kind the account: `from_saved` once saved resolution resolved.
    pub fn set_kind(&self, kind: TimingKind) {
        self.account().kind = kind;
    }

    /// The worker crossed its work-start transition (ADR-0024).
    pub fn mark_work_started(&self) {
        self.account().work_started = true;
    }

    #[must_use]
    pub fn work_started(&self) -> bool {
        self.account().work_started
    }

    /// The pool writes this account from here on; a handler's
    /// [`TimingGuard`] that drops afterwards writes nothing.
    pub fn hand_emit_to_pool(&self) {
        self.account().pool_owns_emit = true;
    }

    /// The pool's result reached the handler, which writes the account
    /// from here on; a handler guard dropped before it does writes
    /// `outcome=abandoned`.
    pub fn hand_emit_back(&self) {
        self.account().pool_owns_emit = false;
    }

    /// Whether the final `query_timing` has been written.
    #[must_use]
    pub fn emitted(&self) -> bool {
        self.account().emitted
    }

    /// Take the write latch. `Some` carries what the event needs from
    /// the account; `None` means someone else already wrote it.
    fn claim(&self) -> Option<(TimingKind, bool)> {
        let mut account = self.account();
        if account.emitted {
            return None;
        }
        account.emitted = true;
        Some((account.kind, account.work_started))
    }

    /// Write the complete account, closed at `end`, unless it has been
    /// written already. Returns whether this call wrote it.
    pub fn emit_complete(&self, outcome: Outcome, end: Instant) -> bool {
        let Some((kind, work_started)) = self.claim() else {
            return false;
        };
        let totals = self.clock().close_at(end).unwrap_or_default();
        emit_query_timing(&TimingRecord {
            query_id: self.query_id(),
            kind,
            correlation: self.correlation(),
            outcome,
            work_started,
            complete: true,
            observed_us: micros(end.saturating_duration_since(self.origin())),
            totals: &totals,
        });
        true
    }

    /// Write the partial account now — the completed totals and the
    /// phase still running — unless it has been written already. Returns
    /// whether this call wrote it.
    ///
    /// The window ends at the instant the clock cut its copy at, under
    /// its lock, so a worker that finishes a phase while the request is
    /// deciding cannot book time past the end of the window.
    pub fn emit_partial(&self, outcome: Outcome) -> bool {
        let Some((kind, work_started)) = self.claim() else {
            return false;
        };
        let (totals, now) = self
            .clock()
            .snapshot()
            .unwrap_or_else(|| (PhaseTotals::default(), Instant::now()));
        emit_query_timing(&TimingRecord {
            query_id: self.query_id(),
            kind,
            correlation: self.correlation(),
            outcome,
            work_started,
            complete: false,
            observed_us: micros(now.saturating_duration_since(self.origin())),
            totals: &totals,
        });
        true
    }
}

/// The handler's hold on an account: dropped with nothing written and
/// the pool not in charge, it writes `outcome=abandoned`.
///
/// That is the pre-pool abandonment: a caller that walked away during
/// `dsl_check`, `saved_lookup` or `pool_wait` before the pool took over.
/// The account is complete, because no work is left running to report
/// later; a phase cut by the drop is booked through the drop.
#[derive(Debug)]
pub struct TimingGuard(QueryTiming);

impl TimingGuard {
    #[must_use]
    pub fn new(timing: QueryTiming) -> Self {
        Self(timing)
    }

    #[must_use]
    pub fn timing(&self) -> &QueryTiming {
        &self.0
    }
}

impl std::ops::Deref for TimingGuard {
    type Target = QueryTiming;

    fn deref(&self) -> &QueryTiming {
        &self.0
    }
}

impl Drop for TimingGuard {
    fn drop(&mut self) {
        if self.0.account().pool_owns_emit {
            return;
        }
        self.0.emit_complete(Outcome::Abandoned, Instant::now());
    }
}

/// Everything one `query_timing` event says.
#[derive(Debug)]
pub struct TimingRecord<'a> {
    pub query_id: u64,
    pub kind: TimingKind,
    pub correlation: &'a Correlation,
    pub outcome: Outcome,
    pub work_started: bool,
    /// Whether `totals` is the whole account, or a partial one taken
    /// while work was still running.
    pub complete: bool,
    /// Microseconds from the account's origin to the end of its window.
    pub observed_us: u64,
    pub totals: &'a PhaseTotals,
}

impl TimingRecord<'_> {
    /// The observed time no phase measured.
    ///
    /// Computed from the integers the event carries, so the fields sum
    /// exactly: complete, `observed = Σ phases + other`; partial,
    /// `observed = Σ completed + active_elapsed + other`.
    fn other_us(&self) -> u64 {
        let active = self.totals.active.map_or(0, |(_, us)| us);
        let measured = self.totals.booked_us().saturating_add(active);
        debug_assert!(
            measured <= self.observed_us,
            "phases ({measured}us) outran the observed window ({}us)",
            self.observed_us
        );
        self.observed_us.saturating_sub(measured)
    }
}

/// Write one `query_timing` event.
///
/// 30 fields plus the message: the drift test holds the count under
/// tracing's limit of 32 and the phase fields equal to
/// [`QueryPhase::ALL`]. Adding a field is a decision, not an edit.
pub fn emit_query_timing(record: &TimingRecord<'_>) {
    let totals = record.totals;
    tracing::info!(
        target: "trawl_server::query_timing",
        parent: None,
        event_type = "query_timing",
        query_id = record.query_id,
        kind = record.kind.as_str(),
        format = record.kind.format(),
        request_id = record.correlation.request_id(),
        run_id = record.correlation.run_id(),
        outcome = record.outcome.as_str(),
        work_started = record.work_started,
        error_class = record.outcome.error_class(),
        timing_complete = record.complete,
        active_query_phase = totals.active.map(|(phase, _)| phase.as_str()),
        active_elapsed_us = totals.active.map(|(_, elapsed)| elapsed),
        duckdb_attempts = u64::from(totals.attempts),
        fallback = totals.fallback.as_str(),
        query_observed_us = record.observed_us,
        query_other_us = record.other_us(),
        query_dsl_check_us = totals.get(QueryPhase::DslCheck),
        query_saved_lookup_us = totals.get(QueryPhase::SavedLookup),
        query_pool_wait_us = totals.get(QueryPhase::PoolWait),
        query_publication_wait_us = totals.get(QueryPhase::PublicationWait),
        query_startup_us = totals.get(QueryPhase::Startup),
        query_source_us = totals.get(QueryPhase::Source),
        query_hot_snapshot_us = totals.get(QueryPhase::HotSnapshot),
        query_emit_us = totals.get(QueryPhase::Emit),
        query_probe_us = totals.get(QueryPhase::Probe),
        query_bind_us = totals.get(QueryPhase::Bind),
        query_execute_us = totals.get(QueryPhase::Execute),
        query_copy_us = totals.get(QueryPhase::Copy),
        query_post_us = totals.get(QueryPhase::Post),
        query_render_us = totals.get(QueryPhase::Render),
        "query timing"
    );
}

/// Everything one `query_permit_reclaimed` event says.
#[derive(Debug)]
pub struct Reclaim<'a> {
    pub query_id: u64,
    pub kind: WorkKind,
    /// How long the work outlived its request.
    pub retained_for: Duration,
    /// Absent for work that has no timing account (ping, sampling).
    pub correlation: Option<&'a Correlation>,
    pub physical: PhysicalOutcome,
    /// The worker's final totals; absent when the work never started or
    /// has no account.
    pub totals: Option<&'a PhaseTotals>,
}

/// Write one `query_permit_reclaimed` event: the retained permit came
/// back, with the worker's final account when there is one.
///
/// Keeps the pool's target, where the retained-permit pair has always
/// been logged.
pub fn emit_reclaimed(reclaim: &Reclaim<'_>) {
    let totals = reclaim.totals;
    let phase = |p: QueryPhase| totals.and_then(|t| t.get(p));
    tracing::info!(
        target: "trawl_server::pool",
        parent: None,
        event_type = "query_permit_reclaimed",
        query_id = reclaim.query_id,
        kind = reclaim.kind.as_str(),
        retained_ms = u64::try_from(reclaim.retained_for.as_millis()).unwrap_or(u64::MAX),
        request_id = reclaim.correlation.and_then(Correlation::request_id),
        run_id = reclaim.correlation.and_then(Correlation::run_id),
        physical_outcome = reclaim.physical.as_str(),
        duckdb_attempts = totals.map(|t| u64::from(t.attempts)),
        fallback = totals.map(|t| t.fallback.as_str()),
        query_dsl_check_us = phase(QueryPhase::DslCheck),
        query_saved_lookup_us = phase(QueryPhase::SavedLookup),
        query_pool_wait_us = phase(QueryPhase::PoolWait),
        query_publication_wait_us = phase(QueryPhase::PublicationWait),
        query_startup_us = phase(QueryPhase::Startup),
        query_source_us = phase(QueryPhase::Source),
        query_hot_snapshot_us = phase(QueryPhase::HotSnapshot),
        query_emit_us = phase(QueryPhase::Emit),
        query_probe_us = phase(QueryPhase::Probe),
        query_bind_us = phase(QueryPhase::Bind),
        query_execute_us = phase(QueryPhase::Execute),
        query_copy_us = phase(QueryPhase::Copy),
        query_post_us = phase(QueryPhase::Post),
        query_render_us = phase(QueryPhase::Render),
        "retained query permit reclaimed"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tracing_subscriber::prelude::*;
    use trawl_engine::error::EngineError;
    use trawl_engine::timing::{Fallback, PhaseTotals, QueryPhase};

    use super::{
        Correlation, ExportFormat, Outcome, PhysicalOutcome, QueryTiming, Reclaim, TimingGuard,
        TimingKind, TimingRecord, emit_query_timing, emit_reclaimed,
    };
    use crate::error::ServerError;
    use crate::pool::WorkKind;

    /// One captured event: its target, every field its callsite
    /// declares, and the values it recorded, with how each was recorded.
    #[derive(Debug, Clone)]
    struct Event {
        target: String,
        declared: Vec<String>,
        values: HashMap<String, Value>,
        root: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Value {
        U64(u64),
        I64(i64),
        Bool(bool),
        Str(String),
        Debug(String),
    }

    struct Fields<'a>(&'a mut HashMap<String, Value>);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_owned(), Value::Debug(format!("{value:?}")));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0
                .insert(field.name().to_owned(), Value::Str(value.to_owned()));
        }
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name().to_owned(), Value::U64(value));
        }
        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.0.insert(field.name().to_owned(), Value::I64(value));
        }
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.0.insert(field.name().to_owned(), Value::Bool(value));
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<Event>>>);

    impl<S> tracing_subscriber::Layer<S> for Capture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut values = HashMap::new();
            event.record(&mut Fields(&mut values));
            self.0.lock().expect("capture poisoned").push(Event {
                target: event.metadata().target().to_owned(),
                declared: event
                    .metadata()
                    .fields()
                    .iter()
                    .map(|f| f.name().to_owned())
                    .collect(),
                values,
                root: event.is_root(),
            });
        }
    }

    /// Every event `work` emits on this thread, from inside a span, so
    /// a root event is told apart from one that inherited the span.
    fn capture(work: impl FnOnce()) -> Vec<Event> {
        let captured = Capture::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("http_request", uri = "/secret/path");
            let _entered = span.enter();
            work();
        });
        captured.0.lock().expect("capture poisoned").clone()
    }

    fn only(events: &[Event], event_type: &str) -> Event {
        let matching: Vec<_> = events
            .iter()
            .filter(|e| e.values.get("event_type") == Some(&Value::Str(event_type.to_owned())))
            .cloned()
            .collect();
        assert_eq!(matching.len(), 1, "one {event_type}: {events:?}");
        matching.into_iter().next().expect("one")
    }

    fn phase_fields(event: &Event) -> BTreeSet<String> {
        event
            .declared
            .iter()
            .filter(|name| {
                name.starts_with("query_")
                    && name.ends_with("_us")
                    && *name != "query_observed_us"
                    && *name != "query_other_us"
            })
            .cloned()
            .collect()
    }

    fn all_phase_fields() -> BTreeSet<String> {
        QueryPhase::ALL
            .into_iter()
            .map(|p| p.field().to_owned())
            .collect()
    }

    /// Distinct, known microseconds in every phase: 1000 + index.
    fn every_phase() -> PhaseTotals {
        let mut totals = PhaseTotals::default();
        for (i, phase) in QueryPhase::ALL.into_iter().enumerate() {
            totals.set(phase, Some(1_000 + i as u64));
        }
        totals.attempts = 2;
        totals.fallback = Fallback::Both;
        totals
    }

    /// The drift guard: both events declare exactly one field per
    /// [`QueryPhase`], and `query_timing` stays at 31 fields — one under
    /// tracing's limit of 32, so adding a field is a decision.
    #[test]
    fn event_fields_track_the_phase_list() {
        let totals = every_phase();
        let correlation = Correlation::Request("01HZ".into());
        let events = capture(|| {
            emit_query_timing(&TimingRecord {
                query_id: 7,
                kind: TimingKind::Query,
                correlation: &correlation,
                outcome: Outcome::Success,
                work_started: true,
                complete: true,
                observed_us: 1_000_000,
                totals: &totals,
            });
            emit_reclaimed(&Reclaim {
                query_id: 7,
                kind: WorkKind::Query,
                retained_for: Duration::from_millis(3),
                correlation: Some(&correlation),
                physical: PhysicalOutcome::Completed,
                totals: Some(&totals),
            });
        });

        let timing = only(&events, "query_timing");
        assert_eq!(phase_fields(&timing), all_phase_fields());
        assert_eq!(timing.declared.len(), 31, "{:?}", timing.declared);
        assert!(timing.declared.iter().any(|f| f == "message"));

        let reclaim = only(&events, "query_permit_reclaimed");
        assert_eq!(phase_fields(&reclaim), all_phase_fields());
        assert!(reclaim.declared.len() <= 32, "{:?}", reclaim.declared);
    }

    /// Each phase's value lands in its own field, as an integer, and the
    /// events are roots under the `trawl_server` targets.
    #[test]
    fn phase_values_reach_their_own_fields_as_integers() {
        let totals = every_phase();
        let correlation = Correlation::Request("01HZ".into());
        let events = capture(|| {
            emit_query_timing(&TimingRecord {
                query_id: 7,
                kind: TimingKind::Export(ExportFormat::Parquet),
                correlation: &correlation,
                outcome: Outcome::Success,
                work_started: true,
                complete: true,
                observed_us: 1_000_000,
                totals: &totals,
            });
            emit_reclaimed(&Reclaim {
                query_id: 7,
                kind: WorkKind::Export,
                retained_for: Duration::from_millis(3),
                correlation: Some(&correlation),
                physical: PhysicalOutcome::Completed,
                totals: Some(&totals),
            });
        });

        for (event_type, target) in [
            ("query_timing", "trawl_server::query_timing"),
            ("query_permit_reclaimed", "trawl_server::pool"),
        ] {
            let event = only(&events, event_type);
            assert!(event.root, "{event_type} inherits no span");
            assert_eq!(event.target, target);
            for (i, phase) in QueryPhase::ALL.into_iter().enumerate() {
                assert_eq!(
                    event.values.get(phase.field()),
                    Some(&Value::U64(1_000 + i as u64)),
                    "{event_type}.{}",
                    phase.field()
                );
            }
            assert_eq!(event.values.get("duckdb_attempts"), Some(&Value::U64(2)));
            assert_eq!(
                event.values.get("fallback"),
                Some(&Value::Str("both".into()))
            );
            assert_eq!(
                event.values.get("request_id"),
                Some(&Value::Str("01HZ".into()))
            );
            assert_eq!(event.values.get("run_id"), None);
        }

        let timing = only(&events, "query_timing");
        let sum: u64 = (0..14).map(|i| 1_000 + i).sum();
        assert_eq!(
            timing.values.get("query_other_us"),
            Some(&Value::U64(1_000_000 - sum))
        );
        assert_eq!(
            timing.values.get("kind"),
            Some(&Value::Str("export".into()))
        );
        assert_eq!(
            timing.values.get("format"),
            Some(&Value::Str("parquet".into()))
        );
        assert_eq!(
            timing.values.get("timing_complete"),
            Some(&Value::Bool(true))
        );
        assert_eq!(timing.values.get("active_query_phase"), None);
        assert_eq!(timing.values.get("error_class"), None);
        assert!(
            !timing
                .values
                .iter()
                .any(|(name, v)| name != "message" && matches!(v, Value::Debug(_))),
            "no field but the message is Debug-formatted: {:?}",
            timing.values
        );
    }

    /// A partial account names the running phase, books only what
    /// finished, and still sums exactly to its window.
    #[test]
    fn a_partial_account_reports_the_active_phase() {
        let origin = Instant::now();
        let timing = QueryTiming::new(
            origin,
            9,
            Correlation::Request("01HZ".into()),
            TimingKind::Query,
        );
        timing.clock().time(QueryPhase::PoolWait, || ());
        timing.mark_work_started();
        timing.clock().enter(QueryPhase::Bind);

        let events = capture(|| {
            assert!(timing.emit_partial(Outcome::Timeout));
            assert!(
                !timing.emit_complete(Outcome::Success, Instant::now()),
                "the account is written once"
            );
        });
        let event = only(&events, "query_timing");
        let v = |name: &str| event.values.get(name).cloned();
        assert_eq!(v("timing_complete"), Some(Value::Bool(false)));
        assert_eq!(v("outcome"), Some(Value::Str("timeout".into())));
        assert_eq!(v("work_started"), Some(Value::Bool(true)));
        assert_eq!(v("active_query_phase"), Some(Value::Str("bind".into())));
        assert_eq!(v("query_bind_us"), None, "unfinished is unbooked");
        let Some(Value::U64(pool_wait)) = v("query_pool_wait_us") else {
            panic!("pool_wait booked: {event:?}");
        };
        let Some(Value::U64(active)) = v("active_elapsed_us") else {
            panic!("active elapsed present: {event:?}");
        };
        let Some(Value::U64(other)) = v("query_other_us") else {
            panic!("other present: {event:?}");
        };
        let Some(Value::U64(observed)) = v("query_observed_us") else {
            panic!("observed present: {event:?}");
        };
        assert_eq!(pool_wait + active + other, observed);
    }

    /// A worker that finishes a phase after the request decided to write
    /// its partial account, and before the copy is taken, cannot book
    /// time past the window's end: the window is cut with the copy,
    /// under the clock's lock, not at the moment the request decided.
    #[test]
    fn a_partial_window_covers_a_phase_finished_while_deciding() {
        use std::sync::Barrier;
        use trawl_engine::timing::Transition;

        let timing = QueryTiming::new(
            Instant::now(),
            11,
            Correlation::Request("01HZ".into()),
            TimingKind::Query,
        );
        let parked = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        {
            let parked = Arc::clone(&parked);
            let release = Arc::clone(&release);
            timing.clock().hold_on_enter(QueryPhase::Bind, move || {
                parked.wait();
                release.wait();
            });
        }
        let exited = Arc::new(Barrier::new(2));
        let worker = {
            let clock = timing.clock().clone();
            let exited = Arc::clone(&exited);
            std::thread::spawn(move || {
                clock.time(QueryPhase::Emit, || ());
                clock.time(QueryPhase::Bind, || ());
                exited.wait();
            })
        };

        // The worker is inside bind when the request decides to stop
        // waiting; it finishes bind before the request writes.
        parked.wait();
        timing.mark_work_started();
        release.wait();
        exited.wait();
        assert!(
            timing
                .clock()
                .transitions()
                .contains(&Transition::Exit(QueryPhase::Bind)),
            "bind finished before the copy"
        );

        let events = capture(|| {
            assert!(timing.emit_partial(Outcome::Timeout));
        });
        worker.join().expect("the worker finishes");

        let event = only(&events, "query_timing");
        let u = |name: &str| match event.values.get(name) {
            Some(Value::U64(v)) => Some(*v),
            None => None,
            other => panic!("{name} is an integer: {other:?}"),
        };
        let observed = u("query_observed_us").expect("observed");
        let other = u("query_other_us").expect("other");
        let completed: u64 = QueryPhase::ALL
            .into_iter()
            .filter_map(|phase| u(phase.field()))
            .sum();
        let active = u("active_elapsed_us").unwrap_or(0);
        assert!(u("query_bind_us").is_some(), "bind is booked: {event:?}");
        assert!(
            observed >= completed + active,
            "the window covers the totals: {observed} < {completed} + {active}"
        );
        assert_eq!(completed + active + other, observed);
    }

    /// A dropped handler guard writes the abandonment, complete, unless
    /// the pool took over or someone already wrote the account.
    #[test]
    fn a_dropped_guard_writes_abandoned_once() {
        let fresh = || {
            QueryTiming::new(
                Instant::now(),
                3,
                Correlation::Request("01HZ".into()),
                TimingKind::Query,
            )
        };

        let events = capture(|| {
            let guard = TimingGuard::new(fresh());
            guard.clock().enter(QueryPhase::SavedLookup);
            drop(guard);
        });
        let event = only(&events, "query_timing");
        assert_eq!(
            event.values.get("outcome"),
            Some(&Value::Str("abandoned".into()))
        );
        assert_eq!(
            event.values.get("timing_complete"),
            Some(&Value::Bool(true))
        );
        assert_eq!(event.values.get("work_started"), Some(&Value::Bool(false)));
        assert!(
            event.values.contains_key("query_saved_lookup_us"),
            "a request-side wait cut by the drop is booked"
        );

        let events = capture(|| {
            let guard = TimingGuard::new(fresh());
            guard.hand_emit_to_pool();
        });
        assert!(events.is_empty(), "the pool owns it: {events:?}");

        let events = capture(|| {
            let guard = TimingGuard::new(fresh());
            assert!(guard.emit_complete(Outcome::Success, Instant::now()));
        });
        assert_eq!(
            only(&events, "query_timing").values.get("outcome"),
            Some(&Value::Str("success".into()))
        );
    }

    /// A scheduled run carries its run id, a failure its class, and
    /// neither carries text.
    #[test]
    fn a_failed_run_carries_its_class_not_its_text() {
        let timing = QueryTiming::new(
            Instant::now(),
            4,
            Correlation::Run(42),
            TimingKind::Scheduled,
        );
        let error = ServerError::Engine(EngineError::Database(duckdb::Error::InvalidColumnName(
            "zz_secret_column".into(),
        )));
        let events = capture(|| {
            timing.emit_complete(Outcome::of_error(&error), Instant::now());
        });
        let event = only(&events, "query_timing");
        assert_eq!(event.values.get("run_id"), Some(&Value::I64(42)));
        assert_eq!(event.values.get("request_id"), None);
        assert_eq!(event.values.get("format"), None);
        assert_eq!(
            event.values.get("error_class"),
            Some(&Value::Str("database".into()))
        );
        assert!(
            !format!("{event:?}").contains("zz_secret"),
            "no error text: {event:?}"
        );
    }

    #[test]
    fn outcomes_map_from_errors_and_results() {
        assert_eq!(Outcome::of_error(&ServerError::Timeout), Outcome::Timeout);
        assert_eq!(
            Outcome::of_error(&ServerError::Panicked("query")),
            Outcome::Error("panic")
        );
        assert_eq!(
            [
                Outcome::Success,
                Outcome::Error("x"),
                Outcome::CapacityRefused,
                Outcome::Timeout,
                Outcome::Abandoned,
            ]
            .map(Outcome::as_str),
            [
                "success",
                "error",
                "capacity_refused",
                "timeout",
                "abandoned"
            ]
        );

        let ok: Result<(), ServerError> = Ok(());
        assert_eq!(PhysicalOutcome::of_result(&ok), PhysicalOutcome::Completed);
        let cancelled: Result<(), ServerError> = Err(ServerError::Engine(EngineError::Cancelled));
        assert_eq!(
            PhysicalOutcome::of_result(&cancelled),
            PhysicalOutcome::Cancelled
        );
        let panicked: Result<(), ServerError> = Err(ServerError::Panicked("query"));
        assert_eq!(
            PhysicalOutcome::of_result(&panicked),
            PhysicalOutcome::Panicked
        );
        let failed: Result<(), ServerError> = Err(ServerError::Timeout);
        assert_eq!(PhysicalOutcome::of_result(&failed), PhysicalOutcome::Failed);
        assert_eq!(PhysicalOutcome::NotStarted.as_str(), "not_started");
    }

    /// A reclaim for work that never started carries its physical
    /// outcome and no phases.
    #[test]
    fn a_reclaim_without_totals_carries_no_phases() {
        let correlation = Correlation::Request("01HZ".into());
        let events = capture(|| {
            emit_reclaimed(&Reclaim {
                query_id: 5,
                kind: WorkKind::Query,
                retained_for: Duration::from_millis(1),
                correlation: Some(&correlation),
                physical: PhysicalOutcome::NotStarted,
                totals: None,
            });
        });
        let event = only(&events, "query_permit_reclaimed");
        assert_eq!(
            event.values.get("physical_outcome"),
            Some(&Value::Str("not_started".into()))
        );
        assert!(
            !event
                .values
                .keys()
                .any(|k| k.starts_with("query_") && k.ends_with("_us")),
            "{event:?}"
        );
        assert_eq!(event.values.get("duckdb_attempts"), None);
    }
}
