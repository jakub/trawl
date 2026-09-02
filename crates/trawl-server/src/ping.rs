// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Memoised, timeout-bounded liveness pings shared by `/health` probes.
//!
//! `/health` is unauthenticated and unthrottled, so a burst of probes must
//! not stampede the small, shared connection pools (fleet keystore,
//! app-state store) that request handling depends on. Each subsystem keeps a
//! [`PingCache`]; within the TTL every probe reuses the last outcome and
//! touches no connection, and refreshes are serialised so at most one
//! in-flight ping holds a connection at any instant.

use std::time::{Duration, Instant};

/// A liveness-ping outcome and the instant it was taken; the caller's `ttl`
/// decides when it has expired.
#[derive(Debug, Clone)]
pub struct CachedPing {
    /// Ping outcome: `Ok(())` on success, `Err(msg)` on failure/timeout.
    result: Result<(), String>,
    checked_at: Instant,
}

/// Shared memoised ping slot.
pub type PingCache = tokio::sync::Mutex<Option<CachedPing>>;

/// Memoise a liveness ping behind a TTL and a timeout bound.
///
/// Within `ttl` of the last probe the cached outcome (success *or* failure)
/// is returned and `ping` is never invoked; otherwise `ping` runs under a
/// `timeout` bound and its result — including a timeout mapped to `Err` —
/// is cached before returning.
pub async fn ping_cached_with<F, Fut, E>(
    cache: &PingCache,
    ttl: Duration,
    timeout: Duration,
    what: &str,
    ping: F,
) -> Result<(), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    let mut guard = cache.lock().await;
    if let Some(cached) = guard.as_ref()
        && cached.checked_at.elapsed() < ttl
    {
        return cached.result.clone();
    }

    let result = match tokio::time::timeout(timeout, ping()).await {
        Ok(res) => res.map_err(|e| e.to_string()),
        Err(_) => Err(format!(
            "{what} ping timed out after {}s",
            timeout.as_secs()
        )),
    };
    *guard = Some(CachedPing {
        result: result.clone(),
        checked_at: Instant::now(),
    });
    result
}

#[cfg(test)]
mod tests {
    use super::{CachedPing, ping_cached_with};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    const LONG_TTL: Duration = Duration::from_hours(1);
    const LONG_TIMEOUT: Duration = Duration::from_mins(1);

    /// A successful ping is cached: within the TTL a second probe returns the
    /// stored `Ok` without re-invoking the pinger.
    #[tokio::test]
    async fn caches_ok_within_ttl() {
        let cache: TokioMutex<Option<CachedPing>> = TokioMutex::new(None);
        let calls = AtomicUsize::new(0);
        let ping = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<(), String>(())
        };

        assert_eq!(
            ping_cached_with(&cache, LONG_TTL, LONG_TIMEOUT, "keystore", ping).await,
            Ok(())
        );
        assert_eq!(
            ping_cached_with(&cache, LONG_TTL, LONG_TIMEOUT, "keystore", ping).await,
            Ok(())
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second probe must reuse the cache"
        );
    }

    /// A failed ping is cached too: within the TTL the downed-backend error is
    /// replayed without touching the pinger, so a burst cannot stampede a dead
    /// backend.
    #[tokio::test]
    async fn caches_err_within_ttl() {
        let cache: TokioMutex<Option<CachedPing>> = TokioMutex::new(None);
        let calls = AtomicUsize::new(0);
        let ping = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), String>("backend down".to_string())
        };

        let first = ping_cached_with(&cache, LONG_TTL, LONG_TIMEOUT, "keystore", ping).await;
        let second = ping_cached_with(&cache, LONG_TTL, LONG_TIMEOUT, "keystore", ping).await;
        assert_eq!(first, Err("backend down".to_string()));
        assert_eq!(second, Err("backend down".to_string()));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "errors must be cached, not retried"
        );
    }

    /// Once the TTL has elapsed the cache is bypassed and the pinger runs again.
    /// A zero TTL makes every probe expired, so each call re-probes.
    #[tokio::test]
    async fn refreshes_after_ttl_expiry() {
        let cache: TokioMutex<Option<CachedPing>> = TokioMutex::new(None);
        let calls = AtomicUsize::new(0);
        let ping = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<(), String>(())
        };

        ping_cached_with(&cache, Duration::ZERO, LONG_TIMEOUT, "keystore", ping)
            .await
            .unwrap();
        ping_cached_with(&cache, Duration::ZERO, LONG_TIMEOUT, "keystore", ping)
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "expired cache must re-probe"
        );
    }

    /// A ping that outlives the timeout bound is mapped to a timeout `Err`
    /// rather than blocking, and that error is cached like any other outcome.
    #[tokio::test(start_paused = true)]
    async fn maps_timeout_to_err() {
        let cache: TokioMutex<Option<CachedPing>> = TokioMutex::new(None);
        let timeout = Duration::from_secs(2);
        let ping = || async {
            // Far outlives the 2s bound; paused-clock auto-advance fires the
            // timeout first, so the test itself never really waits.
            tokio::time::sleep(Duration::from_hours(1)).await;
            Ok::<(), String>(())
        };

        let result = ping_cached_with(&cache, LONG_TTL, timeout, "keystore", ping).await;
        assert_eq!(result, Err("keystore ping timed out after 2s".to_string()));

        // The timeout error is now cached: a probe within the TTL replays it
        // without invoking a (this time instant) pinger.
        let cached = ping_cached_with(&cache, LONG_TTL, timeout, "keystore", || async {
            Ok::<(), String>(())
        })
        .await;
        assert_eq!(cached, Err("keystore ping timed out after 2s".to_string()));
    }
}
