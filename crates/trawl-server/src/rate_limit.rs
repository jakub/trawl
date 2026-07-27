// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-key rate limiting middleware using the governor crate (GCRA algorithm).
//!
//! Every API key gets an independent token bucket keyed by its keystore row
//! id (`VerifiedKey.id`) with a config-default RPM ceiling (ADR-0006 slice 0).
//! Buckets are created lazily on first request and never evicted: the map is
//! bounded by the keystore's key count (dozens in this deployment class), the
//! same bound the old prefix-keyed store had. Per-role class-of-service
//! returns in slice 1 as a `rate_rpm` role attribute.
//!
//! The ceiling is per *route class*, not per principal: the interactive API
//! routes use `default_rpm` and `/api/v1/ingest` uses `ingest_rpm`, each with
//! its own bucket map wired by the router. A log shipper needs orders of
//! magnitude more requests/minute than a human running `DuckDB` scans, so one
//! shared number would have to be sized for the shipper — handing every
//! interactive key that budget.

use std::num::NonZeroU32;
use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use fleet_auth::VerifiedKey;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;

use crate::config::RateLimitConfig;
use crate::error::ServerError;

/// A keyed rate limiter: one bucket per API key id.
type KeyedLimiter = RateLimiter<i64, DashMapStateStore<i64>, DefaultClock>;

/// Per-key rate limiter. `None` means rate limiting is disabled.
#[derive(Debug, Clone)]
pub struct RateLimitState {
    limiter: Option<Arc<KeyedLimiter>>,
}

/// Build a keyed limiter for a given requests-per-minute value.
/// Returns `None` if `rpm` is 0 (disabled).
fn make_limiter(rpm: u32) -> Option<Arc<KeyedLimiter>> {
    let nz = NonZeroU32::new(rpm)?;
    let quota = Quota::per_minute(nz);
    Some(Arc::new(RateLimiter::keyed(quota)))
}

impl RateLimitState {
    /// Limiter for the interactive API routes (query, export, stream, …).
    /// A `default_rpm` of 0 disables limiting.
    pub fn interactive(config: &RateLimitConfig) -> Self {
        Self {
            limiter: make_limiter(config.default_rpm),
        }
    }

    /// Limiter for `/api/v1/ingest`, on its own bucket map so a shipper-sized
    /// ceiling never leaks onto the query routes. An `ingest_rpm` of 0
    /// disables limiting.
    pub fn ingest(config: &RateLimitConfig) -> Self {
        Self {
            limiter: make_limiter(config.ingest_rpm),
        }
    }
}

/// Axum middleware that enforces per-key rate limits.
///
/// Must run AFTER the auth middleware (needs [`VerifiedKey`] in extensions)
/// and INSIDE the mandatory `require_trawl_grant` policy layer — grantless
/// keys 403 before ever reaching this middleware (pinned by the
/// `ac3_grantless_key_never_reaches_rate_limiter` integration test), so no
/// bypass branch is needed here. Returns 429 when the rate limit is exceeded.
pub async fn rate_limit_middleware(request: Request, next: Next) -> Result<Response, ServerError> {
    let rate_state = request
        .extensions()
        .get::<RateLimitState>()
        .cloned()
        .ok_or_else(|| ServerError::Internal("rate limit state not in extensions".into()))?;

    let verified = request
        .extensions()
        .get::<VerifiedKey>()
        .ok_or_else(|| ServerError::Internal("verified key not in extensions".into()))?;

    if let Some(limiter) = rate_state.limiter.as_ref()
        && limiter.check_key(&verified.id).is_err()
    {
        tracing::warn!(
            event_type = "rate_limit_exceeded",
            user = %verified.name,
            key_id = verified.id,
            prefix = %verified.prefix,
            "rate limit exceeded"
        );
        return Err(ServerError::RateLimited);
    }

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_rpm(default_rpm: u32) -> RateLimitConfig {
        RateLimitConfig {
            default_rpm,
            ..RateLimitConfig::default()
        }
    }

    #[test]
    fn zero_rpm_disables_limiter() {
        let state = RateLimitState::interactive(&config_with_rpm(0));
        assert!(state.limiter.is_none());
    }

    #[test]
    fn rate_limit_rejects_after_burst() {
        let state = RateLimitState::interactive(&config_with_rpm(5));
        let limiter = state.limiter.as_ref().unwrap();

        let key_id: i64 = 42;

        // Burst should allow some requests, then start rejecting.
        let mut accepted = 0;
        for _ in 0..20 {
            if limiter.check_key(&key_id).is_ok() {
                accepted += 1;
            }
        }

        // With 5 rpm, the burst capacity is 5 — first 5 should pass.
        assert_eq!(accepted, 5, "expected burst of 5 for 5 rpm quota");
    }

    #[test]
    fn ingest_ceiling_is_separate_from_the_interactive_one() {
        let config = RateLimitConfig {
            default_rpm: 1,
            ingest_rpm: 5,
            ..RateLimitConfig::default()
        };
        let interactive = RateLimitState::interactive(&config);
        let ingest = RateLimitState::ingest(&config);
        let key_id: i64 = 7;

        // Same key, same id: exhausting the interactive bucket must not touch
        // the ingest one, and the ingest ceiling must not leak the other way.
        let interactive_limiter = interactive.limiter.as_ref().unwrap();
        assert!(interactive_limiter.check_key(&key_id).is_ok());
        assert!(
            interactive_limiter.check_key(&key_id).is_err(),
            "interactive burst is default_rpm (1), not ingest_rpm"
        );

        let ingest_limiter = ingest.limiter.as_ref().unwrap();
        let mut accepted = 0;
        for _ in 0..20 {
            if ingest_limiter.check_key(&key_id).is_ok() {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 5, "ingest burst is ingest_rpm (5)");
    }

    #[test]
    fn different_key_ids_have_independent_limits() {
        let state = RateLimitState::interactive(&config_with_rpm(2));
        let limiter = state.limiter.as_ref().unwrap();

        // Exhaust key id 1's bucket.
        let key_a: i64 = 1;
        for _ in 0..10 {
            let _ = limiter.check_key(&key_a);
        }
        assert!(
            limiter.check_key(&key_a).is_err(),
            "key A should be limited"
        );

        // Key id 2 still has its own independent quota — the AC1 core:
        // exhausting one key never starves another, role or no role.
        let key_b: i64 = 2;
        assert!(
            limiter.check_key(&key_b).is_ok(),
            "key B should not be limited"
        );
    }
}
