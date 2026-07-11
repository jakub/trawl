// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-role rate limiting middleware using the governor crate (GCRA algorithm).
//!
//! Each role gets an independent keyed rate limiter, keyed by API key prefix.
//! This means separate keys within the same role have independent quotas,
//! and different roles can have different request-per-minute limits.

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
use crate::policy::{Role, TrawlAuthz as _};

/// A keyed rate limiter: one bucket per API key prefix within a role.
type KeyedLimiter = RateLimiter<String, DashMapStateStore<String>, DefaultClock>;

/// Per-role rate limiters. `None` means rate limiting is disabled for that role.
#[derive(Debug, Clone)]
pub struct RateLimitState {
    admin: Option<Arc<KeyedLimiter>>,
    analyst: Option<Arc<KeyedLimiter>>,
    reader: Option<Arc<KeyedLimiter>>,
    ingest: Option<Arc<KeyedLimiter>>,
}

/// Build a keyed limiter for a given requests-per-minute value.
/// Returns `None` if `rpm` is 0 (disabled).
fn make_limiter(rpm: u32) -> Option<Arc<KeyedLimiter>> {
    let nz = NonZeroU32::new(rpm)?;
    let quota = Quota::per_minute(nz);
    Some(Arc::new(RateLimiter::keyed(quota)))
}

impl RateLimitState {
    /// Build rate limiters from config. A rate of 0 disables limiting for that role.
    pub fn from_config(config: &RateLimitConfig) -> Self {
        Self {
            admin: make_limiter(config.admin),
            analyst: make_limiter(config.analyst),
            reader: make_limiter(config.reader),
            ingest: make_limiter(config.ingest),
        }
    }

    /// Get the limiter for a given role, if rate limiting is enabled for it.
    fn limiter_for_role(&self, role: Role) -> Option<&Arc<KeyedLimiter>> {
        match role {
            Role::Admin => self.admin.as_ref(),
            Role::Analyst => self.analyst.as_ref(),
            Role::Reader => self.reader.as_ref(),
            Role::Ingest => self.ingest.as_ref(),
        }
    }
}

/// Axum middleware that enforces per-role, per-key rate limits.
///
/// Must run AFTER the auth middleware (needs [`VerifiedKey`] in extensions).
/// Returns 429 when the rate limit is exceeded.
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

    // Per-role rate limiting is gated on the trawl-app role. Keys without
    // a trawl grant can no longer reach this middleware — the mandatory
    // `require_trawl_grant` policy layer 403s them first — but the bypass
    // branch stays as belt-and-suspenders (AC3 asserts grantless keys never
    // get here).
    if let Some(trawl_role) = verified.trawl_role() {
        if let Some(limiter) = rate_state.limiter_for_role(trawl_role)
            && limiter.check_key(&verified.prefix).is_err()
        {
            tracing::warn!(
                event_type = "rate_limit_exceeded",
                user = %verified.name,
                role = %trawl_role,
                prefix = %verified.prefix,
                "rate limit exceeded"
            );
            return Err(ServerError::RateLimited);
        }
    } else {
        tracing::debug!(
            prefix = %verified.prefix,
            name = %verified.name,
            "rate limiter bypassed: no trawl-app grant"
        );
    }

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_rpm_disables_limiter() {
        let config = RateLimitConfig {
            admin: 0,
            analyst: 60,
            reader: 0,
            ingest: 1000,
        };
        let state = RateLimitState::from_config(&config);
        assert!(state.admin.is_none());
        assert!(state.analyst.is_some());
        assert!(state.reader.is_none());
        assert!(state.ingest.is_some());
    }

    #[test]
    fn limiter_for_role_returns_correct_limiter() {
        let config = RateLimitConfig {
            admin: 100,
            analyst: 60,
            reader: 30,
            ingest: 1000,
        };
        let state = RateLimitState::from_config(&config);
        assert!(state.limiter_for_role(Role::Admin).is_some());
        assert!(state.limiter_for_role(Role::Analyst).is_some());
        assert!(state.limiter_for_role(Role::Reader).is_some());
        assert!(state.limiter_for_role(Role::Ingest).is_some());
    }

    #[test]
    fn rate_limit_rejects_after_burst() {
        let config = RateLimitConfig {
            admin: 0,
            analyst: 5, // 5 req/min — very tight for testing
            reader: 0,
            ingest: 0,
        };
        let state = RateLimitState::from_config(&config);
        let limiter = state.limiter_for_role(Role::Analyst).unwrap();

        let key = "testprefix".to_owned();

        // Burst should allow some requests, then start rejecting.
        let mut accepted = 0;
        for _ in 0..20 {
            if limiter.check_key(&key).is_ok() {
                accepted += 1;
            }
        }

        // With 5 rpm, the burst capacity is 5 — first 5 should pass.
        assert_eq!(accepted, 5, "expected burst of 5 for 5 rpm quota");
    }

    #[test]
    fn different_keys_have_independent_limits() {
        let config = RateLimitConfig {
            admin: 0,
            analyst: 2,
            reader: 0,
            ingest: 0,
        };
        let state = RateLimitState::from_config(&config);
        let limiter = state.limiter_for_role(Role::Analyst).unwrap();

        // Exhaust key1's limit.
        let key1 = "key1_prefix".to_owned();
        for _ in 0..10 {
            let _ = limiter.check_key(&key1);
        }
        assert!(limiter.check_key(&key1).is_err(), "key1 should be limited");

        // key2 should still have its own independent quota.
        let key2 = "key2_prefix".to_owned();
        assert!(
            limiter.check_key(&key2).is_ok(),
            "key2 should not be limited"
        );
    }
}
