// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Trawl's app-side authorization policy over fleet-auth principals.
//!
//! fleet-auth's [`require_bearer`] deliberately authenticates keys from ANY
//! fleet app — it authenticates, it does not authorise. This module is the
//! mandatory policy layer behind it (ADR-0004, reshaped by ADR-0006):
//!
//! - [`Permission`]: trawl's compile-time permission vocabulary. Roles are
//!   data (fleet keystore rows); permissions are code — a permission string
//!   exists because a handler checks it. Unknown strings in a role are
//!   ignored (fail closed, warn log).
//! - [`TrawlAuthz`]: extension trait giving [`fleet_auth::VerifiedKey`] the
//!   `has_permission()` surface every handler checks.
//! - [`require_trawl_grant`]: middleware that 403s keys resolving zero
//!   recognized trawl permissions BEFORE anything else (rate limiter,
//!   `/whoami`) sees them, and stamps [`TrawlPolicyApplied`] on every
//!   response it passes so the envelope normalizer can tell trawl-shaped
//!   bodies apart.
//! - [`normalize_auth_errors`]: rewrites `require_bearer`'s flat
//!   `{"error","detail"}` failure bodies into trawl's [`ErrorResponse`]
//!   envelope so the wire contract at the trust boundary is unchanged.
//! - [`wire_kind`]: maps fleet-auth's [`PrincipalKind`] twin onto the
//!   `trawl_api` wire type.
//!
//! [`require_bearer`]: fleet_auth::require_bearer
//! [`PrincipalKind`]: fleet_auth::PrincipalKind
//! [`ErrorResponse`]: trawl_api::ErrorResponse

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use fleet_auth::{TRAWL_APP, VerifiedKey};

use crate::error::ServerError;

// -- permissions --------------------------------------------------------------

/// Discrete permissions trawl's handlers check.
///
/// The `Role` enum and its compile-time role → permission tables died with
/// ADR-0006: roles are data-defined bundles of these strings in the fleet
/// keystore. The dead `KeyManage` variant died with them (no handler ever
/// checked it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Execute search queries, view history, list running queries.
    Query,
    /// Read schema and field catalog.
    SchemaRead,
    /// Validate DSL syntax without executing.
    Validate,
    /// CRUD operations on saved queries.
    SavedQuery,
    /// Export query results to file formats.
    Export,
    /// Subscribe to live SSE event streams.
    Stream,
    /// Cancel running queries (own queries; admin can cancel any via `ServerManage`).
    QueryCancel,
    /// Manage server configuration and view stats.
    ServerManage,
    /// Write events via the ingest endpoint.
    Ingest,
}

impl Permission {
    /// Every permission trawl recognizes, in canonical wire order. `/whoami`
    /// emits resolved permissions in this order so the golden wire tests
    /// stay deterministic regardless of role storage order.
    pub const ALL: &[Self] = &[
        Self::Query,
        Self::SchemaRead,
        Self::Validate,
        Self::SavedQuery,
        Self::Export,
        Self::Stream,
        Self::QueryCancel,
        Self::ServerManage,
        Self::Ingest,
    ];

    /// Snake-case string representation for wire formats and keystore rows.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::SchemaRead => "schema_read",
            Self::Validate => "validate",
            Self::SavedQuery => "saved_query",
            Self::Export => "export",
            Self::Stream => "stream",
            Self::QueryCancel => "query_cancel",
            Self::ServerManage => "server_manage",
            Self::Ingest => "ingest",
        }
    }

    /// Parse a keystore permission string. `None` for anything trawl does
    /// not recognize — callers must treat unknown strings as absent
    /// capability (fail closed), never as an error that blocks the key.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "query" => Some(Self::Query),
            "schema_read" => Some(Self::SchemaRead),
            "validate" => Some(Self::Validate),
            "saved_query" => Some(Self::SavedQuery),
            "export" => Some(Self::Export),
            "stream" => Some(Self::Stream),
            "query_cancel" => Some(Self::QueryCancel),
            "server_manage" => Some(Self::ServerManage),
            "ingest" => Some(Self::Ingest),
            _ => None,
        }
    }
}

// -- authz extension trait ----------------------------------------------------

/// Trawl-side authorization surface over a fleet-auth [`VerifiedKey`].
///
/// Handler call sites keep the exact shape they had under the role era:
/// `verified.has_permission(Permission::Export)`.
pub trait TrawlAuthz {
    /// Check whether this key's resolved trawl-namespace permission union
    /// grants `perm`.
    fn has_permission(&self, perm: Permission) -> bool;

    /// Whether the key resolves at least one RECOGNIZED trawl permission —
    /// the "may enter trawl at all" question. A key whose roles carry only
    /// strings trawl doesn't recognize has nothing usable here (fail
    /// closed), exactly like the old unknown-role case.
    fn has_any_trawl_permission(&self) -> bool;

    /// The key's recognized trawl permissions in canonical
    /// [`Permission::ALL`] order. Unrecognized strings are dropped with a
    /// warn log — they advertise no capability any handler checks.
    fn trawl_permissions(&self) -> Vec<Permission>;
}

impl TrawlAuthz for VerifiedKey {
    fn has_permission(&self, perm: Permission) -> bool {
        self.has_app_permission(TRAWL_APP, perm.as_str())
    }

    fn has_any_trawl_permission(&self) -> bool {
        Permission::ALL
            .iter()
            .any(|p| self.has_app_permission(TRAWL_APP, p.as_str()))
    }

    fn trawl_permissions(&self) -> Vec<Permission> {
        let raw = self.permissions_for(TRAWL_APP);
        for s in raw {
            if Permission::parse(s).is_none() {
                tracing::warn!(
                    permission = %s,
                    "unrecognized trawl permission string in role — ignored (fail closed)"
                );
            }
        }
        Permission::ALL
            .iter()
            .copied()
            .filter(|p| raw.iter().any(|s| s == p.as_str()))
            .collect()
    }
}

// -- wire shims ---------------------------------------------------------------

/// Map fleet-auth's [`fleet_auth::PrincipalKind`] onto the `trawl_api` wire
/// twin. The `/whoami` wire shape is owned by trawl-api; fleet-auth defines
/// a nominally-distinct equivalent.
pub fn wire_kind(kind: fleet_auth::PrincipalKind) -> trawl_api::PrincipalKind {
    match kind {
        fleet_auth::PrincipalKind::Human => trawl_api::PrincipalKind::Human,
        fleet_auth::PrincipalKind::Service => trawl_api::PrincipalKind::Service,
    }
}

// -- mandatory policy middleware ----------------------------------------------

/// Response-extension marker: this response was produced behind trawl's
/// policy layer (successful authn + either a policy verdict or a downstream
/// handler), so its body is already trawl-shaped. [`normalize_auth_errors`]
/// leaves marked responses alone and rewrites everything else.
#[derive(Clone, Copy, Debug)]
pub struct TrawlPolicyApplied;

/// Axum middleware: reject authenticated keys that resolve no usable trawl
/// capability (zero recognized trawl permissions across all their roles)
/// with an opaque 403 — before the rate limiter, `/whoami`, or any handler
/// sees them.
///
/// Must run AFTER `fleet_auth::require_bearer_only` (needs [`VerifiedKey`] in
/// request extensions). Mounted on BOTH authenticated sub-routers (`/api/v1`
/// tree and `/ingest`), so it is a mandatory layer, not a per-handler
/// convention.
pub async fn require_trawl_grant(req: Request, next: Next) -> Response {
    let Some(verified) = req.extensions().get::<VerifiedKey>() else {
        // require_bearer_only always inserts the key; missing means mis-mounted
        // middleware — fail closed and loudly.
        tracing::error!("policy: VerifiedKey missing from request extensions (mis-mounted layer?)");
        return mark(
            ServerError::Internal("verified key not in extensions".into()).into_response(),
        );
    };

    if !verified.has_any_trawl_permission() {
        count_auth_failure("no_trawl_grant");
        tracing::info!(
            event_type = "auth_failure",
            reason = "no_trawl_grant",
            name = %verified.name,
            prefix = %verified.prefix,
            roles = %verified.roles_display(),
            "policy: authenticated key resolves no usable trawl permission (403)"
        );
        return mark(ServerError::Forbidden("no trawl grant for this key".into()).into_response());
    }

    mark(next.run(req).await)
}

/// Stamp [`TrawlPolicyApplied`] on a response.
fn mark(mut resp: Response) -> Response {
    resp.extensions_mut().insert(TrawlPolicyApplied);
    resp
}

/// Count one rejected request against `trawl_auth_failures_total{reason}`.
///
/// This is the ONLY in-product signal for a failed authentication. The
/// events behind a 401/503 come from fleet-auth's bearer shell, which sits
/// outside the rate limiter, so [`crate::telemetry::PRE_AUTH_TARGETS`]
/// keeps them off the WAL — an unauthenticated flood must not become
/// durable corpus growth. A counter has no such problem: `reason` is a
/// closed, code-defined set, so the series count is fixed however hard the
/// endpoint is hammered, and credential stuffing, token brute force or a
/// revoked key still in use stay alarmable on `/metrics`.
fn count_auth_failure(reason: &'static str) {
    metrics::counter!(crate::metrics::AUTH_FAILURES_TOTAL, "reason" => reason).increment(1);
}

/// Axum middleware: keep trawl's [`trawl_api::ErrorResponse`] envelope at the
/// trust boundary (ADR-0004 AC5).
///
/// Mounted directly OUTSIDE `fleet_auth::require_bearer_only`. Responses that
/// carry the [`TrawlPolicyApplied`] marker passed authn and are already
/// trawl-shaped; anything else with an auth-relevant status was
/// short-circuited by the bearer shell (fleet-auth's flat
/// `{"error","detail"}` bodies) and is rewritten to a fixed opaque envelope:
///
/// - 401 → opaque `auth_error` (missing/malformed/invalid/revoked/expired —
///   indistinguishable by design)
/// - 403 → opaque `forbidden` (defensive; policy 403s are already marked)
/// - 500 → `internal_error`
/// - 503 → `service_unavailable` "auth backend unavailable" — never
///   postgres detail
///
/// Every rewritten response is also counted under
/// `trawl_auth_failures_total{reason}` ([`count_auth_failure`]): the
/// underlying fleet-auth events are stdout-only, so without this counter a
/// 401 storm would leave no in-product trace at all.
pub async fn normalize_auth_errors(req: Request, next: Next) -> Response {
    use axum::http::StatusCode;
    use trawl_api::ErrorCode;

    let resp = next.run(req).await;
    if resp.extensions().get::<TrawlPolicyApplied>().is_some() {
        return resp;
    }

    let (code, message, reason) = match resp.status() {
        StatusCode::UNAUTHORIZED => (
            ErrorCode::AuthError,
            "authentication failed",
            "unauthorized",
        ),
        StatusCode::FORBIDDEN => (ErrorCode::Forbidden, "forbidden", "forbidden"),
        StatusCode::INTERNAL_SERVER_ERROR => (
            ErrorCode::InternalError,
            "internal server error",
            "internal",
        ),
        StatusCode::SERVICE_UNAVAILABLE => (
            ErrorCode::ServiceUnavailable,
            "auth backend unavailable",
            "backend_unavailable",
        ),
        _ => return resp,
    };
    count_auth_failure(reason);

    let envelope = trawl_api::ErrorResponse {
        error: trawl_api::ErrorEnvelope::simple(code, message),
    };
    (resp.status(), axum::Json(envelope)).into_response()
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware::from_fn;
    use axum::routing::get;
    use fleet_auth::{PrincipalKind, Role, RolePermission};
    use tower::ServiceExt as _;

    use super::*;

    fn role(name: &str, perms: &[(&str, &str)]) -> Role {
        Role {
            id: 0,
            name: name.into(),
            rate_rpm: None,
            permissions: perms
                .iter()
                .map(|(app, permission)| RolePermission {
                    app: (*app).into(),
                    permission: (*permission).into(),
                })
                .collect(),
        }
    }

    fn key_with(roles: Vec<Role>) -> VerifiedKey {
        VerifiedKey::from_roles(1, "abcd1234", "k", PrincipalKind::Human, roles)
    }

    // -- permission parsing ---------------------------------------------------

    #[test]
    fn parse_roundtrips_every_permission() {
        for &perm in Permission::ALL {
            assert_eq!(Permission::parse(perm.as_str()), Some(perm));
        }
    }

    #[test]
    fn parse_rejects_unknown_and_dead_strings() {
        assert_eq!(Permission::parse("superuser"), None);
        assert_eq!(
            Permission::parse("key_manage"),
            None,
            "key_manage is dead — no handler checks it"
        );
    }

    // -- TrawlAuthz over fleet-auth VerifiedKey ------------------------------

    #[test]
    fn authz_resolves_permissions_from_roles() {
        let key = key_with(vec![role(
            "trawl-admin",
            &[("trawl", "query"), ("trawl", "server_manage")],
        )]);
        assert!(key.has_permission(Permission::ServerManage));
        assert!(key.has_permission(Permission::Query));
        assert!(!key.has_permission(Permission::Ingest));
        assert!(key.has_any_trawl_permission());
        assert_eq!(
            key.trawl_permissions(),
            vec![Permission::Query, Permission::ServerManage]
        );
    }

    #[test]
    fn authz_foreign_app_only_key_has_nothing() {
        let key = key_with(vec![role(
            "coastwatch-siem_consumer",
            &[("coastwatch", "ioc_exports_read")],
        )]);
        assert!(!key.has_any_trawl_permission());
        assert!(!key.has_permission(Permission::Query));
        assert!(key.trawl_permissions().is_empty());
    }

    #[test]
    fn authz_unrecognized_strings_dropped_fail_closed() {
        // AC2: unknown permission strings in a role are ignored — the
        // recognized remainder survives, the unknown advertises nothing.
        let key = key_with(vec![role(
            "future",
            &[
                ("trawl", "query"),
                ("trawl", "warp_drive"),
                ("trawl", "key_manage"),
            ],
        )]);
        assert_eq!(key.trawl_permissions(), vec![Permission::Query]);
        assert!(key.has_permission(Permission::Query));
        assert!(key.has_any_trawl_permission());
    }

    #[test]
    fn authz_unrecognized_only_key_is_fail_closed() {
        let key = key_with(vec![role("mystery", &[("trawl", "warp_drive")])]);
        assert!(!key.has_any_trawl_permission());
        assert!(key.trawl_permissions().is_empty());
    }

    #[test]
    fn trawl_permissions_canonical_order_regardless_of_role_order() {
        let key = key_with(vec![role(
            "scrambled",
            &[("trawl", "stream"), ("trawl", "query"), ("trawl", "export")],
        )]);
        assert_eq!(
            key.trawl_permissions(),
            vec![Permission::Query, Permission::Export, Permission::Stream]
        );
    }

    // -- wire shims -----------------------------------------------------------

    #[test]
    fn wire_kind_maps_both_variants() {
        assert_eq!(
            wire_kind(fleet_auth::PrincipalKind::Human),
            trawl_api::PrincipalKind::Human
        );
        assert_eq!(
            wire_kind(fleet_auth::PrincipalKind::Service),
            trawl_api::PrincipalKind::Service
        );
    }

    // -- middleware -----------------------------------------------------------

    /// Router with the policy layer and a trivial handler, plus a request
    /// extension carrying the given key (simulating `require_bearer`).
    async fn run_policy(key: Option<VerifiedKey>) -> Response {
        let app = Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer(from_fn(require_trawl_grant));
        let mut req = HttpRequest::builder()
            .uri("/x")
            .body(Body::empty())
            .unwrap();
        if let Some(k) = key {
            req.extensions_mut().insert(k);
        }
        app.oneshot(req).await.unwrap()
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn policy_passes_granted_key_and_marks_response() {
        let resp = run_policy(Some(key_with(vec![role(
            "trawl-reader",
            &[("trawl", "query")],
        )])))
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.extensions().get::<TrawlPolicyApplied>().is_some(),
            "successful responses must carry the policy marker"
        );
    }

    #[tokio::test]
    async fn policy_403s_grantless_key_with_trawl_envelope() {
        let resp = run_policy(Some(key_with(vec![role(
            "coastwatch-viewer",
            &[("coastwatch", "stories_read")],
        )])))
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(resp.extensions().get::<TrawlPolicyApplied>().is_some());
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "forbidden");
    }

    #[tokio::test]
    async fn policy_403s_roleless_key() {
        let resp = run_policy(Some(key_with(vec![]))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn policy_403s_unrecognized_permissions_only_key() {
        let resp = run_policy(Some(key_with(vec![role(
            "mystery",
            &[("trawl", "warp_drive")],
        )])))
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn policy_500s_when_key_missing() {
        let resp = run_policy(None).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Simulated bearer shell: short-circuits with a fleet-auth-flat body.
    async fn run_normalizer(inner_status: StatusCode, marked: bool) -> Response {
        let app = Router::new()
            .route(
                "/x",
                get(move || async move {
                    let mut resp = (
                        inner_status,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        r#"{"error":"unauthorized","detail":"fleet-flat body"}"#,
                    )
                        .into_response();
                    if marked {
                        resp.extensions_mut().insert(TrawlPolicyApplied);
                    }
                    resp
                }),
            )
            .layer(from_fn(normalize_auth_errors));
        app.oneshot(
            HttpRequest::builder()
                .uri("/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn normalizer_rewrites_unmarked_401_to_trawl_envelope() {
        let resp = run_normalizer(StatusCode::UNAUTHORIZED, false).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "auth_error");
        assert_eq!(json["error"]["message"], "authentication failed");
        assert!(json.get("detail").is_none(), "flat body must be gone");
    }

    #[tokio::test]
    async fn normalizer_rewrites_unmarked_503_without_backend_detail() {
        let resp = run_normalizer(StatusCode::SERVICE_UNAVAILABLE, false).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "service_unavailable");
        assert_eq!(json["error"]["message"], "auth backend unavailable");
    }

    #[tokio::test]
    async fn normalizer_rewrites_unmarked_403_to_trawl_envelope() {
        let resp = run_normalizer(StatusCode::FORBIDDEN, false).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "forbidden");
        assert_eq!(json["error"]["message"], "forbidden");
        assert!(json.get("detail").is_none(), "flat body must be gone");
    }

    #[tokio::test]
    async fn normalizer_rewrites_unmarked_500_to_trawl_envelope() {
        let resp = run_normalizer(StatusCode::INTERNAL_SERVER_ERROR, false).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "internal_error");
        assert_eq!(json["error"]["message"], "internal server error");
        assert!(json.get("detail").is_none(), "flat body must be gone");
    }

    #[tokio::test]
    async fn normalizer_leaves_marked_responses_alone() {
        let resp = run_normalizer(StatusCode::UNAUTHORIZED, true).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let json = body_json(resp).await;
        assert_eq!(json["detail"], "fleet-flat body", "must not be rewritten");
    }

    #[tokio::test]
    async fn normalizer_ignores_non_auth_statuses() {
        let resp = run_normalizer(StatusCode::TOO_MANY_REQUESTS, false).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let json = body_json(resp).await;
        assert_eq!(json["detail"], "fleet-flat body", "429 passes through");
    }

    /// The pre-authn events are stdout-only, so `/metrics` is the only
    /// in-product signal a failed authentication leaves. A local recorder
    /// (not the global one — `telemetry.rs` already installs that) proves
    /// each reason lands, and that pass-through statuses do not inflate it.
    #[test]
    fn auth_failures_counter_records_every_rejection_reason() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // `with_local_recorder` is thread-local and a current-thread runtime
        // drives the futures on this very thread, so the middleware's
        // `counter!` calls resolve to `recorder`.
        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                run_normalizer(StatusCode::UNAUTHORIZED, false).await;
                run_normalizer(StatusCode::SERVICE_UNAVAILABLE, false).await;
                run_normalizer(StatusCode::INTERNAL_SERVER_ERROR, false).await;
                run_normalizer(StatusCode::FORBIDDEN, false).await;
                run_policy(Some(key_with(vec![role(
                    "coastwatch-viewer",
                    &[("coastwatch", "stories_read")],
                )])))
                .await;
                // Neither of these is an auth rejection.
                run_normalizer(StatusCode::TOO_MANY_REQUESTS, false).await;
                run_policy(Some(key_with(vec![role(
                    "trawl-reader",
                    &[("trawl", "query")],
                )])))
                .await;
            });
        });

        let rendered = handle.render();
        for reason in [
            "unauthorized",
            "backend_unavailable",
            "internal",
            "forbidden",
            "no_trawl_grant",
        ] {
            assert!(
                rendered.contains(&format!(
                    "trawl_auth_failures_total{{reason=\"{reason}\"}} 1"
                )),
                "missing or miscounted reason {reason}: {rendered}"
            );
        }
        assert_eq!(
            rendered.matches("trawl_auth_failures_total{").count(),
            5,
            "closed label set — no extra series: {rendered}"
        );
    }
}
