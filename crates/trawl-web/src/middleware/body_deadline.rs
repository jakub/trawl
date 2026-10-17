// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One deadline for every request body the proxy reads (ADR-0054,
//! amended 2026-10-17).
//!
//! The deadline is a layer over the whole router, not a step in each
//! handler. It wraps the request body before any extractor or handler
//! reads it, so login's `Json`, the forwarder's own collection and any
//! route added later all read under
//! [`ProxyLimits::body_read_timeout`](crate::config::ProxyLimits). The
//! deadline is armed when the request enters the layer, which is once
//! hyper has the head, and it covers the whole body: progress does not
//! reset it. A body that is not whole by the deadline fails its next
//! read, and the layer answers the request 408 `timeout` with the
//! envelope whatever the extractor or handler made of the failed read.
//! The count the request holds is released with that response head, as
//! with any other answer. A body nobody reads never fires the deadline,
//! and a handler that has its body whole is not timed by it.
//!
//! Without this, a client needs no token to hold one of the proxy's
//! counts for as long as it likes: it sends a login head, which is
//! unauthenticated by definition, and never finishes the body.

use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::BoxError;
use axum::body::{Body, BodyDataStream, Bytes};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use futures::Stream;
use tokio::time::Sleep;

use crate::error::ProxyError;

/// The error a request body yields once its deadline has passed. The
/// forwarder classifies it as [`ProxyError::BodyTimeout`]; an axum
/// extractor turns it into whatever refusal it has, which
/// [`bound_body_read`] then replaces.
#[derive(Debug)]
pub struct BodyDeadlineElapsed;

impl std::fmt::Display for BodyDeadlineElapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body not received within the body deadline")
    }
}

impl std::error::Error for BodyDeadlineElapsed {}

/// Read the request body under `deadline`, and answer 408 `timeout` when
/// it passes before the body is whole.
pub async fn bound_body_read(
    State(deadline): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let elapsed = Arc::new(AtomicBool::new(false));
    let body = Body::from_stream(DeadlineStream {
        inner: body.into_data_stream(),
        deadline: Box::pin(tokio::time::sleep(deadline)),
        elapsed: Arc::clone(&elapsed),
    });
    let response = next.run(Request::from_parts(parts, body)).await;
    // The body was owned by the handler's future, which has returned, so
    // the flag is final: it is set only by a read that found the
    // deadline passed.
    if elapsed.load(Ordering::Acquire) {
        return ProxyError::BodyTimeout.into_response();
    }
    response
}

/// The request body's data, until the deadline passes.
struct DeadlineStream {
    inner: BodyDataStream,
    deadline: Pin<Box<Sleep>>,
    elapsed: Arc<AtomicBool>,
}

impl Stream for DeadlineStream {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.elapsed.load(Ordering::Relaxed) {
            return Poll::Ready(Some(Err(BodyDeadlineElapsed.into())));
        }
        // The body first, so bytes that are ready at the deadline are not
        // refused for arriving with it.
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(next) => {
                Poll::Ready(next.map(|chunk| chunk.map_err(axum::Error::into_inner)))
            }
            Poll::Pending => match this.deadline.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.elapsed.store(true, Ordering::Release);
                    Poll::Ready(Some(Err(BodyDeadlineElapsed.into())))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::post;
    use futures::StreamExt as _;
    use tower::ServiceExt as _;

    use super::*;

    /// A body of `first` and then nothing, forever.
    fn stalled_after(first: &'static str) -> Body {
        let stalled =
            futures::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(first)) })
                .chain(futures::stream::pending());
        Body::from_stream(stalled)
    }

    fn post_to(path: &str, body: Body) -> Request {
        Request::post(path).body(body).unwrap()
    }

    /// A route on axum's own `Bytes` extractor, which has no deadline of
    /// its own, is answered 408 with the envelope when its body stalls,
    /// and sees its body when it arrives whole. The layer covers a route
    /// whoever writes it, so a future route cannot forget the deadline.
    #[tokio::test]
    async fn any_route_reading_a_body_is_answered_408_at_the_deadline() {
        let app = Router::new()
            .route(
                "/probe",
                post(|body: Bytes| async move { body.len().to_string() }),
            )
            .layer(axum::middleware::from_fn_with_state(
                Duration::from_millis(100),
                bound_body_read,
            ));

        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot(post_to("/probe", stalled_after("ab"))),
        )
        .await
        .expect("the deadline answered the request")
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(80),
            "answered before the deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .unwrap(),
            "no-store"
        );
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let envelope: trawl_api::ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(envelope.error.code, trawl_api::ErrorCode::Timeout);
        assert_eq!(envelope.error.message, crate::error::BODY_TIMEOUT_MESSAGE);

        let whole = app
            .oneshot(post_to("/probe", Body::from("abc")))
            .await
            .unwrap();
        assert_eq!(whole.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(whole.into_body(), 16).await.unwrap(),
            "3"
        );
    }

    /// The deadline is on the body, not the handler: a handler that
    /// never reads its stalled body and answers after the deadline has
    /// passed is answered as it chose.
    #[tokio::test]
    async fn an_unread_body_never_fires_the_deadline() {
        let app = Router::new()
            .route(
                "/late",
                post(|| async {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    "late"
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                Duration::from_millis(50),
                bound_body_read,
            ));

        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot(post_to("/late", stalled_after("{"))),
        )
        .await
        .expect("the handler answered")
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(resp.into_body(), 16).await.unwrap(),
            "late"
        );
    }
}
