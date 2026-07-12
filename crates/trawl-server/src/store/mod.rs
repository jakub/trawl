// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres app-state store: query history, saved queries, schedules, and
//! report runs (ADR-0004 slice 3).
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

pub mod error;
pub mod history;
pub mod saved;
pub mod schedule;

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection as _, PgPool};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use error::StoreError;
pub use history::{HistoryEntry, HistoryPage, HistoryStore};
pub use saved::{SavedQuery, SavedQueryDetails, SavedQueryStore, ScheduleWithStats};
pub use schedule::{ReportRun, RunClaim, Schedule, ScheduleStore, format_interval, parse_interval};

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

/// App-state storage: one shared pool, three store facades, and the guard
/// holding the sole-writer advisory lock. Cheap to clone.
#[derive(Debug, Clone)]
pub struct StorageState {
    /// Shared app-state pool (exposed for liveness pings and tests).
    pub pool: PgPool,
    /// Query history store.
    pub history: HistoryStore,
    /// Saved queries store.
    pub saved: SavedQueryStore,
    /// Schedules + report runs store.
    pub schedule: ScheduleStore,
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
    /// Boot order is load-bearing: connect pool → take the session advisory
    /// lock (BEFORE migrate — the lock exists to prevent two instances
    /// racing boot-time migration) → run migrations → open the stores.
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
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

        // Sole-writer enforcement on a dedicated session connection (pool
        // connections can be recycled, which would silently drop the lock).
        let mut lock_conn = PgConnection::connect(database_url)
            .await
            .map_err(StoreError::Unavailable)?;
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
