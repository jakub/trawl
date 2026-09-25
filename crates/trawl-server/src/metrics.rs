// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Prometheus metrics: metric name constants, descriptions, and gauge collection.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use metrics::{describe_counter, describe_gauge, describe_histogram, gauge};

use crate::hot_buffer::HotBuffer;

// -- metric name constants ---------------------------------------------------

pub const QUERIES_TOTAL: &str = "trawl_queries_total";
pub const QUERY_DURATION: &str = "trawl_query_duration_seconds";
pub const INGEST_EVENTS_TOTAL: &str = "trawl_ingest_events_total";
pub const INGEST_EVENTS_REJECTED_TOTAL: &str = "trawl_ingest_events_rejected_total";
pub const INGEST_REPAIRS_TOTAL: &str = "trawl_ingest_repairs_total";
/// Events a profile producer (syslog, telemetry) had to drop, by
/// `{profile, reason}`.
///
/// Those producers have no one to reject to, so a drop means the server
/// refused its own boot-validated assertion: a bug, not a sender's
/// mistake. The whole closed label matrix is published at zero
/// (`producer::init_profile_reject_metrics`), because an absent
/// increment on a present series is what proves the salvage profiles are
/// rejection-free — an absent series only proves nothing was wired up.
/// The HTTP door keeps [`INGEST_EVENTS_REJECTED_TOTAL`], where per-event
/// rejection is the contract.
pub const INGEST_PROFILE_REJECT_TOTAL: &str = "trawl_ingest_profile_reject_total";
/// Accepted events whose severity source mapped to nothing on the `OTel`
/// ladder, so `_severity` was omitted (ADR-0013 §2). Deliberately a
/// counter rather than a repair code: derivation into the `_` namespace
/// touches nothing sender-visible, so there is nothing to confess in
/// `_repairs` — but a sender whose whole feed lands unmapped is an ops
/// question, and this is where it shows.
pub const SEVERITY_UNMAPPED_TOTAL: &str = "trawl_severity_unmapped_total";
pub const HOT_BUFFER_EVENTS: &str = "trawl_hot_buffer_events";
pub const HOT_BUFFER_BYTES: &str = "trawl_hot_buffer_bytes";
/// The full event cap (`ingest.hot_buffer_max_events`).
pub const HOT_BUFFER_MAX_EVENTS: &str = "trawl_hot_buffer_max_events";
/// The full byte cap (`ingest.hot_buffer_max_bytes`).
pub const HOT_BUFFER_MAX_BYTES: &str = "trawl_hot_buffer_max_bytes";
/// Seconds since the oldest resident batch was inserted; 0 when empty.
pub const HOT_BUFFER_OLDEST_BATCH_AGE_SECONDS: &str = "trawl_hot_buffer_oldest_batch_age_seconds";
/// [`crate::hot_buffer::AdmissionState`] as 0 (open), 1 (pressure) or
/// 2 (refusing).
pub const HOT_BUFFER_ADMISSION_STATE: &str = "trawl_hot_buffer_admission_state";
/// Reservations refused, by `{producer, kind}`
/// ([`crate::ingest::producer::ProducerKind`] ×
/// [`crate::hot_buffer::Refusal`]); the full matrix is zero-initialized.
pub const HOT_BUFFER_ADMISSION_REFUSALS_TOTAL: &str = "trawl_hot_buffer_admission_refusals_total";
/// The configured compaction interval, so an alert can scale the drain
/// stall threshold to it.
pub const COMPACTION_INTERVAL_SECONDS: &str = "trawl_compaction_interval_seconds";
pub const ACTIVE_CONNECTIONS: &str = "trawl_active_connections";
/// Pool permits held by work whose request already answered (ADR-0024).
///
/// A subset of the held permits, not an addition to them, and
/// deliberately label-free: the only cardinality a query id or a key
/// name could add here is unbounded.
pub const QUERY_PERMITS_RETAINED: &str = "trawl_query_permits_retained";
pub const PARQUET_FILES: &str = "trawl_parquet_files_total";
pub const PARQUET_BYTES: &str = "trawl_parquet_size_bytes";
pub const HEALTH_CHECK: &str = "trawl_health_check";
pub const SYSLOG_EVENTS_TOTAL: &str = "trawl_syslog_events_total";
pub const SYSLOG_PARSE_ERRORS_TOTAL: &str = "trawl_syslog_parse_errors_total";
pub const SYSLOG_EVENTS_DROPPED_TOTAL: &str = "trawl_syslog_events_dropped_total";
pub const SYSLOG_TCP_CONNECTIONS: &str = "trawl_syslog_tcp_connections";
pub const WAL_FILES: &str = "trawl_wal_files";
pub const WAL_BYTES: &str = "trawl_wal_bytes";
pub const CATALOG_CONFLICTS_TOTAL: &str = "trawl_catalog_conflicts_total";
pub const CATALOG_ROWS_NULLED_TOTAL: &str = "trawl_catalog_rows_nulled_total";
pub const CATALOG_CONFORM_REWRITES_TOTAL: &str = "trawl_catalog_conform_rewrites_total";
pub const CATALOG_CONFORM_SKIPPED_TOTAL: &str = "trawl_catalog_conform_skipped_total";
pub const CATALOG_PINS_REJECTED_TOTAL: &str = "trawl_catalog_pins_rejected_total";
pub const CATALOG_SAMPLE_CAPTURE_FAILURES_TOTAL: &str =
    "trawl_catalog_sample_capture_failures_total";
/// Batches whose catalog bookkeeping ran out of its wall-clock budget and
/// was abandoned, labelled by the write that was in flight when the budget
/// expired ([`BookkeepingWrite`]).
///
/// The log line that goes with it (`catalog_bookkeeping_timeout`) says the
/// same thing once per batch; this is the series to alert on, because a
/// sustained postgres outage costs one budget per batch and the evidence it
/// abandons never comes back on its own. Retry exhaustion that fails FAST
/// is a different failure and does not count here: it stays on
/// `catalog_bookkeeping_error`.
pub const CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL: &str = "trawl_catalog_bookkeeping_timeouts_total";
pub const CATALOG_PINNED_FIELDS: &str = "trawl_catalog_pinned_fields";
pub const CATALOG_PIN_CAPACITY: &str = "trawl_catalog_pin_capacity";
pub const CATALOG_DEGRADED_FIELDS: &str = "trawl_catalog_degraded_fields";
pub const CATALOG_PINS_GC_TOTAL: &str = "trawl_catalog_pins_gc_total";
pub const CATALOG_REPIN_JOBS_TOTAL: &str = "trawl_catalog_repin_jobs_total";
pub const CATALOG_REPIN_RUNNING: &str = "trawl_catalog_repin_running";
pub const CATALOG_REPIN_FILES_TOTAL: &str = "trawl_catalog_repin_files_total";
pub const CATALOG_REPIN_FILES_DONE: &str = "trawl_catalog_repin_files_done";
pub const CATALOG_REPIN_ROWS_NULLED_TOTAL: &str = "trawl_catalog_repin_rows_nulled_total";
pub const CATALOG_REPIN_ROWS_RESURRECTED_TOTAL: &str = "trawl_catalog_repin_rows_resurrected_total";
pub const CATALOG_REPIN_DURATION_SECONDS: &str = "trawl_catalog_repin_duration_seconds";
pub const RETENTION_SUPPRESSED: &str = "trawl_retention_suppressed";
pub const SCHEDULER_WINDOW_TRUNCATED_TOTAL: &str = "trawl_scheduler_window_truncated_total";
pub const AUTH_FAILURES_TOTAL: &str = "trawl_auth_failures_total";
pub const TELEMETRY_WAL_WRITE_FAILURES_TOTAL: &str = "trawl_telemetry_wal_write_failures_total";
pub const TELEMETRY_EVENTS_DROPPED_TOTAL: &str = "trawl_telemetry_events_dropped_total";
pub const TELEMETRY_BYTES_DROPPED_TOTAL: &str = "trawl_telemetry_bytes_dropped_total";
pub const TELEMETRY_BUFFER_EVENTS: &str = "trawl_telemetry_buffer_events";
pub const TELEMETRY_BUFFER_BYTES: &str = "trawl_telemetry_buffer_bytes";

// -- operational alert measurements -----------------------------------------

pub const SYSLOG_WAL_EVENTS_DISCARDED_TOTAL: &str = "trawl_syslog_wal_events_discarded_total";
pub const SYSLOG_WRITE_TASKS_FAILED_TOTAL: &str = "trawl_syslog_write_tasks_failed_total";
pub const WAL_DURABILITY_FAILURES_TOTAL: &str = "trawl_wal_durability_failures_total";
pub const COMPACTION_OPERATION_FAILURES_TOTAL: &str = "trawl_compaction_operation_failures_total";
pub const FILES_QUARANTINED_TOTAL: &str = "trawl_files_quarantined_total";
/// Publication markers recovery looked at, by `outcome`
/// ([`crate::ingest::publication_marker::RecoveryOutcomeKind`]). A
/// `contradictory` or `failed` outcome leaves the marker blocking its
/// service's compaction until a later pass or an operator resolves it.
pub const PUBLICATION_RECOVERY_TOTAL: &str = "trawl_publication_recovery_total";

/// Failed durability operations on an already-published WAL file. Each
/// failure rejects the write it belonged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalDurabilityOperation {
    ParentDirectorySync,
}

impl WalDurabilityOperation {
    pub const ALL: [Self; 1] = [Self::ParentDirectorySync];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ParentDirectorySync => "parent_directory_sync",
        }
    }
}

/// Independent failed attempts, never the mixed dashboard error tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOperation {
    WalRootScan,
    WalEnvironmentScan,
    Chunk,
    DailyRollupScan,
    DailyRollupUnit,
    PendingRollupScan,
    PendingRollupRecovery,
    ConsumedWalRemoval,
}

impl CompactionOperation {
    pub const ALL: [Self; 8] = [
        Self::WalRootScan,
        Self::WalEnvironmentScan,
        Self::Chunk,
        Self::DailyRollupScan,
        Self::DailyRollupUnit,
        Self::PendingRollupScan,
        Self::PendingRollupRecovery,
        Self::ConsumedWalRemoval,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::WalRootScan => "wal_root_scan",
            Self::WalEnvironmentScan => "wal_environment_scan",
            Self::Chunk => "chunk",
            Self::DailyRollupScan => "daily_rollup_scan",
            Self::DailyRollupUnit => "daily_rollup_unit",
            Self::PendingRollupScan => "pending_rollup_scan",
            Self::PendingRollupRecovery => "pending_rollup_recovery",
            Self::ConsumedWalRemoval => "consumed_wal_removal",
        }
    }

    /// Record at the failed attempt's owner, never again during propagation.
    pub(crate) fn record_failure(self) {
        metrics::counter!(COMPACTION_OPERATION_FAILURES_TOTAL, "operation" => self.label())
            .increment(1);
    }
}

/// Successfully isolated files; this is not an event-loss count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineKind {
    Wal,
    Parquet,
    RollupTemporary,
}

impl QuarantineKind {
    pub const ALL: [Self; 3] = [Self::Wal, Self::Parquet, Self::RollupTemporary];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Wal => "wal",
            Self::Parquet => "parquet",
            Self::RollupTemporary => "rollup_temporary",
        }
    }
}

/// Why the syslog receive queue abandoned an event: the `reason` label on
/// [`SYSLOG_EVENTS_DROPPED_TOTAL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyslogDropReason {
    /// The queue was full because the batcher stopped taking from it while
    /// hot-buffer admission refused its pending groups.
    Backpressure,
    /// The queue was full or closed for any other reason.
    QueueFull,
}

impl SyslogDropReason {
    pub const ALL: [Self; 2] = [Self::Backpressure, Self::QueueFull];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Backpressure => "backpressure",
            Self::QueueFull => "queue_full",
        }
    }
}

/// Existing telemetry reasons. A consumed crashed batch may be durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryDropReason {
    PreinitCap,
    BufferCap,
    WriteCrashed,
    /// An unmetered failure event past the per-minute persistence cap
    /// (ADR-0040). It still reached stdout; no alert selects it.
    UnmeteredCap,
}

impl TelemetryDropReason {
    pub const ALL: [Self; 4] = [
        Self::PreinitCap,
        Self::BufferCap,
        Self::WriteCrashed,
        Self::UnmeteredCap,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::PreinitCap => "preinit_cap",
            Self::BufferCap => "buffer_cap",
            Self::WriteCrashed => "write_crashed",
            Self::UnmeteredCap => "unmetered_cap",
        }
    }
}

/// Production and recorder tests share this configuration. Counter baselines
/// must survive idle upkeep: do not enable exporter expiry for these counters.
/// The exporter currently defaults to no expiry; keep that default here.
pub fn prometheus_builder() -> metrics_exporter_prometheus::PrometheusBuilder {
    metrics_exporter_prometheus::PrometheusBuilder::new()
}

/// Publish every series selected by the starter alerts, regardless of which
/// producers are enabled. Call after recorder installation, before serving.
/// Repeated calls only register/increment by zero; they never reset counters.
/// This cannot reconstruct increments made before recorder installation.
pub fn init_operational_alert_metrics() {
    for name in [
        SYSLOG_WAL_EVENTS_DISCARDED_TOTAL,
        SYSLOG_WRITE_TASKS_FAILED_TOTAL,
        TELEMETRY_WAL_WRITE_FAILURES_TOTAL,
    ] {
        metrics::counter!(name).increment(0);
    }
    for reason in SyslogDropReason::ALL {
        metrics::counter!(SYSLOG_EVENTS_DROPPED_TOTAL, "reason" => reason.label()).increment(0);
    }
    for producer in crate::ingest::producer::ProducerKind::ALL {
        for kind in crate::hot_buffer::Refusal::ALL {
            metrics::counter!(HOT_BUFFER_ADMISSION_REFUSALS_TOTAL,
                "producer" => producer.as_str(),
                "kind" => kind.label())
            .increment(0);
        }
    }
    for reason in TelemetryDropReason::ALL {
        metrics::counter!(TELEMETRY_EVENTS_DROPPED_TOTAL, "reason" => reason.label()).increment(0);
    }
    for reason in [
        crate::ingest::envelope::RejectReason::WalFailure,
        crate::ingest::envelope::RejectReason::HotBufferFull,
        crate::ingest::envelope::RejectReason::IngestBatchTooLarge,
    ] {
        metrics::counter!(INGEST_EVENTS_REJECTED_TOTAL, "reason" => reason.as_str()).increment(0);
    }
    for operation in WalDurabilityOperation::ALL {
        metrics::counter!(WAL_DURABILITY_FAILURES_TOTAL, "operation" => operation.label())
            .increment(0);
    }
    for operation in CompactionOperation::ALL {
        metrics::counter!(COMPACTION_OPERATION_FAILURES_TOTAL, "operation" => operation.label())
            .increment(0);
    }
    for kind in QuarantineKind::ALL {
        metrics::counter!(FILES_QUARANTINED_TOTAL, "kind" => kind.label()).increment(0);
    }
    init_publication_recovery_metrics();
}

/// Publish the publication-recovery outcome matrix at zero, so a flat
/// `contradictory` series reads as "none seen" rather than "never wired up".
/// Repeated calls never reset counters.
pub fn init_publication_recovery_metrics() {
    for outcome in crate::ingest::publication_marker::RecoveryOutcomeKind::ALL {
        metrics::counter!(PUBLICATION_RECOVERY_TOTAL, "outcome" => outcome.label()).increment(0);
    }
}

// -- bookkeeping write identity ----------------------------------------------

/// The two catalog bookkeeping writes one compacted batch makes, in the
/// order it makes them.
///
/// One enum, two spellings, because the metric and the log answer different
/// questions. [`Self::label`] is the closed `write` label value on
/// [`CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL`], named for what the write is FOR;
/// [`Self::table`] is the postgres table the log line has always named, kept
/// verbatim so an operator's existing search for `write="field_services"`
/// still finds its lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookkeepingWrite {
    /// `record_conflicts`: the values this batch's pins shelved.
    Conflicts,
    /// `touch_services`: which service was seen carrying which field.
    Observations,
}

impl BookkeepingWrite {
    /// Every variant, for zero-initializing the label matrix.
    pub const ALL: [BookkeepingWrite; 2] = [Self::Conflicts, Self::Observations];

    /// The metric label value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Conflicts => "conflicts",
            Self::Observations => "observations",
        }
    }

    /// The postgres table, as the log field spells it.
    #[must_use]
    pub fn table(self) -> &'static str {
        match self {
            Self::Conflicts => "field_conflicts",
            Self::Observations => "field_services",
        }
    }
}

// -- description registration ------------------------------------------------

/// Register metric descriptions (help text + units). Call once at startup.
#[allow(clippy::too_many_lines)] // a flat list of describe calls, one per metric
pub fn describe_metrics() {
    describe_counter!(QUERIES_TOTAL, "Total number of queries executed");
    describe_histogram!(QUERY_DURATION, "Query execution duration in seconds");
    describe_counter!(INGEST_EVENTS_TOTAL, "Total number of ingested events");
    describe_counter!(
        INGEST_EVENTS_REJECTED_TOTAL,
        "Total number of rejected ingest events"
    );
    describe_counter!(
        INGEST_PROFILE_REJECT_TOTAL,
        "Events dropped by a producer profile that cannot reject to its \
         sender (syslog, internal telemetry), labelled by profile and \
         reason; the label matrix is zero-initialized, so a flat series \
         is the rejection-free invariant holding"
    );
    describe_counter!(
        INGEST_REPAIRS_TOTAL,
        "Repairs applied to accepted ingest events, labelled by repair code \
         and service (codes also recorded per-event in _repairs); services \
         beyond the first 256 seen collapse into service=\"<other>\""
    );
    describe_gauge!(
        HOT_BUFFER_EVENTS,
        "Current number of events in the hot buffer"
    );
    describe_gauge!(HOT_BUFFER_BYTES, "Current byte size of the hot buffer");
    describe_gauge!(
        HOT_BUFFER_MAX_EVENTS,
        "Hot-buffer event cap (ingest.hot_buffer_max_events); external \
         producers may fill 15/16 of it, self-telemetry all of it"
    );
    describe_gauge!(
        HOT_BUFFER_MAX_BYTES,
        "Hot-buffer serialized-byte cap (ingest.hot_buffer_max_bytes); \
         external producers may fill 15/16 of it, self-telemetry all of it"
    );
    describe_gauge!(
        HOT_BUFFER_OLDEST_BATCH_AGE_SECONDS,
        "Seconds since the oldest resident hot-buffer batch was inserted, \
         0 when the buffer is empty; rising past a few compaction intervals \
         means compaction is not draining"
    );
    describe_gauge!(
        HOT_BUFFER_ADMISSION_STATE,
        "Hot-buffer admission state: 0 = open, 1 = pressure (at or above \
         half of either cap, compaction drains early), 2 = refusing (a \
         reservation was refused for lack of space; clears below a quarter \
         of both caps)"
    );
    describe_counter!(
        HOT_BUFFER_ADMISSION_REFUSALS_TOTAL,
        "Hot-buffer reservations refused, labelled by producer (http, \
         syslog, trawld) and kind (full = no free space, retry after \
         compaction drains; oversized = larger than the producer's ceiling, \
         can never fit)"
    );
    describe_gauge!(
        COMPACTION_INTERVAL_SECONDS,
        "Configured compaction interval (ingest.compaction_interval_secs)"
    );
    describe_gauge!(ACTIVE_CONNECTIONS, "Number of in-flight HTTP requests");
    describe_gauge!(
        QUERY_PERMITS_RETAINED,
        "Executor-pool permits held by query work whose request already \
         answered (a subset of the permits in use)"
    );
    describe_gauge!(
        PARQUET_FILES,
        "Ingested Parquet file count from the last complete measurement, excluding saved reports. Retained after collection failure."
    );
    describe_gauge!(
        PARQUET_BYTES,
        "Ingested Parquet bytes from the last complete measurement, excluding saved reports. Retained after collection failure."
    );
    describe_gauge!(
        HEALTH_CHECK,
        "Subsystem health (1 = ok, 0 = failed), labeled by subsystem"
    );
    describe_counter!(
        SYSLOG_EVENTS_TOTAL,
        "Total events ingested via syslog listener"
    );
    describe_counter!(
        SYSLOG_PARSE_ERRORS_TOTAL,
        "Total unparseable syslog messages"
    );
    describe_counter!(
        SYSLOG_EVENTS_DROPPED_TOTAL,
        "Syslog events abandoned because the batch queue was full or closed, \
         labelled by reason (backpressure = the queue was full while hot-buffer \
         admission was refusing, queue_full = any other full or closed queue); \
         TCP and UDP combined"
    );
    describe_counter!(
        SYSLOG_WAL_EVENTS_DISCARDED_TOTAL,
        "Syslog events abandoned from normal ingestion after their group WAL write failed; temporary bytes or sender copies may remain"
    );
    describe_counter!(
        SYSLOG_WRITE_TASKS_FAILED_TOTAL,
        "Failed syslog flush tasks with uncertain write outcome; earlier groups or the current group may already be durable"
    );
    describe_counter!(
        WAL_DURABILITY_FAILURES_TOTAL,
        "Failed WAL durability operations after file publication, labelled by operation; each failure rejects its write, whose file is withdrawn when possible"
    );
    describe_counter!(
        COMPACTION_OPERATION_FAILURES_TOTAL,
        "Failed compaction operation attempts, labelled by operation; excludes successful quarantines, idle work and intentional suppression"
    );
    describe_counter!(
        PUBLICATION_RECOVERY_TOTAL,
        "Compaction publication markers examined by recovery, labelled by outcome (published, unpublished, contradictory, failed); contradictory and failed markers keep their service's compaction blocked"
    );
    describe_counter!(
        FILES_QUARANTINED_TOTAL,
        "Files successfully renamed out of normal processing with bytes retained, labelled by kind; temporary rollup sources may remain intact, not an event-loss count"
    );
    describe_gauge!(
        SYSLOG_TCP_CONNECTIONS,
        "Current active syslog TCP connections"
    );
    describe_gauge!(
        WAL_FILES,
        "WAL ndjson file count from the last complete measurement, including active files. Retained after collection failure."
    );
    describe_gauge!(
        WAL_BYTES,
        "WAL ndjson bytes from the last complete measurement, including active files. Retained after collection failure."
    );
    describe_counter!(
        CATALOG_CONFLICTS_TOTAL,
        "Field-catalog type conflicts recorded at compaction (a batch column \
         TRY_CAST to its pinned type), labelled by service (same 256-service \
         cap as trawl_ingest_repairs_total; never a field-name label)"
    );
    describe_counter!(
        CATALOG_ROWS_NULLED_TOTAL,
        "Rows whose value a catalog-conforming cast nulled (original \
         recoverable from _raw), labelled by service"
    );
    describe_counter!(
        CATALOG_CONFORM_REWRITES_TOTAL,
        "Parquet files rewritten by the boot conformance pass to match the \
         field catalog"
    );
    describe_counter!(
        CATALOG_CONFORM_SKIPPED_TOTAL,
        "Parquet files the boot conformance pass could not read (truncated, \
         bit-rotted, or foreign) and skipped; they stay outside the catalog \
         invariant and the pass re-runs on the next boot"
    );
    describe_counter!(
        CATALOG_PINS_REJECTED_TOTAL,
        "Fields denied a catalog pin, labelled by reason (name_too_long, \
         cap); their columns are not stored and the values remain in _raw"
    );
    describe_counter!(
        CATALOG_SAMPLE_CAPTURE_FAILURES_TOTAL,
        "Batches whose misfit-sample capture failed (typically an out-of-memory \
         on a column with pathological misfit cardinality). The conflict COUNTS \
         are still recorded and the values remain in _raw — only the sample \
         evidence is missing"
    );
    describe_counter!(
        CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL,
        "Compacted batches whose catalog bookkeeping was abandoned at its \
         two-second budget, labelled by the write in flight (conflicts = \
         the shelved-value evidence, observations = the per-service field \
         sightings). Compaction keeps draining the WAL, so nothing stalls, \
         but the abandoned write is a permanent hole: a lost observation \
         is re-made only when that service next sends that field. A rising \
         series means postgres is too slow for the budget"
    );
    describe_gauge!(
        CATALOG_PINNED_FIELDS,
        "Field-catalog pins in use. Ingest never gives a slot back, so \
         this only climbs until an operator runs pin gc — alert on it \
         against trawl_catalog_pin_capacity, well before the cap starts \
         denying pins"
    );
    describe_gauge!(
        CATALOG_PIN_CAPACITY,
        "Field-catalog pin ceiling (store::catalog::MAX_PINNED_FIELDS); a \
         field arriving at a full catalog is never stored as a column"
    );
    describe_gauge!(
        CATALOG_DEGRADED_FIELDS,
        "Pinned fields the analyzer currently calls degraded: their pin has \
         been shelving values for over a day, in volume. Alert on it RISING \
         — the remedy is an operator-approved `trawl schema repin`, and \
         nothing clears the count on its own. Advisory and \
         sender-influenceable by construction: one misbehaving producer can \
         raise it, which is why it may never gate anything automatically"
    );
    describe_counter!(
        CATALOG_PINS_GC_TOTAL,
        "Pin slots reclaimed by `trawl schema gc-pins`: fields nothing had \
         observed for the dead window and no standing parquet footer \
         declared. Operator-triggered, so it moves in steps and only when \
         somebody runs the command; dry runs never increment it"
    );
    describe_counter!(
        CATALOG_REPIN_JOBS_TOTAL,
        "Repin jobs finished, labelled by outcome (succeeded, failed, \
         refused_needs_force, blocked) — dry runs count as succeeded"
    );
    describe_gauge!(
        CATALOG_REPIN_RUNNING,
        "1 while a repin rewrite is executing (one at a time, install-wide)"
    );
    describe_gauge!(
        CATALOG_REPIN_FILES_TOTAL,
        "Affected files the running (or last) repin job's scan found"
    );
    describe_gauge!(
        CATALOG_REPIN_FILES_DONE,
        "Affected files the running repin job has rewritten so far"
    );
    describe_counter!(
        CATALOG_REPIN_ROWS_NULLED_TOTAL,
        "Stored values repin rewrites could not keep under the new pin \
         (forced lossy repins; originals remain findable in _raw)"
    );
    describe_counter!(
        CATALOG_REPIN_ROWS_RESURRECTED_TOTAL,
        "Values repin rewrites recovered from _raw into the structured column"
    );
    describe_histogram!(
        CATALOG_REPIN_DURATION_SECONDS,
        "End-to-end repin job duration (build, catch-up, cutover, sweep)"
    );
    describe_gauge!(
        RETENTION_SUPPRESSED,
        "1 while retention sweeps (age AND disk pressure) stand down for \
         repin staging on the data root or for publication markers under the \
         WAL root that cannot be read, 0 when they run. Unlike \
         trawl_catalog_repin_running this stays 1 for staging no job owns \
         — a boot replay whose sweep keeps failing — so alert on it held \
         high across ticks: the archive grows unbounded meanwhile"
    );
    describe_counter!(
        SCHEDULER_WINDOW_TRUNCATED_TOTAL,
        "Report runs, scheduled or manual, whose since_last catch-up window \
         was clamped to max_catchup_intervals"
    );
    describe_counter!(
        AUTH_FAILURES_TOTAL,
        "Requests rejected by the authenticated routers' auth stack, \
         labelled by reason (unauthorized = missing, malformed, invalid, \
         revoked or expired bearer token; backend_unavailable = the \
         keystore failed to answer; no_trawl_grant = a verified key that \
         resolves no usable trawl permission; forbidden / internal = \
         defensive, an unmarked rejection from the bearer shell). The \
         reason set is closed and carries no key, name or path label. The \
         events themselves are stdout-only by design — every one of these \
         rejections is decided outside the per-key rate limiter (see \
         telemetry::UNMETERED_TARGETS) — so this counter is the only \
         in-product signal for credential stuffing, token brute force and \
         a revoked key still in use"
    );
    describe_counter!(
        TELEMETRY_WAL_WRITE_FAILURES_TOTAL,
        "Self-telemetry WAL write failures; an ordinary failed batch is \
         retained for retry, while a panicked or cancelled write task also \
         records its consumed batch under dropped reason write_crashed; \
         that batch may already be durable, so both signals can describe one crash"
    );
    describe_counter!(
        TELEMETRY_EVENTS_DROPPED_TOTAL,
        "Self-telemetry events dropped, labelled by reason (preinit_cap = \
         bootstrap buffer overflow before the WAL writer was injected, \
         buffer_cap = the shared active+queue+in-flight memory budget was \
         full during a prolonged WAL outage, write_crashed = a panicked or \
         cancelled blocking write consumed the batch, possibly after durable \
         publication; this reason does not prove permanent event loss; \
         unmetered_cap = an unmetered server-failure event past the fixed \
         60-per-minute persistence cap, still written to stdout)"
    );
    describe_counter!(
        TELEMETRY_BYTES_DROPPED_TOTAL,
        "Self-telemetry ndjson bytes dropped, labelled by reason (exact \
         for buffer_cap and write_crashed; a mean-line-size estimate for \
         preinit_cap, which drops before serialization)"
    );
    describe_gauge!(
        TELEMETRY_BUFFER_EVENTS,
        "Self-telemetry events held in memory — active buffer, retry queue \
         and the batch in flight through a WAL write; nonzero across cycles \
         means the WAL is unhealthy — warns before loss begins"
    );
    describe_gauge!(
        TELEMETRY_BUFFER_BYTES,
        "Estimated bytes charged against ingest.telemetry_buffer_max_bytes \
         by ALL self-telemetry memory — active buffer, retry queue and the \
         in-flight batch (serialized bytes plus retained event maps)"
    );

    // A described gauge has no series until something sets it, and the
    // degraded count is set by a postgres read on the schema-refresh tick:
    // a node that boots with the store unreachable would export nothing at
    // all, which a dashboard reads exactly like "no degraded fields". Seed
    // it here — at registration, before the first tick — so absence means
    // "not scraped" and 0 means "none". The refresh's error path keeps the
    // previous value for the same reason.
    gauge!(CATALOG_DEGRADED_FIELDS).set(0.0);

    // Same reasoning for the bookkeeping timeouts, and one step stronger:
    // a test that asserts bookkeeping stayed quiet reads a DELTA, and a
    // delta over an absent series cannot tell "no timeouts" from "never
    // wired up". Publish both label values at zero here.
    for write in BookkeepingWrite::ALL {
        metrics::counter!(
            CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL,
            "write" => write.label(),
        )
        .increment(0);
    }
}

// -- bounded label values ----------------------------------------------------

/// Maximum distinct `service` label values admitted to `trawl_ingest_repairs_total`.
///
/// Every other label in this crate (`reason`, `status`, `transport`, `subsystem`)
/// comes from a closed, code-defined set. `service` is client-supplied and its
/// charset admits effectively unbounded values, while the prometheus recorder
/// retains counter series for the process lifetime — so without a cap any key
/// holding `ingest` could grow the registry and the `/metrics` payload without
/// bound by posting events with fresh service names.
pub const REPAIR_SERVICE_LABEL_CAP: usize = 256;

/// Label value that novel services collapse into once the cap is reached.
///
/// The angle brackets are outside the ingest service charset (alphanumeric,
/// dash, underscore, dot), so this can never collide with a real service name.
pub const OVERFLOW_SERVICE_LABEL: &str = "<other>";

/// Process-wide set of service names already admitted as a repair label value.
fn repair_service_labels() -> &'static Mutex<HashSet<String>> {
    static LABELS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    LABELS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Map a client-supplied service name onto a bounded `trawl_ingest_repairs_total`
/// label value.
///
/// Already-admitted services pass through verbatim; once
/// [`REPAIR_SERVICE_LABEL_CAP`] distinct services have been admitted, further
/// novel names return [`OVERFLOW_SERVICE_LABEL`] so the series count stays
/// bounded at `cap + 1` per repair code.
pub fn repair_service_label(service: &str) -> String {
    let mut admitted = repair_service_labels()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    bounded_label(&mut admitted, service, REPAIR_SERVICE_LABEL_CAP)
}

/// Cap-enforcing core of [`repair_service_label`], split out so it is testable
/// without the process-wide static.
fn bounded_label(admitted: &mut HashSet<String>, service: &str, cap: usize) -> String {
    if admitted.contains(service) {
        return service.to_string();
    }
    if admitted.len() >= cap {
        return OVERFLOW_SERVICE_LABEL.to_string();
    }
    admitted.insert(service.to_string());
    service.to_string()
}

// -- gauge collection --------------------------------------------------------

/// The configured compaction interval in seconds; `u64::MAX` until
/// [`set_compaction_interval_secs`] runs.
static COMPACTION_INTERVAL_SECS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Record the configured compaction interval for
/// [`COMPACTION_INTERVAL_SECONDS`]. Call once at startup, before serving;
/// [`collect_gauges`] publishes it from then on.
pub fn set_compaction_interval_secs(secs: u64) {
    COMPACTION_INTERVAL_SECS.store(secs, Ordering::Relaxed);
}

fn compaction_interval_secs() -> Option<u64> {
    let secs = COMPACTION_INTERVAL_SECS.load(Ordering::Relaxed);
    (secs != u64::MAX).then_some(secs)
}

/// Preserve the existing storage collection cadence, including failed attempts.
const STORAGE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StorageTotals {
    files: u64,
    bytes: u64,
}

#[derive(Clone, Copy)]
struct CompleteStorageSample {
    totals: StorageTotals,
    completed_at: Instant,
}

#[derive(Default)]
struct StorageState {
    complete: Option<CompleteStorageSample>,
    /// Completion of any attempt throttles retries independently of sample age.
    attempt_finished_at: Option<Instant>,
    failed: bool,
}

/// One source's attempts and gauge publication share ownership. Dashboard reads
/// only lock `state`, which is never held over a scan or gauge publication.
#[derive(Default)]
struct StorageCache {
    attempt: Mutex<()>,
    state: Mutex<StorageState>,
}

/// A coherent view assembled under one short cache lock.
pub(crate) struct StorageSnapshot {
    pub files: u64,
    pub bytes: u64,
    pub measurement: trawl_api::StorageMeasurement,
}

impl StorageCache {
    fn snapshot(&self, configured: bool, now: Instant) -> StorageSnapshot {
        use trawl_api::StorageMeasurementStatus as Status;
        let state = self.state.lock().expect("storage cache poisoned");
        let (status, sample) = if !configured {
            (Status::NotConfigured, None)
        } else if state.failed {
            (Status::Failed, state.complete)
        } else if state.complete.is_some() {
            (Status::Complete, state.complete)
        } else {
            (Status::NotSampled, None)
        };
        let totals = sample.map_or_else(StorageTotals::default, |s| s.totals);
        StorageSnapshot {
            files: totals.files,
            bytes: totals.bytes,
            measurement: trawl_api::StorageMeasurement {
                status,
                sample_age_secs: sample
                    .map(|s| now.saturating_duration_since(s.completed_at).as_secs()),
            },
        }
    }

    /// `clock`, `scan`, and `publish` are instance-scoped seams: tests control
    /// completion times, filesystem failures, and publication barriers without
    /// changing process-global caches or the metrics recorder.
    fn collect(
        &self,
        clock: impl Fn() -> Instant,
        scan: impl FnOnce() -> std::io::Result<StorageTotals>,
        mut publish: impl FnMut(StorageTotals),
    ) {
        let _owner = self.attempt.lock().expect("storage attempt poisoned");
        // Recheck after ownership: another emitter or scrape may have finished
        // while this caller waited. Never publish gauges outside this owner.
        let due = {
            let state = self.state.lock().expect("storage cache poisoned");
            state
                .attempt_finished_at
                .is_none_or(|at| clock().saturating_duration_since(at) >= STORAGE_CACHE_TTL)
        };
        if due {
            let result = scan();
            let completed_at = clock();
            let mut state = self.state.lock().expect("storage cache poisoned");
            state.attempt_finished_at = Some(completed_at);
            state.failed = result.is_err();
            if let Ok(totals) = result {
                state.complete = Some(CompleteStorageSample {
                    totals,
                    completed_at,
                });
            }
        }
        let sample = self.state.lock().expect("storage cache poisoned").complete;
        if let Some(sample) = sample {
            // No first-success sample means no invented numeric gauge. A failed
            // attempt retains complete totals, including genuinely measured zero.
            publish(sample.totals);
        }
    }
}

fn parquet_cache() -> &'static StorageCache {
    static CACHE: OnceLock<StorageCache> = OnceLock::new();
    CACHE.get_or_init(StorageCache::default)
}

fn wal_cache() -> &'static StorageCache {
    static CACHE: OnceLock<StorageCache> = OnceLock::new();
    CACHE.get_or_init(StorageCache::default)
}

/// Update gauges that require periodic polling (hot buffer + parquet + WAL files).
///
/// Called by Prometheus scrapes and the stats emitter. Filesystem scans and
/// competing attempts may block; dashboard readers only read the short cache.
#[allow(clippy::cast_precision_loss)] // gauge values are f64; precision loss beyond 2^52 is fine
pub fn collect_gauges(
    hot_buffer: Option<&Arc<HotBuffer>>,
    fallback_glob: &str,
    wal_dir: Option<&Path>,
    retained_permits: usize,
) {
    // Callers snapshot the pool count before collection. No pool registry lock
    // travels with this number through a storage scan or attempt-owner wait.
    metrics::gauge!(QUERY_PERMITS_RETAINED).set(retained_permits as f64);

    if let Some(buf) = hot_buffer {
        metrics::gauge!(HOT_BUFFER_EVENTS).set(buf.event_count() as f64);
        metrics::gauge!(HOT_BUFFER_BYTES).set(buf.byte_count() as f64);
        metrics::gauge!(HOT_BUFFER_MAX_EVENTS).set(buf.config().max_events as f64);
        metrics::gauge!(HOT_BUFFER_MAX_BYTES).set(buf.config().max_bytes as f64);
        metrics::gauge!(HOT_BUFFER_OLDEST_BATCH_AGE_SECONDS)
            .set(buf.oldest_batch_age().map_or(0.0, |age| age.as_secs_f64()));
        metrics::gauge!(HOT_BUFFER_ADMISSION_STATE).set(f64::from(buf.admission_state().as_u8()));
    }
    if let Some(secs) = compaction_interval_secs() {
        metrics::gauge!(COMPACTION_INTERVAL_SECONDS).set(secs as f64);
    }

    // Parquet file gauges — walk the glob pattern's parent directory.
    collect_parquet_gauges(fallback_glob);

    if let Some(dir) = wal_dir {
        collect_wal_gauges(dir);
    }
}

/// Collect the ingested Parquet totals with the shared attempt/cache policy.
fn collect_parquet_gauges(fallback_glob: &str) {
    // Preserve the fallback glob's existing root selection.
    let base = fallback_glob
        .find('*')
        .map_or(fallback_glob, |pos| &fallback_glob[..pos]);
    // Preserve `/` itself rather than turning an absolute root into an empty path.
    let trimmed = base.trim_end_matches('/');
    let base = Path::new(if trimmed.is_empty() && base.starts_with('/') {
        "/"
    } else {
        trimmed
    });
    parquet_cache().collect(
        Instant::now,
        || scan_storage(base, StorageKind::Parquet),
        |totals| publish_storage_gauges(StorageKind::Parquet, totals),
    );
}

/// One walked `.parquet` file and its size on disk.
pub(crate) type ParquetEntry = (std::path::PathBuf, u64);

/// One path the walk could not enumerate, and why.
pub(crate) type WalkError = (std::path::PathBuf, std::io::Error);

/// Recursively walk a directory collecting `.parquet` file paths and sizes.
///
/// Strict: any IO error anywhere under `dir` fails the whole walk. A caller
/// that must not be taken down by one unreadable corner of the tree wants
/// [`walk_parquet_files_lossy`] instead.
pub(crate) fn walk_parquet_files(dir: &Path) -> std::io::Result<Vec<ParquetEntry>> {
    let (results, errors) = walk_parquet_files_lossy(dir);
    match errors.into_iter().next() {
        Some((_, e)) => Err(e),
        None => Ok(results),
    }
}

/// The same walk, isolating IO failures instead of aborting on the first:
/// returns every file that was enumerable plus one `(path, error)` pair per
/// directory or entry that was not.
///
/// A caller that treats an unreadable path as a per-path skip — the boot
/// conformance pass, where one unreadable subdirectory must not keep the
/// daemon down — needs the readable remainder, not an early return.
pub(crate) fn walk_parquet_files_lossy(dir: &Path) -> (Vec<ParquetEntry>, Vec<WalkError>) {
    let mut results = Vec::new();
    let mut errors = Vec::new();
    walk_dir_recursive(dir, &mut results, &mut errors);
    (results, errors)
}

fn walk_dir_recursive(dir: &Path, results: &mut Vec<ParquetEntry>, errors: &mut Vec<WalkError>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push((dir.to_path_buf(), e));
            return;
        }
    };
    for entry in entries {
        // A failed entry has no path of its own to name, so it is attributed
        // to the directory being read.
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                errors.push((dir.to_path_buf(), e));
                continue;
            }
        };
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                errors.push((path, e));
                continue;
            }
        };
        if ft.is_dir() {
            // Skip `scheduled/` — contains saved query result parquet, not ingested logs.
            if entry.file_name() == "scheduled" {
                continue;
            }
            walk_dir_recursive(&path, results, errors);
        } else if ft.is_file() && path.extension().is_some_and(|ext| ext == "parquet") {
            match entry.metadata() {
                Ok(meta) => results.push((path, meta.len())),
                Err(e) => errors.push((path, e)),
            }
        }
    }
}

// -- storage measurement walking --------------------------------------------

#[derive(Clone, Copy)]
enum StorageKind {
    Wal,
    Parquet,
}

impl StorageKind {
    fn selected(self, path: &Path) -> bool {
        path.extension().is_some_and(|ext| {
            ext == match self {
                Self::Wal => "ndjson",
                Self::Parquet => "parquet",
            }
        })
    }

    fn excluded_dir(self, path: &Path) -> bool {
        matches!(self, Self::Parquet) && path.file_name().is_some_and(|name| name == "scheduled")
    }
}

/// Operations at which tests can inject faults or remove real descendants.
/// Production still uses the same filesystem calls and error policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorageWalkOp {
    ReadDir,
    Entry,
    FileType,
    Metadata,
    ConfirmAbsent,
    CompleteRoot,
}

fn scan_storage(root: &Path, kind: StorageKind) -> std::io::Result<StorageTotals> {
    scan_storage_with(root, kind, &mut |_, _| Ok(()))
}

fn scan_storage_with(
    root: &Path,
    kind: StorageKind,
    before: &mut impl FnMut(StorageWalkOp, &Path) -> std::io::Result<()>,
) -> std::io::Result<StorageTotals> {
    let totals = scan_storage_dir(root, root, kind, before)?;
    // Even a successful empty traversal must finish with an enumerable root.
    // Consume entries as read_dir can succeed and subsequently yield an error.
    before(StorageWalkOp::CompleteRoot, root)?;
    for entry in std::fs::read_dir(root)? {
        before(StorageWalkOp::Entry, root)?;
        entry?;
    }
    Ok(totals)
}

fn confirmed_absent(
    root: &Path,
    path: &Path,
    error: &std::io::Error,
    before: &mut impl FnMut(StorageWalkOp, &Path) -> std::io::Result<()>,
) -> bool {
    path != root
        && error.kind() == std::io::ErrorKind::NotFound
        && before(StorageWalkOp::ConfirmAbsent, path)
            .and_then(|()| std::fs::symlink_metadata(path))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn scan_storage_dir(
    root: &Path,
    dir: &Path,
    kind: StorageKind,
    before: &mut impl FnMut(StorageWalkOp, &Path) -> std::io::Result<()>,
) -> std::io::Result<StorageTotals> {
    let entries = match before(StorageWalkOp::ReadDir, dir).and_then(|()| std::fs::read_dir(dir)) {
        Ok(entries) => entries,
        Err(error) if confirmed_absent(root, dir, &error, before) => {
            return Ok(StorageTotals::default());
        }
        Err(error) => return Err(error),
    };
    let mut totals = StorageTotals::default();
    for entry in entries {
        // An iterator failure has no trustworthy descendant path. Never excuse
        // it as a disappearing file, even when its error kind is NotFound.
        before(StorageWalkOp::Entry, dir)?;
        let entry = entry?;
        let path = entry.path();
        let file_type =
            match before(StorageWalkOp::FileType, &path).and_then(|()| entry.file_type()) {
                Ok(file_type) => file_type,
                Err(error) if confirmed_absent(root, &path, &error, before) => continue,
                Err(error) => return Err(error),
            };
        if file_type.is_dir() && !kind.excluded_dir(&path) {
            let child = scan_storage_dir(root, &path, kind, before)?;
            totals.files += child.files;
            totals.bytes += child.bytes;
        } else if file_type.is_file() && kind.selected(&path) {
            let metadata =
                match before(StorageWalkOp::Metadata, &path).and_then(|()| entry.metadata()) {
                    Ok(metadata) => metadata,
                    Err(error) if confirmed_absent(root, &path, &error, before) => continue,
                    Err(error) => return Err(error),
                };
            totals.files += 1;
            totals.bytes += metadata.len();
        }
    }
    Ok(totals)
}

/// WAL and Parquet use separate owners, but the same invariant implementation.
fn collect_wal_gauges(wal_dir: &Path) {
    wal_cache().collect(
        Instant::now,
        || scan_storage(wal_dir, StorageKind::Wal),
        |totals| publish_storage_gauges(StorageKind::Wal, totals),
    );
}

/// Called only while the source's attempt owner is held, for both cache hits
/// and fresh attempts. Dashboard-only availability is not exported as gauges.
#[allow(clippy::cast_precision_loss)]
fn publish_storage_gauges(kind: StorageKind, totals: StorageTotals) {
    let (files, bytes) = match kind {
        StorageKind::Wal => (WAL_FILES, WAL_BYTES),
        StorageKind::Parquet => (PARQUET_FILES, PARQUET_BYTES),
    };
    metrics::gauge!(files).set(totals.files as f64);
    metrics::gauge!(bytes).set(totals.bytes as f64);
}

/// Read a coherent WAL tuple, using actual writer presence even before collection.
pub(crate) fn cached_wal_stats(configured: bool) -> StorageSnapshot {
    wal_cache().snapshot(configured, Instant::now())
}

/// The fallback archive is configured even on a query-only cold start. An absent
/// directory is a failed measurement, never a measured empty directory.
pub(crate) fn cached_parquet_stats() -> StorageSnapshot {
    parquet_cache().snapshot(true, Instant::now())
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Missing series are a failure, never an implicit zero.
    pub fn sample(handle: &metrics_exporter_prometheus::PrometheusHandle, series: &str) -> u64 {
        let rendered = handle.render();
        rendered
            .lines()
            .find_map(|line| {
                let (name, value) = line.rsplit_once(' ')?;
                (name == series).then(|| value.parse().expect("integer counter sample"))
            })
            .unwrap_or_else(|| panic!("missing series {series} in {rendered}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_failure() -> std::io::Error {
        std::io::Error::other("injected storage failure")
    }

    fn assert_storage(
        cache: &StorageCache,
        now: Instant,
        status: trawl_api::StorageMeasurementStatus,
        totals: StorageTotals,
        age: Option<u64>,
    ) {
        let snapshot = cache.snapshot(true, now);
        assert_eq!(
            (snapshot.files, snapshot.bytes),
            (totals.files, totals.bytes)
        );
        assert_eq!(snapshot.measurement.status, status);
        assert_eq!(snapshot.measurement.sample_age_secs, age);
    }

    #[test]
    fn storage_attempts_retain_samples_and_throttle_failures_separately() {
        use std::time::Duration;
        use trawl_api::StorageMeasurementStatus as Status;
        let cache = StorageCache::default();
        let start = Instant::now();
        let empty = StorageTotals::default();
        let totals = StorageTotals {
            files: 2,
            bytes: 17,
        };
        let absent = cache.snapshot(false, start);
        assert_eq!(absent.measurement.status, Status::NotConfigured);
        assert_eq!(absent.measurement.sample_age_secs, None);
        assert_eq!((absent.files, absent.bytes), (0, 0));
        assert_storage(&cache, start, Status::NotSampled, empty, None);
        cache.collect(
            || start,
            || Err(io_failure()),
            |_| panic!("no sample to publish"),
        );
        assert_storage(&cache, start, Status::Failed, empty, None);
        cache.collect(
            || start + Duration::from_secs(29),
            || panic!("failure retry too soon"),
            |_| panic!("invented zero"),
        );
        let completed = start + Duration::from_secs(30);
        cache.collect(
            || completed,
            || Ok(totals),
            |sample| assert_eq!(sample, totals),
        );
        assert_storage(&cache, completed, Status::Complete, totals, Some(0));
        let failed = completed + Duration::from_secs(30);
        cache.collect(
            || failed,
            || Err(io_failure()),
            |sample| assert_eq!(sample, totals),
        );
        assert_storage(&cache, failed, Status::Failed, totals, Some(30));
        cache.collect(
            || failed + Duration::from_secs(29),
            || panic!("last success must not control retries"),
            |sample| assert_eq!(sample, totals),
        );
        assert_storage(
            &cache,
            failed + Duration::from_secs(29),
            Status::Failed,
            totals,
            Some(59),
        );
        let recovered = failed + Duration::from_secs(30);
        cache.collect(
            || recovered,
            || Ok(empty),
            |sample| assert_eq!(sample, empty),
        );
        assert_storage(&cache, recovered, Status::Complete, empty, Some(0));
    }

    #[test]
    fn storage_age_and_retry_start_at_scan_completion() {
        use std::{cell::Cell, time::Duration};
        let cache = StorageCache::default();
        let start = Instant::now();
        let now = Cell::new(start);
        cache.collect(
            || now.get(),
            || {
                now.set(start + Duration::from_secs(80));
                Ok(StorageTotals::default())
            },
            |_| {},
        );
        assert_storage(
            &cache,
            now.get(),
            trawl_api::StorageMeasurementStatus::Complete,
            StorageTotals::default(),
            Some(0),
        );
        cache.collect(
            || start + Duration::from_secs(109),
            || panic!("completion controls TTL"),
            |_| {},
        );
    }

    #[test]
    fn storage_readers_do_not_wait_for_scans_and_waiters_recheck_due() {
        use std::sync::mpsc;
        let cache = Arc::new(StorageCache::default());
        let start = Instant::now();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = cache.clone();
        let thread = std::thread::spawn(move || {
            first.collect(
                || start,
                || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(StorageTotals {
                        files: 4,
                        bytes: 44,
                    })
                },
                |_| {},
            );
        });
        entered_rx.recv().unwrap();
        assert_storage(
            &cache,
            start,
            trawl_api::StorageMeasurementStatus::NotSampled,
            StorageTotals::default(),
            None,
        );
        assert!(cache.attempt.try_lock().is_err(), "scan owns attempt mutex");
        let second = cache.clone();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            waiting_tx.send(()).unwrap();
            second.collect(
                || start,
                || panic!("waiter must recheck due"),
                |sample| assert_eq!(sample.files, 4),
            );
        });
        waiting_rx.recv().unwrap();
        release_tx.send(()).unwrap();
        thread.join().unwrap();
        waiter.join().unwrap();
        assert_storage(
            &cache,
            start,
            trawl_api::StorageMeasurementStatus::Complete,
            StorageTotals {
                files: 4,
                bytes: 44,
            },
            Some(0),
        );
    }

    #[test]
    fn storage_gauge_publication_remains_inside_attempt_ownership() {
        use std::{sync::mpsc, time::Duration};
        let cache = Arc::new(StorageCache::default());
        let start = Instant::now();
        let published = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = cache.clone();
        let values = published.clone();
        let thread = std::thread::spawn(move || {
            first.collect(
                || start,
                || {
                    Ok(StorageTotals {
                        files: 1,
                        bytes: 10,
                    })
                },
                |totals| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    values.lock().unwrap().push(totals.files);
                },
            );
        });
        entered_rx.recv().unwrap();
        assert!(
            cache.attempt.try_lock().is_err(),
            "publication must retain ownership"
        );
        assert_eq!(
            cache.snapshot(true, start).files,
            1,
            "cache reads remain available during publication"
        );
        let second = cache.clone();
        let values = published.clone();
        let waiter = std::thread::spawn(move || {
            second.collect(
                || start + Duration::from_secs(31),
                || {
                    Ok(StorageTotals {
                        files: 2,
                        bytes: 20,
                    })
                },
                |totals| values.lock().unwrap().push(totals.files),
            );
        });
        release_tx.send(()).unwrap();
        thread.join().unwrap();
        waiter.join().unwrap();
        assert_eq!(*published.lock().unwrap(), [1, 2]);
        assert_eq!(
            cache.snapshot(true, start + Duration::from_secs(31)).files,
            2
        );
    }

    #[test]
    fn storage_prometheus_gauges_require_success_and_retain_complete_totals() {
        use std::time::Duration;
        for kind in [StorageKind::Wal, StorageKind::Parquet] {
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            let cache = StorageCache::default();
            let now = Instant::now();
            let (files, bytes) = match kind {
                StorageKind::Wal => (WAL_FILES, WAL_BYTES),
                StorageKind::Parquet => (PARQUET_FILES, PARQUET_BYTES),
            };
            metrics::with_local_recorder(&recorder, || {
                cache.collect(
                    || now,
                    || Err(io_failure()),
                    |totals| publish_storage_gauges(kind, totals),
                );
                assert!(!handle.render().contains(files));
                cache.collect(
                    || now + Duration::from_secs(30),
                    || {
                        Ok(StorageTotals {
                            files: 3,
                            bytes: 19,
                        })
                    },
                    |totals| publish_storage_gauges(kind, totals),
                );
                cache.collect(
                    || now + Duration::from_secs(60),
                    || Err(io_failure()),
                    |totals| publish_storage_gauges(kind, totals),
                );
                let output = handle.render();
                assert!(output.contains(&format!("{files} 3\n")), "{output}");
                assert!(output.contains(&format!("{bytes} 19\n")), "{output}");
                cache.collect(
                    || now + Duration::from_secs(90),
                    || Ok(StorageTotals::default()),
                    |totals| publish_storage_gauges(kind, totals),
                );
                assert!(handle.render().contains(&format!("{files} 0\n")));
            });
        }
    }

    #[test]
    fn storage_walk_empty_selection_and_missing_roots() {
        for (kind, extension) in [
            (StorageKind::Wal, "ndjson"),
            (StorageKind::Parquet, "parquet"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            assert_eq!(
                scan_storage(tmp.path(), kind).unwrap(),
                StorageTotals::default()
            );
            let env = tmp.path().join("prod");
            std::fs::create_dir(&env).unwrap();
            std::fs::write(env.join(format!("selected.{extension}")), "123").unwrap();
            std::fs::write(env.join("ignored.tmp"), "12345").unwrap();
            assert_eq!(
                scan_storage(tmp.path(), kind).unwrap(),
                StorageTotals { files: 1, bytes: 3 }
            );
            let absent = tmp.path().join("query-only-archive-not-created");
            assert!(scan_storage(&absent, kind).is_err());
            let file = env.join(format!("selected.{extension}"));
            assert!(scan_storage(&file, kind).is_err());
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("scheduled")).unwrap();
        std::fs::write(tmp.path().join("scheduled/report.parquet"), "123").unwrap();
        assert_eq!(
            scan_storage(tmp.path(), StorageKind::Parquet).unwrap(),
            StorageTotals::default()
        );
    }

    #[test]
    fn storage_walk_rejects_coverage_errors_without_replacing_complete_sample() {
        use std::time::Duration;
        for (kind, extension) in [
            (StorageKind::Wal, "ndjson"),
            (StorageKind::Parquet, "parquet"),
        ] {
            for operation in [
                StorageWalkOp::ReadDir,
                StorageWalkOp::Entry,
                StorageWalkOp::FileType,
                StorageWalkOp::Metadata,
                StorageWalkOp::CompleteRoot,
            ] {
                let tmp = tempfile::tempdir().unwrap();
                let nested = tmp.path().join("prod");
                std::fs::create_dir(&nested).unwrap();
                std::fs::write(nested.join(format!("events.{extension}")), "12345").unwrap();
                let cache = StorageCache::default();
                let now = Instant::now();
                cache.collect(|| now, || scan_storage(tmp.path(), kind), |_| {});
                cache.collect(
                    || now + Duration::from_secs(30),
                    || {
                        scan_storage_with(tmp.path(), kind, &mut |op, path| {
                            if op == operation && (op != StorageWalkOp::ReadDir || path == nested) {
                                Err(io_failure())
                            } else {
                                Ok(())
                            }
                        })
                    },
                    |totals| assert_eq!(totals, StorageTotals { files: 1, bytes: 5 }),
                );
                assert_storage(
                    &cache,
                    now + Duration::from_secs(30),
                    trawl_api::StorageMeasurementStatus::Failed,
                    StorageTotals { files: 1, bytes: 5 },
                    Some(30),
                );
            }
        }
    }

    #[test]
    fn storage_walk_only_skips_confirmed_absent_descendants() {
        for (kind, extension) in [
            (StorageKind::Wal, "ndjson"),
            (StorageKind::Parquet, "parquet"),
        ] {
            for operation in [
                StorageWalkOp::ReadDir,
                StorageWalkOp::FileType,
                StorageWalkOp::Metadata,
            ] {
                let tmp = tempfile::tempdir().unwrap();
                let nested = tmp.path().join("prod");
                std::fs::create_dir(&nested).unwrap();
                let file = nested.join(format!("events.{extension}"));
                std::fs::write(&file, "123").unwrap();
                let target = if operation == StorageWalkOp::ReadDir {
                    &nested
                } else {
                    &file
                };
                let totals = scan_storage_with(tmp.path(), kind, &mut |op, path| {
                    if op == operation && path == target {
                        if path.is_dir() {
                            std::fs::remove_dir_all(path)?;
                        } else {
                            std::fs::remove_file(path)?;
                        }
                        return Err(std::io::ErrorKind::NotFound.into());
                    }
                    Ok(())
                })
                .unwrap();
                assert_eq!(totals, StorageTotals::default());
            }
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join(format!("events.{extension}")), "123").unwrap();
            for operation in [
                StorageWalkOp::Entry,
                StorageWalkOp::FileType,
                StorageWalkOp::Metadata,
            ] {
                assert!(
                    scan_storage_with(tmp.path(), kind, &mut |op, _| {
                        if op == operation {
                            Err(std::io::ErrorKind::NotFound.into())
                        } else {
                            Ok(())
                        }
                    })
                    .is_err(),
                    "NotFound alone is not confirmed absence"
                );
            }
        }
    }

    #[test]
    fn storage_walk_rejects_root_removed_at_completion_and_recovers() {
        for kind in [StorageKind::Wal, StorageKind::Parquet] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("archive");
            std::fs::create_dir(&root).unwrap();
            assert!(
                scan_storage_with(&root, kind, &mut |operation, path| {
                    if operation == StorageWalkOp::CompleteRoot {
                        std::fs::remove_dir(path)?;
                    }
                    Ok(())
                })
                .is_err()
            );
            std::fs::create_dir(&root).unwrap();
            assert_eq!(scan_storage(&root, kind).unwrap(), StorageTotals::default());
        }
    }

    #[test]
    fn query_only_boot_acceptance_is_not_a_complete_empty_measurement() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        crate::epoch::ensure_current_epoch(&root, &tmp.path().join("wal"), false).unwrap();
        let cache = StorageCache::default();
        let now = Instant::now();
        cache.collect(
            || now,
            || scan_storage(&root, StorageKind::Parquet),
            |_| panic!("absent archive was not measured"),
        );
        assert_storage(
            &cache,
            now,
            trawl_api::StorageMeasurementStatus::Failed,
            StorageTotals::default(),
            None,
        );
        assert_eq!(
            cache.snapshot(false, now).measurement.status,
            trawl_api::StorageMeasurementStatus::NotConfigured
        );
    }

    #[test]
    fn anonymous_entry_failure_after_partial_progress_rejects_the_attempt() {
        for (kind, extension) in [
            (StorageKind::Wal, "ndjson"),
            (StorageKind::Parquet, "parquet"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            for name in ["one", "two"] {
                std::fs::write(tmp.path().join(format!("{name}.{extension}")), "123").unwrap();
            }
            let mut entries = 0;
            let mut measured = 0;
            let result = scan_storage_with(tmp.path(), kind, &mut |operation, _| {
                if operation == StorageWalkOp::Metadata {
                    measured += 1;
                }
                if operation == StorageWalkOp::Entry {
                    entries += 1;
                    if entries == 2 {
                        return Err(std::io::ErrorKind::NotFound.into());
                    }
                }
                Ok(())
            });
            assert_eq!(measured, 1, "one file was already counted");
            assert!(result.is_err());
        }
    }

    #[test]
    fn absence_confirmation_errors_and_completion_entry_errors_fail() {
        for (kind, extension) in [
            (StorageKind::Wal, "ndjson"),
            (StorageKind::Parquet, "parquet"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join(format!("one.{extension}")), "123").unwrap();
            assert!(
                scan_storage_with(tmp.path(), kind, &mut |operation, _| match operation {
                    StorageWalkOp::Metadata => Err(std::io::ErrorKind::NotFound.into()),
                    StorageWalkOp::ConfirmAbsent =>
                        Err(std::io::ErrorKind::PermissionDenied.into()),
                    _ => Ok(()),
                })
                .is_err()
            );
            let mut completing = false;
            assert!(
                scan_storage_with(tmp.path(), kind, &mut |operation, _| {
                    if operation == StorageWalkOp::CompleteRoot {
                        completing = true;
                    }
                    if completing && operation == StorageWalkOp::Entry {
                        Err(io_failure())
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
        }
    }

    #[test]
    fn operational_alert_baselines_survive_idle_upkeep_and_reinitialization() {
        let recorder = prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // No ingest state, listeners or telemetry writer is constructed.
        // These series must exist even when every producer is disabled.
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            init_operational_alert_metrics();
            let selected = [
                "trawl_syslog_events_dropped_total{reason=\"backpressure\"}",
                "trawl_syslog_events_dropped_total{reason=\"queue_full\"}",
                "trawl_hot_buffer_admission_refusals_total{producer=\"http\",kind=\"full\"}",
                "trawl_hot_buffer_admission_refusals_total{producer=\"http\",kind=\"oversized\"}",
                "trawl_hot_buffer_admission_refusals_total{producer=\"syslog\",kind=\"full\"}",
                "trawl_hot_buffer_admission_refusals_total{producer=\"syslog\",kind=\"oversized\"}",
                "trawl_hot_buffer_admission_refusals_total{producer=\"trawld\",kind=\"full\"}",
                "trawl_hot_buffer_admission_refusals_total{producer=\"trawld\",kind=\"oversized\"}",
                "trawl_syslog_wal_events_discarded_total",
                "trawl_syslog_write_tasks_failed_total",
                "trawl_telemetry_wal_write_failures_total",
                "trawl_telemetry_events_dropped_total{reason=\"preinit_cap\"}",
                "trawl_telemetry_events_dropped_total{reason=\"buffer_cap\"}",
                "trawl_telemetry_events_dropped_total{reason=\"write_crashed\"}",
                "trawl_telemetry_events_dropped_total{reason=\"unmetered_cap\"}",
                "trawl_ingest_events_rejected_total{reason=\"wal_failure\"}",
                "trawl_ingest_events_rejected_total{reason=\"hot_buffer_full\"}",
                "trawl_ingest_events_rejected_total{reason=\"ingest_batch_too_large\"}",
                "trawl_wal_durability_failures_total{operation=\"parent_directory_sync\"}",
                "trawl_compaction_operation_failures_total{operation=\"wal_root_scan\"}",
                "trawl_compaction_operation_failures_total{operation=\"wal_environment_scan\"}",
                "trawl_compaction_operation_failures_total{operation=\"chunk\"}",
                "trawl_compaction_operation_failures_total{operation=\"daily_rollup_scan\"}",
                "trawl_compaction_operation_failures_total{operation=\"daily_rollup_unit\"}",
                "trawl_compaction_operation_failures_total{operation=\"pending_rollup_scan\"}",
                "trawl_compaction_operation_failures_total{operation=\"pending_rollup_recovery\"}",
                "trawl_compaction_operation_failures_total{operation=\"consumed_wal_removal\"}",
                "trawl_files_quarantined_total{kind=\"wal\"}",
                "trawl_files_quarantined_total{kind=\"parquet\"}",
                "trawl_files_quarantined_total{kind=\"rollup_temporary\"}",
                "trawl_publication_recovery_total{outcome=\"contradictory\"}",
                "trawl_publication_recovery_total{outcome=\"failed\"}",
            ];
            for series in selected {
                assert_eq!(test_support::sample(&handle, series), 0);
            }
            // Exercise idle upkeep with today's production configuration,
            // which has no exporter expiry. This short wait does not prove
            // survival past an arbitrary timeout added in a future change.
            std::thread::sleep(std::time::Duration::from_millis(10));
            handle.run_upkeep();
            for series in selected {
                assert_eq!(test_support::sample(&handle, series), 0);
            }
            metrics::counter!(SYSLOG_WAL_EVENTS_DISCARDED_TOTAL).increment(3);
            metrics::counter!(TELEMETRY_EVENTS_DROPPED_TOTAL,
                "reason" => TelemetryDropReason::WriteCrashed.label())
            .increment(2);
            init_operational_alert_metrics();
            handle.run_upkeep();
            for series in selected {
                let expected = match series {
                    "trawl_syslog_wal_events_discarded_total" => 3,
                    "trawl_telemetry_events_dropped_total{reason=\"write_crashed\"}" => 2,
                    _ => 0,
                };
                assert_eq!(test_support::sample(&handle, series), expected);
            }
        });
    }

    #[test]
    fn publication_recovery_outcomes_start_at_zero_and_survive_reinitialization() {
        let recorder = prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            init_publication_recovery_metrics();
            let series =
                |label: &str| format!("{PUBLICATION_RECOVERY_TOTAL}{{outcome=\"{label}\"}}");
            for label in ["published", "unpublished", "contradictory", "failed"] {
                assert_eq!(test_support::sample(&handle, &series(label)), 0);
            }
            crate::ingest::publication_marker::RecoveryOutcomeKind::Contradictory.record();
            init_publication_recovery_metrics();
            assert_eq!(test_support::sample(&handle, &series("contradictory")), 1);
        });
    }

    #[test]
    fn describe_metrics_does_not_panic() {
        // Install a test recorder so describe calls succeed.
        let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
        let _handle = builder.install_recorder().expect("install test recorder");
        describe_metrics();
    }

    #[test]
    fn wal_walk_counts_files_inside_env_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wal_dir = tmp.path();

        // WAL files land in `wal_dir/{env}/` (ADR-0009) — a flat scan of
        // `wal_dir` would see only directories and report 0/0.
        for (env, service, bytes) in [("prod", "nginx", "aaaa"), ("dev", "api", "bb")] {
            let env_dir = wal_dir.join(env);
            std::fs::create_dir_all(&env_dir).expect("create env dir");
            std::fs::write(env_dir.join(format!("{service}_1_abcd.ndjson")), bytes)
                .expect("write wal file");
            // Non-ndjson siblings (in-flight tmp, quarantined) must not count.
            std::fs::write(env_dir.join(format!("{service}_2_abcd.tmp")), "zzzz")
                .expect("write tmp file");
            std::fs::write(env_dir.join(format!("{service}_3_abcd.corrupt")), "zzzz")
                .expect("write corrupt file");
        }

        let totals = scan_storage(wal_dir, StorageKind::Wal).unwrap();
        assert_eq!(totals.files, 2);
        assert_eq!(totals.bytes, 6);
    }

    #[test]
    fn bounded_label_admits_up_to_the_cap_then_collapses() {
        let mut admitted = HashSet::new();
        for i in 0..3 {
            let svc = format!("svc-{i}");
            assert_eq!(bounded_label(&mut admitted, &svc, 3), svc);
        }

        // Novel services past the cap collapse into the overflow bucket...
        assert_eq!(
            bounded_label(&mut admitted, "svc-3", 3),
            OVERFLOW_SERVICE_LABEL
        );
        assert_eq!(
            bounded_label(&mut admitted, "svc-4", 3),
            OVERFLOW_SERVICE_LABEL
        );
        // ...and do not consume admission slots, so the series count is capped.
        assert_eq!(admitted.len(), 3);

        // Already-admitted services keep reporting under their own name.
        assert_eq!(bounded_label(&mut admitted, "svc-1", 3), "svc-1");
    }

    #[test]
    fn overflow_label_cannot_collide_with_a_service_name() {
        assert!(
            !OVERFLOW_SERVICE_LABEL
                .bytes()
                .all(crate::ingest::pipeline::is_valid_service_char)
        );
    }

    #[test]
    fn collect_gauges_no_hot_buffer_no_panic() {
        // With no recorder installed and no hot buffer, should be a no-op.
        collect_gauges(None, "/nonexistent/path/**/*.parquet", None, 0);
    }

    /// One rendered sample value, verbatim (gauges need not be integers).
    fn gauge_value<'a>(rendered: &'a str, series: &str) -> &'a str {
        rendered
            .lines()
            .find_map(|line| {
                let (name, value) = line.rsplit_once(' ')?;
                (name == series).then_some(value)
            })
            .unwrap_or_else(|| panic!("missing series {series} in {rendered}"))
    }

    #[test]
    fn admission_gauges_follow_the_hot_buffer() {
        use crate::hot_buffer::{AdmissionState, HotBufferConfig};
        use crate::ingest::producer::ProducerKind;

        let recorder = prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            init_operational_alert_metrics();
            set_compaction_interval_secs(10);
            let buf = Arc::new(HotBuffer::new(HotBufferConfig {
                max_events: 100,
                max_bytes: 1_000,
            }));
            collect_gauges(Some(&buf), "/nonexistent/path/**/*.parquet", None, 0);
            let rendered = handle.render();
            assert_eq!(gauge_value(&rendered, HOT_BUFFER_MAX_EVENTS), "100");
            assert_eq!(gauge_value(&rendered, HOT_BUFFER_MAX_BYTES), "1000");
            assert_eq!(
                gauge_value(&rendered, HOT_BUFFER_OLDEST_BATCH_AGE_SECONDS),
                "0"
            );
            assert_eq!(gauge_value(&rendered, HOT_BUFFER_ADMISSION_STATE), "0");
            assert_eq!(gauge_value(&rendered, COMPACTION_INTERVAL_SECONDS), "10");

            buf.insert_for_test(Arc::new(crate::bus::IngestBatch {
                batch_id: "prod/a".into(),
                service: "svc".into(),
                byte_size: 600,
                events: vec![serde_json::Map::new(); 10],
            }));
            std::thread::sleep(std::time::Duration::from_millis(5));
            collect_gauges(Some(&buf), "/nonexistent/path/**/*.parquet", None, 0);
            let rendered = handle.render();
            assert_eq!(
                gauge_value(&rendered, HOT_BUFFER_ADMISSION_STATE),
                AdmissionState::Pressure.as_u8().to_string()
            );
            let age: f64 = gauge_value(&rendered, HOT_BUFFER_OLDEST_BATCH_AGE_SECONDS)
                .parse()
                .unwrap();
            assert!(age >= 0.005, "{age}");

            assert!(
                buf.reserve(
                    ProducerKind::Http,
                    crate::hot_buffer::Charge {
                        events: 1,
                        bytes: 400,
                    }
                )
                .is_err()
            );
            collect_gauges(Some(&buf), "/nonexistent/path/**/*.parquet", None, 0);
            let rendered = handle.render();
            assert_eq!(gauge_value(&rendered, HOT_BUFFER_ADMISSION_STATE), "2");
            assert_eq!(
                test_support::sample(
                    &handle,
                    "trawl_hot_buffer_admission_refusals_total{producer=\"http\",kind=\"full\"}"
                ),
                1
            );
        });
    }

    #[test]
    fn syslog_drop_reasons_are_the_frozen_label_set() {
        let labels: Vec<&str> = SyslogDropReason::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(labels, ["backpressure", "queue_full"]);
    }

    /// Every `trawl_*` metric name the alert pack selects.
    fn alert_pack_metric_names(rules: &str) -> std::collections::BTreeSet<&str> {
        let bytes = rules.as_bytes();
        let mut names = std::collections::BTreeSet::new();
        let mut i = 0;
        while let Some(offset) = rules[i..].find("trawl_") {
            let start = i + offset;
            let preceded =
                start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
            let end = start
                + rules[start..]
                    .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
                    .unwrap_or(rules.len() - start);
            if !preceded {
                names.insert(&rules[start..end]);
            }
            i = end;
        }
        names
    }

    #[test]
    fn every_metric_the_alert_pack_selects_renders_after_init_and_collection() {
        // The packaged alert rules only fire on series that exist: a rule
        // over a name nothing publishes at boot silently never fires. Init
        // plus one gauge collection must publish every name they select.
        const RULES: &str = include_str!("../../../monitoring/prometheus/trawl.rules.yml");
        let names = alert_pack_metric_names(RULES);
        assert!(
            names.contains("trawl_syslog_events_dropped_total"),
            "extraction found {names:?}"
        );

        let recorder = prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            init_operational_alert_metrics();
            set_compaction_interval_secs(10);
            let buf = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
                max_events: trawl_config::DEFAULT_HOT_BUFFER_MAX_EVENTS,
                max_bytes: trawl_config::DEFAULT_HOT_BUFFER_MAX_BYTES,
            }));
            collect_gauges(Some(&buf), "/nonexistent/path/**/*.parquet", None, 0);
            let rendered = handle.render();
            for name in names {
                let series_prefix = [format!("{name} "), format!("{name}{{")];
                assert!(
                    rendered.lines().any(|line| {
                        !line.starts_with('#')
                            && series_prefix.iter().any(|p| line.starts_with(p.as_str()))
                    }),
                    "the alert pack selects {name}, which init and collection never publish:\n{rendered}"
                );
            }
        });
    }
}
