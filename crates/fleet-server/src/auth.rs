//! Bearer token authentication middleware for axum.
//!
//! Extracts the `Authorization: Bearer <token>` header, verifies the
//! token against the `KeyStore` in a blocking task, and injects the
//! [`VerifiedKey`] into request extensions for downstream handlers.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use fleet_auth::store::KeyStore;

use crate::error::ServerError;

/// Axum middleware that authenticates requests via bearer token.
///
/// On success, injects [`VerifiedKey`] into request extensions.
/// Returns 401 on missing/invalid/expired/revoked tokens.
pub async fn auth_middleware(request: Request, next: Next) -> Result<Response, ServerError> {
    let auth_db_path = request
        .extensions()
        .get::<Arc<PathBuf>>()
        .cloned()
        .ok_or_else(|| ServerError::Internal("auth_db_path not in extensions".into()))?;

    let token = extract_bearer_token(request.headers())
        .ok_or_else(|| ServerError::Unauthorized("missing or invalid Authorization header".into()))?
        .to_owned();

    let verified = tokio::task::spawn_blocking(move || {
        let store = KeyStore::open(&*auth_db_path)?;
        store.verify_key(&token)
    })
    .await
    .map_err(|e| ServerError::Internal(format!("auth task panicked: {e}")))?
    .map_err(ServerError::Auth)?;

    let mut request = request;
    request.extensions_mut().insert(verified);

    Ok(next.run(request).await)
}

/// Extract the bearer token from the Authorization header.
fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
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
}
