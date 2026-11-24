// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One deadline for every request body the proxy reads (ADR-0054).

use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::BoxError;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use hyper::body::{Frame, SizeHint};
use tokio::time::Sleep;

use crate::error::ProxyError;

#[derive(Debug)]
pub struct BodyDeadlineElapsed;

impl std::fmt::Display for BodyDeadlineElapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body not received within the body deadline")
    }
}

impl std::error::Error for BodyDeadlineElapsed {}

pub async fn bound_body_read(
    State(deadline): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let elapsed = Arc::new(AtomicBool::new(false));
    let body = Body::new(DeadlineBody {
        inner: body,
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

struct DeadlineBody {
    inner: Body,
    deadline: Pin<Box<Sleep>>,
    elapsed: Arc<AtomicBool>,
}

impl HttpBody for DeadlineBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.elapsed.load(Ordering::Relaxed) {
            return Poll::Ready(Some(Err(BodyDeadlineElapsed.into())));
        }
        // The body first, so bytes that are ready at the deadline are not
        // refused for arriving with it.
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(next) => {
                Poll::Ready(next.map(|frame| frame.map_err(axum::Error::into_inner)))
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

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
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

    fn stalled_after(first: &'static str) -> Body {
        let stalled =
            futures::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(first)) })
                .chain(futures::stream::pending());
        Body::from_stream(stalled)
    }

    fn post_to(path: &str, body: Body) -> Request {
        Request::post(path).body(body).unwrap()
    }

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

    #[tokio::test]
    async fn the_body_under_the_deadline_keeps_its_size_hint() {
        let app = Router::new()
            .route(
                "/hint",
                post(|request: Request| async move {
                    format!("{:?}", request.body().size_hint().exact())
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                Duration::from_secs(5),
                bound_body_read,
            ));

        for (body, hint) in [
            (Body::from("abc"), "Some(3)"),
            (Body::empty(), "Some(0)"),
            (stalled_after("ab"), "None"),
        ] {
            let resp = app.clone().oneshot(post_to("/hint", body)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                axum::body::to_bytes(resp.into_body(), 16).await.unwrap(),
                hint
            );
        }
    }

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
