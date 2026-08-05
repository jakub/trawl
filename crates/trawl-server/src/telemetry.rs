// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Internal telemetry: custom tracing [`Layer`] that writes server events
//! to the ingest WAL as `service:trawld`.
//!
//! ## Bootstrap
//!
//! The tracing subscriber is initialized right after the top-level config
//! parses. A config read/parse/validation failure happens BEFORE any
//! subscriber exists and surfaces only through an explicit stderr
//! diagnostic in `main` — it is never captured here. Post-parse
//! initialization events (cert generation, epoch gate, etc.) ARE
//! captured: [`WalHandle`] wraps an [`OnceLock`] — the layer registers at
//! init time and buffers events in memory until [`WalHandle::set`]
//! injects the writer after startup. A 1 MiB cap bounds that pre-init
//! buffer if the writer is never set; drops past it are counted under
//! reason `preinit_cap` (event count exact; bytes estimated from the
//! mean buffered line size, because the drop happens before
//! serialization and the telemetry-disabled path must stay cheap).
//!
//! ## What is persisted (the stdout/telemetry split)
//!
//! The stdout logger and this layer build their filters from the SAME
//! resolved directive string, but they are not the same filter: the WAL
//! layer additionally refuses the targets in
//! [`PRE_AUTH_TARGETS`]. Those events are emitted from fleet-auth's bearer
//! shell, which runs BEFORE the rate limiter, and from the accept loop,
//! which runs before there is even a TLS session — persisting them would
//! let an unauthenticated client turn a connection or request flood into
//! durable corpus growth. They stay on stdout, where retention is the
//! operator's log pipeline rather than trawl's own disk.
//!
//! ## Buffering and the bounded retry queue
//!
//! Events are serialized to ndjson and accumulated in an active buffer
//! (bytes + their event maps, swapped together). Each flush cycle stages
//! the active buffer as one [`Batch`] on a FIFO retry queue, then writes
//! pending batches oldest-first. Once a write SUCCEEDS in a cycle the
//! rest of the queue drains COALESCED: consecutive batches are
//! concatenated up to [`MAX_DRAIN_UNIT_BYTES`] and written as one WAL
//! file, so recovering from a long outage costs writes proportional to
//! queued BYTES rather than to the flush ticks it lasted. That is safe for
//! the hot-buffer `batch_id` contract — it must stay `{env}/{wal-file-stem}`
//! of the file the events landed in, and a coalesced unit lands in exactly
//! one file, so it has exactly one stem and publishes as one `IngestBatch`.
//! While the volume is still failing nothing merges, so the cap keeps its
//! per-tick shedding granularity.
//!
//! A failed write RETAINS the batch for retry (rate-limited stderr +
//! `trawl_telemetry_wal_write_failures_total`); a transient storage error
//! no longer loses the batch. Total retained memory is capped by
//! `[ingest] telemetry_buffer_max_bytes` — the charge is an ESTIMATE
//! (serialized ndjson counted twice, once for the bytes and once for the
//! retained maps which hold roughly the same payload, plus a fixed
//! per-event map overhead), mirroring the hot-buffer setting's estimate
//! semantics.
//!
//! The budget is ONE shared allowance over every byte the layer holds —
//! the active buffer, the retry queue, and the batch currently in flight
//! through a WAL write (popped from the queue but still in memory) — and
//! it is enforced at EVENT INSERTION, not at staging. A cap applied only
//! to the queue after staging would be no cap at all: while a wedged
//! `spawn_blocking` write holds a batch, `on_event` would keep appending
//! to the active buffer without any bound. Admission sheds the OLDEST
//! staged batches first (current operational state is worth more than
//! history), and when there is nothing left to shed — the in-flight batch
//! cannot be reclaimed — it drops the incoming event rather than
//! exempting it. Every drop is counted exactly under reason `buffer_cap`,
//! and `trawl_telemetry_buffer_{events,bytes}` gauge the WHOLE charge, not
//! just the queue.
//!
//! ## Durability before visibility
//!
//! A batch is inserted into the hot buffer and published to the event bus
//! strictly AFTER its WAL write succeeds, exactly once (the batch is
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
//! from OBSERVING it), the final drain runs under a wall-clock budget,
//! and `trawld`'s `Runtime::shutdown_timeout` bounds the process exit
//! itself — a plain runtime drop waits on started blocking tasks forever.
//! A truly wedged fsync therefore leaves one lingering blocking thread at
//! process exit (accepted and preferable to hanging shutdown).
//!
//! ## Infinite recursion guard
//!
//! The flush path uses `eprintln!` for error reporting, NEVER
//! `tracing::*`: a tracing event inside the layer's own flush path would
//! re-enter `on_event` and loop forever. Two narrow exceptions hold
//! because `on_event` only BUFFERS (it takes the active-buffer lock,
//! which the flush path never holds while emitting): the
//! `telemetry_dropped` recovery event after a successful write, and
//! `WalWriter::write`'s own best-effort dir-fsync warning. The invariant
//! is: **no locks are held across `writer.write`, and flush-path tracing
//! may only buffer.**

use std::collections::{BTreeMap, VecDeque};
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

// ---------------------------------------------------------------------------
// Default log filter: the cross-packaging contract
// ---------------------------------------------------------------------------

/// The default tracing filter installed when `RUST_LOG` is unset or invalid.
///
/// This exact string is the cross-packaging contract (issue #56): the Helm
/// chart's `logLevel`, the Debian environment example, and the operator docs
/// all carry it verbatim. It deliberately enumerates every target Trawl
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
/// This is the STDOUT filter. Persistence is narrower: see
/// [`PRE_AUTH_TARGETS`] and [`wal_filter`].
pub const DEFAULT_LOG_FILTER: &str = "trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info";

/// Target for accept-loop diagnostics that fire before any request — and
/// therefore before any authentication — exists: a failed TLS handshake, a
/// connection-level error.
///
/// A deliberately-overridden target (not `trawl_server::transport::http`)
/// so [`PRE_AUTH_TARGETS`] can exclude it from persistence without
/// silencing the rest of the transport module.
pub const PREAUTH_TRANSPORT_TARGET: &str = "preauth.transport";

/// Targets emitted from the PRE-AUTHENTICATION request path: logged, never
/// persisted as `service=trawld` telemetry.
///
/// fleet-auth's bearer shell warns on every missing/malformed header and
/// every invalid or revoked key, and reports keystore trouble under
/// `auth.backend` — all of it from middleware that sits OUTSIDE
/// `rate_limit_middleware` (the limiter needs a verified key, so it cannot
/// run before authn). Cheaper still is [`PREAUTH_TRANSPORT_TARGET`]: the
/// accept loop warns on every failed TLS handshake, which a bare TCP
/// connect-and-close is enough to provoke — no request, no TLS session, no
/// key. Writing any of it to the WAL would hand an unauthenticated client a
/// durable-write amplifier: one ~400-byte record per rejected connection or
/// request, compacted into the corpus and competing with real log data for
/// retention.
///
/// The events are not lost — they keep flowing to stdout (and to the
/// legacy JSON log file) under the same directives, where retention is the
/// operator's log pipeline. What is lost from the corpus is only the
/// per-request repetition: trawld's own post-authn `auth_failure`
/// (`trawl_server`), `storage.backend`, and the catalog/health events that
/// a backend outage also produces all still persist.
///
/// Exclusion from the corpus is NOT the loss of the signal. Every one of
/// these rejections is counted on `/metrics` as
/// `trawl_auth_failures_total{reason}` from trawl's own policy layer
/// (`crate::policy::count_auth_failure`), which sits outside the bearer
/// shell and therefore sees exactly the 401/503 it produces. A counter with
/// a closed label set cannot be amplified — the series count is fixed
/// however hard an unauthenticated client hammers the endpoint — so
/// credential stuffing, token brute force and a revoked key still in use
/// stay alarmable without handing anyone a durable-write lever.
///
/// Matching is by target segment, so `fleet_auth` covers
/// `fleet_auth::middleware` but never a `fleet_authority` target.
pub const PRE_AUTH_TARGETS: [&str; 3] = ["fleet_auth", "auth.backend", PREAUTH_TRANSPORT_TARGET];

/// Whether events on `target` may be persisted as telemetry — false for
/// every [`PRE_AUTH_TARGETS`] entry and its module descendants.
#[must_use]
pub fn is_persisted_target(target: &str) -> bool {
    !PRE_AUTH_TARGETS.iter().any(|excluded| {
        target == *excluded
            || target
                .strip_prefix(excluded)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

/// The [`WalLayer`]'s filter: the resolved directives AND
/// [`is_persisted_target`].
///
/// A second, non-configurable predicate rather than an appended
/// `fleet_auth=off` directive: `EnvFilter` resolves by specificity, so an
/// operator `RUST_LOG` naming `fleet_auth::middleware=info` would outrank
/// an appended target-level `off` and quietly restore the amplifier. The
/// pre-auth exclusion is an invariant of what trawl writes to its own
/// disk, not a log level.
pub fn wal_filter<S>(directives: &str) -> impl tracing_subscriber::layer::Filter<S> + 'static
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    use tracing_subscriber::filter::FilterExt;

    tracing_subscriber::EnvFilter::new(directives).and(tracing_subscriber::filter::filter_fn(
        |meta: &tracing::Metadata<'_>| is_persisted_target(meta.target()),
    ))
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
    /// Create a new empty handle.
    pub fn new() -> Self {
        Self(Arc::new(OnceLock::new()))
    }

    /// Inject the WAL writer and the env telemetry events land under
    /// (`default_env`). Called once after config is loaded. Subsequent
    /// calls are silently ignored (first write wins).
    pub fn set(&self, writer: Arc<WalWriter>, env: &str) {
        let _ = self.0.set((writer, env.into()));
    }

    /// Get the writer and env, if available.
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

/// One staged flush unit: the serialized ndjson lines and the event maps
/// they were serialized from. Written to the WAL as ONE file, so the
/// hot-buffer `batch_id` ↔ WAL-filename-stem contract holds per batch.
struct Batch {
    bytes: Vec<u8>,
    events: Vec<serde_json::Map<String, serde_json::Value>>,
}

/// Maximum serialized ndjson one drain unit may carry into a single WAL
/// write.
///
/// The retry queue is bounded in BYTES, and [`WalLayerInner::stage`] makes
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
/// under ONE lock so the two representations can never skew.
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

/// Running charge of STAGED memory: everything queued in `pending` plus
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
}

struct WalLayerInner {
    /// Env stamped onto telemetry events (`default_env`).
    env: String,
    handle: WalHandle,
    /// Active buffer: events accumulated since the last stage.
    active: Mutex<ActiveBuffer>,
    /// FIFO retry queue of staged batches awaiting a successful WAL write.
    pending: Mutex<VecDeque<Batch>>,
    /// Charge of `pending` PLUS any batch in flight through a WAL write.
    staged: StagedCharge,
    /// The ONE cap on estimated memory charged by the active buffer, the
    /// retry queue and the in-flight batch together
    /// (`[ingest] telemetry_buffer_max_bytes`). Atomic so tests can
    /// tighten it after construction.
    max_buffer_bytes: AtomicUsize,
    /// Cached hostname, resolved once at layer creation.
    host: String,
    /// Loss accounting for the recovery event and metrics.
    dropped: DropCounters,
    /// Last time a WAL failure was reported to stderr (rate limit).
    last_stderr: Mutex<Option<Instant>>,
    /// Deferred event bus for real-time fanout (SSE streaming).
    bus: OnceLock<Arc<crate::bus::LocalEventBus>>,
    /// Deferred hot buffer for synchronous insertion (query freshness).
    hot_buffer: OnceLock<Arc<crate::hot_buffer::HotBuffer>>,
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
    /// Create a new layer backed by the given handle. `env` is the env
    /// telemetry events are stamped with (`default_env`) — the records
    /// carry it as a column, matching where the WAL handle files them.
    /// Uses the default memory budget; production passes the configured
    /// `[ingest] telemetry_buffer_max_bytes` via
    /// [`WalLayer::new_with_buffer_cap`].
    pub fn new(handle: WalHandle, env: &str) -> Self {
        Self::new_with_buffer_cap(
            handle,
            env,
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
        )
    }

    /// [`WalLayer::new`] with an explicit shared memory budget over the
    /// active buffer, retry queue and in-flight batch
    /// (`[ingest] telemetry_buffer_max_bytes`).
    pub fn new_with_buffer_cap(handle: WalHandle, env: &str, max_buffer_bytes: usize) -> Self {
        let host = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_default();
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
                dropped: DropCounters::default(),
                last_stderr: Mutex::new(None),
                bus: OnceLock::new(),
                hot_buffer: OnceLock::new(),
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
    /// the blocking pool.
    pub fn flush(&self) {
        let Some((writer, env)) = self.inner.handle.get() else {
            return;
        };
        self.inner.stage();
        let mut coalesce = false;
        while let Some(batch) = self.inner.pop_drain_unit(coalesce) {
            match writer.write(env, "trawld", &batch.bytes) {
                Ok(wal_path) => {
                    coalesce = true;
                    self.inner.publish(env, &wal_path, batch);
                }
                Err(e) => {
                    self.inner.record_write_failure(&e);
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
    /// COALESCED units of at most [`MAX_DRAIN_UNIT_BYTES`], so recovering
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
            // Captured before the batch moves into the closure, so a lost
            // batch can still be released from the shared accounting.
            let in_flight = (batch.events.len(), batch_charge(&batch));
            let joined = tokio::task::spawn_blocking(move || {
                let result = w.write(&batch_env, "trawld", &batch.bytes);
                (result, batch)
            })
            .await;
            match joined {
                Ok((Ok(wal_path), batch)) => {
                    coalesce = true;
                    self.inner.publish(env, &wal_path, batch);
                }
                Ok((Err(e), batch)) => {
                    self.inner.record_write_failure(&e);
                    self.inner.requeue_front(batch);
                    break;
                }
                Err(join_err) => {
                    // The batch was consumed by the panicked/cancelled
                    // closure and cannot be recovered. WalWriter::write
                    // does not panic in practice.
                    self.inner.staged.release(in_flight.0, in_flight.1);
                    self.inner
                        .record_write_failure(&std::io::Error::other(join_err));
                    break;
                }
            }
        }
        self.inner.update_gauges();
    }
}

impl WalLayerInner {
    /// Swap the active buffer into a pending [`Batch`].
    ///
    /// Staging MOVES charge from the active buffer onto the queue without
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
    /// NEWEST event is the honest end of the ladder: exempting it (as a
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

        // Shed the OLDEST staged batches first: current operational state
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
            "reason" => "buffer_cap"
        )
        .increment(events);
        metrics::counter!(
            crate::metrics::TELEMETRY_BYTES_DROPPED_TOTAL,
            "reason" => "buffer_cap"
        )
        .increment(bytes);
    }

    /// Pop the oldest pending batches for one write attempt. With
    /// `coalesce`, they are concatenated oldest-first while they fit in
    /// [`MAX_DRAIN_UNIT_BYTES`] (the first is always taken, however large).
    /// Every line already ends in `\n`, so concatenation is valid ndjson,
    /// and the merged unit lands in ONE WAL file — one filename stem, one
    /// published `IngestBatch`.
    ///
    /// `coalesce` is set only once a write has SUCCEEDED in this cycle:
    /// merging exists to bound the RECOVERY drain, and merging while the
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
    fn publish(&self, env: &str, wal_path: &std::path::Path, batch: Batch) {
        // The batch leaves the layer's accounting here: the hot buffer
        // takes ownership under its OWN `hot_buffer_max_bytes` budget.
        self.staged
            .release(batch.events.len(), batch_charge(&batch));

        if !batch.events.is_empty() {
            // batch_id MUST match the WAL filename stem so compaction can
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
                service: "trawld".into(),
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
        if preinit_events + cap_events > 0 {
            tracing::warn!(
                event_type = "telemetry_dropped",
                dropped_events = preinit_events + cap_events,
                dropped_bytes = preinit_bytes + cap_bytes,
                dropped_events_preinit_cap = preinit_events,
                dropped_bytes_preinit_cap = preinit_bytes,
                dropped_events_buffer_cap = cap_events,
                dropped_bytes_buffer_cap = cap_bytes,
                "telemetry events were lost (see reason totals; \
                 preinit_cap bytes are a mean-line-size estimate)"
            );
        }
    }

    /// Record a WAL write failure: scrapeable counter plus rate-limited
    /// stderr (the independent last-resort channel while self-ingestion
    /// is unavailable). MUST NOT use tracing — see the module docs.
    fn record_write_failure(&self, e: &std::io::Error) {
        metrics::counter!(crate::metrics::TELEMETRY_WAL_WRITE_FAILURES_TOTAL).increment(1);
        let mut last = self.last_stderr.lock();
        let due = last.is_none_or(|t| t.elapsed() >= Duration::from_mins(1));
        if due {
            eprintln!("[trawl-telemetry] WAL write failed (batch retained for retry): {e}");
            *last = Some(Instant::now());
        }
    }

    /// Refresh the buffer-depth gauges (per flush cycle). They report the
    /// WHOLE charge against `telemetry_buffer_max_bytes` — active buffer,
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

    /// Store one field under its ASCII-folded name — the fold-at-the-door
    /// rule (ADR-0009). Telemetry writes straight into the WAL and hot
    /// buffer without routing through `envelope::canonicalize`, and a
    /// tracing field name is any Rust-side identifier
    /// (`tracing::info!(myField = 1)` is legal), so an unfolded name here
    /// would become a column spelling the (folded) catalog pin never
    /// matches. Trawl's own call sites are `snake_case`; this makes that a
    /// guarantee instead of a convention.
    fn insert_folded(&mut self, field: &Field, value: serde_json::Value) {
        self.fields.insert(field.name().to_ascii_lowercase(), value);
    }
}

impl Visit for JsonVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert_folded(field, json!(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert_folded(field, json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert_folded(field, json!(value));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.insert_folded(field, json!(value));
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.insert_folded(field, json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert_folded(field, json!(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert_folded(field, json!(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert_folded(field, json!(format!("{value:?}")));
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
        // Pre-init cap: if the writer isn't set yet and the buffer is
        // already over 1 MiB, drop this event to prevent unbounded growth
        // (e.g. if telemetry is disabled and the writer is never injected).
        // The drop is counted: event count exact, bytes estimated from the
        // mean buffered line size — it happens before serialization and
        // the telemetry-disabled path must stay cheap.
        const PRE_INIT_CAP: usize = 1024 * 1024;
        if self.inner.handle.get().is_none() {
            let estimate = {
                let active = self.inner.active.lock();
                if active.bytes.len() < PRE_INIT_CAP {
                    None
                } else {
                    Some((active.bytes.len() / active.events.len().max(1)) as u64)
                }
            };
            if let Some(mean_line_bytes) = estimate {
                self.inner
                    .dropped
                    .preinit_events
                    .fetch_add(1, Ordering::Relaxed);
                self.inner
                    .dropped
                    .preinit_bytes
                    .fetch_add(mean_line_bytes, Ordering::Relaxed);
                metrics::counter!(
                    crate::metrics::TELEMETRY_EVENTS_DROPPED_TOTAL,
                    "reason" => "preinit_cap"
                )
                .increment(1);
                metrics::counter!(
                    crate::metrics::TELEMETRY_BYTES_DROPPED_TOTAL,
                    "reason" => "preinit_cap"
                )
                .increment(mean_line_bytes);
                return;
            }
        }

        // Collect event-level fields.
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

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let level = metadata.level().as_str().to_ascii_lowercase();
        let mut record = serde_json::Map::with_capacity(12 + span_fields.len());
        record.insert("_time".into(), json!(&now));
        record.insert("_ingested".into(), json!(&now));
        record.insert("env".into(), json!(&self.inner.env));
        record.insert("service".into(), json!("trawld"));
        record.insert("host".into(), json!(&self.inner.host));
        // The server is the producer, so severity maps directly from the
        // tracing level onto the OTel ladder (ADR-0009).
        if let Some(n) = trawl_core::severity::number_for_token(&level) {
            record.insert("severity".into(), json!(n));
        }
        record.insert("severity_text".into(), json!(&level));
        record.insert("target".into(), json!(metadata.target()));
        record.insert("event_type".into(), json!(event_type));
        record.insert("message".into(), json!(message));

        // Merge remaining span + event fields.
        for (k, v) in span_fields {
            record.entry(k).or_insert(v);
        }

        // `_raw` is required by the envelope: for server-generated events
        // the canonical serialization of the record IS the most original
        // form available.
        let raw = serde_json::Value::Object(record.clone()).to_string();
        record.insert("_raw".into(), json!(raw));

        // Serialize, then push bytes and map under ONE lock so the two
        // representations of the active buffer can never skew (a stage
        // between the two pushes would publish a map whose bytes never
        // reached the WAL). serde_json::to_vec on Value cannot fail.
        let mut line = serde_json::to_vec(&serde_json::Value::Object(record.clone()))
            .expect("JSON serialization of Value is infallible");
        line.push(b'\n');

        // The shared budget is enforced HERE, over the active buffer, the
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
/// Shutdown is bounded even while a PERIODIC flush is wedged: the periodic
/// flush is itself raced against `shutdown_rx`, so the signal is observed
/// without waiting for an fsync that may never return. Abandoning an
/// in-flight flush costs the in-flight batch's VISIBILITY, never its
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

    // -- default filter contract (issue #56 F1) -----------------------------

    /// Capture layer recording (target, level, message) triples.
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
    fn pre_auth_targets_are_never_persisted() {
        assert!(!is_persisted_target("fleet_auth"));
        assert!(!is_persisted_target("fleet_auth::middleware"));
        assert!(!is_persisted_target("auth.backend"));
        // The accept loop's pre-TLS diagnostics: a bare TCP
        // connect-and-close is enough to emit one.
        assert!(!is_persisted_target(PREAUTH_TRANSPORT_TARGET));
        // Prefix matching is per segment, not per byte.
        assert!(is_persisted_target("fleet_authority"));
        assert!(is_persisted_target("fleet_auth_shim::x"));
        // Everything trawld emits itself keeps persisting, including the
        // post-authn auth_failure event and the storage alarm target.
        assert!(is_persisted_target("trawl_server::policy"));
        assert!(is_persisted_target("trawld"));
        assert!(is_persisted_target("storage.backend"));
    }

    /// The pre-authn auth events are logged (previous test) but must never
    /// reach the WAL layer: fleet-auth's bearer shell runs OUTSIDE the rate
    /// limiter, so persisting them would let an unauthenticated flood grow
    /// the corpus one durable record per rejected request.
    #[test]
    fn wal_filter_drops_pre_auth_targets_the_stdout_filter_keeps() {
        use tracing_subscriber::prelude::*;

        let capture = CaptureLayer::default();
        let events = Arc::clone(&capture.events);
        let subscriber = tracing_subscriber::registry()
            .with(capture.with_filter(wal_filter(DEFAULT_LOG_FILTER)));
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::warn!(target: "fleet_auth::middleware", "auth: missing or malformed bearer header");
        tracing::warn!(target: "fleet_auth::middleware", "auth: invalid or revoked key");
        tracing::error!(target: "auth.backend", "auth: db error");
        tracing::warn!(target: PREAUTH_TRANSPORT_TARGET, event_type = "tls_handshake_failed", "TLS handshake failed");
        tracing::info!(target: "trawl_server::policy", "policy: auth failure (post-authn)");
        tracing::error!(target: "storage.backend", "app-state store error");
        tracing::info!(target: "trawld", "starting trawld");

        let seen = events.lock();
        let targets: Vec<&str> = seen.iter().map(|(t, _)| t.as_str()).collect();
        for excluded in [
            "fleet_auth::middleware",
            "auth.backend",
            PREAUTH_TRANSPORT_TARGET,
        ] {
            assert!(
                !targets.contains(&excluded),
                "pre-authn target {excluded} must not be persisted; saw {targets:?}"
            );
        }
        for kept in ["trawl_server::policy", "storage.backend", "trawld"] {
            assert!(
                targets.contains(&kept),
                "target {kept} must still be persisted; saw {targets:?}"
            );
        }
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

        // Buffer should have data now.
        assert!(!layer_ref.inner.active.lock().bytes.is_empty());

        // Flush to WAL.
        layer_ref.flush();
        assert!(layer_ref.inner.active.lock().bytes.is_empty());

        // Verify WAL file was written.
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
        assert_eq!(parsed["severity"], 9, "tracing info maps to OTel 9");
        assert_eq!(parsed["severity_text"], "info");
        assert!(parsed["target"].is_string());
    }

    /// Tracing field names are Rust-side identifiers and CAN be mixed case
    /// (`tracing::info!(myField = 1)` is legal); this path writes straight
    /// into the WAL/hot buffer without `envelope::canonicalize`, so the
    /// visitor folds at collection — an unfolded name would become a
    /// column spelling the folded catalog pin never matches.
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

    // -- bounded retry queue (issue #56 F2/F3) ------------------------------

    /// A WAL root that is a FILE makes every write fail (`create_dir_all`
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

        // Write fails: the batch must be RETAINED, and nothing published.
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

        // Repair the volume, retry WITHOUT emitting new events.
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

        // Three flush cycles against a broken volume → three pending batches.
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
        // recovery record is itself an event under the SAME shared budget,
        // and a cap sized for exactly two events would shed a survivor to
        // make room for it.
        layer_ref.inner.max_buffer_bytes.store(
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
            Ordering::Relaxed,
        );

        // Repair; survivors drain in FIFO order. The recovery record is
        // emitted during the draining cycle but — flush-path tracing may
        // only BUFFER — reaches the WAL on the cycle after it.
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
    /// cost one fsynced WAL file per tick. The lead write proves the volume
    /// unmerged, then the remaining queue coalesces into a single write —
    /// one file, one `batch_id`, one published batch — with every event
    /// preserved in FIFO order.
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

        // The lead batch, then ONE published batch for the coalesced rest.
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

    /// The budget must cover ACTIVE and IN-FLIGHT memory, not just the
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

    /// The frozen-volume case: the flush is BLOCKED, not failing. A wedged
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
            // Occupy the pool's ONLY blocking thread: every later
            // `spawn_blocking` — the WAL write included — is queued and
            // never runs.
            tokio::task::spawn_blocking(move || {
                parked_tx.send(()).unwrap();
                let _ = release_rx.recv();
            });
            parked_rx.recv().unwrap();

            // A healthy WAL root: the write would SUCCEED if it ever ran,
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

        // The batch should arrive on the subscriber.
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
