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
//!
//! The ingest ceiling is additionally gated on [`Permission::Ingest`]: the
//! handler's permission check runs downstream of this middleware (axum resolves
//! every extractor, including the 16 MB `body: Bytes`, before the handler body
//! runs), so without the gate ANY trawl-granted key — reader included — would
//! ride the shipper-sized bucket on the heaviest endpoint. Keys that cannot
//! ingest stay on the interactive buckets, the same map the query routes use.
//! This is route-class eligibility, not per-role bucketing: buckets are still
//! keyed solely by `VerifiedKey.id`.

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
use crate::policy::{Permission, TrawlAuthz as _};

/// A keyed rate limiter: one bucket per API key id.
type KeyedLimiter = RateLimiter<i64, DashMapStateStore<i64>, DefaultClock>;

/// Per-key rate limiter for one route class. `None` limiters mean rate
/// limiting is disabled for that class.
#[derive(Debug, Clone)]
pub struct RateLimitState {
    class: RouteClass,
}

/// Which bucket map(s) a route class draws on.
#[derive(Debug, Clone)]
enum RouteClass {
    /// Interactive API routes: every authenticated key shares the `default_rpm`
    /// ceiling on its own per-key bucket.
    Interactive(Option<Arc<KeyedLimiter>>),
    /// `/api/v1/ingest`: shipper-sized `ingest_rpm` buckets for keys that hold
    /// [`Permission::Ingest`], interactive buckets for every other key.
    Ingest {
        shipper: Option<Arc<KeyedLimiter>>,
        interactive: Option<Arc<KeyedLimiter>>,
    },
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
            class: RouteClass::Interactive(make_limiter(config.default_rpm)),
        }
    }

    /// Limiter for `/api/v1/ingest`, on its own bucket map so a shipper-sized
    /// ceiling never leaks onto the query routes. An `ingest_rpm` of 0
    /// disables limiting.
    ///
    /// Only keys holding [`Permission::Ingest`] earn that ceiling. Everyone
    /// else shares `interactive`'s buckets — literally the same map the query
    /// routes use, so hitting `/ingest` cannot buy a key extra interactive
    /// budget either.
    pub fn ingest(config: &RateLimitConfig, interactive: &Self) -> Self {
        let fallback = match &interactive.class {
            RouteClass::Interactive(limiter) => limiter.clone(),
            RouteClass::Ingest { interactive, .. } => interactive.clone(),
        };
        Self {
            class: RouteClass::Ingest {
                shipper: make_limiter(config.ingest_rpm),
                interactive: fallback,
            },
        }
    }

    /// The bucket map this request must draw on, given whether the calling key
    /// may ingest. `None` means the applicable class has limiting disabled.
    fn limiter_for(&self, may_ingest: bool) -> Option<&Arc<KeyedLimiter>> {
        match &self.class {
            RouteClass::Interactive(limiter) => limiter.as_ref(),
            RouteClass::Ingest {
                shipper,
                interactive,
            } => {
                if may_ingest {
                    shipper.as_ref()
                } else {
                    interactive.as_ref()
                }
            }
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
///
/// On `/api/v1/ingest` the ceiling depends on the key: only keys holding
/// [`Permission::Ingest`] get the shipper-sized bucket (see
/// [`RateLimitState::ingest`]).
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

    if let Some(limiter) = rate_state.limiter_for(verified.has_permission(Permission::Ingest))
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
        assert!(state.limiter_for(false).is_none());
    }

    #[test]
    fn rate_limit_rejects_after_burst() {
        let state = RateLimitState::interactive(&config_with_rpm(5));
        let limiter = state.limiter_for(false).unwrap();

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
        let ingest = RateLimitState::ingest(&config, &interactive);
        let key_id: i64 = 7;

        // Same key, same id: exhausting the interactive bucket must not touch
        // the ingest one, and the ingest ceiling must not leak the other way.
        let interactive_limiter = interactive.limiter_for(false).unwrap();
        assert!(interactive_limiter.check_key(&key_id).is_ok());
        assert!(
            interactive_limiter.check_key(&key_id).is_err(),
            "interactive burst is default_rpm (1), not ingest_rpm"
        );

        // `true` = the key holds Permission::Ingest, so it earns ingest_rpm.
        let ingest_limiter = ingest.limiter_for(true).unwrap();
        let mut accepted = 0;
        for _ in 0..20 {
            if ingest_limiter.check_key(&key_id).is_ok() {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 5, "ingest burst is ingest_rpm (5)");
    }

    /// The shipper-sized ceiling is earned by `Permission::Ingest`, not by
    /// reaching the route: the handler rejects permissionless keys only AFTER
    /// this middleware, so an ungated ingest bucket would hand every reader key
    /// the shipper budget on the heaviest endpoint.
    #[test]
    fn ingest_ceiling_needs_the_ingest_permission() {
        let config = RateLimitConfig {
            default_rpm: 1,
            ingest_rpm: 5,
            ..RateLimitConfig::default()
        };
        let interactive = RateLimitState::interactive(&config);
        let ingest = RateLimitState::ingest(&config, &interactive);
        let key_id: i64 = 11;

        // A key without the permission draws on the interactive buckets —
        // the very same map the query routes use, not a second allowance.
        let unprivileged = ingest.limiter_for(false).unwrap();
        assert!(unprivileged.check_key(&key_id).is_ok());
        assert!(
            unprivileged.check_key(&key_id).is_err(),
            "permissionless keys are held to default_rpm (1) on /ingest"
        );
        assert!(
            interactive
                .limiter_for(false)
                .unwrap()
                .check_key(&key_id)
                .is_err(),
            "the fallback shares the interactive bucket map, so the interactive quota is spent too"
        );
    }

    #[test]
    fn different_key_ids_have_independent_limits() {
        let state = RateLimitState::interactive(&config_with_rpm(2));
        let limiter = state.limiter_for(false).unwrap();

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
