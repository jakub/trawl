// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres app-state store: query history, saved queries, schedules,
//! report runs, the field catalog, and repin jobs (ADR-0004).
//!
//! trawld owns the dedicated `trawl` database outright: it takes a session
//! advisory lock at boot (sole-writer enforcement), then auto-migrates via
//! [`sqlx::migrate!`]. The fleet keystore database forbids auto-migration
//! because two binaries share it; the rationale doesn't transfer here —
//! single-replica trawld is the only writer by design.
//!
//! The advisory lock is *session*-scoped: postgres releases it the instant
//! the holding connection dies (pg restart, an idle-timeout device culling
//! the socket, a `pg_terminate_backend`). A background guard task therefore
//! keepalive-probes the lock connection and, the moment the probe fails,
//! flips a lock-lost signal — surfaced through `/health` and awaited by
//! `main` to terminate the daemon before a second instance can acquire the
//! freed lock and become a concurrent writer.

pub mod catalog;
pub mod error;
pub mod history;
pub mod repin;
pub mod saved;
pub mod schedule;
pub mod status;

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use catalog::{
    AckOutcome, CatalogStore, ConflictListRow, ConflictServicePair, DegradedAck, FieldConflict,
    FieldConflictRow, FieldHealthSnapshot, FieldListFilter, FieldPinRow, FieldServiceRow,
    FieldSummaryRow, GcPinRow, MAX_CONFLICT_SAMPLE_BYTES, MAX_CONFLICT_SAMPLES,
    MAX_CONFLICTS_PER_FIELD, MAX_PINNED_FIELDS, PURGE_COMMIT_BOUND, PinProposal, PurgedPin,
    PurgedPins, ServiceCursor, ServiceObservation,
};
pub use error::{StoreError, WindowWriteError};
pub use history::{HistoryEntry, HistoryPage, HistoryStore};
pub use repin::{
    CutoverOutcome, JobTotals, RepinClaim, RepinJob, RepinJobStatus, RepinPlan, RepinStore,
};
pub use saved::{SavedQuery, SavedQueryDetails, SavedQueryStore, ScheduleWithStats};
pub use schedule::{
    ClaimedManualRun, ClaimedRun, DueClaim, DueClaimError, FinishOutcome, FlipOutcome,
    MAX_DURATION_SECS, ManualRunClaim, ReportRun, RunClaim, Schedule, ScheduleStore,
    format_interval, parse_duration_secs, parse_interval,
};
pub use status::RunStatus;

use crate::ping::{PingCache, ping_cached_with};

/// Embedded schema migrations for the trawl app-state database, applied by
/// trawld at boot (after the advisory lock, before the stores open).
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Application-wide advisory lock key on the `trawl` database.
///
/// Arbitrary but fixed: any second trawld pointed at the same database fails
/// startup instead of racing boot-time migration or letting
/// `cleanup_stale_runs` stomp a live sibling's runs.
const ADVISORY_LOCK_KEY: i64 = 0x0074_7261_776c_2131; // "trawl!1"

/// Advisory lock key serialising the two transactions that change which
/// fields the catalog pins: a repin's claim ([`RepinStore::claim`]) and pin
/// gc's purge ([`CatalogStore::delete_pins`]).
///
/// Both are check-then-act across process boundaries. gc reads its
/// candidates, proves them dead against the corpus, then deletes; a repin
/// reads a field's pin from the in-process cache, then claims a job that
/// will later flip that very row. Without a shared lock the two interleave:
/// a claim landing after gc's last look leaves `finish_cutover` updating a
/// `field_types` row gc has deleted, a zero-row UPDATE that restores the pin
/// in memory only, so the type authority dies with the process. Taken as an
/// `xact` lock, postgres releases it at commit or rollback, so neither side
/// can strand it.
///
/// A DIFFERENT key from [`ADVISORY_LOCK_KEY`] on purpose: session and
/// transaction advisory locks share one lock space, and trawld holds the
/// session lock for its whole life, so reusing that key would deadlock
/// every claim and every purge.
pub(crate) const CATALOG_LIFECYCLE_LOCK_KEY: i64 = 0x0074_7261_776c_2143; // "trawl!C"

/// Take [`CATALOG_LIFECYCLE_LOCK_KEY`] for the rest of `tx`.
pub(crate) async fn lock_catalog_lifecycle(tx: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(CATALOG_LIFECYCLE_LOCK_KEY)
        .execute(tx)
        .await
        .map(|_| ())
}

/// Maximum connections in the app-state pool. Small on purpose: trawld is
/// the sole writer and the workload is light CRUD.
const MAX_CONNECTIONS: u32 = 8;

/// Bound on the boot-time connection attempt (see `connect`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the guard task probes the lock-holding session. Doubles as a
/// keepalive that stops an idle-timeout network device from culling the
/// connection, and as the upper bound on how long a genuinely lost lock
/// goes undetected — kept short because every second of an undetected loss
/// is a second in which a second writer could start.
const LOCK_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Owns the guard task that keepalive-probes the lock connection. Aborting
/// the task on drop drops the lock connection with it, so a graceful
/// shutdown (the last `StorageState` clone dropping) releases the advisory
/// lock — the behaviour the boot-idempotency path depends on.
#[derive(Debug)]
struct LockGuard(JoinHandle<()>);

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// App-state storage: one shared pool, five store facades, and the guard
/// holding the sole-writer advisory lock. Cheap to clone.
#[derive(Debug, Clone)]
pub struct StorageState {
    /// Shared app-state pool. Private: the store facades each hold their own
    /// clone, and `ping`/`ping_cached` reach it in-module — nothing outside
    /// this crate bypasses a facade to run raw SQL against it.
    pool: PgPool,
    /// Query history store.
    pub history: HistoryStore,
    /// Saved queries store.
    pub saved: SavedQueryStore,
    /// Schedules + report runs store.
    pub schedule: ScheduleStore,
    /// Field catalog: type pins, per-service observations, conflicts.
    pub catalog: CatalogStore,
    /// Repin jobs: the persisted one-at-a-time operator-triggered field
    /// repin.
    pub repin: RepinStore,
    /// Guard task owning the dedicated session connection that holds
    /// `pg_advisory_lock`. Lives exactly as long as the state; its `Drop`
    /// aborts the task, dropping the connection and releasing the lock.
    _lock_guard: Arc<LockGuard>,
    /// Watch flipped to `true` the moment the guard detects the lock is
    /// gone. Surfaced by `ping_cached` (so `/health` reports it) and awaited
    /// by `main` to terminate the daemon before a second writer can start.
    lock_lost_rx: watch::Receiver<bool>,
    /// Memoised storage liveness ping shared by every `/health` probe.
    storage_ping: Arc<PingCache>,
}

/// Keepalive-probe the lock-holding session forever. The first probe failure
/// means the connection — and with it the session-scoped advisory lock — is
/// gone; flip the watch and return so the socket drops.
///
/// A raw [`PgConnection`] never reconnects, so a failed probe is proof the
/// original session died: the lock is definitively released and this
/// instance must stop writing.
async fn guard_advisory_lock(mut conn: PgConnection, lost_tx: watch::Sender<bool>) {
    let mut ticker = tokio::time::interval(LOCK_KEEPALIVE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // consume the immediate first tick

    loop {
        ticker.tick().await;
        if let Err(e) = sqlx::query("SELECT 1").execute(&mut conn).await {
            tracing::error!(
                event_type = "storage_lock_lost",
                error = %e,
                "sole-writer advisory lock lost: the app-state session died and \
                 postgres has released the lock; another trawld can now acquire it \
                 and become a second writer. terminating to preserve the \
                 single-writer invariant."
            );
            let _ = lost_tx.send(true);
            return;
        }
    }
}

impl StorageState {
    /// Connect to the trawl app-state database and prepare it for use.
    ///
    /// Builds the pool, then hands it to [`Self::from_pool`], which owns the
    /// load-bearing part of the boot order.
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        // StorageState::connect is the named production pool owner for
        // trawld's app-state database (ADR-0021 ruling 3).
        #[allow(clippy::disallowed_methods)]
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            // Boot fails fast on an unreachable database instead of
            // spinning inside sqlx's default 30s acquire deadline — the
            // supervisor (systemd/k8s) owns retry policy, and the startup
            // error names the provisioning runbook.
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect(database_url)
            .await
            .map_err(StoreError::Unavailable)?;

        Self::from_pool(pool).await
    }

    /// Prepare an already-built pool for use: advisory lock, migrate, open
    /// the stores.
    ///
    /// The pool is the only input, and that is the whole point: the
    /// sole-writer lock is taken on a connection acquired from THIS pool,
    /// so nothing here can name a different database than the writes do. A
    /// second parameter naming a DSN could disagree with the pool, and two
    /// processes locking two different databases both believe they are the
    /// sole writer.
    ///
    /// The contract that makes it airtight is on the CALLER, and it runs
    /// for the LIFETIME of the returned [`StorageState`], not just for the
    /// duration of this call: the pool's connect target must never be
    /// reconfigured. A sqlx 0.9 `Pool` clone shares one `Arc`, so
    /// `set_connect_options` on a clone the caller kept repoints every
    /// connection the stores open from then on, while the detached lock
    /// session below stays on the old database. The result is a process
    /// holding the sole-writer lock on one database and writing to
    /// another. Repointing mid-call splits the lock from the migration the
    /// same way.
    ///
    /// Every in-repo caller MOVES a pool it just built into this function
    /// and keeps no clone of its own, so no runtime check is worth adding
    /// for a shape nothing writes.
    ///
    /// The connection is then detached ([`sqlx::pool::PoolConnection::detach`]):
    /// it leaves pool management entirely and is never recycled, so the
    /// session-scoped `pg_advisory_lock` cannot be released underneath us
    /// by a pooled connection going back on the idle list. The pool refills
    /// the slot on demand, so the process ceiling is `max_connections` plus
    /// this one dedicated session.
    ///
    /// Boot order is load-bearing here, not in the caller: take the session
    /// advisory lock BEFORE migrate — the lock exists to prevent two
    /// instances racing boot-time migration.
    pub async fn from_pool(pool: PgPool) -> Result<Self, StoreError> {
        // Sole-writer enforcement on a dedicated session connection, minted
        // from the pool and detached so nothing can recycle it.
        let mut lock_conn: PgConnection = pool
            .acquire()
            .await
            .map_err(StoreError::Unavailable)?
            .detach();
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(ADVISORY_LOCK_KEY)
            .fetch_one(&mut lock_conn)
            .await
            .map_err(StoreError::Unavailable)?;
        if !locked {
            return Err(StoreError::LockHeld);
        }

        MIGRATOR.run(&pool).await?;

        // Guard the lock for the process lifetime: keepalive-probe its
        // session and flip `lock_lost_rx` the instant it dies.
        let (lost_tx, lock_lost_rx) = watch::channel(false);
        let guard = tokio::spawn(guard_advisory_lock(lock_conn, lost_tx));

        tracing::info!(
            event_type = "storage_ready",
            "app-state database migrated and advisory-locked"
        );

        Ok(Self {
            history: HistoryStore::new(pool.clone()),
            saved: SavedQueryStore::new(pool.clone()),
            schedule: ScheduleStore::new(pool.clone()),
            catalog: CatalogStore::new(pool.clone()),
            repin: RepinStore::new(pool.clone()),
            pool,
            _lock_guard: Arc::new(LockGuard(guard)),
            lock_lost_rx,
            storage_ping: Arc::new(PingCache::new(None)),
        })
    }

    /// A receiver that flips to `true` when the sole-writer advisory lock is
    /// lost. `main` awaits this to terminate the daemon before a second
    /// instance can acquire the freed lock.
    #[must_use]
    pub fn lock_lost(&self) -> watch::Receiver<bool> {
        self.lock_lost_rx.clone()
    }

    /// Raw liveness ping against the app-state pool.
    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    /// Liveness ping memoised behind a TTL and bounded by a timeout —
    /// mirrors the fleet-keystore ping guarding `/health` (see
    /// [`crate::ping`] for the rationale).
    ///
    /// A lost sole-writer lock short-circuits to an error *before* the cache:
    /// the pool itself may still ping fine (it reconnects transparently),
    /// which is precisely the split-brain hazard `/health` must expose.
    pub async fn ping_cached(&self) -> Result<(), String> {
        if *self.lock_lost_rx.borrow() {
            return Err(
                "sole-writer advisory lock lost — trawld is terminating to avoid \
                 a second writer"
                    .to_owned(),
            );
        }
        ping_cached_with(
            &self.storage_ping,
            Self::PING_CACHE_TTL,
            Self::PING_TIMEOUT,
            "storage",
            || self.ping(),
        )
        .await
    }

    /// Upper bound on a single storage liveness ping (matches the keystore
    /// bound: non-critical → `Degraded` → HTTP 200, never a stall).
    const PING_TIMEOUT: Duration = Duration::from_secs(2);

    /// How long a storage ping outcome is reused before a fresh probe.
    const PING_CACHE_TTL: Duration = Duration::from_secs(5);
}
