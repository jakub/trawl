// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bearer token authentication middleware for axum.
//!
//! Extracts the `Authorization: Bearer <token>` header, verifies the
//! token against the `KeyStore` in a blocking task, and injects the
//! [`VerifiedKey`] into request extensions for downstream handlers.
//!
//! An in-memory TTL cache ([`AuthCache`]) skips argon2id verification
//! on cache hits, improving authed throughput by ~10x.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use trawl_auth::keys::VerifiedKey;
use trawl_auth::store::KeyStore;

use crate::error::ServerError;

/// Cached authentication entry.
struct CachedAuth {
    verified: VerifiedKey,
    cached_at: Instant,
}

/// TTL-based auth token cache backed by `DashMap`.
///
/// On hit, skips argon2id + `SQLite` entirely. On miss, verifies
/// normally and inserts. Revoked keys stay valid for up to TTL
/// (acceptable for homelab — default 5 min).
#[derive(Debug)]
pub struct AuthCache {
    entries: DashMap<String, CachedAuth>,
    ttl: Duration,
}

impl AuthCache {
    /// Create a new cache with the given TTL.
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            ttl,
        }
    }

    /// Look up a token. Returns `None` on miss or expiry.
    fn get(&self, token: &str) -> Option<VerifiedKey> {
        let entry = self.entries.get(token)?;
        if entry.cached_at.elapsed() < self.ttl {
            Some(entry.verified.clone())
        } else {
            drop(entry); // release ref before removal
            self.entries.remove(token);
            None
        }
    }

    /// Insert a verified key into the cache.
    fn insert(&self, token: String, verified: VerifiedKey) {
        self.entries.insert(
            token,
            CachedAuth {
                verified,
                cached_at: Instant::now(),
            },
        );
    }
}

// DashMap's Debug doesn't include values, so our derived Debug is fine.
impl std::fmt::Debug for CachedAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedAuth")
            .field("user", &self.verified.name)
            .field("role", &self.verified.role)
            .finish_non_exhaustive()
    }
}

/// Axum middleware that authenticates requests via bearer token.
///
/// On success, injects [`VerifiedKey`] into request extensions.
/// Returns 401 on missing/invalid/expired/revoked tokens.
pub async fn auth_middleware(request: Request, next: Next) -> Result<Response, ServerError> {
    let path = request.uri().path().to_owned();

    let key_store = request
        .extensions()
        .get::<Arc<Mutex<KeyStore>>>()
        .cloned()
        .ok_or_else(|| ServerError::Internal("key_store not in extensions".into()))?;

    let auth_cache = request.extensions().get::<Arc<AuthCache>>().cloned();

    let Some(raw_token) = extract_bearer_token(request.headers()) else {
        tracing::warn!(event_type = "auth_failure", path = %path, reason = "missing_header", "auth failed: missing or malformed Authorization header");
        return Err(ServerError::Unauthorized(
            "missing or invalid Authorization header".into(),
        ));
    };

    // Fast path: check cache before expensive argon2id verification.
    if let Some(ref cache) = auth_cache
        && let Some(verified) = cache.get(raw_token)
    {
        tracing::debug!(
            event_type = "auth_cache_hit",
            user = %verified.name,
            path = %path,
            "authenticated (cached)"
        );
        let mut request = request;
        request.extensions_mut().insert(verified);
        return Ok(next.run(request).await);
    }

    let token = raw_token.to_owned();
    let token_for_cache = token.clone();

    let verify_start = Instant::now();
    let verified = match tokio::task::spawn_blocking(move || {
        let store = key_store.lock();
        store.verify_key(&token)
    })
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(auth_err)) => {
            tracing::warn!(event_type = "auth_failure", path = %path, reason = "invalid_token", "auth failed: invalid or revoked token");
            return Err(ServerError::Auth(auth_err));
        }
        Err(join_err) => {
            tracing::error!(event_type = "auth_failure", path = %path, reason = "task_panic", error = %join_err, "auth task panicked");
            return Err(ServerError::Internal(format!(
                "auth task panicked: {join_err}"
            )));
        }
    };

    let verify_ms = verify_start.elapsed().as_millis();
    tracing::info!(
        event_type = "auth_cache_miss",
        user = %verified.name,
        path = %path,
        verify_ms,
        "authenticated (cache miss, argon2id verified)"
    );

    // Populate cache on successful verification.
    if let Some(cache) = auth_cache {
        cache.insert(token_for_cache, verified.clone());
    }

    let mut request = request;
    request.extensions_mut().insert(verified);

    Ok(next.run(request).await)
}

/// Extract the bearer token from the Authorization header.
///
/// RFC 7235: auth-scheme comparison is case-insensitive.
fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get("authorization")?.to_str().ok()?;
    let bytes = value.as_bytes();
    if bytes.len() < 7 || !bytes[..7].eq_ignore_ascii_case(b"bearer ") {
        return None;
    }
    Some(&value[7..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer flt_testtoken123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), Some("flt_testtoken123"));
    }

    #[test]
    fn rejects_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn rejects_non_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn accepts_lowercase_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "bearer flt_testtoken123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), Some("flt_testtoken123"));
    }

    #[test]
    fn accepts_mixed_case_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "BEARER flt_testtoken123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), Some("flt_testtoken123"));
    }

    #[test]
    fn auth_cache_hit_returns_verified_key() {
        use trawl_auth::roles::Role;

        let cache = AuthCache::new(Duration::from_secs(300));
        let key = VerifiedKey {
            id: 1,
            prefix: "flt_test".into(),
            name: "test-user".into(),
            role: Role::Admin,
        };
        cache.insert("token123".into(), key.clone());

        let result = cache.get("token123");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test-user");
    }

    #[test]
    fn auth_cache_miss_returns_none() {
        let cache = AuthCache::new(Duration::from_secs(300));
        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn auth_cache_expired_returns_none() {
        use trawl_auth::roles::Role;

        let cache = AuthCache::new(Duration::from_millis(1));
        let key = VerifiedKey {
            id: 1,
            prefix: "flt_test".into(),
            name: "test-user".into(),
            role: Role::Admin,
        };
        cache.insert("token123".into(), key);

        // Sleep past TTL.
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get("token123").is_none());
    }
}
