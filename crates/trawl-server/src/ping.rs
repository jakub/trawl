// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Memoised, timeout-bounded liveness pings shared by `/health` probes.
//!
//! `/health` is unauthenticated and unthrottled, so a burst of probes must
//! not stampede the small, shared connection pools (fleet keystore,
//! app-state store) that request handling depends on.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct CachedPing {
    result: Result<(), String>,
    checked_at: Instant,
}

pub type PingCache = tokio::sync::Mutex<Option<CachedPing>>;

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

        let cached = ping_cached_with(&cache, LONG_TTL, timeout, "keystore", || async {
            Ok::<(), String>(())
        })
        .await;
        assert_eq!(cached, Err("keystore ping timed out after 2s".to_string()));
    }
}
