// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-key rate limiting middleware using the governor crate (GCRA algorithm).
//!
//! Every API key gets an independent token bucket keyed by its keystore row
//! id (`VerifiedKey.id`). The ceiling is resolved per request (ADR-0006):
//!
//! - When any of the key's roles set `rate_rpm`, the key's effective RPM is
//!   the max across its roles and replaces the route class's config
//!   default. The two are never max'd or summed together.
//! - Otherwise the route class default applies: interactive API routes use
//!   `default_rpm`, `/api/v1/ingest` uses `ingest_rpm`.
//!
//! governor fixes one quota per keyed limiter, so a per-key ceiling cannot
//! come from a single map: each route class holds a `DashMap` of limiters
//! indexed by effective RPM, buckets still keyed solely by key id. The maps
//! are bounded by the number of distinct `rate_rpm` values operators define
//! (plus the class default), a handful at fleet scale, and buckets by the
//! keystore's key count.
//!
//! A `default_rpm`/`ingest_rpm` of 0 disables limiting for keys on the
//! class default. A role-set override can never be 0 (the schema CHECKs
//! `rate_rpm > 0`), so "0 disables" remains a config-only semantic.
//!
//! The ingest ceiling is additionally gated on [`Permission::Ingest`]: the
//! handler's own permission check runs downstream of this middleware (axum
//! resolves every extractor, including the 16 MB `body: Bytes`, before the
//! handler body runs), so without the gate any trawl-granted key, reader
//! keys included, would ride the shipper-sized bucket on the heaviest
//! endpoint.
//! Keys that cannot ingest stay on the interactive class, the same maps the
//! query routes use. This is route-class eligibility, not per-role
//! bucketing.

use std::num::NonZeroU32;
use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use dashmap::DashMap;
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

/// One route class's limiters: the config default plus a lazily-populated
/// map of keyed limiters indexed by effective RPM.
#[derive(Debug)]
struct ClassLimiters {
    /// The config default for this class. 0 disables limiting for keys
    /// without a role override.
    default_rpm: u32,
    /// Keyed limiter per effective RPM. Two keys resolving the same
    /// effective RPM share a map (same quota) but never a bucket (buckets
    /// are keyed by key id).
    maps: DashMap<u32, Arc<KeyedLimiter>>,
}

impl ClassLimiters {
    fn new(default_rpm: u32) -> Self {
        Self {
            default_rpm,
            maps: DashMap::new(),
        }
    }

    /// The limiter a key with the given role override draws on in this
    /// class. `None` means limiting is disabled (config default 0 and no
    /// role override — overrides are CHECK-constrained > 0).
    fn limiter_for(&self, rate_rpm_override: Option<u32>) -> Option<Arc<KeyedLimiter>> {
        let effective = rate_rpm_override.unwrap_or(self.default_rpm);
        let nz = NonZeroU32::new(effective)?;
        Some(
            self.maps
                .entry(effective)
                .or_insert_with(|| Arc::new(RateLimiter::keyed(Quota::per_minute(nz))))
                .clone(),
        )
    }
}

/// Per-key rate limiter state for one route class.
#[derive(Debug, Clone)]
pub struct RateLimitState {
    class: RouteClass,
}

/// Which limiter class(es) a route draws on.
#[derive(Debug, Clone)]
enum RouteClass {
    /// Interactive API routes: `default_rpm` ceiling unless a role override
    /// applies.
    Interactive(Arc<ClassLimiters>),
    /// `/api/v1/ingest`: shipper-sized `ingest_rpm` ceiling for keys that
    /// hold [`Permission::Ingest`], the interactive class for every other
    /// key.
    Ingest {
        shipper: Arc<ClassLimiters>,
        interactive: Arc<ClassLimiters>,
    },
}

impl RateLimitState {
    /// Limiter for the interactive API routes (query, export, stream, …).
    /// A `default_rpm` of 0 disables limiting for keys without a role
    /// override.
    pub fn interactive(config: &RateLimitConfig) -> Self {
        Self {
            class: RouteClass::Interactive(Arc::new(ClassLimiters::new(config.default_rpm))),
        }
    }

    /// Limiter for `/api/v1/ingest`, on its own limiter maps so a
    /// shipper-sized ceiling never leaks onto the query routes. An
    /// `ingest_rpm` of 0 disables limiting for keys without a role override.
    ///
    /// Only keys holding [`Permission::Ingest`] earn that ceiling. Everyone
    /// else shares `interactive`'s class — literally the same maps the query
    /// routes use, so hitting `/ingest` cannot buy a key extra interactive
    /// budget either.
    pub fn ingest(config: &RateLimitConfig, interactive: &Self) -> Self {
        let fallback = match &interactive.class {
            RouteClass::Interactive(class) => Arc::clone(class),
            RouteClass::Ingest { interactive, .. } => Arc::clone(interactive),
        };
        Self {
            class: RouteClass::Ingest {
                shipper: Arc::new(ClassLimiters::new(config.ingest_rpm)),
                interactive: fallback,
            },
        }
    }

    /// The limiter this request must draw on, given the calling key's
    /// ingest eligibility and role RPM override. `None` means limiting is
    /// disabled for this key on this route.
    fn limiter_for(
        &self,
        may_ingest: bool,
        rate_rpm_override: Option<u32>,
    ) -> Option<Arc<KeyedLimiter>> {
        match &self.class {
            RouteClass::Interactive(class) => class.limiter_for(rate_rpm_override),
            RouteClass::Ingest {
                shipper,
                interactive,
            } => {
                if may_ingest {
                    shipper.limiter_for(rate_rpm_override)
                } else {
                    interactive.limiter_for(rate_rpm_override)
                }
            }
        }
    }
}

/// Axum middleware that enforces per-key rate limits.
///
/// Must run after the auth middleware (needs [`VerifiedKey`] in extensions)
/// and inside the mandatory `require_trawl_grant` policy layer: grantless
/// keys 403 before ever reaching this middleware (pinned by the
/// `ac3_grantless_key_never_reaches_rate_limiter` integration test), so no
/// bypass branch is needed here. Returns 429 when the rate limit is exceeded.
///
/// On `/api/v1/ingest` the class depends on the key: only keys holding
/// [`Permission::Ingest`] get the shipper-sized class (see
/// [`RateLimitState::ingest`]). In both classes a role `rate_rpm` override
/// replaces the class default.
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

    if let Some(limiter) = rate_state.limiter_for(
        verified.has_permission(Permission::Ingest),
        verified.rate_rpm(),
    ) && limiter.check_key(&verified.id).is_err()
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

    /// Count how many of `attempts` requests a limiter admits for a key.
    fn spend(limiter: &Arc<KeyedLimiter>, key_id: i64, attempts: usize) -> usize {
        (0..attempts)
            .filter(|_| limiter.check_key(&key_id).is_ok())
            .count()
    }

    #[test]
    fn zero_rpm_disables_limiter() {
        let state = RateLimitState::interactive(&config_with_rpm(0));
        assert!(state.limiter_for(false, None).is_none());
    }

    #[test]
    fn rate_limit_rejects_after_burst() {
        let state = RateLimitState::interactive(&config_with_rpm(5));
        let limiter = state.limiter_for(false, None).unwrap();

        // With 5 rpm, the burst capacity is 5 — first 5 should pass.
        assert_eq!(
            spend(&limiter, 42, 20),
            5,
            "expected burst of 5 for 5 rpm quota"
        );
    }

    /// A role `rate_rpm` replaces the class default and is never max'd with
    /// it. That cuts both ways: an override larger than the default raises
    /// the ceiling, an override smaller than the default lowers it.
    #[test]
    fn role_override_replaces_the_class_default_in_both_directions() {
        let state = RateLimitState::interactive(&config_with_rpm(5));

        let raised = state.limiter_for(false, Some(50)).unwrap();
        assert_eq!(spend(&raised, 1, 100), 50, "override raises past default");

        let lowered = state.limiter_for(false, Some(2)).unwrap();
        assert_eq!(
            spend(&lowered, 2, 20),
            2,
            "override lowers below default — never max(default, override)"
        );
    }

    /// An override even beats a disabled class default: `rate_rpm` on the
    /// role re-enables limiting for that key.
    #[test]
    fn role_override_applies_even_when_default_disabled() {
        let state = RateLimitState::interactive(&config_with_rpm(0));
        let limiter = state.limiter_for(false, Some(3)).unwrap();
        assert_eq!(spend(&limiter, 9, 10), 3);
    }

    /// Two keys resolving the same effective RPM share a limiter map (one
    /// quota) but never a bucket: spending one key's budget leaves the
    /// other untouched.
    #[test]
    fn same_effective_rpm_shares_map_not_buckets() {
        let state = RateLimitState::interactive(&config_with_rpm(5));
        let a = state.limiter_for(false, Some(2)).unwrap();
        let b = state.limiter_for(false, Some(2)).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same effective rpm → same map");

        assert_eq!(spend(&a, 7, 10), 2, "key 7 exhausts its bucket");
        assert_eq!(spend(&b, 8, 10), 2, "key 8's bucket is untouched");
    }

    #[test]
    fn ingest_ceiling_is_separate_from_the_interactive_one() {
        let config = RateLimitConfig {
            default_rpm: 1,
            ingest_rpm: 5,
        };
        let interactive = RateLimitState::interactive(&config);
        let ingest = RateLimitState::ingest(&config, &interactive);
        let key_id: i64 = 7;

        // Same key, same id: exhausting the interactive bucket must not touch
        // the ingest one, and the ingest ceiling must not leak the other way.
        let interactive_limiter = interactive.limiter_for(false, None).unwrap();
        assert!(interactive_limiter.check_key(&key_id).is_ok());
        assert!(
            interactive_limiter.check_key(&key_id).is_err(),
            "interactive burst is default_rpm (1), not ingest_rpm"
        );

        // `true` = the key holds Permission::Ingest, so it earns ingest_rpm.
        let ingest_limiter = ingest.limiter_for(true, None).unwrap();
        assert_eq!(
            spend(&ingest_limiter, key_id, 20),
            5,
            "ingest burst is ingest_rpm (5)"
        );
    }

    /// A key whose roles set `rate_rpm` gets that one effective number in
    /// each class it touches, spent separately per class (ADR-0006).
    #[test]
    fn override_applies_per_class_with_separate_budgets() {
        let config = RateLimitConfig {
            default_rpm: 1,
            ingest_rpm: 100,
        };
        let interactive = RateLimitState::interactive(&config);
        let ingest = RateLimitState::ingest(&config, &interactive);
        let key_id: i64 = 21;

        let on_query = interactive.limiter_for(false, Some(4)).unwrap();
        assert_eq!(spend(&on_query, key_id, 10), 4, "override on interactive");

        let on_ingest = ingest.limiter_for(true, Some(4)).unwrap();
        assert_eq!(
            spend(&on_ingest, key_id, 10),
            4,
            "same override on ingest, spent from its own budget"
        );
    }

    /// The shipper-sized ceiling is earned by `Permission::Ingest`, not by
    /// reaching the route: the handler rejects permissionless keys only after
    /// this middleware, so an ungated ingest class would hand every reader key
    /// the shipper budget on the heaviest endpoint.
    #[test]
    fn ingest_ceiling_needs_the_ingest_permission() {
        let config = RateLimitConfig {
            default_rpm: 1,
            ingest_rpm: 5,
        };
        let interactive = RateLimitState::interactive(&config);
        let ingest = RateLimitState::ingest(&config, &interactive);
        let key_id: i64 = 11;

        // A key without the permission draws on the interactive class —
        // the very same maps the query routes use, not a second allowance.
        let unprivileged = ingest.limiter_for(false, None).unwrap();
        assert!(unprivileged.check_key(&key_id).is_ok());
        assert!(
            unprivileged.check_key(&key_id).is_err(),
            "permissionless keys are held to default_rpm (1) on /ingest"
        );
        assert!(
            interactive
                .limiter_for(false, None)
                .unwrap()
                .check_key(&key_id)
                .is_err(),
            "the fallback shares the interactive class, so the interactive quota is spent too"
        );
    }

    #[test]
    fn different_key_ids_have_independent_limits() {
        let state = RateLimitState::interactive(&config_with_rpm(2));
        let limiter = state.limiter_for(false, None).unwrap();

        // Exhaust key id 1's bucket.
        let key_a: i64 = 1;
        for _ in 0..10 {
            let _ = limiter.check_key(&key_a);
        }
        assert!(
            limiter.check_key(&key_a).is_err(),
            "key A should be limited"
        );

        // Key id 2 still has its own independent quota: exhausting one key
        // never starves another, role or no role.
        let key_b: i64 = 2;
        assert!(
            limiter.check_key(&key_b).is_ok(),
            "key B should not be limited"
        );
    }
}
