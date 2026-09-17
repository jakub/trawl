// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Prometheus metrics: metric name constants, descriptions, and gauge collection.

use std::collections::HashSet;
use std::path::Path;
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

/// Failed durability operations on an already-published WAL file.
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

/// Existing telemetry reasons. A consumed crashed batch may be durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryDropReason {
    PreinitCap,
    BufferCap,
    WriteCrashed,
}

impl TelemetryDropReason {
    pub const ALL: [Self; 3] = [Self::PreinitCap, Self::BufferCap, Self::WriteCrashed];

    pub const fn label(self) -> &'static str {
        match self {
            Self::PreinitCap => "preinit_cap",
            Self::BufferCap => "buffer_cap",
            Self::WriteCrashed => "write_crashed",
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
        SYSLOG_EVENTS_DROPPED_TOTAL,
        SYSLOG_WAL_EVENTS_DISCARDED_TOTAL,
        SYSLOG_WRITE_TASKS_FAILED_TOTAL,
        TELEMETRY_WAL_WRITE_FAILURES_TOTAL,
    ] {
        metrics::counter!(name).increment(0);
    }
    for reason in TelemetryDropReason::ALL {
        metrics::counter!(TELEMETRY_EVENTS_DROPPED_TOTAL, "reason" => reason.label()).increment(0);
    }
    metrics::counter!(INGEST_EVENTS_REJECTED_TOTAL,
        "reason" => crate::ingest::envelope::RejectReason::WalFailure.as_str())
    .increment(0);
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
    describe_gauge!(ACTIVE_CONNECTIONS, "Number of in-flight HTTP requests");
    describe_gauge!(
        QUERY_PERMITS_RETAINED,
        "Executor-pool permits held by query work whose request already \
         answered (a subset of the permits in use)"
    );
    describe_gauge!(PARQUET_FILES, "Total number of parquet data files");
    describe_gauge!(PARQUET_BYTES, "Total byte size of parquet data files");
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
        "Syslog events abandoned because the batch queue was full or closed; TCP and UDP combined"
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
        "Failed WAL durability operations after file publication, labelled by operation; the directory entry may not survive a crash, not an event discard"
    );
    describe_counter!(
        COMPACTION_OPERATION_FAILURES_TOTAL,
        "Failed compaction operation attempts, labelled by operation; excludes successful quarantines, idle work and intentional suppression"
    );
    describe_counter!(
        FILES_QUARANTINED_TOTAL,
        "Files successfully renamed out of normal processing with bytes retained, labelled by kind; temporary rollup sources may remain intact, not an event-loss count"
    );
    describe_gauge!(
        SYSLOG_TCP_CONNECTIONS,
        "Current active syslog TCP connections"
    );
    describe_gauge!(WAL_FILES, "Number of pending WAL (ndjson) files");
    describe_gauge!(WAL_BYTES, "Total byte size of pending WAL files");
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
         repin staging on the data root, 0 when they run. Unlike \
         trawl_catalog_repin_running this stays 1 for staging no job owns \
         — a boot replay whose sweep keeps failing — so alert on it held \
         high across ticks: the archive grows unbounded meanwhile"
    );
    describe_counter!(
        SCHEDULER_WINDOW_TRUNCATED_TOTAL,
        "Scheduled report runs whose since_last catch-up window was clamped \
         to max_catchup_intervals"
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
         publication; this reason does not prove permanent event loss)"
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

/// TTL for the parquet stats cache. At most one filesystem walk per this interval,
/// regardless of scrape frequency or stats emitter cadence.
const PARQUET_CACHE_TTL_SECS: u64 = 30;

/// Cached parquet file statistics to avoid repeated filesystem walks.
struct CachedParquetStats {
    file_count: u64,
    total_bytes: u64,
    /// `None` means never cached — first call always triggers a walk.
    last_updated: Option<Instant>,
}

/// Module-level cache for parquet gauge values.
fn parquet_cache() -> &'static Mutex<CachedParquetStats> {
    static CACHE: OnceLock<Mutex<CachedParquetStats>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(CachedParquetStats {
            file_count: 0,
            total_bytes: 0,
            last_updated: None,
        })
    })
}

/// Update gauges that require periodic polling (hot buffer + parquet + WAL files).
///
/// Cheap enough to call on every prometheus scrape and in the stats emitter.
#[allow(clippy::cast_precision_loss)] // gauge values are f64; precision loss beyond 2^52 is fine
pub fn collect_gauges(
    hot_buffer: Option<&Arc<HotBuffer>>,
    fallback_glob: &str,
    wal_dir: Option<&Path>,
    retained_permits: usize,
) {
    // The caller reads the pool on its own thread and passes the number
    // in: this function runs on the blocking pool and holds no server
    // state, which is what keeps a filesystem walk off the reactor
    // without dragging a lock across it.
    metrics::gauge!(QUERY_PERMITS_RETAINED).set(retained_permits as f64);

    if let Some(buf) = hot_buffer {
        metrics::gauge!(HOT_BUFFER_EVENTS).set(buf.event_count() as f64);
        metrics::gauge!(HOT_BUFFER_BYTES).set(buf.byte_count() as f64);
    }

    // Parquet file gauges — walk the glob pattern's parent directory.
    collect_parquet_gauges(fallback_glob);

    if let Some(dir) = wal_dir {
        collect_wal_gauges(dir);
    }
}

/// Scan parquet files on disk and update the file count / byte size gauges.
///
/// Uses a 30s TTL cache to avoid repeated filesystem walks. If the cache is
/// fresh, sets gauges from cached values and returns immediately.
#[allow(clippy::cast_precision_loss)]
fn collect_parquet_gauges(fallback_glob: &str) {
    let cache = parquet_cache();

    // Fast path: serve from cache if fresh.
    {
        let cached = cache.lock().expect("parquet cache poisoned");
        let is_fresh = cached
            .last_updated
            .is_some_and(|t| t.elapsed().as_secs() < PARQUET_CACHE_TTL_SECS);
        if is_fresh {
            metrics::gauge!(PARQUET_FILES).set(cached.file_count as f64);
            metrics::gauge!(PARQUET_BYTES).set(cached.total_bytes as f64);
            return;
        }
    }

    // The fallback glob looks like "/path/to/data/**/*.parquet". Extract the
    // base directory (everything before the first glob wildcard).
    let base = fallback_glob
        .find('*')
        .map_or(fallback_glob, |pos| &fallback_glob[..pos]);
    let base = Path::new(base.trim_end_matches('/'));

    if !base.is_dir() {
        return;
    }

    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    if let Ok(entries) = walk_parquet_files(base) {
        for (_, size) in entries {
            file_count += 1;
            total_bytes += size;
        }
    }

    metrics::gauge!(PARQUET_FILES).set(file_count as f64);
    metrics::gauge!(PARQUET_BYTES).set(total_bytes as f64);

    let mut cached = cache.lock().expect("parquet cache poisoned");
    cached.file_count = file_count;
    cached.total_bytes = total_bytes;
    cached.last_updated = Some(Instant::now());
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

// -- WAL gauge collection ----------------------------------------------------

/// TTL for the WAL stats cache (same cadence as parquet).
const WAL_CACHE_TTL_SECS: u64 = 30;

/// Cached WAL file statistics.
struct CachedWalStats {
    file_count: u64,
    total_bytes: u64,
    last_updated: Option<Instant>,
}

/// Module-level cache for WAL gauge values.
fn wal_cache() -> &'static Mutex<CachedWalStats> {
    static CACHE: OnceLock<Mutex<CachedWalStats>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(CachedWalStats {
            file_count: 0,
            total_bytes: 0,
            last_updated: None,
        })
    })
}

/// Recursively tally `.ndjson` files under `dir` into `file_count`/`total_bytes`.
///
/// Recursive by necessity: WAL files live one level down in `wal_dir/{env}/`
/// (ADR-0009), so a flat scan of `wal_dir` sees only directories and reports
/// 0/0 forever — blinding the operator's only stalled-compaction signal.
/// Unreadable directories and entries are skipped rather than aborting the
/// walk, so one bad env still yields the rest of the fleet's numbers.
fn walk_wal_files(dir: &Path, file_count: &mut u64, total_bytes: &mut u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(Result::ok) {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_wal_files(&entry.path(), file_count, total_bytes);
        } else if ft.is_file() && entry.path().extension().is_some_and(|ext| ext == "ndjson") {
            *file_count += 1;
            if let Ok(meta) = entry.metadata() {
                *total_bytes += meta.len();
            }
        }
    }
}

/// Scan WAL directory for `.ndjson` files and update gauge metrics.
///
/// Uses a 30s TTL cache to avoid repeated directory scans.
#[allow(clippy::cast_precision_loss)]
fn collect_wal_gauges(wal_dir: &Path) {
    let cache = wal_cache();

    // Fast path: serve from cache if fresh.
    {
        let cached = cache.lock().expect("wal cache poisoned");
        let is_fresh = cached
            .last_updated
            .is_some_and(|t| t.elapsed().as_secs() < WAL_CACHE_TTL_SECS);
        if is_fresh {
            metrics::gauge!(WAL_FILES).set(cached.file_count as f64);
            metrics::gauge!(WAL_BYTES).set(cached.total_bytes as f64);
            return;
        }
    }

    if !wal_dir.is_dir() {
        return;
    }

    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    walk_wal_files(wal_dir, &mut file_count, &mut total_bytes);

    metrics::gauge!(WAL_FILES).set(file_count as f64);
    metrics::gauge!(WAL_BYTES).set(total_bytes as f64);

    let mut cached = cache.lock().expect("wal cache poisoned");
    cached.file_count = file_count;
    cached.total_bytes = total_bytes;
    cached.last_updated = Some(Instant::now());
}

// -- public cache accessors (for dashboard monitor) --------------------------

/// Read the cached WAL file stats. Returns `(file_count, total_bytes)`.
///
/// Returns `(0, 0)` if the cache has never been populated (no prometheus
/// scrape or stats-emit has run yet).
pub fn cached_wal_stats() -> (u64, u64) {
    let cached = wal_cache().lock().expect("wal cache poisoned");
    (cached.file_count, cached.total_bytes)
}

/// Read the cached parquet file stats. Returns `(file_count, total_bytes)`.
pub fn cached_parquet_stats() -> (u64, u64) {
    let cached = parquet_cache().lock().expect("parquet cache poisoned");
    (cached.file_count, cached.total_bytes)
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
                "trawl_syslog_events_dropped_total",
                "trawl_syslog_wal_events_discarded_total",
                "trawl_syslog_write_tasks_failed_total",
                "trawl_telemetry_wal_write_failures_total",
                "trawl_telemetry_events_dropped_total{reason=\"preinit_cap\"}",
                "trawl_telemetry_events_dropped_total{reason=\"buffer_cap\"}",
                "trawl_telemetry_events_dropped_total{reason=\"write_crashed\"}",
                "trawl_ingest_events_rejected_total{reason=\"wal_failure\"}",
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

        let mut file_count = 0;
        let mut total_bytes = 0;
        walk_wal_files(wal_dir, &mut file_count, &mut total_bytes);

        assert_eq!(file_count, 2);
        assert_eq!(total_bytes, 6);
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
}
