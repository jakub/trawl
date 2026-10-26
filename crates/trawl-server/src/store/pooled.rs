// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A transaction on a pooled connection that no cancellation returns to
//! the pool open.
//!
//! sqlx counts a transaction it begins only once Postgres has answered the
//! `BEGIN`. A future dropped while that answer is on its way queues no
//! `ROLLBACK`, and sqlx's own return path pings the connection and accepts
//! its in-transaction state. A caller that later acquired it would run
//! inside a transaction it never opened: its writes would report success
//! before any commit, its locks would stay held, and unrelated work would
//! later commit or roll them back.
//!
//! No pooled connection returns inside a transaction. The app-state pool
//! checks every connection it takes back and closes one still inside a
//! transaction ([`super`] module docs), at one round trip per release; that
//! catches every path, a raw `pool.begin()` included. [`PooledTransaction`]
//! does not wait for the check: it opens its transaction on a connection
//! already acquired, and from the moment it sends `BEGIN` until Postgres
//! answers its `COMMIT` or `ROLLBACK`, dropping it closes the connection
//! instead of returning it. Postgres ends the transaction with the
//! session, and nothing waits for an answer still on its way.
//!
//! The rule for a pooled connection: a client-side bound may cut short the
//! acquire, where nothing has been sent, or an autocommit statement, which
//! leaves no transaction behind (the return path waits for its answer).
//! A transaction is opened as a [`PooledTransaction`], its statements are
//! bounded in Postgres, and it ends with an awaited `COMMIT` or
//! `ROLLBACK`. A client-side bound may cut short its `BEGIN`, a statement
//! or its `ROLLBACK` only because the drop closes the connection, and a
//! transaction cut short so ends by that close, never by a later
//! `ROLLBACK`: the connection would first have to read the answer the
//! bound stopped awaiting. A `COMMIT` already sent is never cut short.

use std::ops::{Deref, DerefMut};

use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnection, Postgres};

/// A transaction on a pooled connection (module docs). Statements run on
/// the connection it dereferences to.
#[derive(Debug)]
pub(crate) struct PooledTransaction {
    conn: PoolConnection<Postgres>,
    /// From before `BEGIN` is sent until Postgres has answered `COMMIT` or
    /// `ROLLBACK`: while it holds, a drop closes the connection.
    open: bool,
}

impl PooledTransaction {
    /// Begin a transaction on `conn`, awaiting Postgres' answer. A failed
    /// `BEGIN` closes the connection.
    pub(crate) async fn begin(conn: PoolConnection<Postgres>) -> Result<Self, sqlx::Error> {
        let mut tx = Self { conn, open: true };
        sqlx::raw_sql("BEGIN").execute(&mut *tx.conn).await?;
        Ok(tx)
    }

    /// Commit, and give the connection back to the pool once Postgres has
    /// answered. A failed commit closes the connection.
    pub(crate) async fn commit(mut self) -> Result<(), sqlx::Error> {
        sqlx::raw_sql("COMMIT").execute(&mut *self.conn).await?;
        self.open = false;
        Ok(())
    }

    /// Roll back, and give the connection back to the pool once Postgres
    /// has answered. A failed rollback closes the connection, which ends
    /// the transaction too.
    pub(crate) async fn rollback(mut self) -> Result<(), sqlx::Error> {
        sqlx::raw_sql("ROLLBACK").execute(&mut *self.conn).await?;
        self.open = false;
        Ok(())
    }
}

impl Deref for PooledTransaction {
    type Target = PgConnection;

    fn deref(&self) -> &PgConnection {
        &self.conn
    }
}

impl DerefMut for PooledTransaction {
    fn deref_mut(&mut self) -> &mut PgConnection {
        &mut self.conn
    }
}

impl Drop for PooledTransaction {
    fn drop(&mut self) {
        if self.open {
            self.conn.close_on_drop();
        }
    }
}

#[cfg(test)]
mod pg_tests {
    use sqlx::PgPool;

    use super::PooledTransaction;

    /// The session's backend pid, and whether it runs outside any
    /// transaction. One simple-protocol query, so the statement starts
    /// the transaction it runs in unless one is already open; the extended
    /// protocol stamps a new statement time at each message.
    async fn session(conn: &mut sqlx::PgConnection) -> (i32, bool) {
        use sqlx::Row as _;
        let row = sqlx::raw_sql(
            "SELECT pg_backend_pid(), transaction_timestamp() = statement_timestamp()",
        )
        .fetch_one(conn)
        .await
        .unwrap();
        (row.get(0), row.get(1))
    }

    /// A transaction dropped open never reaches the pool again: the next
    /// acquire gets another session, outside any transaction, and the
    /// dropped one's transaction-scoped lock is gone with it.
    #[sqlx::test(migrations = false)]
    async fn a_transaction_dropped_open_closes_its_connection(pool: PgPool) {
        let mut tx = PooledTransaction::begin(pool.acquire().await.unwrap())
            .await
            .unwrap();
        let dropped: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(310)")
            .execute(&mut *tx)
            .await
            .unwrap();
        drop(tx);

        let mut next = pool.acquire().await.unwrap();
        let (pid, outside) = session(&mut next).await;
        assert_ne!(pid, dropped, "the dropped transaction's session came back");
        assert!(outside, "the next acquire runs inside a transaction");
        let gone = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let held: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_locks
                     WHERE locktype = 'advisory' AND objid = 310
                       AND database = (SELECT oid FROM pg_database
                                       WHERE datname = current_database())",
                )
                .fetch_one(&mut *next)
                .await
                .unwrap();
                if held == 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "the closed session's lock outlived it");
    }

    /// A committed transaction gives its session back to the pool, outside
    /// any transaction.
    #[sqlx::test(migrations = false)]
    async fn a_committed_transaction_returns_its_connection(pool: PgPool) {
        let mut tx = PooledTransaction::begin(pool.acquire().await.unwrap())
            .await
            .unwrap();
        let committed: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while pool.num_idle() == 0 {
            assert!(std::time::Instant::now() < give_up, "never returned");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let (pid, outside) = session(&mut pool.acquire().await.unwrap()).await;
        assert_eq!(pid, committed, "the committed session was not reused");
        assert!(outside, "the reused session is still inside a transaction");
    }
}
