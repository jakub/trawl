// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared pieces for the request-limit tests: held routes, an edge config,
//! and request and response helpers.

#![allow(dead_code)] // each test module uses a subset of these items

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, Response, StatusCode};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tower::ServiceExt as _;
use trawl_server::config::RateLimitConfig;
use trawl_server::state::HttpConfig;

/// A failure deadline, never an ordering device: every wait in these tests
/// is on a channel or a gate, and this only turns a hang into a named
/// failure.
pub const DEADLINE: Duration = Duration::from_secs(20);

pub const REGULAR_MESSAGE: &str = "trawld is at its HTTP request limit \
     ([server] max_concurrent_requests); the request was not processed; retry later with backoff";

pub const PROBE_MESSAGE: &str = "trawld is at its HTTP probe allowance; the request was not processed; retry later with \
     backoff";

pub const CONTROL_MESSAGE: &str = "trawld is at its HTTP control allowance; the request was not processed; retry later with \
     backoff";

#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Stdout {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Every test that reads [`stdout_lines`] calls this first.
pub fn capture_logs() {
    captured();
}

fn captured() -> &'static Stdout {
    static STDOUT: OnceLock<Stdout> = OnceLock::new();
    STDOUT.get_or_init(install_capture)
}

fn install_capture() -> Stdout {
    use tracing_subscriber::util::SubscriberInitExt as _;
    let stdout = Stdout::default();
    let (subscriber, _) = trawl_server::telemetry::build_subscriber(
        trawl_server::telemetry::DEFAULT_LOG_FILTER,
        trawl_server::telemetry::LogSinks {
            stdout_ansi: false,
            stdout: Some(stdout.clone()),
            wal: None,
            file_log: false,
        },
    );
    subscriber.init();
    stdout
}

pub fn stdout_lines() -> Vec<String> {
    let bytes = captured().0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

pub fn field(line: &str, name: &str) -> Option<String> {
    let at = line.find(&format!(" {name}="))? + name.len() + 2;
    let rest = &line[at..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"').map(|end| quoted[..end].to_owned())
    } else {
        Some(rest.split_whitespace().next().unwrap_or("").to_owned())
    }
}

pub fn http_config(limit: usize) -> HttpConfig {
    HttpConfig {
        max_request_body_bytes: 128 * 1024,
        max_concurrent_requests: limit,
        shutdown_drain_secs: 5,
        cors_allowed_origins: vec![],
        ingest_max_body_bytes: None,
        rate_limit: RateLimitConfig::default(),
    }
}

pub struct Holds {
    entered_tx: mpsc::UnboundedSender<&'static str>,
    entered_rx: mpsc::UnboundedReceiver<&'static str>,
    gates: Mutex<HashMap<&'static str, Arc<Semaphore>>>,
}

impl Default for Holds {
    fn default() -> Self {
        Self::new()
    }
}

impl Holds {
    pub fn new() -> Self {
        let (entered_tx, entered_rx) = mpsc::unbounded_channel();
        Self {
            entered_tx,
            entered_rx,
            gates: Mutex::new(HashMap::new()),
        }
    }

    fn gate(&self, label: &'static str) -> Arc<Semaphore> {
        Arc::clone(
            self.gates
                .lock()
                .unwrap()
                .entry(label)
                .or_insert_with(|| Arc::new(Semaphore::new(0))),
        )
    }

    pub fn handler(
        &self,
        label: &'static str,
    ) -> impl Fn() -> std::pin::Pin<Box<dyn Future<Output = StatusCode> + Send>>
    + Clone
    + Send
    + Sync
    + 'static {
        let entered = self.entered_tx.clone();
        let gate = self.gate(label);
        move || {
            let entered = entered.clone();
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                entered.send(label).unwrap();
                // A closed gate releases everyone: see `release_all`.
                if let Ok(permit) = gate.acquire().await {
                    permit.forget();
                }
                StatusCode::OK
            })
        }
    }

    pub fn free(
        &self,
        label: &'static str,
    ) -> impl Fn() -> std::pin::Pin<Box<dyn Future<Output = StatusCode> + Send>>
    + Clone
    + Send
    + Sync
    + 'static {
        let entered = self.entered_tx.clone();
        move || {
            let entered = entered.clone();
            Box::pin(async move {
                entered.send(label).unwrap();
                StatusCode::OK
            })
        }
    }

    pub async fn entered(&mut self) -> &'static str {
        tokio::time::timeout(DEADLINE, self.entered_rx.recv())
            .await
            .expect("a held request entered its handler before the deadline")
            .expect("the entry channel is open")
    }

    pub async fn entered_n(&mut self, n: usize) -> Vec<&'static str> {
        let mut labels = Vec::with_capacity(n);
        for _ in 0..n {
            labels.push(self.entered().await);
        }
        labels.sort_unstable();
        labels
    }

    pub fn nothing_entered(&mut self) -> bool {
        matches!(
            self.entered_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        )
    }

    pub fn release(&self, label: &'static str) {
        self.gate(label).add_permits(1);
    }

    pub fn release_all(&self) {
        for gate in self.gates.lock().unwrap().values() {
            gate.close();
        }
    }
}

pub fn request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

pub fn send(app: &Router, request: Request<Body>) -> JoinHandle<Response<Body>> {
    let app = app.clone();
    tokio::spawn(async move { app.oneshot(request).await.unwrap() })
}

pub async fn call(app: &Router, request: Request<Body>) -> Response<Body> {
    tokio::time::timeout(DEADLINE, app.clone().oneshot(request))
        .await
        .expect("the response head arrived without waiting on a held request")
        .unwrap()
}

pub async fn finish(handle: JoinHandle<Response<Body>>) -> Response<Body> {
    tokio::time::timeout(DEADLINE, handle)
        .await
        .expect("the released request finished before the deadline")
        .expect("the request task did not panic")
}

pub async fn body_bytes(response: Response<Body>) -> Bytes {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
}

pub async fn body_json(response: Response<Body>) -> serde_json::Value {
    serde_json::from_slice(&body_bytes(response).await).expect("a JSON body")
}

pub async fn assert_refused(response: Response<Body>, message: &str) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "request_limit_reached", "{body}");
    assert_eq!(body["error"]["message"], message, "{body}");
    body
}

/// Every request-count series is a whole count, so it is read as one: a
/// gauge renders as a float, and `Display` writes a whole float without a
/// fraction.
pub fn sample(rendered: &str, name: &str, allowance: &str) -> u64 {
    let series = format!("{name}{{allowance=\"{allowance}\"}} ");
    let value: f64 = rendered
        .lines()
        .find_map(|line| line.strip_prefix(&series))
        .unwrap_or_else(|| panic!("no {series:?} in:\n{rendered}"))
        .trim()
        .parse()
        .unwrap();
    value
        .to_string()
        .parse()
        .unwrap_or_else(|_| panic!("{series:?} is not a whole count: {value}"))
}

pub struct PollRecorder {
    polled: Arc<AtomicBool>,
}

impl PollRecorder {
    pub fn body() -> (Body, Arc<AtomicBool>) {
        let polled = Arc::new(AtomicBool::new(false));
        let stream = Self {
            polled: Arc::clone(&polled),
        };
        (Body::from_stream(stream), polled)
    }

    pub fn was_polled(flag: &AtomicBool) -> bool {
        flag.load(Ordering::SeqCst)
    }
}

impl tokio_stream::Stream for PollRecorder {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.polled.store(true, Ordering::SeqCst);
        Poll::Ready(Some(Ok(Bytes::from_static(b"{\"polled\":true}\n"))))
    }
}
