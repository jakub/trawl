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

pub mod error;
pub mod history;
pub mod saved;
pub mod schedule;

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection as _, PgPool};

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

/// App-state storage: one shared pool, three store facades, and the session
/// connection holding the sole-writer advisory lock. Cheap to clone.
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
    /// Dedicated session connection holding `pg_advisory_lock` for the
    /// process lifetime. Never queried again — dropping it releases the
    /// lock, so it must live exactly as long as the state.
    _advisory_lock: Arc<tokio::sync::Mutex<PgConnection>>,
    /// Memoised storage liveness ping shared by every `/health` probe.
    storage_ping: Arc<PingCache>,
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

        tracing::info!(
            event_type = "storage_ready",
            "app-state database migrated and advisory-locked"
        );

        Ok(Self {
            history: HistoryStore::new(pool.clone()),
            saved: SavedQueryStore::new(pool.clone()),
            schedule: ScheduleStore::new(pool.clone()),
            pool,
            _advisory_lock: Arc::new(tokio::sync::Mutex::new(lock_conn)),
            storage_ping: Arc::new(PingCache::new(None)),
        })
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
    pub async fn ping_cached(&self) -> Result<(), String> {
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
