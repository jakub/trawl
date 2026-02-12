//! Bearer token authentication middleware for axum.
//!
//! Extracts the `Authorization: Bearer <token>` header, verifies the
//! token against the `KeyStore` in a blocking task, and injects the
//! [`VerifiedKey`] into request extensions for downstream handlers.

use std::sync::Arc;

use parking_lot::Mutex;

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
    let path = request.uri().path().to_owned();

    let key_store = request
        .extensions()
        .get::<Arc<Mutex<KeyStore>>>()
        .cloned()
        .ok_or_else(|| ServerError::Internal("key_store not in extensions".into()))?;

    let Some(raw_token) = extract_bearer_token(request.headers()) else {
        tracing::warn!(path = %path, "auth failed: missing or malformed Authorization header");
        return Err(ServerError::Unauthorized(
            "missing or invalid Authorization header".into(),
        ));
    };
    let token = raw_token.to_owned();

    let verified = match tokio::task::spawn_blocking(move || {
        let store = key_store.lock();
        store.verify_key(&token)
    })
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(auth_err)) => {
            tracing::warn!(path = %path, "auth failed: invalid or revoked token");
            return Err(ServerError::Auth(auth_err));
        }
        Err(join_err) => {
            tracing::error!(path = %path, error = %join_err, "auth task panicked");
            return Err(ServerError::Internal(format!(
                "auth task panicked: {join_err}"
            )));
        }
    };

    tracing::info!(
        key_name = %verified.name,
        role = %verified.role,
        path = %path,
        "authenticated"
    );

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
}
