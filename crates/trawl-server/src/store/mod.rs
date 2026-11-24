// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres app-state store: query history, saved queries, schedules,
//! report runs, the field catalog, repin jobs (ADR-0004), captured-result
//! metadata (ADR-0057), and investigations (ADR-0059).
//!
//! The advisory lock is *session*-scoped: postgres releases it the instant
//! the holding connection dies (pg restart, an idle-timeout device culling
//! the socket, a `pg_terminate_backend`). A background guard task therefore
//! keepalive-probes the lock connection and, the moment the probe fails,
//! flips a lock-lost signal — surfaced through `/health` and awaited by
//! `main` to terminate the daemon before a second instance can acquire the
//! freed lock and become a concurrent writer.
//!
//! No pooled connection goes back to the pool inside a transaction. A
//! transaction sqlx begins counts only once Postgres has answered its
//! `BEGIN`, so a request dropped while that answer is on its way leaves the
//! session inside a transaction sqlx never queued a `ROLLBACK` for, and the
//! pool's own return check accepts it. The pool checks every connection it
//! takes back ([`released_outside_transaction`]) and closes one still inside
//! a transaction.

pub mod catalog;
pub mod error;
pub mod history;
pub mod investigations;
pub(crate) mod pooled;
pub mod repin;
pub mod results;
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
pub use investigations::{
    Appended, Citing, CreateOutcome, DeleteOutcome, InvestigationRow, InvestigationStore, LogPage,
    NewInvestigation, Overwrite, Overwritten, RetentionRule, StoreActor,
};
pub use repin::{
    CutoverOutcome, JobTotals, RepinClaim, RepinJob, RepinJobStatus, RepinPlan, RepinStore,
    Requester,
};
pub use results::{
    CaptureInsert, CaptureRow, ExpiredFile, FactsDocument, KeptInsert, KeptStep, NewCapture,
    ResultsStore, Usage,
};
pub use saved::{SavedQuery, SavedQueryDetails, SavedQueryStore, ScheduleWithStats};
pub use schedule::{
    CLAIM_WAIT, ClaimedManualRun, ClaimedRun, DUE_SCHEDULE_PAGE, DueClaim, DueClaimError,
    DueSchedule, FinishOutcome, FlipOutcome, MAX_DURATION_SECS, ManualRunClaim, ReportRun,
    RunClaim, Schedule, ScheduleStore, format_interval, parse_duration_secs, parse_interval,
};
pub use status::{RunOrigin, RunStatus};

use crate::ping::{PingCache, ping_cached_with};

pub mod migrations;

const ADVISORY_LOCK_KEY: i64 = 0x0074_7261_776c_2131; // "trawl!1"

/// Observed in `pg_locks`, never taken (`trawld --doctor`, ADR-0047). A
/// held lock says some trawld owns this database, not that it runs on this
/// host.
pub async fn writer_lock_held(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    advisory_lock_granted(conn, ADVISORY_LOCK_KEY).await
}

/// A single `bigint` key appears with its high 32 bits in `classid`, its low
/// 32 bits in `objid`, and `objsubid` 1; `pg_locks` lists every database's
/// locks, so the row must also name this one.
pub(crate) async fn advisory_lock_granted(
    conn: &mut PgConnection,
    key: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_locks
            WHERE locktype = 'advisory' AND granted AND objsubid = 1
              AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
              AND classid::int8 = ($1 >> 32) & 4294967295
              AND objid::int8 = $1 & 4294967295
        )",
    )
    .bind(key)
    .fetch_one(conn)
    .await
}

/// Advisory lock key serialising the two transactions that change which
/// fields the catalog pins: a repin's claim ([`RepinStore::claim`]) and pin
/// gc's purge ([`CatalogStore::delete_pins`]).
///
/// A DIFFERENT key from [`ADVISORY_LOCK_KEY`] on purpose: session and
/// transaction advisory locks share one lock space, and trawld holds the
/// session lock for its whole life, so reusing that key would deadlock
/// every claim and every purge.
pub(crate) const CATALOG_LIFECYCLE_LOCK_KEY: i64 = 0x0074_7261_776c_2143; // "trawl!C"

pub(crate) async fn lock_catalog_lifecycle(tx: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(CATALOG_LIFECYCLE_LOCK_KEY)
        .execute(tx)
        .await
        .map(|_| ())
}

/// One simple-protocol query, so it starts the transaction it runs in
/// unless one is open already: the start of its transaction is the start
/// of the statement only in autocommit. A failed transaction refuses the
/// query with `in_failed_sql_transaction`, and is inside one too.
async fn released_outside_transaction(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    use sqlx::Row as _;
    let outside = match sqlx::raw_sql("SELECT transaction_timestamp() = statement_timestamp()")
        .fetch_one(&mut *conn)
        .await
    {
        Ok(row) => row.try_get(0)?,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("25P02") => false,
        Err(e) => return Err(e),
    };
    if !outside {
        metrics::counter!(
            crate::metrics::POOL_CONNECTIONS_RETURNED_IN_TRANSACTION_TOTAL,
            "pool" => "storage"
        )
        .increment(1);
        tracing::warn!(
            event_type = "pool_connection_returned_in_transaction",
            pool = "storage",
            "a connection came back to the pool inside a transaction; it is closed, not reused"
        );
    }
    Ok(outside)
}

const MAX_CONNECTIONS: u32 = 8;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The upper bound on how long a genuinely lost lock goes undetected, kept
/// short because every second of an undetected loss is a second in which a
/// second writer could start.
const LOCK_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Aborting the task on drop drops the lock connection with it, so a graceful
/// shutdown releases the advisory lock.
#[derive(Debug)]
struct LockGuard(JoinHandle<()>);

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug, Clone)]
pub struct StorageState {
    pool: PgPool,
    pub history: HistoryStore,
    pub saved: SavedQueryStore,
    pub schedule: ScheduleStore,
    pub catalog: CatalogStore,
    pub repin: RepinStore,
    pub results: ResultsStore,
    pub investigations: InvestigationStore,
    _lock_guard: Arc<LockGuard>,
    lock_lost_rx: watch::Receiver<bool>,
    storage_ping: Arc<PingCache>,
}

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
            tracing::debug!(
                event_type = "storage_lock_lost",
                error = %e,
                "sole-writer advisory lock lost: the app-state session died and \
                 postgres has released the lock; another trawld can now acquire it \
                 and become a second writer. terminating to preserve the \
                 single-writer invariant."
            );
            let fault = crate::error::ServerError::from(StoreError::from(e));
            tracing::error!(
                event_type = "storage_lock_lost",
                error_class = fault.error_class(),
                cause_kind = fault.cause_kind().as_str(),
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
            .after_release(|conn, _| Box::pin(released_outside_transaction(conn)))
            .connect(database_url)
            .await
            .map_err(StoreError::Unavailable)?;

        Self::from_pool(pool).await
    }

    /// The pool's connect target must never be reconfigured for the LIFETIME of
    /// the returned [`StorageState`]. A sqlx 0.9 `Pool` clone shares one `Arc`, so
    /// `set_connect_options` on a clone the caller kept repoints every
    /// connection the stores open from then on, while the detached lock
    /// session below stays on the old database.
    ///
    /// Boot order is load-bearing here, not in the caller: take the session
    /// advisory lock BEFORE migrate.
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

        migrations::migrate(&pool).await?;

        let (lost_tx, lock_lost_rx) = watch::channel(false);
        let guard = tokio::spawn(guard_advisory_lock(lock_conn, lost_tx));

        tracing::info!(
            event_type = "storage_ready",
            "app-state database migrated and advisory-locked"
        );

        let results = ResultsStore::new(pool.clone());
        Ok(Self {
            history: HistoryStore::new(pool.clone()),
            saved: SavedQueryStore::new(pool.clone()),
            schedule: ScheduleStore::new(pool.clone()),
            catalog: CatalogStore::new(pool.clone()),
            repin: RepinStore::new(pool.clone()),
            investigations: results.investigations(),
            results,
            pool,
            _lock_guard: Arc::new(LockGuard(guard)),
            lock_lost_rx,
            storage_ping: Arc::new(PingCache::new(None)),
        })
    }

    #[must_use]
    pub fn lock_lost(&self) -> watch::Receiver<bool> {
        self.lock_lost_rx.clone()
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

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

    const PING_TIMEOUT: Duration = Duration::from_secs(2);

    const PING_CACHE_TTL: Duration = Duration::from_secs(5);
}

#[cfg(test)]
mod pg_tests {
    use std::time::Duration;

    use sqlx::{ConnectOptions as _, PgPool};

    use super::StorageState;

    async fn lock_held(pool: &PgPool) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_locks
                            WHERE locktype = 'advisory' AND objid = 310
                              AND database = (SELECT oid FROM pg_database
                                              WHERE datname = current_database()))",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = false)]
    async fn a_connection_returned_inside_a_transaction_is_closed(pool: PgPool) {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let proxy = crate::answer_hold::Proxy::start(&pool.connect_options()).await;
        let url = proxy.options(&pool.connect_options()).to_url_lossy();
        let state = StorageState::connect(url.as_str()).await.unwrap();

        proxy.hold_next(b"BEGIN");
        let begin = {
            let pool = state.pool.clone();
            tokio::spawn(async move {
                pool.begin_with("BEGIN; SELECT pg_advisory_xact_lock(310)")
                    .await
                    .map(drop)
            })
        };
        proxy.holding().await;
        begin.abort();
        assert!(begin.await.unwrap_err().is_cancelled());
        proxy.release();

        let gone = tokio::time::timeout(Duration::from_secs(10), async {
            while lock_held(&pool).await {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            gone.is_ok(),
            "the dropped transaction's lock is still held: its session is back in the pool"
        );
        let settled = tokio::time::timeout(Duration::from_secs(10), async {
            while state.pool.num_idle() != state.pool.size() as usize {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(settled.is_ok(), "a connection never finished coming back");
        let outside: bool = sqlx::Row::get(
            &sqlx::raw_sql("SELECT transaction_timestamp() = statement_timestamp()")
                .fetch_one(&state.pool)
                .await
                .unwrap(),
            0,
        );
        assert!(
            outside,
            "the pool's next statement ran inside a transaction"
        );
        assert_eq!(
            crate::metrics::test_support::sample(
                &handle,
                "trawl_pool_connections_returned_in_transaction_total{pool=\"storage\"}"
            ),
            1
        );
    }
}
