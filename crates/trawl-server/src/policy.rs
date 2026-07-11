// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Trawl's app-side authorization policy over fleet-auth principals.
//!
//! fleet-auth's [`require_bearer`] deliberately authenticates keys from ANY
//! fleet app — it authenticates, it does not authorise. This module is the
//! mandatory policy layer behind it (ADR-0004):
//!
//! - [`Role`] / [`Permission`]: the trawl permission model, moved here from
//!   trawl-auth (app policy, not substrate).
//! - [`TrawlAuthz`]: extension trait giving [`fleet_auth::VerifiedKey`] the
//!   `trawl_role()` / `has_permission()` surface every handler checks.
//! - [`require_trawl_grant`]: middleware that 403s keys with no trawl grant
//!   or an unknown trawl role BEFORE anything else (rate limiter, `/whoami`)
//!   sees them, and stamps [`TrawlPolicyApplied`] on every response it
//!   passes so the envelope normalizer can tell trawl-shaped bodies apart.
//! - [`normalize_auth_errors`]: rewrites `require_bearer`'s flat
//!   `{"error","detail"}` failure bodies into trawl's [`ErrorResponse`]
//!   envelope so the wire contract at the trust boundary is unchanged.
//! - wire shims mapping fleet-auth's [`PrincipalKind`]/[`RoleAssignment`]
//!   twins onto the `trawl_api` wire types (`/whoami` shape is frozen).
//!
//! [`require_bearer`]: fleet_auth::require_bearer

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use fleet_auth::{TRAWL_APP, VerifiedKey};

use crate::error::ServerError;

// -- roles + permissions (ported from trawl-auth, minus rusqlite) ------------

/// The four roles in the trawl permission model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Full access — key management, server config, queries, schema.
    Admin,
    /// Power user — query execution, schema, saved queries, export, streaming.
    Analyst,
    /// Basic access — query execution, schema, cancel own queries.
    Reader,
    /// Write-only log ingestion (used by vector/agents).
    Ingest,
}

/// Discrete permissions that can be checked against a role.
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
    /// Manage API keys (create, list, revoke).
    KeyManage,
    /// Manage server configuration and view stats.
    ServerManage,
    /// Write events via the ingest endpoint.
    Ingest,
}

impl Permission {
    /// Snake-case string representation for wire formats.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::SchemaRead => "schema_read",
            Self::Validate => "validate",
            Self::SavedQuery => "saved_query",
            Self::Export => "export",
            Self::Stream => "stream",
            Self::QueryCancel => "query_cancel",
            Self::KeyManage => "key_manage",
            Self::ServerManage => "server_manage",
            Self::Ingest => "ingest",
        }
    }
}

impl Role {
    /// All defined roles.
    pub const ALL: &[Self] = &[Self::Admin, Self::Analyst, Self::Reader, Self::Ingest];

    /// Check whether this role grants the given permission.
    pub fn has_permission(self, perm: Permission) -> bool {
        self.permissions().contains(&perm)
    }

    /// Return all permissions granted by this role.
    pub fn permissions(self) -> &'static [Permission] {
        match self {
            Self::Admin => &[
                Permission::Query,
                Permission::SchemaRead,
                Permission::Validate,
                Permission::SavedQuery,
                Permission::Export,
                Permission::Stream,
                Permission::QueryCancel,
                Permission::KeyManage,
                Permission::ServerManage,
            ],
            Self::Analyst => &[
                Permission::Query,
                Permission::SchemaRead,
                Permission::Validate,
                Permission::SavedQuery,
                Permission::Export,
                Permission::Stream,
                Permission::QueryCancel,
            ],
            Self::Reader => &[
                Permission::Query,
                Permission::SchemaRead,
                Permission::QueryCancel,
            ],
            Self::Ingest => &[Permission::Ingest],
        }
    }

    /// The string representation used in grants and the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Analyst => "analyst",
            Self::Reader => "reader",
            Self::Ingest => "ingest",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "admin" => Ok(Self::Admin),
            "analyst" => Ok(Self::Analyst),
            "reader" => Ok(Self::Reader),
            "ingest" => Ok(Self::Ingest),
            other => Err(format!("unknown role: {other}")),
        }
    }
}

// -- authz extension trait ----------------------------------------------------

/// Trawl-side authorization surface over a fleet-auth [`VerifiedKey`].
///
/// Every call site that previously used trawl-auth's inherent
/// `trawl_role()` / `has_permission()` methods survives on an import swap.
pub trait TrawlAuthz {
    /// Parsed trawl-app role, if this key has a `("trawl", _)` grant with a
    /// role string trawl recognizes.
    fn trawl_role(&self) -> Option<Role>;

    /// Check whether this key has the given trawl-app permission.
    ///
    /// Short-circuits to `false` for keys with no trawl-app grant.
    fn has_permission(&self, perm: Permission) -> bool;
}

impl TrawlAuthz for VerifiedKey {
    fn trawl_role(&self) -> Option<Role> {
        self.role_for(TRAWL_APP).and_then(|r| {
            if let Ok(role) = r.parse() {
                Some(role)
            } else {
                tracing::warn!(role = r, "unrecognized trawl role in assignment");
                None
            }
        })
    }

    fn has_permission(&self, perm: Permission) -> bool {
        self.trawl_role().is_some_and(|r| r.has_permission(perm))
    }
}

// -- wire shims ---------------------------------------------------------------

/// Map fleet-auth's [`fleet_auth::PrincipalKind`] onto the `trawl_api` wire
/// twin. The `/whoami` wire shape is frozen (ADR-0004 AC4); trawl-api owns
/// the wire types, fleet-auth defines nominally-distinct equivalents.
pub fn wire_kind(kind: fleet_auth::PrincipalKind) -> trawl_api::PrincipalKind {
    match kind {
        fleet_auth::PrincipalKind::Human => trawl_api::PrincipalKind::Human,
        fleet_auth::PrincipalKind::Service => trawl_api::PrincipalKind::Service,
    }
}

/// Map fleet-auth [`fleet_auth::RoleAssignment`]s onto the `trawl_api` wire
/// twins. See [`wire_kind`].
pub fn wire_assignments(
    assignments: &[fleet_auth::RoleAssignment],
) -> Vec<trawl_api::RoleAssignment> {
    assignments
        .iter()
        .map(|a| trawl_api::RoleAssignment {
            app: a.app.clone(),
            role: a.role.clone(),
        })
        .collect()
}

// -- mandatory policy middleware ----------------------------------------------

/// Response-extension marker: this response was produced behind trawl's
/// policy layer (successful authn + either a policy verdict or a downstream
/// handler), so its body is already trawl-shaped. [`normalize_auth_errors`]
/// leaves marked responses alone and rewrites everything else.
#[derive(Clone, Copy, Debug)]
pub struct TrawlPolicyApplied;

/// Axum middleware: reject authenticated keys that carry no usable trawl
/// grant (missing `("trawl", _)` assignment OR an unknown trawl role) with
/// an opaque 403 — before the rate limiter, `/whoami`, or any handler sees
/// them.
///
/// Must run AFTER `fleet_auth::require_bearer` (needs [`VerifiedKey`] in
/// request extensions). Mounted on BOTH authenticated sub-routers (`/api/v1`
/// tree and `/ingest`), so it is a mandatory layer, not a per-handler
/// convention.
pub async fn require_trawl_grant(req: Request, next: Next) -> Response {
    let Some(verified) = req.extensions().get::<VerifiedKey>() else {
        // require_bearer always inserts the key; missing means mis-mounted
        // middleware — fail closed and loudly.
        tracing::error!("policy: VerifiedKey missing from request extensions (mis-mounted layer?)");
        return mark(
            ServerError::Internal("verified key not in extensions".into()).into_response(),
        );
    };

    if verified.trawl_role().is_none() {
        tracing::info!(
            event_type = "auth_failure",
            reason = "no_trawl_grant",
            name = %verified.name,
            prefix = %verified.prefix,
            assignments = %verified.assignments_display(),
            "policy: authenticated key has no usable trawl grant (403)"
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

/// Axum middleware: keep trawl's [`trawl_api::ErrorResponse`] envelope at the
/// trust boundary (ADR-0004 AC5).
///
/// Mounted directly OUTSIDE `fleet_auth::require_bearer`. Responses that
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
pub async fn normalize_auth_errors(req: Request, next: Next) -> Response {
    use axum::http::StatusCode;
    use trawl_api::ErrorCode;

    let resp = next.run(req).await;
    if resp.extensions().get::<TrawlPolicyApplied>().is_some() {
        return resp;
    }

    let (code, message) = match resp.status() {
        StatusCode::UNAUTHORIZED => (ErrorCode::AuthError, "authentication failed"),
        StatusCode::FORBIDDEN => (ErrorCode::Forbidden, "forbidden"),
        StatusCode::INTERNAL_SERVER_ERROR => (ErrorCode::InternalError, "internal server error"),
        StatusCode::SERVICE_UNAVAILABLE => {
            (ErrorCode::ServiceUnavailable, "auth backend unavailable")
        }
        _ => return resp,
    };

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
    use fleet_auth::{PrincipalKind, RoleAssignment};
    use tower::ServiceExt as _;

    use super::*;

    // -- role/permission tables (ported from trawl-auth) --------------------

    #[test]
    fn admin_has_all_non_ingest_permissions() {
        assert!(Role::Admin.has_permission(Permission::Query));
        assert!(Role::Admin.has_permission(Permission::SchemaRead));
        assert!(Role::Admin.has_permission(Permission::Validate));
        assert!(Role::Admin.has_permission(Permission::SavedQuery));
        assert!(Role::Admin.has_permission(Permission::Export));
        assert!(Role::Admin.has_permission(Permission::Stream));
        assert!(Role::Admin.has_permission(Permission::QueryCancel));
        assert!(Role::Admin.has_permission(Permission::KeyManage));
        assert!(Role::Admin.has_permission(Permission::ServerManage));
        // admin and ingest are orthogonal
        assert!(!Role::Admin.has_permission(Permission::Ingest));
    }

    #[test]
    fn analyst_has_power_user_permissions() {
        assert!(Role::Analyst.has_permission(Permission::Query));
        assert!(Role::Analyst.has_permission(Permission::SchemaRead));
        assert!(Role::Analyst.has_permission(Permission::Validate));
        assert!(Role::Analyst.has_permission(Permission::SavedQuery));
        assert!(Role::Analyst.has_permission(Permission::Export));
        assert!(Role::Analyst.has_permission(Permission::Stream));
        assert!(Role::Analyst.has_permission(Permission::QueryCancel));
        assert!(!Role::Analyst.has_permission(Permission::KeyManage));
        assert!(!Role::Analyst.has_permission(Permission::ServerManage));
        assert!(!Role::Analyst.has_permission(Permission::Ingest));
    }

    #[test]
    fn reader_has_only_basic_permissions() {
        assert!(Role::Reader.has_permission(Permission::Query));
        assert!(Role::Reader.has_permission(Permission::SchemaRead));
        assert!(Role::Reader.has_permission(Permission::QueryCancel));
        assert!(!Role::Reader.has_permission(Permission::Validate));
        assert!(!Role::Reader.has_permission(Permission::SavedQuery));
        assert!(!Role::Reader.has_permission(Permission::Export));
        assert!(!Role::Reader.has_permission(Permission::Stream));
        assert!(!Role::Reader.has_permission(Permission::KeyManage));
        assert!(!Role::Reader.has_permission(Permission::ServerManage));
        assert!(!Role::Reader.has_permission(Permission::Ingest));
    }

    #[test]
    fn ingest_has_only_ingest_permission() {
        assert!(Role::Ingest.has_permission(Permission::Ingest));
        assert!(!Role::Ingest.has_permission(Permission::Query));
        assert!(!Role::Ingest.has_permission(Permission::SchemaRead));
        assert!(!Role::Ingest.has_permission(Permission::Validate));
        assert!(!Role::Ingest.has_permission(Permission::SavedQuery));
        assert!(!Role::Ingest.has_permission(Permission::Export));
        assert!(!Role::Ingest.has_permission(Permission::Stream));
        assert!(!Role::Ingest.has_permission(Permission::QueryCancel));
        assert!(!Role::Ingest.has_permission(Permission::KeyManage));
        assert!(!Role::Ingest.has_permission(Permission::ServerManage));
    }

    #[test]
    fn display_roundtrip() {
        for &role in Role::ALL {
            let s = role.to_string();
            let parsed: Role = s.parse().unwrap();
            assert_eq!(role, parsed);
        }
    }

    #[test]
    fn fromstr_invalid() {
        let result = "superuser".parse::<Role>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown role"));
    }

    // -- TrawlAuthz over fleet-auth VerifiedKey ------------------------------

    fn key_with(assignments: Vec<RoleAssignment>) -> VerifiedKey {
        VerifiedKey {
            id: 1,
            prefix: "abcd1234".into(),
            name: "k".into(),
            kind: PrincipalKind::Human,
            assignments,
        }
    }

    fn trawl_grant(role: &str) -> Vec<RoleAssignment> {
        vec![RoleAssignment {
            app: TRAWL_APP.into(),
            role: role.into(),
        }]
    }

    #[test]
    fn authz_trawl_grant_resolves_role_and_permissions() {
        let key = key_with(trawl_grant("admin"));
        assert_eq!(key.trawl_role(), Some(Role::Admin));
        assert!(key.has_permission(Permission::ServerManage));
        assert!(!key.has_permission(Permission::Ingest));
    }

    #[test]
    fn authz_foreign_app_only_key_has_nothing() {
        let key = key_with(vec![RoleAssignment {
            app: "coastwatch".into(),
            role: "siem_consumer".into(),
        }]);
        assert_eq!(key.trawl_role(), None);
        assert!(!key.has_permission(Permission::Query));
    }

    #[test]
    fn authz_unknown_trawl_role_is_none() {
        // A grant exists but trawl doesn't recognize the role string —
        // policy treats this exactly like no grant (fail closed).
        let key = key_with(trawl_grant("superuser"));
        assert_eq!(key.trawl_role(), None);
        assert!(!key.has_permission(Permission::Query));
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

    #[test]
    fn wire_assignments_preserve_order_and_fields() {
        let fleet = vec![
            RoleAssignment {
                app: "coastwatch".into(),
                role: "viewer".into(),
            },
            RoleAssignment {
                app: "trawl".into(),
                role: "admin".into(),
            },
        ];
        let wire = wire_assignments(&fleet);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].app, "coastwatch");
        assert_eq!(wire[0].role, "viewer");
        assert_eq!(wire[1].app, "trawl");
        assert_eq!(wire[1].role, "admin");
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
        let resp = run_policy(Some(key_with(trawl_grant("reader")))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.extensions().get::<TrawlPolicyApplied>().is_some(),
            "successful responses must carry the policy marker"
        );
    }

    #[tokio::test]
    async fn policy_403s_grantless_key_with_trawl_envelope() {
        let resp = run_policy(Some(key_with(vec![RoleAssignment {
            app: "coastwatch".into(),
            role: "viewer".into(),
        }])))
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(resp.extensions().get::<TrawlPolicyApplied>().is_some());
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "forbidden");
    }

    #[tokio::test]
    async fn policy_403s_unknown_trawl_role() {
        let resp = run_policy(Some(key_with(trawl_grant("superuser")))).await;
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
}
