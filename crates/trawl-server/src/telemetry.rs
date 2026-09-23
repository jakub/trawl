// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Internal telemetry: custom tracing [`Layer`] that writes server events
//! to the ingest WAL as `service:trawld`.
//!
//! ## Bootstrap
//!
//! The tracing subscriber is initialized right after the top-level config
//! parses. A config read/parse/validation failure happens before any
//! subscriber exists and surfaces only through an explicit stderr
//! diagnostic in `main`, never here. Post-parse initialization events
//! (cert generation, epoch gate, etc.) are captured: [`WalHandle`] wraps
//! an [`OnceLock`], so the layer registers at init time and buffers
//! events in memory until [`WalHandle::set`] injects the writer after
//! startup. A 1 MiB cap bounds that pre-init buffer if the writer is
//! never set; drops past it are counted under reason `preinit_cap` (event
//! count exact; bytes estimated from the mean buffered line size, because
//! the drop happens before serialization and the telemetry-disabled path
//! must stay cheap).
//!
//! ## What is persisted (the stdout/telemetry split)
//!
//! The stdout logger and this layer share one filter built from the
//! resolved directive string, but they do not persist the same events:
//! the WAL layer additionally refuses the targets in
//! [`UNMETERED_TARGETS`]. Those events are emitted from request handling
//! that no per-key rate limiter has metered yet — fleet-auth's bearer
//! shell, which runs before the limiter; the accept loop, which runs
//! before there is even a TLS session; trawl's own grant check, which
//! 403s a grantless key outside the limiter by design. Persisting them
//! would let a client the limiter cannot slow turn a connection or
//! request flood into durable corpus growth. They stay on stdout, where
//! retention is the operator's log pipeline rather than trawl's own disk.
//!
//! One unmetered event does persist, under a cap: the failure event of a
//! server 5xx no limiter metered
//! ([`UNMETERED_FAILURE_TARGET`]).
//! A server fault before admission, the auth backend down say, is the
//! incident an operator searches for afterwards, so the layer admits those
//! events under one process-wide fixed window of
//! [`UNMETERED_FAILURE_CAP_PER_MINUTE`] (ADR-0040). Past the cap they stay
//! on stdout only and are counted under drop reason `unmetered_cap`. Only
//! that exact target is capped: a target beneath it is excluded like any
//! other unmetered descendant.
//!
//! ## Buffering and the bounded retry queue
//!
//! Events are serialized to ndjson and accumulated in an active buffer
//! (bytes + their event maps, swapped together). Each flush cycle stages
//! the active buffer as one [`Batch`] on a FIFO retry queue, then writes
//! pending batches oldest-first. Once a write succeeds in a cycle the
//! rest of the queue drains coalesced: consecutive batches are
//! concatenated up to [`MAX_DRAIN_UNIT_BYTES`] and written as one WAL
//! file, so recovering from a long outage costs writes proportional to
//! queued bytes rather than to the flush ticks it lasted. That is safe for
//! the hot-buffer `batch_id` contract — it must stay `{env}/{wal-file-stem}`
//! of the file the events landed in, and a coalesced unit lands in exactly
//! one file, so it has exactly one stem and publishes as one `IngestBatch`.
//! While the volume is still failing nothing merges, so the cap keeps its
//! per-tick shedding granularity.
//!
//! A normal failed write retains the batch for retry (rate-limited stderr +
//! `trawl_telemetry_wal_write_failures_total`), so a transient storage error
//! delays events instead of losing them. If the blocking write task itself
//! panics or is cancelled, its consumed in-memory batch cannot be requeued
//! and is counted once under drop reason `write_crashed`, in addition to
//! that one write-failure count. The WAL may already be durable: the batch
//! count describes an uncertain outcome, not confirmed permanent loss.
//! Total retained memory is capped by
//! `[ingest] telemetry_buffer_max_bytes` — the charge is an estimate
//! (serialized ndjson counted twice, once for the bytes and once for the
//! retained maps which hold roughly the same payload, plus a fixed
//! per-event map overhead), mirroring the hot-buffer setting's estimate
//! semantics.
//!
//! The budget is one shared allowance over every byte the layer holds —
//! the active buffer, the retry queue, and the batch currently in flight
//! through a WAL write (popped from the queue but still in memory) — and
//! it is enforced at event insertion, not at staging. A cap applied only
//! to the queue after staging would be no cap at all: while a wedged
//! `spawn_blocking` write holds a batch, `on_event` would keep appending
//! to the active buffer without any bound. Admission sheds the oldest
//! staged batches first (current operational state is worth more than
//! history), and when there is nothing left to shed — the in-flight batch
//! cannot be reclaimed — it drops the incoming event rather than
//! exempting it. Every drop is counted exactly under reason `buffer_cap`,
//! and `trawl_telemetry_buffer_{events,bytes}` gauge the whole charge, not
//! just the queue.
//!
//! ## Durability before visibility
//!
//! A batch is inserted into the hot buffer and published to the event bus
//! strictly after its WAL write succeeds, exactly once (the batch is
//! popped on success, so re-publication is structurally impossible).
//! Queries and SSE can never observe telemetry that would disappear after
//! a restart.
//!
//! ## Blocking I/O and shutdown
//!
//! The async flush task runs each WAL write (create, write, fsync,
//! rename, dir-fsync) on the blocking pool via `spawn_blocking` —
//! [`WalLayer::flush`] stays synchronous for tests only. Shutdown is
//! bounded end to end on a frozen volume: the periodic flush is raced
//! against the shutdown signal (so a wedged fsync cannot keep the task
//! from observing it), the final drain runs under a wall-clock budget,
//! and `trawld`'s `Runtime::shutdown_timeout` bounds the process exit
//! itself — a plain runtime drop waits on started blocking tasks forever.
//! A truly wedged fsync therefore leaves one lingering blocking thread at
//! process exit (accepted and preferable to hanging shutdown).
//!
//! ## Infinite recursion guard
//!
//! The flush path uses `eprintln!` for error reporting, never
//! `tracing::*`: a tracing event inside the layer's own flush path would
//! re-enter `on_event` and loop forever. Two narrow exceptions hold
//! because `on_event` only buffers (it takes the active-buffer lock,
//! which the flush path never holds while emitting): the
//! `telemetry_dropped` recovery event after a successful write, and
//! `WalWriter::write`'s own best-effort dir-fsync warning. The invariant
//! is: **no locks are held across `writer.write`, and flush-path tracing
//! may only buffer.**
//!
//! `on_event` calls `envelope::canonicalize`, so the guard extends to the
//! door: `ingest/envelope.rs` and `ingest/producer.rs` never call
//! `tracing`, which is why a refusal there is a metric and a silent drop
//! rather than a warning. `tests/canonicalize_no_tracing.rs` reads both
//! modules and asserts the token is absent outside `#[cfg(test)]`.

use std::collections::{BTreeMap, VecDeque};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::ingest::wal::WalWriter;
use crate::transport::failure::UNMETERED_FAILURE_TARGET;

// ---------------------------------------------------------------------------
// Default log filter: the cross-packaging contract
// ---------------------------------------------------------------------------

/// The default tracing filter installed when `RUST_LOG` is unset or invalid.
///
/// This exact string is the cross-packaging contract: the Helm chart's
/// `logLevel`, the Debian environment example, and the operator docs all
/// carry it verbatim. It deliberately enumerates every target Trawl
/// emits under rather than using a global `info` (which would enable noisy
/// dependency targets):
///
/// - `trawl_server` — the library crate (handlers, ingest, compaction, …);
/// - `trawld` — the binary's own module path (startup banner, config
///   warnings, split-brain lock loss, task panics) — required for the
///   invalid-`RUST_LOG` `config_warning` to be visible at all;
/// - `fleet_auth` — the auth middleware crate;
/// - `auth.backend` / `storage.backend` — deliberately-overridden targets
///   that make backend failures independently alarmable;
/// - [`PREAUTH_TRANSPORT_TARGET`] — the accept loop's pre-TLS diagnostics,
///   overridden off `trawl_server` precisely so they can be excluded from
///   persistence, and therefore needing their own directive to stay
///   visible on stdout at all.
///
/// This is the filter every sink shares. Persistence is narrower: see
/// [`UNMETERED_TARGETS`] and [`is_persisted_target`].
pub const DEFAULT_LOG_FILTER: &str = "trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info";

/// Target for accept-loop diagnostics that fire before any request — and
/// therefore before any authentication — exists: a failed TLS handshake, a
/// connection-level error.
///
/// A deliberately-overridden target (not `trawl_server::transport::http`)
/// so [`UNMETERED_TARGETS`] can exclude it from persistence without
/// silencing the rest of the transport module.
pub const PREAUTH_TRANSPORT_TARGET: &str = "preauth.transport";

/// Target for the policy layer's grant rejection — authenticated, but
/// rejected before the rate limiter runs.
///
/// `require_trawl_grant` is mounted outside `rate_limit_middleware` on
/// purpose (a grantless key must not spend a bucket to be told no; pinned
/// by the `ac3_grantless_key_never_reaches_rate_limiter` integration
/// test), so its 403 is unmetered — and the fleet keystore is explicitly
/// designed to hold keys with zero trawl permissions (a coastwatch-only
/// key is one). A holder of any such key could otherwise turn every 403
/// into a durable record. A sub-target of `trawl_server::policy` rather
/// than a new root: `trawl_server=info` already enables it on stdout, so
/// it needs no directive of its own in [`DEFAULT_LOG_FILTER`], while
/// [`UNMETERED_TARGETS`] keeps it — and only it — out of the corpus. The
/// rest of the policy module (including [`normalize_auth_errors`]'s own
/// post-metering events) still persists.
///
/// [`normalize_auth_errors`]: crate::policy::normalize_auth_errors
pub const UNMETERED_POLICY_TARGET: &str = "trawl_server::policy::unmetered";

/// Targets emitted from request handling that no per-key rate limiter has
/// metered, and the panic diagnostic: logged, never persisted as
/// `service=trawld` telemetry.
///
/// fleet-auth's bearer shell warns on every missing or malformed header and
/// every invalid or revoked key, and reports keystore trouble under
/// `auth.backend`, all of it from middleware outside `rate_limit_middleware`
/// (the limiter needs a verified key, so it cannot run before authn).
/// [`PREAUTH_TRANSPORT_TARGET`] is cheaper still: the accept loop warns on
/// every failed TLS handshake, which a bare TCP connect-and-close is enough
/// to provoke. [`UNMETERED_POLICY_TARGET`] is trawl's own grant check, also
/// outside the limiter, so an authenticated key that resolves no trawl
/// permission — a shared-keystore neighbour's key, say — gets its 403
/// unmetered too. Persisting any of it would hand a client the limiter
/// cannot slow a durable-write amplifier: one ~400-byte record per rejected
/// connection or request, compacted into the corpus and competing with real
/// log data for retention.
///
/// Excluding them from the corpus does not lose the signal. They keep
/// flowing to stdout under the same directives, where retention is the
/// operator's log pipeline, and every rejection is counted on `/metrics` as
/// `trawl_auth_failures_total{reason}` from trawl's own policy layer
/// (`crate::policy::count_auth_failure`), which sits outside the bearer
/// shell and therefore sees exactly the 401/503 it produces. A counter with
/// a closed label set cannot be amplified — the series count is fixed
/// however hard an unauthenticated client hammers the endpoint — so
/// credential stuffing, token brute force and a revoked key still in use
/// stay alarmable. Everything trawld emits behind the limiter still
/// persists, the rest of `trawl_server` and `storage.backend` included.
///
/// [`UNMETERED_FAILURE_TARGET`], the failure event of a 5xx no limiter
/// metered, is in this list for its descendants only. A 401 or 403 is the
/// caller's fault; a 5xx before admission is the server's, the auth backend
/// down say, and it is the incident an operator searches for afterwards.
/// The same amplifier argument still holds, so [`is_persisted_target`]
/// exempts that exact target and [`WalLayer`] admits it under
/// [`UNMETERED_FAILURE_CAP_PER_MINUTE`] instead of without a bound
/// (ADR-0040). A target beneath it has no cap and stays excluded.
///
/// [`PANIC_TARGET`] is here for a different reason. The panic diagnostic
/// says where a panic happened, on stdout only; the caught request's own
/// failure event is the persisted record (ADR-0040).
///
/// Matching is by target segment, so `fleet_auth` covers
/// `fleet_auth::middleware` but never a `fleet_authority` target — and
/// `trawl_server::policy::unmetered` excludes only itself and its own
/// descendants, never `trawl_server::policy`.
pub const UNMETERED_TARGETS: [&str; 6] = [
    "fleet_auth",
    "auth.backend",
    PREAUTH_TRANSPORT_TARGET,
    UNMETERED_POLICY_TARGET,
    UNMETERED_FAILURE_TARGET,
    PANIC_TARGET,
];

/// Target of the panic diagnostic [`install_panic_hook`] emits. A
/// sub-target of `trawl_server`, so [`DEFAULT_LOG_FILTER`] keeps it on
/// stdout with no directive of its own, and in [`UNMETERED_TARGETS`], so
/// it never reaches the WAL.
pub const PANIC_TARGET: &str = "trawl_server::panic";

/// Replace the process panic hook with one that logs where a panic
/// happened and never what it said (ADR-0040).
///
/// The default hook prints the payload to stderr, and a payload can hold
/// anything its format string was given: generated SQL, event values, the
/// caller's DSL, file paths. This hook emits one ERROR event on
/// [`PANIC_TARGET`] with the source file, line and column and the thread's
/// name, and nothing else. It does not chain the previous hook, which
/// would print the payload after all. Call it once the subscriber is
/// installed, so the event has somewhere to go.
///
/// `text_sink` says whether the subscriber has a stdout or file logger to
/// record that event. The WAL refuses [`PANIC_TARGET`], so the event
/// reaches nothing without one (the monitor TUI owns stdout and no
/// `log_file` is configured), or when the log filter disables the target
/// (`RUST_LOG=trawld=debug` names no `trawl_server` directive). In either
/// case the hook also writes one line to `stderr`, directly rather than
/// through tracing: `trawld: panicked at FILE:LINE:COLUMN on thread
/// 'NAME'`, with no payload. When a text sink records the event it writes
/// nothing there, so the location is never printed twice.
///
/// The event is a root (`parent: None`): the request span it may fire
/// inside would lend it the raw path and user agent. The WAL layer refuses
/// the target before taking any lock of its own, so a panic inside that
/// layer cannot deadlock on its way out.
pub fn install_panic_hook<W>(text_sink: bool, stderr: W)
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location();
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("<unnamed>");
        // Asked before the event, of the same dispatcher and filter.
        let recorded = text_sink && tracing::enabled!(target: PANIC_TARGET, tracing::Level::ERROR);
        tracing::error!(
            target: PANIC_TARGET,
            parent: None,
            event_type = "panic",
            file = location.map(std::panic::Location::file),
            line = location.map(std::panic::Location::line),
            column = location.map(std::panic::Location::column),
            thread,
            "panicked"
        );
        if !recorded {
            let at = location.map_or_else(
                || "<unknown>".to_owned(),
                |location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                },
            );
            // One write of the whole line, so concurrent panics do not
            // interleave mid-line. A failed write has nowhere to report.
            let line = format!("trawld: panicked at {at} on thread '{thread}'\n");
            let _ = std::io::Write::write_all(&mut stderr.make_writer(), line.as_bytes());
        }
    }));
}

/// How many unmetered failure events ([`UNMETERED_FAILURE_TARGET`]) the WAL
/// layer persists per fixed one-minute window, across the whole process
/// (ADR-0040). A constant with no operator setting: it bounds the durable
/// writes a client the rate limiter cannot slow can cause, at 60 rows a
/// minute. Events past it go to stdout only and are counted under drop
/// reason `unmetered_cap`.
pub const UNMETERED_FAILURE_CAP_PER_MINUTE: u32 = 60;

/// The fixed window [`UNMETERED_FAILURE_CAP_PER_MINUTE`] counts over.
const UNMETERED_FAILURE_WINDOW: Duration = Duration::from_mins(1);

/// The one fixed-window cap on persisted unmetered failures.
///
/// A window opens at the first event after the previous one closed and
/// admits [`UNMETERED_FAILURE_CAP_PER_MINUTE`] events. The decision and the
/// count update happen under one lock, so concurrent events on any number
/// of threads never admit more than the cap. Fixed memory: no per-peer
/// state, nothing to evict.
#[derive(Default)]
struct UnmeteredFailureCap {
    /// Start of the open window and the events admitted in it.
    window: Mutex<Option<(Instant, u32)>>,
}

impl UnmeteredFailureCap {
    /// Whether an event at `now` may be persisted, counting it if so.
    fn admit(&self, now: Instant) -> bool {
        let mut window = self.window.lock();
        match window.as_mut() {
            Some((start, admitted))
                if now.saturating_duration_since(*start) < UNMETERED_FAILURE_WINDOW =>
            {
                if *admitted < UNMETERED_FAILURE_CAP_PER_MINUTE {
                    *admitted += 1;
                    true
                } else {
                    false
                }
            }
            _ => {
                *window = Some((now, 1));
                true
            }
        }
    }
}

/// Whether events on `target` may be persisted as telemetry — false for
/// every [`UNMETERED_TARGETS`] entry and its module descendants. True for
/// the exact unmetered failure target ([`UNMETERED_FAILURE_TARGET`]),
/// which [`WalLayer`] persists under its own cap; its descendants stay
/// false, because nothing caps them.
///
/// [`WalLayer`] applies it on top of the resolved directives, which every
/// sink shares. A second, non-configurable predicate rather than an
/// appended `fleet_auth=off` directive: `EnvFilter` resolves by
/// specificity, so an operator `RUST_LOG` naming
/// `fleet_auth::middleware=info` would outrank an appended target-level
/// `off` and quietly restore the amplifier. The unmetered exclusion is an
/// invariant of what trawl writes to its own disk, not a log level — which
/// is also why the grant rejection keeps its INFO level and loses its
/// target instead of being demoted to DEBUG.
#[must_use]
pub fn is_persisted_target(target: &str) -> bool {
    target == UNMETERED_FAILURE_TARGET
        || !UNMETERED_TARGETS.iter().any(|excluded| {
            target == *excluded
                || target
                    .strip_prefix(excluded)
                    .is_some_and(|rest| rest.starts_with("::"))
        })
}

/// A resolved log filter: the directive string to install plus an optional
/// warning to emit once the subscriber is up.
#[derive(Debug, Clone)]
pub struct ResolvedLogFilter {
    /// Directive string each layer builds its `EnvFilter` from.
    pub directives: String,
    /// Set when `RUST_LOG` was present but unparseable: a `config_warning`
    /// message carrying the parse error but never the raw env value.
    pub warning: Option<String>,
}

/// Resolve the effective tracing filter from an explicit `RUST_LOG` value.
///
/// - unset → [`DEFAULT_LOG_FILTER`];
/// - set and valid → the operator's value, authoritative;
/// - set and invalid → [`DEFAULT_LOG_FILTER`] plus a warning that carries
///   the parse error but **not** the raw environment value (it may contain
///   anything — pasted secrets included — and the warning is persisted as
///   telemetry).
///
/// Takes the env value as a parameter so it is unit-testable without
/// mutating process env (`unsafe_code = "forbid"`).
pub fn resolve_log_filter(env_value: Option<&str>) -> ResolvedLogFilter {
    match env_value {
        None => ResolvedLogFilter {
            directives: DEFAULT_LOG_FILTER.to_owned(),
            warning: None,
        },
        Some(value) => match tracing_subscriber::EnvFilter::try_new(value) {
            Ok(_) => ResolvedLogFilter {
                directives: value.to_owned(),
                warning: None,
            },
            Err(e) => ResolvedLogFilter {
                directives: DEFAULT_LOG_FILTER.to_owned(),
                warning: Some(format!(
                    "RUST_LOG is set but could not be parsed ({e}); using the \
                     default filter \"{DEFAULT_LOG_FILTER}\""
                )),
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Subscriber construction
// ---------------------------------------------------------------------------

/// The JSON file logger's layer, as the reload handle in [`FileLogHandle`]
/// names it.
pub type JsonLogLayer = tracing_subscriber::fmt::Layer<
    tracing_subscriber::Registry,
    tracing_subscriber::fmt::format::JsonFields,
    tracing_subscriber::fmt::format::Format<tracing_subscriber::fmt::format::Json>,
    tracing_subscriber::fmt::writer::BoxMakeWriter,
>;

/// Swaps the JSON file logger's writer from stderr to the configured file
/// once storage admission has succeeded.
pub type FileLogHandle =
    tracing_subscriber::reload::Handle<JsonLogLayer, tracing_subscriber::Registry>;

/// Where trawld's own events go. Each sink is optional, so every startup
/// shape (monitor or not, telemetry or file log or neither) is one call to
/// [`build_subscriber`].
pub struct LogSinks<W> {
    /// Human-readable lines. `None` while the monitor TUI owns the terminal:
    /// interleaved log output would corrupt it.
    pub stdout: Option<W>,
    /// Self-telemetry into the ingest WAL. Production registers it in place
    /// of the JSON file logger.
    pub wal: Option<WalLayer>,
    /// A JSON logger that writes to stderr until [`FileLogHandle`] points it
    /// at the configured file. Logging must not create occupancy in a fresh
    /// root or alter refused storage, so the file opens only after
    /// admission.
    pub file_log: bool,
}

impl<W> std::fmt::Debug for LogSinks<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogSinks")
            .field("stdout", &self.stdout.is_some())
            .field("wal", &self.wal)
            .field("file_log", &self.file_log)
            .finish()
    }
}

/// Build trawld's tracing subscriber from the resolved directives and the
/// selected sinks. `main` installs the result with `.init()`, which also
/// installs the `log` bridge.
///
/// Returns the file logger's reload handle when [`LogSinks::file_log`] is
/// set.
pub fn build_subscriber<W>(
    directives: &str,
    sinks: LogSinks<W>,
) -> (tracing::Dispatch, Option<FileLogHandle>)
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::fmt;
    use tracing_subscriber::layer::SubscriberExt;

    let (file_layer, file_handle) = if sinks.file_log {
        let file_layer = fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(fmt::writer::BoxMakeWriter::new(std::io::stderr));
        let (file_layer, handle) = tracing_subscriber::reload::Layer::new(file_layer);
        (Some(file_layer), Some(handle))
    } else {
        (None, None)
    };

    // One global filter from the directives `resolve_log_filter` resolved
    // (and validated when operator-supplied), shared by every sink. The WAL
    // layer narrows it with `is_persisted_target` itself.
    //
    // Never a per-layer filter per sink. tracing-subscriber keeps per-layer
    // verdicts in a thread-local bitmap that only a dispatched span or event
    // consumes. An `enabled()` query that every per-layer filter refuses
    // still answers true (the registry reports "any enabled" unless all 64
    // filter bits are set), so sqlx's slow-statement `log_enabled!` probe
    // goes on to a `tracing::event!` callsite whose interest is `never` and
    // dispatches nothing. The refusals stay in the bitmap, and the next
    // span or event on that worker thread inherits them: a request span
    // vanished from stdout and the WAL alike, and an event was dropped.
    let filter = tracing_subscriber::EnvFilter::new(directives);
    let subscriber = tracing_subscriber::registry()
        .with(file_layer)
        .with(sinks.stdout.map(|writer| fmt::layer().with_writer(writer)))
        .with(sinks.wal)
        .with(filter);
    (subscriber.into(), file_handle)
}

// ---------------------------------------------------------------------------
// WalHandle: deferred writer injection
// ---------------------------------------------------------------------------

/// Shared handle for deferred WAL writer injection.
///
/// Cloned into the [`WalLayer`] and retained by `main()`. Once the WAL
/// writer is ready, [`set`](Self::set) activates event capture.
#[derive(Debug, Clone)]
pub struct WalHandle(Arc<OnceLock<(Arc<WalWriter>, Arc<str>)>>);

impl Default for WalHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl WalHandle {
    pub fn new() -> Self {
        Self(Arc::new(OnceLock::new()))
    }

    /// Inject the WAL writer and the env telemetry events land under
    /// (`default_env`). Called once after config is loaded. Subsequent
    /// calls are silently ignored (first write wins).
    pub fn set(&self, writer: Arc<WalWriter>, env: &str) {
        let _ = self.0.set((writer, env.into()));
    }

    fn get(&self) -> Option<&(Arc<WalWriter>, Arc<str>)> {
        self.0.get()
    }
}

// ---------------------------------------------------------------------------
// WalLayer: tracing Layer implementation
// ---------------------------------------------------------------------------

/// Tracing layer that serializes events as ndjson and writes them to the
/// ingest WAL as `service:trawld`.
#[derive(Clone)]
pub struct WalLayer {
    inner: Arc<WalLayerInner>,
}

/// The service every telemetry event is filed under — the `trawld`
/// profile's fixed assertion, the WAL filename, and the `service` column,
/// all from one spelling.
const TELEMETRY_SERVICE: &str = "trawld";

/// One staged flush unit: the serialized ndjson lines and the event maps
/// they were serialized from. Written to the WAL as a single file, so the
/// hot-buffer `batch_id` ↔ WAL-filename-stem contract holds per batch.
struct Batch {
    bytes: Vec<u8>,
    events: Vec<serde_json::Map<String, serde_json::Value>>,
}

/// Maximum serialized ndjson one drain unit may carry into a single WAL
/// write.
///
/// The retry queue is bounded in bytes, and [`WalLayerInner::stage`] makes
/// one batch per flush tick, so a long WAL outage can leave thousands of
/// tiny batches queued. Writing them one file each would mean thousands of
/// sequential create + write + fsync + rename + dir-fsync round trips —
/// tens of seconds of blocking-pool I/O during which nothing new is
/// published — and as many WAL files for compaction to chew through.
/// Coalescing bounds the recovery drain by queued bytes instead: with the
/// default 16 MiB budget the entire queue leaves in a handful of writes.
/// A unit whose first batch alone exceeds this is still written (a drain
/// must never stall), so the ceiling is a target, not a hard limit.
const MAX_DRAIN_UNIT_BYTES: usize = 4 * 1024 * 1024;

/// Documented per-event overhead estimate charged on top of the serialized
/// bytes for a retained event map (allocator overhead, map buckets,
/// `String` headers). Like the hot buffer's `max_bytes`, the resulting
/// charge is an estimate, not an exact accounting.
const EVENT_MAP_OVERHEAD_BYTES: usize = 256;

/// Estimated memory charged against `telemetry_buffer_max_bytes` for
/// `bytes` of serialized ndjson holding `events` retained maps: the ndjson
/// counted twice (the serialized buffer plus the retained maps, which hold
/// roughly the same payload again) plus [`EVENT_MAP_OVERHEAD_BYTES`] per
/// event. The same formula charges the active buffer, a queued batch and
/// an in-flight batch, so the shared budget is comparable across all three
/// and staging moves charge without changing the total.
fn charge_of(bytes: usize, events: usize) -> usize {
    bytes * 2 + events * EVENT_MAP_OVERHEAD_BYTES
}

/// [`charge_of`] for one staged batch.
fn batch_charge(batch: &Batch) -> usize {
    charge_of(batch.bytes.len(), batch.events.len())
}

/// The active (not yet staged) buffer: ndjson bytes and their event maps,
/// under one lock so the two representations can never skew.
#[derive(Default)]
struct ActiveBuffer {
    bytes: Vec<u8>,
    events: Vec<serde_json::Map<String, serde_json::Value>>,
}

impl ActiveBuffer {
    fn charge(&self) -> usize {
        charge_of(self.bytes.len(), self.events.len())
    }
}

/// Running charge of staged memory: everything queued in `pending` plus
/// the batch currently in flight through a WAL write. Tracked as counters
/// rather than derived from `pending` for two reasons: the in-flight batch
/// is not in the queue and would otherwise vanish from the accounting
/// while a wedged write holds it, and `on_event` can then evaluate the
/// shared budget without walking or locking the queue.
#[derive(Default)]
struct StagedCharge {
    events: AtomicUsize,
    bytes: AtomicUsize,
}

impl StagedCharge {
    /// Charge a batch on staging.
    fn charge(&self, batch: &Batch) {
        self.events.fetch_add(batch.events.len(), Ordering::Relaxed);
        self.bytes.fetch_add(batch_charge(batch), Ordering::Relaxed);
    }

    /// Release a batch's charge once it leaves memory — published after a
    /// durable write, shed at the cap, or lost with a panicked write task.
    /// Saturating: the accounting is an estimate, never a panic source.
    fn release(&self, events: usize, bytes: usize) {
        let _ = self
            .events
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(events))
            });
        let _ = self
            .bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(bytes))
            });
    }
}

/// Drop accounting, reset when the `telemetry_dropped` recovery event is
/// emitted after a successful write.
#[derive(Default)]
struct DropCounters {
    /// Events dropped by the pre-init 1 MiB cap (exact).
    preinit_events: AtomicU64,
    /// Bytes dropped by the pre-init cap (mean-line-size estimate — the
    /// drop happens before serialization).
    preinit_bytes: AtomicU64,
    /// Events dropped by retry-queue overflow (exact).
    cap_events: AtomicU64,
    /// ndjson bytes dropped by retry-queue overflow (exact).
    cap_bytes: AtomicU64,
    /// Events consumed by a panicked or cancelled blocking write (exact).
    crashed_events: AtomicU64,
    /// ndjson bytes consumed by a panicked or cancelled write (exact).
    crashed_bytes: AtomicU64,
    /// Unmetered failure events past [`UNMETERED_FAILURE_CAP_PER_MINUTE`]
    /// (exact). No byte count: the event is refused before serialization.
    unmetered_cap_events: AtomicU64,
}

struct WalLayerInner {
    /// Env the `trawld` profile asserts on every telemetry event
    /// (`default_env`, boot-validated and always a member of `envs`).
    env: String,
    /// The effective env allowlist, threaded so the door can run its
    /// ordinary `env` validation on the assertion like any other.
    envs: Arc<[String]>,
    /// The boot-resolved per-profile derivation policy. Telemetry is an
    /// ordinary sender (ADR-0013): its bare `level` rides the configured
    /// `severity_from` chain, exactly as a vector-shipped app's would.
    derivation: Arc<crate::ingest::producer::Derivation>,
    handle: WalHandle,
    /// Active buffer: events accumulated since the last stage.
    active: Mutex<ActiveBuffer>,
    /// FIFO retry queue of staged batches awaiting a successful WAL write.
    pending: Mutex<VecDeque<Batch>>,
    /// Charge of `pending` PLUS any batch in flight through a WAL write.
    staged: StagedCharge,
    /// The one cap on estimated memory charged by the active buffer, the
    /// retry queue and the in-flight batch together
    /// (`[ingest] telemetry_buffer_max_bytes`). Atomic so tests can
    /// tighten it after construction.
    max_buffer_bytes: AtomicUsize,
    /// Cached hostname, resolved once at layer creation. `None` when the
    /// lookup failed or returned nothing: the profile then asserts
    /// absence and the door keeps the event with `host` omitted plus a
    /// `host.omitted` repair, rather than stamping a meaningless `host=""`.
    host: Option<String>,
    /// Loss accounting for the recovery event and metrics.
    dropped: DropCounters,
    /// The process-wide cap on persisted unmetered failures. One per
    /// process because `trawld` builds one layer and every clone shares
    /// this inner.
    unmetered_failures: UnmeteredFailureCap,
    /// When the last `telemetry_dropped` record whose only loss was
    /// `unmetered_cap` was emitted. Such a record is emitted at most once
    /// per [`UNMETERED_FAILURE_WINDOW`]: its own write publishes, and a
    /// client forcing one capped failure per flush tick would otherwise buy
    /// one durable record per tick. Held-back counts accumulate for the next
    /// permitted record.
    last_unmetered_report: Mutex<Option<Instant>>,
    /// Test-only clock offset, so a test can move the cap's window without
    /// waiting a minute.
    #[cfg(test)]
    clock_offset: Mutex<Duration>,
    /// Last time a WAL failure was reported to stderr (rate limit).
    last_stderr: Mutex<Option<Instant>>,
    /// Deferred event bus for real-time fanout (SSE streaming).
    bus: OnceLock<Arc<crate::bus::LocalEventBus>>,
    /// Deferred hot buffer for synchronous insertion (query freshness).
    hot_buffer: OnceLock<Arc<crate::hot_buffer::HotBuffer>>,
    /// Test-only fault injection at the ownership boundary where the
    /// blocking closure has consumed a batch and a `JoinError` loses it.
    #[cfg(test)]
    panic_next_write: AtomicBool,
    #[cfg(test)]
    pause_before_insert:
        Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

#[derive(Clone, Copy)]
enum WriteFailureDisposition {
    Retained,
    Dropped,
}

impl std::fmt::Debug for WalLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalLayer")
            .field("active", &self.inner.handle.get().is_some())
            .field("buffer_bytes", &self.inner.active.lock().bytes.len())
            .field("pending_batches", &self.inner.pending.lock().len())
            .finish()
    }
}

impl WalLayer {
    /// A layer for tests: one-env allowlist, the packaged derivation
    /// policy, the default memory budget.
    ///
    /// Production goes through [`WalLayer::new_with_buffer_cap`], which
    /// takes the real allowlist and the boot-resolved policy — a test
    /// that only cares about buffering, flushing or shedding should not
    /// have to assemble either.
    #[cfg(test)]
    pub fn new(handle: WalHandle, env: &str) -> Self {
        Self::new_with_buffer_cap(
            handle,
            &[env.to_owned()],
            env,
            Arc::new(crate::ingest::producer::Derivation::defaults()),
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
        )
    }

    /// Create a layer backed by the given handle.
    ///
    /// `envs`/`env` are the ingest allowlist and `default_env`: the
    /// `trawld` profile asserts that env on every event, so the door runs
    /// its ordinary validation on a value boot-validation already proved
    /// (`env.defaulted` is structurally unreachable here). `derivation`
    /// is the one resolved source policy every profile shares.
    /// `max_buffer_bytes` is the shared budget over the active buffer,
    /// retry queue and in-flight batch
    /// (`[ingest] telemetry_buffer_max_bytes`).
    pub fn new_with_buffer_cap(
        handle: WalHandle,
        envs: &[String],
        env: &str,
        derivation: Arc<crate::ingest::producer::Derivation>,
        max_buffer_bytes: usize,
    ) -> Self {
        let host = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .filter(|h| !h.is_empty());
        Self {
            inner: Arc::new(WalLayerInner {
                handle,
                active: Mutex::new(ActiveBuffer {
                    bytes: Vec::with_capacity(8192),
                    events: Vec::new(),
                }),
                pending: Mutex::new(VecDeque::new()),
                staged: StagedCharge::default(),
                max_buffer_bytes: AtomicUsize::new(max_buffer_bytes),
                host,
                env: env.to_owned(),
                envs: envs.into(),
                derivation,
                dropped: DropCounters::default(),
                unmetered_failures: UnmeteredFailureCap::default(),
                last_unmetered_report: Mutex::new(None),
                #[cfg(test)]
                clock_offset: Mutex::new(Duration::ZERO),
                last_stderr: Mutex::new(None),
                bus: OnceLock::new(),
                hot_buffer: OnceLock::new(),
                #[cfg(test)]
                panic_next_write: AtomicBool::new(false),
                #[cfg(test)]
                pause_before_insert: Mutex::new(None),
            }),
        }
    }

    /// Inject the event bus for real-time fanout (SSE streaming).
    /// Called once after `AppState` is constructed. Subsequent calls are
    /// silently ignored (first write wins).
    pub fn set_bus(&self, bus: Arc<crate::bus::LocalEventBus>) {
        let _ = self.inner.bus.set(bus);
    }

    /// Inject the hot buffer for synchronous event insertion.
    /// Called once after `AppState` is constructed. Subsequent calls are
    /// silently ignored (first write wins).
    pub fn set_hot_buffer(&self, buf: Arc<crate::hot_buffer::HotBuffer>) {
        let _ = self.inner.hot_buffer.set(buf);
    }

    /// Synchronous flush: stage the active buffer and drain the retry
    /// queue with direct (non-`spawn_blocking`) writes.
    ///
    /// Test-only convenience — the production flush task's sole write path
    /// is [`WalLayer::flush_cycle`], which runs the durability barriers on
    /// the blocking pool. Call outside an async runtime task.
    pub fn flush(&self) {
        let Some((writer, env)) = self.inner.handle.get() else {
            return;
        };
        self.inner.stage();
        let mut coalesce = false;
        while let Some(batch) = self.inner.pop_drain_unit(coalesce) {
            let publication = self.inner.hot_buffer.get().map(|buf| buf.publication());
            let _ingest = publication.as_ref().map(|gate| gate.blocking_ingest());
            match writer.write(env, TELEMETRY_SERVICE, &batch.bytes) {
                Ok(wal_path) => {
                    coalesce = true;
                    self.inner.publish(env, &wal_path, batch);
                }
                Err(e) => {
                    self.inner
                        .record_write_failure(&e, WriteFailureDisposition::Retained);
                    self.inner.requeue_front(batch);
                    break;
                }
            }
        }
        self.inner.update_gauges();
    }

    /// Async flush cycle: stage the active buffer, then write pending
    /// batches oldest-first with each WAL write (and both of its
    /// durability barriers) on the blocking pool. Stops at the first
    /// failure, retaining the failed unit at the queue front.
    ///
    /// Once a write has succeeded, the rest of the queue drains in
    /// coalesced units of at most [`MAX_DRAIN_UNIT_BYTES`], so recovering
    /// from a long outage costs writes proportional to queued bytes rather
    /// than to the number of flush ticks the outage lasted.
    pub async fn flush_cycle(&self) {
        let Some((writer, env)) = self.inner.handle.get() else {
            return;
        };
        self.inner.stage();
        let mut coalesce = false;
        while let Some(batch) = self.inner.pop_drain_unit(coalesce) {
            let w = Arc::clone(writer);
            let batch_env = Arc::clone(env);
            let inner = Arc::clone(&self.inner);
            let dispatch = tracing::dispatcher::get_default(Clone::clone);
            // Captured before the batch moves into the closure, so a lost
            // batch can still be released from the shared accounting.
            let in_flight = (batch.events.len(), batch_charge(&batch), batch.bytes.len());
            #[cfg(test)]
            let panic_write = self.inner.panic_next_write.swap(false, Ordering::Relaxed);
            let joined = tokio::task::spawn_blocking(move || {
                let _dispatch = tracing::dispatcher::set_default(&dispatch);
                #[cfg(test)]
                assert!(!panic_write, "injected telemetry WAL write panic");
                // The task keeps the guard through insertion even if its
                // async caller is cancelled while the WAL write runs.
                let publication = inner.hot_buffer.get().map(|buf| buf.publication());
                let _ingest = publication.as_ref().map(|gate| gate.blocking_ingest());
                match w.write(&batch_env, TELEMETRY_SERVICE, &batch.bytes) {
                    Ok(wal_path) => {
                        inner.publish(&batch_env, &wal_path, batch);
                        inner.update_gauges();
                        Ok(())
                    }
                    Err(e) => Err((e, batch)),
                }
            })
            .await;
            match joined {
                Ok(Ok(())) => {
                    coalesce = true;
                }
                Ok(Err((e, batch))) => {
                    self.inner
                        .record_write_failure(&e, WriteFailureDisposition::Retained);
                    self.inner.requeue_front(batch);
                    break;
                }
                Err(join_err) => {
                    // The batch was consumed by the panicked/cancelled
                    // closure and cannot be recovered. WalWriter::write
                    // does not panic in practice.
                    self.inner.staged.release(in_flight.0, in_flight.1);
                    self.inner
                        .record_crashed_drop(in_flight.0 as u64, in_flight.2 as u64);
                    self.inner.record_write_failure(
                        &std::io::Error::other(join_err),
                        WriteFailureDisposition::Dropped,
                    );
                    break;
                }
            }
        }
        self.inner.update_gauges();
    }
}

impl WalLayerInner {
    /// The unmetered cap's clock: monotonic, and movable by tests.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn cap_clock(&self) -> Instant {
        let now = Instant::now();
        #[cfg(test)]
        let now = now + *self.clock_offset.lock();
        now
    }

    /// Whether an unmetered failure event may be persisted. Past the cap,
    /// counts it as dropped under reason `unmetered_cap`.
    fn admit_unmetered_failure(&self) -> bool {
        if self.unmetered_failures.admit(self.cap_clock()) {
            return true;
        }
        self.dropped
            .unmetered_cap_events
            .fetch_add(1, Ordering::Relaxed);
        metrics::counter!(
            crate::metrics::TELEMETRY_EVENTS_DROPPED_TOTAL,
            "reason" => crate::metrics::TelemetryDropReason::UnmeteredCap.label()
        )
        .increment(1);
        false
    }

    /// The pre-init cap: while the writer is not yet set, the active
    /// buffer is the only place events can go, so it is bounded on its
    /// own. Returns whether this event must be dropped.
    ///
    /// Reached before any per-event work — the drop is counted with an
    /// exact event count and an estimated byte charge from the mean
    /// buffered line size, because it happens before serialization and
    /// the telemetry-disabled path (where the writer is never injected)
    /// must stay cheap.
    fn shed_at_preinit_cap(&self) -> bool {
        const PRE_INIT_CAP: usize = 1024 * 1024;
        if self.handle.get().is_some() {
            return false;
        }
        let estimate = {
            let active = self.active.lock();
            if active.bytes.len() < PRE_INIT_CAP {
                None
            } else {
                Some((active.bytes.len() / active.events.len().max(1)) as u64)
            }
        };
        let Some(mean_line_bytes) = estimate else {
            return false;
        };
        self.dropped.preinit_events.fetch_add(1, Ordering::Relaxed);
        self.dropped
            .preinit_bytes
            .fetch_add(mean_line_bytes, Ordering::Relaxed);
        metrics::counter!(
            crate::metrics::TELEMETRY_EVENTS_DROPPED_TOTAL,
            "reason" => crate::metrics::TelemetryDropReason::PreinitCap.label()
        )
        .increment(1);
        metrics::counter!(
            crate::metrics::TELEMETRY_BYTES_DROPPED_TOTAL,
            "reason" => crate::metrics::TelemetryDropReason::PreinitCap.label()
        )
        .increment(mean_line_bytes);
        true
    }

    /// Swap the active buffer into a pending [`Batch`].
    ///
    /// Staging moves charge from the active buffer onto the queue without
    /// changing the total, so it enforces no cap of its own — the shared
    /// budget is enforced at event insertion ([`Self::admit`]), which is
    /// the only place that can bound the active buffer while a wedged
    /// write holds a batch in flight.
    ///
    /// No-op until the writer is set: pre-init events stay in the active
    /// buffer under the pre-init cap, preserving the bootstrap-buffering
    /// contract.
    fn stage(&self) {
        if self.handle.get().is_none() {
            return;
        }
        let batch = {
            let mut active = self.active.lock();
            if active.bytes.is_empty() {
                return;
            }
            Batch {
                bytes: std::mem::take(&mut active.bytes),
                events: std::mem::take(&mut active.events),
            }
        };
        self.staged.charge(&batch);
        self.pending.lock().push_back(batch);
    }

    /// Admit one serialized event of `line_len` ndjson bytes against the
    /// shared budget, shedding staged batches if that is what it takes.
    ///
    /// Returns `false` when the event must be dropped — which happens only
    /// once the queue is empty and the active buffer plus the
    /// unreclaimable in-flight batch already fill the budget. Dropping the
    /// newest event is the honest end of the ladder: exempting it (as a
    /// `len() > 1` queue guard does) is what turns a cap into unbounded
    /// growth under a WAL stall.
    fn admit(&self, line_len: usize) -> bool {
        let cap = self.max_buffer_bytes.load(Ordering::Relaxed);
        let incoming = charge_of(line_len, 1);
        let over = {
            let active = self.active.lock();
            (active.charge() + self.staged.bytes.load(Ordering::Relaxed) + incoming)
                .saturating_sub(cap)
        };
        if over == 0 {
            return true;
        }

        // Shed the oldest staged batches first: current operational state
        // is worth more than history, and the in-flight batch is already
        // owned by the write task and cannot be reclaimed.
        let mut reclaimed = 0usize;
        {
            let mut pending = self.pending.lock();
            while reclaimed < over {
                let Some(dropped) = pending.pop_front() else {
                    break;
                };
                reclaimed += batch_charge(&dropped);
                self.staged
                    .release(dropped.events.len(), batch_charge(&dropped));
                self.record_cap_drop(dropped.events.len() as u64, dropped.bytes.len() as u64);
            }
        }
        if reclaimed >= over {
            return true;
        }

        self.record_cap_drop(1, line_len as u64);
        false
    }

    /// Count `events`/`bytes` lost to the shared budget: local accounting
    /// for the `telemetry_dropped` recovery record plus the scrapeable
    /// counters (which stay visible precisely while self-ingestion is not).
    fn record_cap_drop(&self, events: u64, bytes: u64) {
        self.dropped.cap_events.fetch_add(events, Ordering::Relaxed);
        self.dropped.cap_bytes.fetch_add(bytes, Ordering::Relaxed);
        metrics::counter!(
            crate::metrics::TELEMETRY_EVENTS_DROPPED_TOTAL,
            "reason" => crate::metrics::TelemetryDropReason::BufferCap.label()
        )
        .increment(events);
        metrics::counter!(
            crate::metrics::TELEMETRY_BYTES_DROPPED_TOTAL,
            "reason" => crate::metrics::TelemetryDropReason::BufferCap.label()
        )
        .increment(bytes);
    }

    /// Count the in-memory batch consumed by a panicked or cancelled task.
    /// Its WAL may already be durable. This is separate from the one WAL
    /// failure count: the metrics describe different facts about that attempt.
    fn record_crashed_drop(&self, events: u64, bytes: u64) {
        self.dropped
            .crashed_events
            .fetch_add(events, Ordering::Relaxed);
        self.dropped
            .crashed_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        metrics::counter!(
            crate::metrics::TELEMETRY_EVENTS_DROPPED_TOTAL,
            "reason" => crate::metrics::TelemetryDropReason::WriteCrashed.label()
        )
        .increment(events);
        metrics::counter!(
            crate::metrics::TELEMETRY_BYTES_DROPPED_TOTAL,
            "reason" => crate::metrics::TelemetryDropReason::WriteCrashed.label()
        )
        .increment(bytes);
    }

    /// Pop the oldest pending batches for one write attempt. With
    /// `coalesce`, they are concatenated oldest-first while they fit in
    /// [`MAX_DRAIN_UNIT_BYTES`] (the first is always taken, however large).
    /// Every line already ends in `\n`, so concatenation is valid ndjson,
    /// and the merged unit lands in one WAL file — one filename stem, one
    /// published `IngestBatch`.
    ///
    /// `coalesce` is set only once a write has succeeded in this cycle:
    /// merging exists to bound the recovery drain, and merging while the
    /// volume is still failing would only coarsen the cap's shedding
    /// granularity (one `admit` would shed a merged unit where a per-tick
    /// batch is all it needed to reclaim).
    ///
    /// No lock is held by the caller across the write itself. The unit
    /// stays charged to [`Self::staged`] while it is in flight — it is
    /// still resident memory, and a wedged write must not make it invisible
    /// to the cap. [`charge_of`] is linear in bytes and events, so merging
    /// moves no charge and the shared budget is unaffected.
    fn pop_drain_unit(&self, coalesce: bool) -> Option<Batch> {
        let mut pending = self.pending.lock();
        let mut unit = pending.pop_front()?;
        if !coalesce {
            return Some(unit);
        }
        while let Some(next) = pending.front() {
            if unit.bytes.len() + next.bytes.len() > MAX_DRAIN_UNIT_BYTES {
                break;
            }
            let mut next = pending.pop_front().expect("peeked front exists");
            unit.bytes.append(&mut next.bytes);
            unit.events.append(&mut next.events);
        }
        Some(unit)
    }

    /// Put a failed batch back at the queue front, preserving FIFO order.
    fn requeue_front(&self, batch: Batch) {
        self.pending.lock().push_front(batch);
    }

    /// Publish a durably-written batch to the hot buffer and event bus —
    /// strictly after WAL success, exactly once (the batch was popped).
    /// Then emit the `telemetry_dropped` recovery record if any loss
    /// accumulated (safe from recursion: `on_event` only buffers).
    /// The caller holds the publication read guard from before WAL writing.
    fn publish(&self, env: &str, wal_path: &std::path::Path, batch: Batch) {
        #[cfg(test)]
        {
            let pause = self.pause_before_insert.lock().take();
            if let Some((entered, release)) = pause {
                let _ = entered.send(());
                let _ = release.recv();
            }
        }
        // The batch leaves the layer's accounting here: the hot buffer
        // takes ownership under its own `hot_buffer_max_bytes` budget.
        self.staged
            .release(batch.events.len(), batch_charge(&batch));

        if !batch.events.is_empty() {
            // batch_id must match the WAL filename stem so compaction can
            // drain the hot buffer after writing parquet.
            use crate::bus::{EventBus, IngestBatch};
            let batch_id: Arc<str> = format!(
                "{env}/{}",
                wal_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("trawld_unknown")
            )
            .into();
            let byte_size = batch.bytes.len();
            let batch = Arc::new(IngestBatch {
                batch_id,
                service: TELEMETRY_SERVICE.into(),
                events: batch.events,
                byte_size,
            });
            if let Some(buf) = self.hot_buffer.get() {
                buf.insert(Arc::clone(&batch));
            }
            if let Some(bus) = self.bus.get() {
                let _ = bus.publish(batch);
            }
        }

        let preinit_events = self.dropped.preinit_events.swap(0, Ordering::Relaxed);
        let preinit_bytes = self.dropped.preinit_bytes.swap(0, Ordering::Relaxed);
        let cap_events = self.dropped.cap_events.swap(0, Ordering::Relaxed);
        let cap_bytes = self.dropped.cap_bytes.swap(0, Ordering::Relaxed);
        let crashed_events = self.dropped.crashed_events.swap(0, Ordering::Relaxed);
        let crashed_bytes = self.dropped.crashed_bytes.swap(0, Ordering::Relaxed);
        let unmetered_cap_events = if preinit_events + cap_events + crashed_events > 0 {
            self.dropped.unmetered_cap_events.swap(0, Ordering::Relaxed)
        } else {
            self.take_unmetered_only_drops()
        };
        if preinit_events + cap_events + crashed_events + unmetered_cap_events > 0 {
            tracing::warn!(
                event_type = "telemetry_dropped",
                dropped_events =
                    preinit_events + cap_events + crashed_events + unmetered_cap_events,
                dropped_bytes = preinit_bytes + cap_bytes + crashed_bytes,
                dropped_events_preinit_cap = preinit_events,
                dropped_bytes_preinit_cap = preinit_bytes,
                dropped_events_buffer_cap = cap_events,
                dropped_bytes_buffer_cap = cap_bytes,
                dropped_events_write_crashed = crashed_events,
                dropped_bytes_write_crashed = crashed_bytes,
                dropped_events_unmetered_cap = unmetered_cap_events,
                "telemetry events were lost (see reason totals; \
                 preinit_cap bytes are a mean-line-size estimate; \
                 unmetered_cap counts no bytes)"
            );
        }
    }

    /// The `unmetered_cap` count for a recovery record that would report
    /// nothing else: all of it when no such record was emitted within the
    /// last [`UNMETERED_FAILURE_WINDOW`], otherwise zero, leaving the count
    /// to accumulate. Unmetered failures are client-driven, and the record's
    /// own write publishes, so without this a client forcing one capped
    /// failure per flush tick buys one durable row and WAL file per tick. With
    /// it, the capped failures and their drop records persist at most the cap
    /// plus one row per window. Decided under the lock, so concurrent
    /// publishes cannot both take a window's one record.
    fn take_unmetered_only_drops(&self) -> u64 {
        let mut last = self.last_unmetered_report.lock();
        if self.dropped.unmetered_cap_events.load(Ordering::Relaxed) == 0 {
            return 0;
        }
        let now = self.cap_clock();
        if last.is_some_and(|at| now.saturating_duration_since(at) < UNMETERED_FAILURE_WINDOW) {
            return 0;
        }
        *last = Some(now);
        self.dropped.unmetered_cap_events.swap(0, Ordering::Relaxed)
    }

    /// Record a WAL write failure: scrapeable counter plus rate-limited
    /// stderr (the independent last-resort channel while self-ingestion
    /// is unavailable). Must not use tracing — see the module docs.
    fn record_write_failure(&self, e: &std::io::Error, disposition: WriteFailureDisposition) {
        metrics::counter!(crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL).increment(1);
        let mut last = self.last_stderr.lock();
        let due = last.is_none_or(|t| t.elapsed() >= Duration::from_mins(1));
        if due {
            let outcome = match disposition {
                WriteFailureDisposition::Retained => "batch retained for retry",
                WriteFailureDisposition::Dropped => "write task crashed; batch counted as dropped",
            };
            eprintln!("[trawl-telemetry] WAL write failed ({outcome}): {e}");
            *last = Some(Instant::now());
        }
    }

    /// Refresh the buffer-depth gauges (per flush cycle). They report the
    /// whole charge against `telemetry_buffer_max_bytes` — active buffer,
    /// retry queue and in-flight batch — so the exported number is the one
    /// the cap is applied to.
    #[allow(clippy::cast_precision_loss)]
    fn update_gauges(&self) {
        let (active_events, active_bytes) = {
            let active = self.active.lock();
            (active.events.len(), active.charge())
        };
        let events = active_events + self.staged.events.load(Ordering::Relaxed);
        let bytes = active_bytes + self.staged.bytes.load(Ordering::Relaxed);
        metrics::gauge!(crate::metrics::TELEMETRY_BUFFER_EVENTS).set(events as f64);
        metrics::gauge!(crate::metrics::TELEMETRY_BUFFER_BYTES).set(bytes as f64);
    }
}

// ---------------------------------------------------------------------------
// JsonVisitor: field collection
// ---------------------------------------------------------------------------

/// Collects tracing fields into a JSON-compatible map.
struct JsonVisitor {
    fields: BTreeMap<String, serde_json::Value>,
}

impl JsonVisitor {
    fn new() -> Self {
        Self {
            fields: BTreeMap::new(),
        }
    }

    /// Store one field under the name the macro spelled.
    ///
    /// A tracing field name is any Rust-side identifier
    /// (`tracing::info!(myField = 1)` is legal), but the ASCII fold stays
    /// at the door in `envelope::canonicalize`, as it does for every
    /// producer: two fields differing only in case then earn
    /// `field.name_case_collision` instead of one silently overwriting the
    /// other in this map.
    fn insert(&mut self, field: &Field, value: serde_json::Value) {
        self.fields.insert(field.name().to_owned(), value);
    }
}

impl Visit for JsonVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, json!(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, json!(value));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.insert(field, json!(value));
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.insert(field, json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, json!(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, json!(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, json!(format!("{value:?}")));
    }
}

// ---------------------------------------------------------------------------
// SpanFields: stored in span extensions for scope walking
// ---------------------------------------------------------------------------

/// Fields recorded on a span, stored in span extensions so [`WalLayer`]
/// can walk the span scope and collect inherited fields.
#[derive(Debug, Default)]
struct SpanFields(BTreeMap<String, serde_json::Value>);

// ---------------------------------------------------------------------------
// Layer implementation
// ---------------------------------------------------------------------------

impl<S> Layer<S> for WalLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        // An unmetered span lends no fields to the events inside it.
        if !is_persisted_target(attrs.metadata().target()) {
            return;
        }
        let mut visitor = JsonVisitor::new();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.fields));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut ext = span.extensions_mut();
            if let Some(fields) = ext.get_mut::<SpanFields>() {
                let mut visitor = JsonVisitor::new();
                values.record(&mut visitor);
                fields.0.extend(visitor.fields);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Unmetered events are logged, never persisted (UNMETERED_TARGETS),
        // except the exact unmetered failure target, which persists under
        // its own process-wide cap (ADR-0040). Its descendants fall through
        // to the exclusion and never reach the cap.
        let target = event.metadata().target();
        if target == UNMETERED_FAILURE_TARGET {
            if !self.inner.admit_unmetered_failure() {
                return;
            }
        } else if !is_persisted_target(target) {
            return;
        }

        // The pre-init cap runs next, before any work this event would
        // otherwise cost — canonicalization included.
        if self.inner.shed_at_preinit_cap() {
            return;
        }

        let mut visitor = JsonVisitor::new();
        event.record(&mut visitor);

        // Walk span scope (root → leaf), collecting inherited fields.
        // Inner spans override outer spans for same-named fields.
        let mut span_fields = BTreeMap::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                let ext = span.extensions();
                if let Some(fields) = ext.get::<SpanFields>() {
                    span_fields.extend(fields.0.clone());
                }
            }
        }

        // Span fields are the base; event fields override.
        span_fields.extend(visitor.fields);

        let metadata = event.metadata();

        // Extract message — tracing stores it as the "message" field.
        let message = span_fields
            .remove("message")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();

        // Derive event_type: prefer explicit field, fall back to message.
        let event_type = span_fields
            .remove("event_type")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| message_to_event_type(&message));

        // One instant per event, used twice: as the `_time` proposal and
        // as the arrival the door stamps into `_ingested`. Equal by
        // construction, so `time.from_ingest` never fires and `_repairs`
        // stays null-dominant on `service=trawld`, and `_ingested` dates
        // the observation rather than the flush up to a tick later.
        let observed_at = chrono::Utc::now();
        let now = observed_at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let level = metadata.level().as_str().to_ascii_lowercase();

        // An ordinary sender payload (ADR-0013): no `env`, `service`,
        // `host`, `_ingested`, `_raw` or `_severity`. The profile asserts
        // identity below, the door stamps the server-owned slots, and
        // `level` rides the configured `severity_from` chain like any
        // app's would — trawld observing trawld is trawld sending, not
        // ingest machinery with privileges.
        let mut payload = serde_json::Map::with_capacity(6 + span_fields.len());
        payload.insert(trawl_core::schema::TIME.into(), json!(&now));
        payload.insert("level".into(), json!(&level));
        payload.insert("target".into(), json!(metadata.target()));
        payload.insert("event_type".into(), json!(event_type));
        payload.insert("message".into(), json!(message));

        // Merge remaining span + event fields.
        for (k, v) in span_fields {
            payload.entry(k).or_insert(v);
        }

        // The one door. Nothing here may call `tracing` — including on
        // the failure path, which is why a refusal is a metric and a
        // silent drop. It is also unreachable by construction: everything
        // the profile asserts is boot-validated, and the zero-initialized
        // `{profile="trawld"}` reject matrix is the evidence for that
        // claim.
        let ctx = crate::ingest::envelope::EnvelopeContext {
            arrival: &now,
            arrival_instant: observed_at,
            envs: &self.inner.envs,
            default_env: &self.inner.env,
            producer: crate::ingest::producer::Producer::Trawld(
                crate::ingest::producer::Asserted {
                    env: &self.inner.env,
                    service: TELEMETRY_SERVICE,
                    host: self.inner.host.as_deref(),
                    // `message: None` asserts nothing — a trawld event's
                    // payload IS its message, so whatever the tracing
                    // macro recorded stands.
                    message: None,
                    repairs: &[],
                },
            ),
            derivation: &self.inner.derivation,
        };
        let canonical = match crate::ingest::envelope::canonicalize(&payload, &ctx) {
            Ok(canonical) => canonical,
            // The message is discarded, the reason is not: the reason is
            // a closed label set, while the message quotes values and
            // would have nowhere to go but a log line emitted from
            // inside the logger.
            Err((_, reason)) => {
                crate::ingest::producer::count_profile_reject(
                    crate::ingest::producer::ProducerKind::Trawld,
                    reason,
                );
                return;
            }
        };
        crate::ingest::producer::count_event_outcome(&canonical);
        let record = canonical.obj;

        // Serialize, then push bytes and map under one lock so the two
        // representations of the active buffer can never skew (a stage
        // between the two pushes would publish a map whose bytes never
        // reached the WAL). serde_json::to_vec on a Map cannot fail.
        let mut line =
            serde_json::to_vec(&record).expect("JSON serialization of a Map is infallible");
        line.push(b'\n');

        // The shared budget is enforced here, over the active buffer, the
        // retry queue and any in-flight batch together — the only point
        // that bounds memory while a wedged WAL write holds a batch.
        if !self.inner.admit(line.len()) {
            return;
        }

        let mut active = self.inner.active.lock();
        active.bytes.extend_from_slice(&line);
        active.events.push(record);
    }
}

// ---------------------------------------------------------------------------
// event_type derivation
// ---------------------------------------------------------------------------

/// Normalize a tracing message to a flat `snake_case` event type.
///
/// Takes the first clause (before `:`) for brevity, lowercases,
/// and replaces non-alphanumeric characters with underscores.
///
/// ```text
/// "query failed: bad request" → "query_failed"
/// "auth failed: missing or malformed Authorization header" → "auth_failed"
/// "query complete" → "query_complete"
/// ```
fn message_to_event_type(message: &str) -> String {
    let base = message.split(':').next().unwrap_or(message).trim();
    let raw: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // Collapse multiple underscores and trim edges.
    raw.split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

// ---------------------------------------------------------------------------
// Flush task
// ---------------------------------------------------------------------------

/// Wall-clock budget for the final shutdown flush. The timeout abandons
/// the await, not the blocking thread — a wedged fsync leaves one
/// lingering blocking thread at process exit rather than hanging shutdown.
/// The process-exit side of that contract is `trawld`'s
/// `Runtime::shutdown_timeout`, which stops the runtime drop from waiting
/// on that thread forever.
const SHUTDOWN_FLUSH_BUDGET: Duration = Duration::from_secs(5);

/// Spawn the periodic buffer flush task.
///
/// Flushes every `interval` to batch WAL writes, with the write itself on
/// the blocking pool ([`WalLayer::flush_cycle`]). Returns a [`JoinHandle`]
/// for shutdown coordination. Send `true` on `shutdown_rx` to trigger a
/// final bounded flush and exit.
///
/// Shutdown is bounded even while a periodic flush is wedged: the periodic
/// flush is itself raced against `shutdown_rx`, so the signal is observed
/// without waiting for an fsync that may never return. Abandoning an
/// in-flight flush costs the in-flight batch's visibility, never its
/// durability — the blocking write it was handed to keeps running, and
/// anything it lands in the WAL is picked up by compaction.
pub fn spawn_flush_task(
    layer: WalLayer,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {
                    tokio::select! {
                        () = layer.flush_cycle() => {}
                        _ = shutdown_rx.changed() => {
                            final_flush(&layer).await;
                            break;
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    final_flush(&layer).await;
                    break;
                }
            }
        }
    })
}

/// The final drain, capped at [`SHUTDOWN_FLUSH_BUDGET`].
///
/// Reports an exhausted budget on stderr, not through `tracing`: this runs
/// inside the flush path, and the layer's buffer is about to be abandoned
/// anyway, so a tracing event would be both re-entrant risk and invisible.
async fn final_flush(layer: &WalLayer) {
    if tokio::time::timeout(SHUTDOWN_FLUSH_BUDGET, layer.flush_cycle())
        .await
        .is_err()
    {
        eprintln!(
            "[trawld] telemetry shutdown flush exceeded {}s budget; \
             abandoning buffered telemetry events",
            SHUTDOWN_FLUSH_BUDGET.as_secs()
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- default filter contract -------------------------------------------

    /// Capture layer recording (target, level) pairs.
    #[derive(Clone, Default)]
    struct CaptureLayer {
        events: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            self.events.lock().push((
                event.metadata().target().to_owned(),
                event.metadata().level().as_str().to_owned(),
            ));
        }
    }

    #[test]
    fn default_filter_passes_all_trawl_targets_and_drops_dependency_noise() {
        use tracing_subscriber::prelude::*;

        let capture = CaptureLayer::default();
        let events = Arc::clone(&capture.events);
        let filter = tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER);
        let subscriber = tracing_subscriber::registry().with(capture.with_filter(filter));
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(target: "trawl_server::handlers", "lib info");
        tracing::error!(target: "trawl_server::ingest", "lib error");
        tracing::info!(target: "trawld", "bin info (startup banner, config_warning)");
        tracing::info!(target: "fleet_auth::middleware", "auth middleware info");
        tracing::error!(target: "auth.backend", "auth backend down");
        tracing::error!(target: "storage.backend", "storage backend down");
        tracing::warn!(target: PREAUTH_TRANSPORT_TARGET, "TLS handshake failed");
        tracing::info!(target: UNMETERED_POLICY_TARGET, "policy: no trawl grant");
        tracing::info!(target: "hyper::proto", "dependency noise");

        let seen = events.lock();
        let targets: Vec<&str> = seen.iter().map(|(t, _)| t.as_str()).collect();
        for expected in [
            "trawl_server::handlers",
            "trawl_server::ingest",
            "trawld",
            "fleet_auth::middleware",
            "auth.backend",
            "storage.backend",
            PREAUTH_TRANSPORT_TARGET,
            // A sub-target of trawl_server, so the contract string needs no
            // directive of its own for it to stay visible on stdout.
            UNMETERED_POLICY_TARGET,
        ] {
            assert!(
                targets.contains(&expected),
                "target {expected} must pass the default filter; saw {targets:?}"
            );
        }
        assert!(
            !targets.contains(&"hyper::proto"),
            "dependency INFO must NOT pass the default filter; saw {targets:?}"
        );
    }

    #[test]
    fn unmetered_targets_are_never_persisted() {
        assert!(!is_persisted_target("fleet_auth"));
        assert!(!is_persisted_target("fleet_auth::middleware"));
        assert!(!is_persisted_target("auth.backend"));
        // The accept loop's pre-TLS diagnostics: a bare TCP
        // connect-and-close is enough to emit one.
        assert!(!is_persisted_target(PREAUTH_TRANSPORT_TARGET));
        // The grant rejection: authenticated, but decided outside the rate
        // limiter, so a grantless key could otherwise write one record per
        // request forever.
        assert!(!is_persisted_target(UNMETERED_POLICY_TARGET));
        assert!(!is_persisted_target("trawl_server::policy::unmetered::x"));
        // The panic diagnostic is stdout-only; the caught request's failure
        // event is the persisted record.
        assert!(!is_persisted_target(PANIC_TARGET));
        // The unmetered failure target itself persists under its cap; its
        // descendants have no cap, so they stay excluded.
        assert!(is_persisted_target(UNMETERED_FAILURE_TARGET));
        assert!(!is_persisted_target(
            "trawl_server::transport::failure::unmetered::x"
        ));
        // Prefix matching is per segment, not per byte.
        assert!(is_persisted_target("fleet_authority"));
        assert!(is_persisted_target("fleet_auth_shim::x"));
        // Everything trawld emits behind the limiter keeps persisting —
        // including the rest of the policy module and the storage alarm
        // target.
        assert!(is_persisted_target("trawl_server::policy"));
        assert!(is_persisted_target("trawl_server::policy::other"));
        assert!(is_persisted_target("trawld"));
        assert!(is_persisted_target("storage.backend"));
    }

    /// The unmetered events are logged (previous test) but must never reach
    /// the WAL layer: fleet-auth's bearer shell and trawl's own grant check
    /// both run outside the rate limiter, so persisting them would let a
    /// client the limiter cannot slow grow the corpus one durable record per
    /// rejected request. An unmetered span lends no fields either. Drives
    /// the subscriber `trawld` installs.
    #[test]
    fn wal_layer_drops_unmetered_targets_the_stdout_filter_keeps() {
        let layer = WalLayer::new(WalHandle::new(), "prod");
        let (subscriber, _) = build_subscriber::<fn() -> std::io::Sink>(
            DEFAULT_LOG_FILTER,
            LogSinks {
                stdout: None,
                wal: Some(layer.clone()),
                file_log: false,
            },
        );
        let _guard = tracing::dispatcher::set_default(&subscriber);

        tracing::warn!(target: "fleet_auth::middleware", "auth: missing or malformed bearer header");
        tracing::warn!(target: "fleet_auth::middleware", "auth: invalid or revoked key");
        tracing::error!(target: "auth.backend", "auth: db error");
        tracing::warn!(target: PREAUTH_TRANSPORT_TARGET, event_type = "tls_handshake_failed", "TLS handshake failed");
        tracing::info!(target: UNMETERED_POLICY_TARGET, event_type = "auth_failure", "policy: no trawl grant (403)");
        tracing::info!(target: "trawl_server::policy", "policy: metered event");
        tracing::error!(target: "storage.backend", "app-state store error");
        tracing::info!(target: "trawld", "starting trawld");
        tracing::info!(target: "hyper::proto", "dependency noise");
        tracing::info_span!(target: "fleet_auth::middleware", "authn", key_hint = "unmetered")
            .in_scope(
                || tracing::info!(target: "trawl_server::handlers", "inside an unmetered span"),
            );

        let events = layer.inner.active.lock().events.clone();
        let targets: Vec<&str> = events
            .iter()
            .map(|event| event["target"].as_str().unwrap())
            .collect();
        for excluded in [
            "fleet_auth::middleware",
            "auth.backend",
            PREAUTH_TRANSPORT_TARGET,
            UNMETERED_POLICY_TARGET,
            "hyper::proto",
        ] {
            assert!(
                !targets.contains(&excluded),
                "target {excluded} must not be persisted; saw {targets:?}"
            );
        }
        for kept in [
            "trawl_server::policy",
            "storage.backend",
            "trawld",
            "trawl_server::handlers",
        ] {
            assert!(
                targets.contains(&kept),
                "target {kept} must still be persisted; saw {targets:?}"
            );
        }
        let inside = events
            .iter()
            .find(|event| event["target"] == "trawl_server::handlers")
            .unwrap();
        assert!(
            !inside.contains_key("key_hint"),
            "an unmetered span must lend no fields to a persisted event: {inside:?}"
        );
    }

    /// Everything a test subscriber writes to stdout.
    #[derive(Clone, Default)]
    struct StdoutCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for StdoutCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for StdoutCapture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl StdoutCapture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().clone()).unwrap()
        }
    }

    /// The window opens at its first event and admits exactly the cap, and
    /// concurrent callers share that one decision.
    #[test]
    fn the_unmetered_failure_cap_is_one_decision_across_threads() {
        let cap = UnmeteredFailureCap::default();
        let opened = Instant::now();
        let admitted = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        if cap.admit(opened) {
                            admitted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(
            admitted.load(Ordering::Relaxed),
            UNMETERED_FAILURE_CAP_PER_MINUTE as usize
        );
        let almost = UNMETERED_FAILURE_WINDOW.saturating_sub(Duration::from_millis(1));
        assert!(!cap.admit(opened + almost), "the window is still full");
        assert!(
            cap.admit(opened + UNMETERED_FAILURE_WINDOW),
            "a new window admits again"
        );
    }

    /// 61 unmetered failures in one window: the first 60 persist, the 61st
    /// reaches stdout only and is counted under `unmetered_cap`, on the
    /// metric and in the `telemetry_dropped` recovery record. Once the
    /// window rolls over, the next one persists again. A target beneath the
    /// capped one persists neither before nor after the cap fills, and never
    /// counts against it or under `unmetered_cap`.
    #[test]
    fn unmetered_5xx_persist_until_the_cap_then_count_as_dropped() {
        use crate::metrics::test_support::sample;
        const DROPPED: &str = "trawl_telemetry_events_dropped_total{reason=\"unmetered_cap\"}";

        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let metrics_handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
            writer.ensure_dir().unwrap();
            let handle = WalHandle::new();
            handle.set(writer, "prod");
            let layer = WalLayer::new(handle, "prod");
            let stdout = StdoutCapture::default();
            let (subscriber, _) = build_subscriber(
                DEFAULT_LOG_FILTER,
                LogSinks {
                    stdout: Some(stdout.clone()),
                    wal: Some(layer.clone()),
                    file_log: false,
                },
            );
            let _guard = tracing::dispatcher::set_default(&subscriber);
            let fail = |n: u32| {
                let request_id = format!("zz-cap-{n:03}");
                tracing::error!(
                    target: UNMETERED_FAILURE_TARGET,
                    event_type = "http_failure",
                    request_id = request_id.as_str(),
                    "request failed"
                );
            };
            let fail_child = |id: &str| {
                tracing::error!(
                    target: "trawl_server::transport::failure::unmetered::x",
                    event_type = "http_failure",
                    request_id = id,
                    "request failed"
                );
            };
            let persisted = || -> Vec<String> {
                layer.flush();
                read_wal_events(&tmp.path().join("prod"))
                    .into_iter()
                    .filter(|event| event["event_type"] == "http_failure")
                    .map(|event| event["request_id"].as_str().unwrap().to_owned())
                    .collect()
            };

            fail_child("zz-child-before");
            for n in 0..=UNMETERED_FAILURE_CAP_PER_MINUTE {
                fail(n);
            }
            fail_child("zz-child-after");
            let ids = persisted();
            let expected: Vec<String> = (0..UNMETERED_FAILURE_CAP_PER_MINUTE)
                .map(|n| format!("zz-cap-{n:03}"))
                .collect();
            assert_eq!(
                ids, expected,
                "exactly the first 60 persist, and no child-target event"
            );
            assert!(
                stdout.text().contains("zz-cap-060"),
                "the 61st still reaches stdout"
            );
            assert_eq!(sample(&metrics_handle, DROPPED), 1);

            // The recovery record is buffered by the write that published
            // the batch, and reaches the WAL with the next one.
            layer.flush();
            let reports: Vec<_> = read_wal_events(&tmp.path().join("prod"))
                .into_iter()
                .filter(|event| event["event_type"] == "telemetry_dropped")
                .collect();
            assert_eq!(reports.len(), 1, "{reports:?}");
            assert_eq!(reports[0]["dropped_events"], 1);
            assert_eq!(reports[0]["dropped_events_unmetered_cap"], 1);

            *layer.inner.clock_offset.lock() += UNMETERED_FAILURE_WINDOW;
            fail(61);
            assert!(
                persisted().contains(&"zz-cap-061".to_owned()),
                "a new window admits again"
            );
            assert_eq!(sample(&metrics_handle, DROPPED), 1);
        });
    }

    /// A client forcing one unmetered failure per flush tick after the cap
    /// fills must not buy one durable row per tick through the recovery
    /// record: each record's write would publish while a new cap drop is
    /// pending and buffer the next record, forever. Within one cap window the
    /// unmetered failures and their drop records persist at most the cap plus
    /// one row, and the drops held back are reported, exactly, on the first
    /// record the next window permits.
    #[test]
    fn cap_drops_cannot_drive_an_unbounded_run_of_drop_records() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();
        let handle = WalHandle::new();
        handle.set(writer, "prod");
        let layer = WalLayer::new(handle, "prod");
        let (subscriber, _) = build_subscriber::<fn() -> std::io::Sink>(
            DEFAULT_LOG_FILTER,
            LogSinks {
                stdout: None,
                wal: Some(layer.clone()),
                file_log: false,
            },
        );
        let _guard = tracing::dispatcher::set_default(&subscriber);
        let fail = |n: u32| {
            tracing::error!(
                target: UNMETERED_FAILURE_TARGET,
                event_type = "http_failure",
                n,
                "request failed"
            );
        };
        let cap = UNMETERED_FAILURE_CAP_PER_MINUTE;
        let overflow = 2 * cap;

        // Fill the cap, then one overflow while an ordinary persisted event
        // kicks the recovery record off, then one overflow per flush tick.
        for n in 0..cap {
            fail(n);
        }
        layer.flush();
        fail(cap);
        tracing::info!(target: "trawld", "one ordinary event");
        layer.flush();
        for n in cap + 1..cap + overflow {
            fail(n);
            layer.flush();
        }
        layer.flush();

        let events = read_wal_events(&tmp.path().join("prod"));
        let count = |event_type: &str| {
            events
                .iter()
                .filter(|event| event["event_type"] == event_type)
                .count()
        };
        assert_eq!(count("http_failure"), cap as usize);
        assert!(
            count("http_failure") + count("telemetry_dropped") <= cap as usize + 1,
            "one cap window persisted {} unmetered failures and {} drop records",
            count("http_failure"),
            count("telemetry_dropped")
        );

        // The next window permits a record again: the first write in it
        // reports every drop held back, and nothing is lost or counted twice.
        *layer.inner.clock_offset.lock() += UNMETERED_FAILURE_WINDOW;
        tracing::info!(target: "trawld", "an ordinary event in the next window");
        layer.flush();
        layer.flush();
        let reported: u64 = read_wal_events(&tmp.path().join("prod"))
            .iter()
            .filter(|event| event["event_type"] == "telemetry_dropped")
            .map(|event| event["dropped_events_unmetered_cap"].as_u64().unwrap())
            .sum();
        assert_eq!(reported, u64::from(overflow));
    }

    #[test]
    fn resolve_log_filter_unset_uses_default() {
        let resolved = resolve_log_filter(None);
        assert_eq!(resolved.directives, DEFAULT_LOG_FILTER);
        assert!(resolved.warning.is_none());
    }

    #[test]
    fn resolve_log_filter_valid_value_is_authoritative() {
        let resolved = resolve_log_filter(Some("trawl_server=debug,hyper=warn"));
        assert_eq!(resolved.directives, "trawl_server=debug,hyper=warn");
        assert!(resolved.warning.is_none());
    }

    #[test]
    fn resolve_log_filter_invalid_value_falls_back_with_one_warning() {
        let raw = "sup3r_s3cret_password=notalevel";
        let resolved = resolve_log_filter(Some(raw));
        assert_eq!(resolved.directives, DEFAULT_LOG_FILTER);
        let warning = resolved.warning.expect("invalid RUST_LOG must warn");
        // The raw env value must never be logged — it can contain anything.
        assert!(
            !warning.contains(raw) && !warning.contains("sup3r_s3cret_password"),
            "warning must not leak the raw env value: {warning}"
        );
        assert!(
            warning.contains("RUST_LOG"),
            "warning names the env var: {warning}"
        );
    }

    #[test]
    fn message_to_event_type_simple() {
        assert_eq!(message_to_event_type("query complete"), "query_complete");
    }

    #[test]
    fn message_to_event_type_with_colon() {
        assert_eq!(
            message_to_event_type("query failed: bad request"),
            "query_failed"
        );
    }

    #[test]
    fn message_to_event_type_auth() {
        assert_eq!(
            message_to_event_type("auth failed: missing or malformed Authorization header"),
            "auth_failed"
        );
    }

    #[test]
    fn message_to_event_type_empty() {
        assert_eq!(message_to_event_type(""), "");
    }

    #[test]
    fn message_to_event_type_special_chars() {
        assert_eq!(
            message_to_event_type("TLS handshake failed"),
            "tls_handshake_failed"
        );
    }

    #[test]
    fn wal_handle_set_and_get() {
        let handle = WalHandle::new();
        assert!(handle.get().is_none());

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        handle.set(Arc::clone(&writer), "prod");

        assert!(handle.get().is_some());
    }

    #[test]
    fn wal_layer_buffers_before_writer_set() {
        use tracing_subscriber::prelude::*;

        let handle = WalHandle::new();
        let layer = WalLayer::new(handle.clone(), "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Emit event before writer is available — should be buffered.
        tracing::info!(event_type = "bootstrap", "pre-init event");
        assert!(!layer_ref.inner.active.lock().bytes.is_empty());

        // Flush without writer — buffer should be retained (not drained).
        layer_ref.flush();
        assert!(!layer_ref.inner.active.lock().bytes.is_empty());

        // Now inject the writer and flush — buffer should drain.
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();
        handle.set(Arc::clone(&writer), "prod");

        layer_ref.flush();
        assert!(layer_ref.inner.active.lock().bytes.is_empty());

        // Verify the bootstrap event reached the WAL.
        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert_eq!(files.len(), 1);
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["event_type"], "bootstrap");
    }

    #[test]
    fn json_visitor_collects_fields() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(
            event_type = "test_fields",
            user = "admin",
            rows = 42u64,
            timed_out = false,
            duration = 1.5f64,
            "fields test"
        );

        layer_ref.flush();

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(parsed["user"], "admin");
        assert_eq!(parsed["rows"], 42);
        assert_eq!(parsed["timed_out"], false);
        assert_eq!(parsed["duration"], 1.5);
    }

    #[test]
    fn wal_layer_buffers_events() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(
            event_type = "test_event",
            user = "alice",
            rows = 42u64,
            "test complete"
        );

        assert!(!layer_ref.inner.active.lock().bytes.is_empty());

        layer_ref.flush();
        assert!(layer_ref.inner.active.lock().bytes.is_empty());

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert_eq!(files.len(), 1, "expected exactly one WAL file");

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(parsed["service"], "trawld");
        assert_eq!(parsed["event_type"], "test_event");
        assert_eq!(parsed["user"], "alice");
        assert_eq!(parsed["rows"], 42);
        assert_eq!(parsed["message"], "test complete");
        assert!(parsed["_time"].is_string());
        assert!(parsed["_ingested"].is_string());
        assert!(parsed["_raw"].is_string());
        assert_eq!(parsed["env"], "prod");
        assert_eq!(
            parsed[trawl_core::schema::SEVERITY],
            9,
            "tracing info maps to OTel 9"
        );
        assert_eq!(
            parsed["level"], "info",
            "the level WORD stays as ordinary sender vocabulary"
        );
        assert!(parsed["target"].is_string());
        assert_eq!(
            parsed["_producer"], "trawld",
            "provenance is data on this door too (ruling 6)"
        );
        assert_eq!(
            parsed["_time"], parsed["_ingested"],
            "one instant serves both: the proposal and the arrival"
        );
        assert!(
            parsed.get("_repairs").is_none(),
            "trawld's own events must stay repair-free: {parsed}"
        );
    }

    // --- the trawld profile at the door (ADR-0013) ---------------------

    /// A layer wired exactly as production wires it, plus the WAL it
    /// writes to. Split from [`records_from`] so a test can hold the
    /// layer (to flush twice, to inspect the buffer).
    fn telemetry_layer(
        derivation: Arc<crate::ingest::producer::Derivation>,
    ) -> (WalLayer, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();
        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");
        let layer = WalLayer::new_with_buffer_cap(
            handle,
            &["prod".to_owned()],
            "prod",
            derivation,
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
        );
        (layer, tmp)
    }

    /// Emit through a real subscriber and read back every record the WAL
    /// received, in order.
    fn records_from(
        derivation: Arc<crate::ingest::producer::Derivation>,
        emit: impl FnOnce(),
    ) -> Vec<serde_json::Value> {
        use tracing_subscriber::prelude::*;

        let (layer, tmp) = telemetry_layer(derivation);
        let layer_ref = layer.clone();
        {
            let subscriber = tracing_subscriber::registry().with(layer);
            let _guard = tracing::subscriber::set_default(subscriber);
            emit();
        }
        layer_ref.flush();

        let mut out = Vec::new();
        for entry in std::fs::read_dir(tmp.path().join("prod")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "ndjson") {
                continue;
            }
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                out.push(serde_json::from_str(line).unwrap());
            }
        }
        out
    }

    /// One record, for the single-event tests.
    fn record_from(
        derivation: Arc<crate::ingest::producer::Derivation>,
        emit: impl FnOnce(),
    ) -> serde_json::Value {
        let mut records = records_from(derivation, emit);
        assert_eq!(records.len(), 1, "expected exactly one record: {records:?}");
        records.pop().unwrap()
    }

    fn packaged() -> Arc<crate::ingest::producer::Derivation> {
        Arc::new(crate::ingest::producer::Derivation::defaults())
    }

    fn repair_codes(record: &serde_json::Value) -> Vec<&str> {
        record["_repairs"]
            .as_str()
            .map(|s| s.split(',').collect())
            .unwrap_or_default()
    }

    /// The compaction-wedge case for telemetry's own field names.
    ///
    /// Compaction pins every dynamic column in postgres before writing the
    /// parquet that carries it, and a name over `MAX_FIELD_NAME_BYTES`
    /// overflows the btree key behind `field_types.field`: the insert errors,
    /// the batch is retained, and the same WAL re-fails every tick, forever,
    /// for `service=trawld` — the service an operator most needs during an
    /// incident. The door drops the field and keeps the event.
    #[test]
    fn an_over_long_tracing_field_name_is_dropped_and_the_event_still_lands() {
        let record = packaged_record_with_long_name();
        assert_eq!(record["event_type"], "wedge_probe");
        assert_eq!(record["message"], "the event must still land");
        assert_eq!(record["survivor"], 1);
        assert!(
            record
                .as_object()
                .unwrap()
                .keys()
                .all(|k| k.len() <= trawl_core::schema::MAX_FIELD_NAME_BYTES),
            "no name over the catalog bound may become a column: {record}"
        );
        assert!(repair_codes(&record).contains(&"field.name_too_long"));
        assert!(
            record["_raw"].as_str().unwrap().contains("wedge_wedge_"),
            "the dropped name and its value stay findable in _raw"
        );
    }

    /// Split out only because the 264-byte field name has to be a literal
    /// token — tracing field names are resolved at compile time.
    fn packaged_record_with_long_name() -> serde_json::Value {
        record_from(packaged(), || {
            tracing::info!(
                event_type = "wedge_probe",
                survivor = 1u64,
                "wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_wedge_" =
                    42u64,
                "the event must still land"
            );
        })
    }

    /// Identity is protected by precedence, not by a namespace.
    ///
    /// Telemetry is an ordinary sender — no `trawld_` prefix — so a span or
    /// event field literally named `service`/`host`/`env` is application
    /// vocabulary that happens to collide with a slot the profile asserts.
    /// It loses, with a repair code, and the displaced value stays findable
    /// in `_raw`.
    #[test]
    fn a_tracing_field_cannot_impersonate_another_service() {
        let record = record_from(packaged(), || {
            let span = tracing::info_span!("proxying", service = "nginx", host = "web01");
            let _entered = span.enter();
            tracing::info!(event_type = "upstream_5xx", env = "lab", "boom");
        });
        assert_eq!(
            record["service"], "trawld",
            "the profile owns the identity slot"
        );
        assert_eq!(record["env"], "prod");
        assert_ne!(record["host"], "web01");
        assert!(repair_codes(&record).contains(&"field.producer_asserted"));
        let raw = record["_raw"].as_str().unwrap();
        for displaced in ["nginx", "web01", "lab"] {
            assert!(
                raw.contains(displaced),
                "{displaced} must stay in _raw: {raw}"
            );
        }
        // The event itself is untouched otherwise.
        assert_eq!(record["event_type"], "upstream_5xx");
        assert_eq!(record["message"], "boom");
    }

    /// The tracing level rides the ordinary `severity_from` chain: nothing
    /// on this path writes `_severity` directly.
    #[test]
    fn the_tracing_level_derives_severity_through_the_configured_chain() {
        for (emit, level, expected) in [
            (0u8, "warn", 13),
            (1, "error", 17),
            (2, "debug", 5),
            (3, "trace", 1),
        ] {
            let record = record_from(packaged(), move || match emit {
                0 => tracing::warn!(event_type = "sev", "x"),
                1 => tracing::error!(event_type = "sev", "x"),
                2 => tracing::debug!(event_type = "sev", "x"),
                _ => tracing::trace!(event_type = "sev", "x"),
            });
            assert_eq!(
                record[trawl_core::schema::SEVERITY],
                expected,
                "tracing {level} must derive to OTel {expected}"
            );
            assert_eq!(
                record["level"], level,
                "the level WORD stays an ordinary column"
            );
        }
    }

    /// `severity_from = []` is legal and means "derive nothing". Telemetry
    /// obeys it like every other door — the level column stays,
    /// `_severity` simply is not there.
    #[test]
    fn an_empty_severity_chain_leaves_trawlds_own_events_unscored() {
        let derivation = Arc::new(
            crate::ingest::producer::Derivation::resolve(&trawl_config::IngestConfig {
                severity_from: Vec::new(),
                ..trawl_config::IngestConfig::default()
            })
            .expect("an empty severity list is legal"),
        );
        let record = record_from(derivation, || {
            tracing::warn!(event_type = "unscored", "x");
        });
        assert!(
            record.get(trawl_core::schema::SEVERITY).is_none(),
            "nothing may write _severity outside derivation: {record}"
        );
        assert_eq!(record["level"], "warn", "the source column is untouched");
    }

    /// A tracing field named `_raw` is ordinary application vocabulary,
    /// not the lifeline: on this door `_raw` is not proposable, so the
    /// field takes the reserved-prefix strip and the door writes the
    /// pre-repair serialization. Otherwise a single mis-named field would
    /// shadow the very thing that carries displaced collision values.
    #[test]
    fn a_raw_named_tracing_field_cannot_shadow_the_lifeline() {
        let record = record_from(packaged(), || {
            let span = tracing::info_span!("collide", service = "nginx");
            let _entered = span.enter();
            tracing::info!(event_type = "raw_probe", _raw = "just a field", "x");
        });
        assert_eq!(record["raw"], "just a field", "it lands bare, value intact");
        let raw = record["_raw"].as_str().unwrap();
        assert!(
            raw.contains("\"service\":\"nginx\""),
            "_raw stays the serialization that carries the displaced value: {raw}"
        );
        for code in ["field.reserved_prefix", "field.producer_asserted"] {
            assert!(repair_codes(&record).contains(&code), "missing {code}");
        }
    }

    /// Telemetry is rejection-free by construction, and the invariant
    /// counter is the evidence.
    ///
    /// Two halves. First, a bounded deterministic sweep over payloads a
    /// tracing visitor could plausibly produce — reserved names, empty
    /// and over-long names, mixed case, non-UTF-safe-looking text, every
    /// JSON scalar, and every identity slot the profile asserts — driven
    /// through the very call `on_event` makes. Second, the closed
    /// `{profile="trawld"}` reject matrix, published at zero and asserted
    /// still at zero: an absent increment on a present series is what
    /// "never happened" looks like, and an absent series would be
    /// indistinguishable from "never wired up".
    #[test]
    fn the_trawld_profile_never_rejects_and_the_counter_proves_it() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::ingest::producer::init_profile_reject_metrics();
            sweep_trawld_payloads();
        });

        let rendered = handle.render();
        for reason in crate::ingest::envelope::RejectReason::ALL {
            let series = format!(
                "{}{{profile=\"trawld\",reason=\"{}\"}} 0",
                crate::metrics::INGEST_PROFILE_REJECT_TOTAL,
                reason.as_str()
            );
            assert!(
                rendered.contains(&series),
                "the trawld profile rejected on {}, or the series is missing: {rendered}",
                reason.as_str()
            );
        }
    }

    /// The generative half of the test above: 600 bounded, seeded
    /// payloads through the very call `on_event` makes. A refusal is
    /// counted rather than panicked, so the caller's counter assertion is
    /// what fails — the invariant is about the metric, not about a
    /// backtrace.
    fn sweep_trawld_payloads() {
        use crate::ingest::envelope::{EnvelopeContext, canonicalize};
        use crate::ingest::producer::{
            Asserted, Derivation, Producer, ProducerKind, count_profile_reject,
        };

        // Names a tracing field could carry, including every one that is
        // load-bearing at the door.
        let names: Vec<String> = [
            "level",
            "target",
            "event_type",
            "message",
            "service",
            "env",
            "host",
            "_raw",
            "_time",
            "_ingested",
            "_repairs",
            "_severity",
            "_producer",
            "_",
            "___",
            "MiXeD",
            "myField",
            "myfield",
            "café",
            "sd_x@1_k",
            "",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .chain(std::iter::once("k".repeat(300)))
        .collect();
        let values = |seed: u64| -> serde_json::Value {
            match seed % 9 {
                0 => json!(""),
                1 => json!("x".repeat(300)),
                2 => json!("nul\u{0}and\ttab"),
                3 => json!(-1i64),
                4 => json!(u64::MAX),
                5 => json!(1.5f64),
                6 => json!(true),
                7 => serde_json::Value::Null,
                _ => json!({ "nested": [1, 2, 3] }),
            }
        };

        let derivation = Derivation::defaults();
        let envs = vec!["prod".to_owned()];
        let arrival_instant = chrono::Utc::now();
        let arrival = arrival_instant.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);

        // A cheap deterministic walk over four-field payloads. Bounded
        // and seeded — no proptest dependency, no flake, and a failing
        // case is reproducible from the round index.
        let mut lcg: u64 = 0x2545_F491_4F6C_DD1D;
        for round in 0..600u64 {
            let mut payload = serde_json::Map::new();
            for _ in 0..4 {
                lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let name = &names[(lcg >> 33) as usize % names.len()];
                payload.insert(name.clone(), values(lcg >> 11));
            }
            let ctx = EnvelopeContext {
                arrival: &arrival,
                arrival_instant,
                envs: &envs,
                default_env: "prod",
                producer: Producer::Trawld(Asserted {
                    env: "prod",
                    service: TELEMETRY_SERVICE,
                    // Alternate the two host shapes: a resolved hostname
                    // and a failed lookup.
                    host: (round % 2 == 0).then_some("box"),
                    message: None,
                    repairs: &[],
                }),
                derivation: &derivation,
            };
            match canonicalize(&payload, &ctx) {
                Ok(canonical) => {
                    assert_eq!(canonical.service, TELEMETRY_SERVICE);
                    assert_eq!(canonical.env, "prod");
                    assert_eq!(canonical.obj["_producer"], "trawld");
                    assert!(
                        canonical
                            .obj
                            .keys()
                            .all(|k| k.len() <= trawl_core::schema::MAX_FIELD_NAME_BYTES),
                        "round {round}: an unstorable name became a column"
                    );
                }
                Err((_, reason)) => count_profile_reject(ProducerKind::Trawld, reason),
            }
        }
    }

    /// Tracing field names are Rust-side identifiers and can be mixed
    /// case (`tracing::info!(myField = 1)` is legal), so an unfolded name
    /// would become a column spelling the folded catalog pin never
    /// matches. The door's universal fold covers this path too.
    #[test]
    fn mixed_case_tracing_field_names_are_ascii_folded() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        #[allow(non_snake_case)]
        {
            tracing::info!(event_type = "fold_test", myField = 42u64, "folded");
        }

        layer_ref.flush();

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(
            parsed["myfield"], 42,
            "the field lands under the folded name"
        );
        assert!(
            parsed.get("myField").is_none(),
            "the unfolded spelling must not exist: {parsed}"
        );
    }

    #[test]
    fn wal_layer_inherits_span_fields() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!(
            "request",
            request_id = "01HZEXAMPLE000000000000000",
            peer_addr = "1.2.3.4"
        );
        let _enter = span.enter();

        tracing::info!(
            event_type = "query_complete",
            rows = 10u64,
            "query complete"
        );

        layer_ref.flush();

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        // Span fields should be inherited.
        assert_eq!(parsed["request_id"], "01HZEXAMPLE000000000000000");
        assert_eq!(parsed["peer_addr"], "1.2.3.4");
        // Event fields should also be present.
        assert_eq!(parsed["rows"], 10);
        assert_eq!(parsed["event_type"], "query_complete");
    }

    #[test]
    fn event_type_explicit_overrides_message() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Explicit event_type should win over message-derived.
        tracing::info!(event_type = "custom_type", "some random message");

        layer_ref.flush();

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(parsed["event_type"], "custom_type");
        assert_eq!(parsed["message"], "some random message");
    }

    // -- bounded retry queue -----------------------------------------------

    /// A WAL root that is a file makes every write fail (`create_dir_all`
    /// of `wal_root/{env}` errors), simulating a broken volume that can be
    /// repaired by replacing the file with a directory.
    fn broken_wal_root(tmp: &std::path::Path) -> PathBuf {
        let root = tmp.join("wal");
        std::fs::write(&root, b"not a directory").unwrap();
        root
    }

    fn repair_wal_root(root: &std::path::Path) {
        std::fs::remove_file(root).unwrap();
        std::fs::create_dir_all(root).unwrap();
    }

    fn read_wal_events(env_dir: &std::path::Path) -> Vec<serde_json::Value> {
        let mut files: Vec<_> = std::fs::read_dir(env_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        // WAL filenames embed unix millis but same-millisecond writes tie;
        // mtime has nanosecond resolution and each write fsyncs, so it
        // reflects write order.
        files.sort_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap());
        files
            .iter()
            .flat_map(|p| {
                std::fs::read_to_string(p)
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    use std::path::PathBuf;

    async fn assert_flush_holds_publication_until_insert(synchronous: bool) {
        use tracing_subscriber::prelude::*;

        let root = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(root.path().join("wal")));
        let handle = WalHandle::new();
        handle.set(writer.clone(), "prod");
        let layer = WalLayer::new(handle, "prod");
        let hot = Arc::new(crate::hot_buffer::HotBuffer::new(
            crate::hot_buffer::HotBufferConfig {
                max_events: 100,
                max_bytes: 100_000,
            },
        ));
        layer.set_hot_buffer(hot.clone());
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                event_type = "publication_test",
                "durable before hot insertion"
            );
        });
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *layer.inner.pause_before_insert.lock() = Some((entered_tx, release_rx));
        let flushing = layer.clone();
        let task = tokio::spawn(async move {
            if synchronous {
                tokio::task::spawn_blocking(move || flushing.flush())
                    .await
                    .unwrap();
            } else {
                flushing.flush_cycle().await;
            }
        });
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        })
        .await
        .unwrap();
        let events = read_wal_events(&writer.dir().join("prod"));
        assert_eq!(events.len(), 1, "the paused telemetry event is durable");
        assert_eq!(events[0]["event_type"], "publication_test");
        assert_eq!(hot.event_count(), 0);
        let publication = hot.publication();
        let reader = tokio::time::timeout(Duration::from_secs(1), publication.read())
            .await
            .expect("ingestion must permit concurrent query readers")
            .unwrap();
        drop(reader);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let mut publishing = Box::pin(publication.write());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), publishing.as_mut())
                .await
                .is_err(),
            "the blocking task retains the guard after cancellation"
        );
        release_tx.send(()).unwrap();
        let _publication = tokio::time::timeout(Duration::from_secs(5), publishing)
            .await
            .expect("insertion must complete before the queued writer enters");
        assert_eq!(hot.event_count(), 1);
        assert_eq!(layer.inner.staged.events.load(Ordering::Relaxed), 0);
        assert_eq!(layer.inner.staged.bytes.load(Ordering::Relaxed), 0);
        let wal_path = std::fs::read_dir(writer.dir().join("prod"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let batch_id = format!("prod/{}", wal_path.file_stem().unwrap().to_str().unwrap());
        hot.drain(&[&batch_id]);
        assert_eq!(
            hot.event_count(),
            0,
            "the batch cannot arrive after its drain"
        );
    }

    #[tokio::test]
    async fn cancelled_async_flush_keeps_wal_and_hot_insert_together() {
        assert_flush_holds_publication_until_insert(false).await;
    }

    #[tokio::test]
    async fn synchronous_flush_keeps_wal_and_hot_insert_together() {
        assert_flush_holds_publication_until_insert(true).await;
    }

    #[tokio::test]
    async fn wal_failure_retains_batch_then_publishes_exactly_once_after_retry() {
        use crate::bus::{EventBus, EventSubscriber, LocalEventBus};
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let wal_root = broken_wal_root(tmp.path());
        let writer = Arc::new(WalWriter::new(wal_root.clone()));

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let bus = Arc::new(LocalEventBus::new(16));
        let mut sub = bus.subscribe();
        let hot = Arc::new(crate::hot_buffer::HotBuffer::new(
            crate::hot_buffer::HotBufferConfig {
                max_events: 1000,
                max_bytes: 1024 * 1024,
            },
        ));

        let layer = WalLayer::new(handle, "prod");
        layer.set_bus(Arc::clone(&bus));
        layer.set_hot_buffer(Arc::clone(&hot));
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(event_type = "retry_test", "event before outage");

        // Write fails: the batch must be retained, and nothing published.
        layer_ref.flush_cycle().await;
        assert_eq!(
            hot.event_count(),
            0,
            "no hot-buffer insert before durability"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub.recv())
                .await
                .is_err(),
            "no bus publish before durability"
        );
        assert_eq!(
            layer_ref.inner.pending.lock().len(),
            1,
            "failed batch retained for retry"
        );

        // Repair the volume, retry without emitting new events.
        repair_wal_root(&wal_root);
        layer_ref.flush_cycle().await;

        // Durable now: published exactly once.
        let batch = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("timed out waiting for batch")
            .expect("recv failed");
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["event_type"], "retry_test");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub.recv())
                .await
                .is_err(),
            "batch published exactly once"
        );
        assert_eq!(
            hot.event_count(),
            1,
            "hot buffer got the batch exactly once"
        );
        assert!(layer_ref.inner.pending.lock().is_empty());

        let events = read_wal_events(&wal_root.join("prod"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "retry_test");
    }

    #[test]
    fn alert_counters_keep_retained_retry_distinct_from_discard() {
        use crate::metrics::test_support::sample;
        use tracing_subscriber::prelude::*;

        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let metrics_handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                crate::metrics::init_operational_alert_metrics();
                let tmp = tempfile::tempdir().unwrap();
                let wal_root = broken_wal_root(tmp.path());
                let writer = Arc::new(WalWriter::new(wal_root.clone()));
                let handle = WalHandle::new();
                let layer = WalLayer::new(handle.clone(), "prod");
                // Disabled and idle flushes must not report a failed attempt.
                layer.flush_cycle().await;
                assert_eq!(
                    sample(
                        &metrics_handle,
                        crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                    ),
                    0
                );
                handle.set(Arc::clone(&writer), "prod");
                layer.flush_cycle().await;
                assert_eq!(
                    sample(
                        &metrics_handle,
                        crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                    ),
                    0
                );
                let hot = Arc::new(crate::hot_buffer::HotBuffer::new(
                    crate::hot_buffer::HotBufferConfig {
                        max_events: 100,
                        max_bytes: 1024 * 1024,
                    },
                ));
                layer.set_hot_buffer(Arc::clone(&hot));
                let _guard = tracing::subscriber::set_default(
                    tracing_subscriber::registry().with(layer.clone()),
                );
                tracing::info!(event_type = "alert_retry", id = 1, "first");
                tracing::info!(event_type = "alert_retry", id = 2, "second");
                layer.flush_cycle().await;
                assert_eq!(
                    sample(
                        &metrics_handle,
                        crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                    ),
                    1
                );
                assert_eq!(layer.inner.pending.lock().front().unwrap().events.len(), 2);
                assert_eq!(hot.event_count(), 0);
                for reason in ["preinit_cap", "buffer_cap", "write_crashed"] {
                    assert_eq!(
                        sample(
                            &metrics_handle,
                            &format!("trawl_telemetry_events_dropped_total{{reason=\"{reason}\"}}")
                        ),
                        0
                    );
                }
                repair_wal_root(&wal_root);
                layer.flush_cycle().await;
                assert_eq!(
                    sample(
                        &metrics_handle,
                        crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                    ),
                    1
                );
                assert!(layer.inner.pending.lock().is_empty());
                assert_eq!(hot.event_count(), 2);
                let events = read_wal_events(&wal_root.join("prod"));
                assert_eq!(events.len(), 2);
                assert!(
                    events
                        .iter()
                        .all(|event| event["event_type"] == "alert_retry")
                );
                layer.flush_cycle().await;
                assert_eq!(read_wal_events(&wal_root.join("prod")).len(), 2);
                assert_eq!(
                    sample(
                        &metrics_handle,
                        crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                    ),
                    1
                );
            });
        });
    }

    #[test]
    fn crashed_write_after_durability_counts_uncertain_batch_and_failed_attempt() {
        use crate::metrics::test_support::sample;
        use tracing_subscriber::prelude::*;

        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let metrics_handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                crate::metrics::init_operational_alert_metrics();
                let tmp = tempfile::tempdir().unwrap();
                let writer = Arc::new(WalWriter::new(tmp.path().join("wal")));
                let handle = WalHandle::new();
                handle.set(Arc::clone(&writer), "prod");
                let layer = WalLayer::new(handle, "prod");
                let hot = Arc::new(crate::hot_buffer::HotBuffer::new(
                    crate::hot_buffer::HotBufferConfig {
                        max_events: 100,
                        max_bytes: 1024 * 1024,
                    },
                ));
                layer.set_hot_buffer(Arc::clone(&hot));
                let _guard = tracing::subscriber::set_default(
                    tracing_subscriber::registry().with(layer.clone()),
                );
                tracing::info!(event_type = "durable_crash", id = 1, "first");
                tracing::info!(event_type = "durable_crash", id = 2, "second");
                writer.panic_after_writes_for_test(1);
                layer.flush_cycle().await;
                assert_eq!(
                    sample(
                        &metrics_handle,
                        crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                    ),
                    1
                );
                assert_eq!(
                    sample(
                        &metrics_handle,
                        "trawl_telemetry_events_dropped_total{reason=\"write_crashed\"}"
                    ),
                    2
                );
                assert_eq!(
                    sample(
                        &metrics_handle,
                        "trawl_telemetry_events_dropped_total{reason=\"preinit_cap\"}"
                    ),
                    0
                );
                assert_eq!(
                    sample(
                        &metrics_handle,
                        "trawl_telemetry_events_dropped_total{reason=\"buffer_cap\"}"
                    ),
                    0
                );
                assert_eq!(hot.event_count(), 0, "panic precedes hot publication");
                assert!(layer.inner.pending.lock().is_empty());
                assert_eq!(layer.inner.staged.events.load(Ordering::Relaxed), 0);
                let events = read_wal_events(&writer.dir().join("prod"));
                assert_eq!(events.len(), 2, "the consumed batch still exists durably");
                assert!(
                    events
                        .iter()
                        .all(|event| event["event_type"] == "durable_crash")
                );
                layer.flush_cycle().await;
                assert_eq!(
                    sample(
                        &metrics_handle,
                        crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                    ),
                    1
                );
                assert_eq!(
                    sample(
                        &metrics_handle,
                        "trawl_telemetry_events_dropped_total{reason=\"write_crashed\"}"
                    ),
                    2
                );
                assert_eq!(read_wal_events(&writer.dir().join("prod")).len(), 2);
            });
        });
    }

    #[test]
    fn capacity_alert_reasons_count_real_admission_drops_without_write_failures() {
        use crate::metrics::test_support::sample;
        use tracing_subscriber::prelude::*;

        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let metrics_handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let layer = WalLayer::new(WalHandle::new(), "prod");
            {
                let _guard = tracing::subscriber::set_default(
                    tracing_subscriber::registry().with(layer.clone()),
                );
                let filler = "x".repeat(16 * 1024);
                for _ in 0..128 {
                    if layer.inner.active.lock().bytes.len() >= 1024 * 1024 {
                        break;
                    }
                    tracing::info!(event_type = "fill_preinit", payload = %filler, "fill");
                }
                assert!(layer.inner.active.lock().bytes.len() >= 1024 * 1024);
                for _ in 0..3 {
                    tracing::info!(event_type = "preinit_discard", "discard");
                }
            }
            assert_eq!(
                sample(
                    &metrics_handle,
                    "trawl_telemetry_events_dropped_total{reason=\"preinit_cap\"}"
                ),
                3
            );
            let tmp = tempfile::tempdir().unwrap();
            let handle = WalHandle::new();
            handle.set(Arc::new(WalWriter::new(tmp.path().join("wal"))), "prod");
            let capped = WalLayer::new(handle, "prod");
            capped.inner.max_buffer_bytes.store(1, Ordering::Relaxed);
            {
                let _guard = tracing::subscriber::set_default(
                    tracing_subscriber::registry().with(capped.clone()),
                );
                for _ in 0..2 {
                    tracing::info!(event_type = "buffer_discard", "too large for cap");
                }
            }
            assert!(capped.inner.active.lock().events.is_empty());
            assert_eq!(
                sample(
                    &metrics_handle,
                    "trawl_telemetry_events_dropped_total{reason=\"buffer_cap\"}"
                ),
                2
            );
            assert_eq!(
                sample(
                    &metrics_handle,
                    "trawl_telemetry_events_dropped_total{reason=\"write_crashed\"}"
                ),
                0
            );
            assert_eq!(
                sample(
                    &metrics_handle,
                    crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                ),
                0
            );
        });
    }

    /// A `JoinError` owns no batch to put back: prove the consumed unit is
    /// accounted exactly once as dropped while the existing write-failure
    /// signal remains exactly once and all depth accounting is released.
    #[test]
    fn panicked_blocking_write_accounts_the_lost_batch_exactly_once() {
        use crate::metrics::test_support::sample;
        use tracing_subscriber::prelude::*;

        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let metrics_handle = recorder.handle();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // The local recorder is thread-local, so keep the future on this
        // thread. The panicking blocking task emits no metrics itself; its
        // JoinError is accounted here after the await.
        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                crate::metrics::init_operational_alert_metrics();
                // The byte counter is diagnostic rather than selected by the
                // starter rules. Register its test baseline explicitly too.
                metrics::counter!(crate::metrics::TELEMETRY_BYTES_DROPPED_TOTAL,
                    "reason" => crate::metrics::TelemetryDropReason::WriteCrashed.label())
                .increment(0);
                let tmp = tempfile::tempdir().unwrap();
                let writer = Arc::new(WalWriter::new(tmp.path().join("wal")));
                writer.ensure_dir().unwrap();

                let handle = WalHandle::new();
                handle.set(Arc::clone(&writer), "prod");

                let layer = WalLayer::new(handle, "prod");
                let layer_ref = layer.clone();
                let subscriber = tracing_subscriber::registry().with(layer);
                let _guard = tracing::subscriber::set_default(subscriber);

                layer_ref.inner.update_gauges();
                let baseline_events =
                    sample(&metrics_handle, crate::metrics::TELEMETRY_BUFFER_EVENTS);
                let baseline_bytes =
                    sample(&metrics_handle, crate::metrics::TELEMETRY_BUFFER_BYTES);
                let crashed_events_before = sample(
                    &metrics_handle,
                    "trawl_telemetry_events_dropped_total{reason=\"write_crashed\"}",
                );
                let crashed_bytes_before = sample(
                    &metrics_handle,
                    "trawl_telemetry_bytes_dropped_total{reason=\"write_crashed\"}",
                );
                let failures_before = sample(
                    &metrics_handle,
                    crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL,
                );

                tracing::info!(event_type = "write_crash_test", "lost in blocking task");
                layer_ref.inner.stage();
                let (expected_events, expected_bytes) = {
                    let pending = layer_ref.inner.pending.lock();
                    let batch = pending.front().expect("event was staged");
                    (
                        u64::try_from(batch.events.len()).unwrap(),
                        u64::try_from(batch.bytes.len()).unwrap(),
                    )
                };
                layer_ref
                    .inner
                    .panic_next_write
                    .store(true, Ordering::Relaxed);

                layer_ref.flush_cycle().await;

                assert_eq!(
                    sample(
                        &metrics_handle,
                        "trawl_telemetry_events_dropped_total{reason=\"write_crashed\"}"
                    ) - crashed_events_before,
                    expected_events,
                    "the staged event delta is counted once"
                );
                assert_eq!(
                    sample(
                        &metrics_handle,
                        "trawl_telemetry_bytes_dropped_total{reason=\"write_crashed\"}"
                    ) - crashed_bytes_before,
                    expected_bytes,
                    "the staged ndjson-byte delta is counted once"
                );
                assert_eq!(
                    sample(
                        &metrics_handle,
                        crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL
                    ) - failures_before,
                    1,
                    "the crashed attempt increments the write-failure counter once"
                );
                assert_eq!(
                    sample(&metrics_handle, crate::metrics::TELEMETRY_BUFFER_EVENTS),
                    baseline_events,
                    "event-depth gauge returns to baseline"
                );
                assert_eq!(
                    sample(&metrics_handle, crate::metrics::TELEMETRY_BUFFER_BYTES),
                    baseline_bytes,
                    "byte-depth gauge returns to baseline"
                );
                assert!(layer_ref.inner.pending.lock().is_empty());
                assert_eq!(layer_ref.inner.staged.events.load(Ordering::Relaxed), 0);
                assert_eq!(layer_ref.inner.staged.bytes.load(Ordering::Relaxed), 0);
            });
        });
    }

    #[tokio::test]
    async fn prolonged_failure_drops_oldest_batches_at_cap_with_exact_accounting() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let wal_root = broken_wal_root(tmp.path());
        let writer = Arc::new(WalWriter::new(wal_root.clone()));

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Two flush cycles against a broken volume → two pending batches.
        tracing::info!(event_type = "batch_a", "first");
        layer_ref.flush_cycle().await;
        tracing::info!(event_type = "batch_b", "second");
        layer_ref.flush_cycle().await;

        // Cap: exactly what A+B charge — admitting C (same shape as A, so
        // the same charge) must shed A alone.
        let (a_bytes, cap) = {
            let pending = layer_ref.inner.pending.lock();
            assert_eq!(pending.len(), 2);
            (
                pending[0].bytes.len() as u64,
                pending.iter().map(batch_charge).sum::<usize>(),
            )
        };
        layer_ref
            .inner
            .max_buffer_bytes
            .store(cap, Ordering::Relaxed);

        tracing::info!(event_type = "batch_c", "third");
        layer_ref.flush_cycle().await;

        {
            let pending = layer_ref.inner.pending.lock();
            assert_eq!(pending.len(), 2, "oldest batch dropped at cap");
        }
        assert_eq!(
            layer_ref.inner.dropped.cap_events.load(Ordering::Relaxed),
            1,
            "exact dropped event count"
        );
        assert_eq!(
            layer_ref.inner.dropped.cap_bytes.load(Ordering::Relaxed),
            a_bytes,
            "exact dropped byte total"
        );

        // Accounting proven. Restore a normal budget before draining: the
        // recovery record is itself an event under the same shared budget,
        // and a cap sized for exactly two events would shed a survivor to
        // make room for it.
        layer_ref.inner.max_buffer_bytes.store(
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
            Ordering::Relaxed,
        );

        // Repair; survivors drain in FIFO order. The recovery record is
        // emitted during the draining cycle but — flush-path tracing may
        // only buffer — reaches the WAL on the cycle after it.
        repair_wal_root(&wal_root);
        layer_ref.flush_cycle().await;
        assert!(layer_ref.inner.pending.lock().is_empty());
        layer_ref.flush_cycle().await;

        let events = read_wal_events(&wal_root.join("prod"));
        let types: Vec<&str> = events
            .iter()
            .filter_map(|e| e["event_type"].as_str())
            .filter(|t| t.starts_with("batch_"))
            .collect();
        assert_eq!(types, vec!["batch_b", "batch_c"], "FIFO order of survivors");

        // The recovery record carries counts and per-reason totals.
        let dropped_report: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "telemetry_dropped")
            .collect();
        assert_eq!(dropped_report.len(), 1, "one recovery event: {events:?}");
        assert_eq!(dropped_report[0]["dropped_events"], 1);
        assert_eq!(dropped_report[0]["dropped_events_buffer_cap"], 1);
        assert_eq!(dropped_report[0]["dropped_bytes_buffer_cap"], a_bytes);
    }

    /// A long outage queues one batch per flush tick; the drain must not
    /// cost one fsynced WAL file per tick. The first batch goes out alone and
    /// proves the volume healthy again; only then does the rest of the queue
    /// coalesce into a single write — one file, one `batch_id`, one published
    /// batch — with every event preserved in FIFO order.
    #[tokio::test]
    async fn queued_batches_coalesce_into_one_wal_write_on_drain() {
        use crate::bus::{EventBus, EventSubscriber, LocalEventBus};
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let wal_root = broken_wal_root(tmp.path());
        let writer = Arc::new(WalWriter::new(wal_root.clone()));

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let bus = Arc::new(LocalEventBus::new(16));
        let mut sub = bus.subscribe();

        let layer = WalLayer::new(handle, "prod");
        layer.set_bus(Arc::clone(&bus));
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Six flush cycles against a broken volume → six pending batches.
        for i in 0..6 {
            tracing::info!(event_type = "outage", seq = i, "during outage");
            layer_ref.flush_cycle().await;
        }
        assert_eq!(layer_ref.inner.pending.lock().len(), 6);

        repair_wal_root(&wal_root);
        layer_ref.flush_cycle().await;
        assert!(layer_ref.inner.pending.lock().is_empty());

        let files: Vec<_> = std::fs::read_dir(wal_root.join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert_eq!(
            files.len(),
            2,
            "six batches drained as a lead write plus one coalesced write"
        );

        let events = read_wal_events(&wal_root.join("prod"));
        let seqs: Vec<i64> = events.iter().filter_map(|e| e["seq"].as_i64()).collect();
        assert_eq!(seqs, (0..6).collect::<Vec<_>>(), "FIFO order preserved");

        // The lead batch, then one published batch for the coalesced rest.
        let lead = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("timed out waiting for lead batch")
            .expect("recv failed");
        assert_eq!(lead.events.len(), 1);
        let coalesced = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("timed out waiting for coalesced batch")
            .expect("recv failed");
        assert_eq!(coalesced.events.len(), 5);
        assert_ne!(
            lead.batch_id, coalesced.batch_id,
            "each write keeps its own WAL-stem batch_id"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub.recv())
                .await
                .is_err(),
            "the coalesced remainder published once"
        );
    }

    /// The budget must cover active and in-flight memory, not just the
    /// queue. A batch handed to a wedged write is popped from `pending`
    /// but still resident; if the cap ignored it (and exempted the newest
    /// batch), a WAL stall plus a log burst would grow memory without
    /// bound.
    #[tokio::test]
    async fn shared_budget_covers_active_and_in_flight_memory() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let wal_root = broken_wal_root(tmp.path());
        let writer = Arc::new(WalWriter::new(wal_root));

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // One batch staged, then handed to a "write" that never returns:
        // popped from the queue, still in memory.
        tracing::info!(event_type = "wedged", "in flight");
        layer_ref.inner.stage();
        let in_flight = layer_ref.inner.pop_drain_unit(false).expect("batch staged");
        let charge = batch_charge(&in_flight);
        assert!(layer_ref.inner.pending.lock().is_empty());
        assert_eq!(
            layer_ref.inner.staged.bytes.load(Ordering::Relaxed),
            charge,
            "an in-flight batch stays charged to the shared budget"
        );

        // Budget is now fully spent by the in-flight batch alone: nothing
        // is left to shed, so the burst must be dropped, not buffered.
        layer_ref
            .inner
            .max_buffer_bytes
            .store(charge, Ordering::Relaxed);
        let filler = "y".repeat(4096);
        for _ in 0..50 {
            tracing::info!(event_type = "burst", payload = %filler, "stall burst");
        }

        {
            let active = layer_ref.inner.active.lock();
            assert!(
                active.bytes.is_empty() && active.events.is_empty(),
                "active buffer must not grow past the shared budget"
            );
        }
        assert_eq!(
            layer_ref.inner.dropped.cap_events.load(Ordering::Relaxed),
            50,
            "every dropped event is counted — the newest is not exempt"
        );
        assert!(
            layer_ref.inner.dropped.cap_bytes.load(Ordering::Relaxed) > 0,
            "dropped bytes are counted"
        );

        // The gauge reports the whole charge, in-flight batch included.
        layer_ref.inner.update_gauges();
        assert_eq!(
            layer_ref.inner.staged.events.load(Ordering::Relaxed),
            in_flight.events.len()
        );

        // Releasing the in-flight batch frees the budget again.
        layer_ref
            .inner
            .staged
            .release(in_flight.events.len(), charge);
        assert_eq!(layer_ref.inner.staged.bytes.load(Ordering::Relaxed), 0);
        layer_ref.inner.max_buffer_bytes.store(
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
            Ordering::Relaxed,
        );
        tracing::info!(event_type = "after", "buffering resumes");
        assert!(!layer_ref.inner.active.lock().events.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_with_failing_writer_exits_within_budget() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let wal_root = broken_wal_root(tmp.path());
        let writer = Arc::new(WalWriter::new(wal_root));

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer_ref);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(event_type = "shutdown_test", "buffered event");

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let join = spawn_flush_task(layer, Duration::from_hours(1), shutdown_rx);
        shutdown_tx.send(true).unwrap();

        tokio::time::timeout(Duration::from_secs(30), join)
            .await
            .expect("flush task must exit within the shutdown budget")
            .expect("flush task panicked");
    }

    /// The frozen-volume case: the flush is blocked, not failing. A wedged
    /// fsync is modelled by saturating the blocking pool the WAL write is
    /// dispatched to — `flush_cycle` then parks with no error to report,
    /// exactly as it would behind a hung `sync_all`. The periodic flush is
    /// in flight when the signal arrives, so this fails (hangs) unless
    /// shutdown is raced against it.
    #[test]
    fn shutdown_exits_while_a_periodic_flush_is_blocked() {
        use tracing_subscriber::prelude::*;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();

        rt.block_on(async {
            // Occupy the pool's only blocking thread: every later
            // `spawn_blocking` — the WAL write included — is queued and
            // never runs.
            tokio::task::spawn_blocking(move || {
                parked_tx.send(()).unwrap();
                let _ = release_rx.recv();
            });
            parked_rx.recv().unwrap();

            // A healthy WAL root: the write would succeed if it ever ran,
            // so nothing here is an error path.
            let tmp = tempfile::tempdir().unwrap();
            let writer = Arc::new(WalWriter::new(tmp.path().join("wal")));
            writer.ensure_dir().unwrap();

            let handle = WalHandle::new();
            handle.set(Arc::clone(&writer), "prod");

            let layer = WalLayer::new(handle, "prod");
            let subscriber = tracing_subscriber::registry().with(layer.clone());
            let _guard = tracing::subscriber::set_default(subscriber);

            tracing::info!(event_type = "shutdown_test", "buffered event");

            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let join = spawn_flush_task(layer, Duration::from_millis(10), shutdown_rx);

            // Let the periodic flush start and wedge before signalling.
            tokio::time::sleep(Duration::from_millis(200)).await;
            shutdown_tx.send(true).unwrap();

            tokio::time::timeout(SHUTDOWN_FLUSH_BUDGET * 4, join)
                .await
                .expect("flush task must exit while a flush is wedged")
                .expect("flush task panicked");
        });

        // Release the parked thread, then bound the runtime drop the same
        // way `trawld`'s `main` does.
        drop(release_tx);
        rt.shutdown_timeout(Duration::from_secs(5));
    }

    #[tokio::test]
    async fn telemetry_metrics_series_are_exposed() {
        use tracing_subscriber::prelude::*;

        let recorder_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .expect("install test recorder");
        crate::metrics::describe_metrics();

        let tmp = tempfile::tempdir().unwrap();
        let wal_root = broken_wal_root(tmp.path());
        let writer = Arc::new(WalWriter::new(wal_root));

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // One failed write, then force a buffer_cap drop.
        tracing::info!(event_type = "m1", "first");
        layer_ref.flush_cycle().await;
        tracing::info!(event_type = "m2", "second");
        layer_ref.flush_cycle().await;
        layer_ref.inner.max_buffer_bytes.store(1, Ordering::Relaxed);
        tracing::info!(event_type = "m3", "third");
        layer_ref.flush_cycle().await;

        let rendered = recorder_handle.render();
        assert!(
            rendered.contains("trawl_telemetry_wal_write_failures_total"),
            "missing failure counter: {rendered}"
        );
        assert!(
            rendered.contains("trawl_telemetry_events_dropped_total{reason=\"buffer_cap\"}"),
            "missing events-dropped counter: {rendered}"
        );
        assert!(
            rendered.contains("trawl_telemetry_bytes_dropped_total{reason=\"buffer_cap\"}"),
            "missing bytes-dropped counter: {rendered}"
        );
        assert!(
            rendered.contains("trawl_telemetry_buffer_events"),
            "missing buffer-events gauge: {rendered}"
        );
        assert!(
            rendered.contains("trawl_telemetry_buffer_bytes"),
            "missing buffer-bytes gauge: {rendered}"
        );
    }

    #[test]
    fn preinit_cap_drops_are_counted() {
        use tracing_subscriber::prelude::*;

        // Writer never set: the pre-init cap must drop events once the
        // active buffer exceeds 1 MiB, and count them (bytes best-estimate).
        let handle = WalHandle::new();
        let layer = WalLayer::new(handle, "prod");
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let filler = "x".repeat(16 * 1024);
        for _ in 0..80 {
            tracing::info!(event_type = "spam", payload = %filler, "fill");
        }
        assert!(
            layer_ref
                .inner
                .dropped
                .preinit_events
                .load(Ordering::Relaxed)
                > 0,
            "pre-init cap drops must be counted"
        );
        assert!(
            layer_ref
                .inner
                .dropped
                .preinit_bytes
                .load(Ordering::Relaxed)
                > 0,
            "pre-init cap byte estimate must be non-zero"
        );
    }

    #[tokio::test]
    async fn flush_publishes_to_event_bus() {
        use crate::bus::{EventBus, EventSubscriber, LocalEventBus};
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer), "prod");

        let bus = Arc::new(LocalEventBus::new(16));
        let mut sub = bus.subscribe();

        let layer = WalLayer::new(handle, "prod");
        layer.set_bus(Arc::clone(&bus));
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(event_type = "test_bus", user = "alice", "bus test event");

        layer_ref.flush();

        let batch = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("timed out waiting for batch")
            .expect("recv failed");

        assert_eq!(batch.service.as_ref(), "trawld");
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["event_type"], "test_bus");
        assert_eq!(batch.events[0]["user"], "alice");
        assert!(batch.byte_size > 0);
        assert!(
            batch.batch_id.starts_with("prod/trawld_"),
            "batch_id is {{env}}/{{stem}}: {}",
            batch.batch_id
        );
    }
}
