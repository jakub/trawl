// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Internal telemetry: custom tracing [`Layer`] that writes server events
//! to the ingest WAL as `service:trawld`.
//!
//! ## Bootstrap
//!
//! The tracing subscriber is initialized before the WAL writer exists (we
//! need tracing for config-loading logs). [`WalHandle`] wraps an
//! [`OnceLock`] — the layer registers at init time and buffers events
//! in memory until [`WalHandle::set`] injects the writer after startup.
//! This ensures bootstrap events (config loading, cert generation, etc.)
//! are captured rather than silently dropped. A 1 MiB cap prevents
//! unbounded growth if the writer is never set.
//!
//! ## Buffering
//!
//! Events are serialized to ndjson and accumulated in an in-memory buffer.
//! A background task flushes the buffer to the WAL every second. This
//! avoids creating hundreds of tiny WAL files under load while keeping
//! latency low.
//!
//! ## Infinite recursion guard
//!
//! [`WalLayerInner::flush`] uses `eprintln!` for error reporting, NEVER
//! `tracing::*`. A tracing event inside the layer's own flush path would
//! re-enter `on_event` and loop forever.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::ingest::wal::WalWriter;

// ---------------------------------------------------------------------------
// WalHandle: deferred writer injection
// ---------------------------------------------------------------------------

/// Shared handle for deferred WAL writer injection.
///
/// Cloned into the [`WalLayer`] and retained by `main()`. Once the WAL
/// writer is ready, [`set`](Self::set) activates event capture.
#[derive(Debug, Clone)]
pub struct WalHandle(Arc<OnceLock<Arc<WalWriter>>>);

impl Default for WalHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl WalHandle {
    /// Create a new empty handle.
    pub fn new() -> Self {
        Self(Arc::new(OnceLock::new()))
    }

    /// Inject the WAL writer. Called once after config is loaded.
    /// Subsequent calls are silently ignored (first write wins).
    pub fn set(&self, writer: Arc<WalWriter>) {
        let _ = self.0.set(writer);
    }

    /// Get the writer, if available.
    fn get(&self) -> Option<&Arc<WalWriter>> {
        self.0.get()
    }
}

// ---------------------------------------------------------------------------
// WalLayer: tracing Layer implementation
// ---------------------------------------------------------------------------

/// Tracing layer that serializes events as ndjson and writes them to the
/// ingest WAL as `service:trawld`.
#[derive(Clone)]
pub struct WalLayer {
    inner: Arc<WalLayerInner>,
}

struct WalLayerInner {
    handle: WalHandle,
    buffer: Mutex<Vec<u8>>,
    /// Cached hostname, resolved once at layer creation.
    host: String,
    /// Bytes lost due to WAL write failures (accumulated, reset on report).
    dropped_bytes: AtomicU64,
    /// Deferred event bus for real-time fanout (SSE streaming).
    bus: OnceLock<Arc<crate::bus::LocalEventBus>>,
    /// Deferred hot buffer for synchronous insertion (query freshness).
    hot_buffer: OnceLock<Arc<crate::hot_buffer::HotBuffer>>,
    /// Event maps accumulated since last flush, for bus/hot buffer publishing.
    event_maps: Mutex<Vec<serde_json::Map<String, serde_json::Value>>>,
}

impl std::fmt::Debug for WalLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalLayer")
            .field("active", &self.inner.handle.get().is_some())
            .field("buffer_bytes", &self.inner.buffer.lock().len())
            .finish()
    }
}

impl WalLayer {
    /// Create a new layer backed by the given handle.
    pub fn new(handle: WalHandle) -> Self {
        let host = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(WalLayerInner {
                handle,
                buffer: Mutex::new(Vec::with_capacity(8192)),
                host,
                dropped_bytes: AtomicU64::new(0),
                bus: OnceLock::new(),
                hot_buffer: OnceLock::new(),
                event_maps: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Inject the event bus for real-time fanout (SSE streaming).
    /// Called once after `AppState` is constructed. Subsequent calls are
    /// silently ignored (first write wins).
    pub fn set_bus(&self, bus: Arc<crate::bus::LocalEventBus>) {
        let _ = self.inner.bus.set(bus);
    }

    /// Inject the hot buffer for synchronous event insertion.
    /// Called once after `AppState` is constructed. Subsequent calls are
    /// silently ignored (first write wins).
    pub fn set_hot_buffer(&self, buf: Arc<crate::hot_buffer::HotBuffer>) {
        let _ = self.inner.hot_buffer.set(buf);
    }

    /// Flush the buffer to the WAL. Called periodically by the background
    /// task and on shutdown.
    pub fn flush(&self) {
        self.inner.flush();
    }
}

impl WalLayerInner {
    /// Swap out the buffer and write its contents to the WAL.
    fn flush(&self) {
        let Some(writer) = self.handle.get() else {
            return;
        };

        let data = {
            let mut buf = self.buffer.lock();
            if buf.is_empty() {
                return;
            }
            std::mem::take(&mut *buf)
        };

        // Drain event maps regardless of WAL write outcome — they mirror
        // the byte buffer and must stay in sync.
        let maps = std::mem::take(&mut *self.event_maps.lock());

        match writer.write("trawld", &data) {
            Err(e) => {
                // MUST NOT use tracing here — infinite recursion.
                eprintln!("[trawl-telemetry] WAL write failed: {e}");
                self.dropped_bytes
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
            }
            Ok(wal_path) if !maps.is_empty() => {
                // Insert into hot buffer synchronously (query freshness),
                // then publish to event bus for SSE streaming.
                // batch_id MUST match the WAL filename stem so compaction
                // can drain the hot buffer after writing parquet.
                use crate::bus::{EventBus, IngestBatch};
                let batch_id: Arc<str> = wal_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("trawld_unknown")
                    .into();
                let batch = Arc::new(IngestBatch {
                    batch_id,
                    service: "trawld".into(),
                    events: maps,
                    byte_size: data.len(),
                });
                if let Some(buf) = self.hot_buffer.get() {
                    buf.insert(Arc::clone(&batch));
                }
                if let Some(bus) = self.bus.get() {
                    let _ = bus.publish(batch);
                }

                // Report any previously dropped bytes. Safe from recursion:
                // on_event only buffers, the tracing event will be picked up
                // on the NEXT flush cycle.
                let prev = self.dropped_bytes.swap(0, Ordering::Relaxed);
                if prev > 0 {
                    tracing::warn!(
                        event_type = "telemetry_dropped",
                        dropped_bytes = prev,
                        "telemetry events were lost due to WAL write failure"
                    );
                }
            }
            Ok(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// JsonVisitor: field collection
// ---------------------------------------------------------------------------

/// Collects tracing fields into a JSON-compatible map.
struct JsonVisitor {
    fields: BTreeMap<String, serde_json::Value>,
}

impl JsonVisitor {
    fn new() -> Self {
        Self {
            fields: BTreeMap::new(),
        }
    }
}

impl Visit for JsonVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), json!(format!("{value:?}")));
    }
}

// ---------------------------------------------------------------------------
// SpanFields: stored in span extensions for scope walking
// ---------------------------------------------------------------------------

/// Fields recorded on a span, stored in span extensions so [`WalLayer`]
/// can walk the span scope and collect inherited fields.
#[derive(Debug, Default)]
struct SpanFields(BTreeMap<String, serde_json::Value>);

// ---------------------------------------------------------------------------
// Layer implementation
// ---------------------------------------------------------------------------

impl<S> Layer<S> for WalLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = JsonVisitor::new();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.fields));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut ext = span.extensions_mut();
            if let Some(fields) = ext.get_mut::<SpanFields>() {
                let mut visitor = JsonVisitor::new();
                values.record(&mut visitor);
                fields.0.extend(visitor.fields);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Pre-init cap: if the writer isn't set yet and the buffer is
        // already over 1 MiB, drop this event to prevent unbounded growth
        // (e.g. if telemetry is disabled and the writer is never injected).
        const PRE_INIT_CAP: usize = 1024 * 1024;
        if self.inner.handle.get().is_none() && self.inner.buffer.lock().len() >= PRE_INIT_CAP {
            return;
        }

        // Collect event-level fields.
        let mut visitor = JsonVisitor::new();
        event.record(&mut visitor);

        // Walk span scope (root → leaf), collecting inherited fields.
        // Inner spans override outer spans for same-named fields.
        let mut span_fields = BTreeMap::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                let ext = span.extensions();
                if let Some(fields) = ext.get::<SpanFields>() {
                    span_fields.extend(fields.0.clone());
                }
            }
        }

        // Span fields are the base; event fields override.
        span_fields.extend(visitor.fields);

        let metadata = event.metadata();

        // Extract message — tracing stores it as the "message" field.
        let message = span_fields
            .remove("message")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();

        // Derive event_type: prefer explicit field, fall back to message.
        let event_type = span_fields
            .remove("event_type")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| message_to_event_type(&message));

        let mut record = serde_json::Map::with_capacity(8 + span_fields.len());
        record.insert(
            "timestamp".into(),
            json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
        record.insert("service".into(), json!("trawld"));
        record.insert("host".into(), json!(&self.inner.host));
        record.insert("level".into(), json!(metadata.level().as_str()));
        record.insert("target".into(), json!(metadata.target()));
        record.insert("event_type".into(), json!(event_type));
        record.insert("message".into(), json!(message));

        // Merge remaining span + event fields.
        for (k, v) in span_fields {
            record.entry(k).or_insert(v);
        }

        // Clone the map for event bus publishing (before moving into Value).
        self.inner.event_maps.lock().push(record.clone());

        // Serialize and buffer. serde_json::to_vec on Value cannot fail.
        let mut line = serde_json::to_vec(&serde_json::Value::Object(record))
            .expect("JSON serialization of Value is infallible");
        line.push(b'\n');

        self.inner.buffer.lock().extend_from_slice(&line);
    }
}

// ---------------------------------------------------------------------------
// event_type derivation
// ---------------------------------------------------------------------------

/// Normalize a tracing message to a flat `snake_case` event type.
///
/// Takes the first clause (before `:`) for brevity, lowercases,
/// and replaces non-alphanumeric characters with underscores.
///
/// ```text
/// "query failed: bad request" → "query_failed"
/// "auth failed: missing or malformed Authorization header" → "auth_failed"
/// "query complete" → "query_complete"
/// ```
fn message_to_event_type(message: &str) -> String {
    let base = message.split(':').next().unwrap_or(message).trim();
    let raw: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // Collapse multiple underscores and trim edges.
    raw.split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

// ---------------------------------------------------------------------------
// Flush task
// ---------------------------------------------------------------------------

/// Spawn the periodic buffer flush task.
///
/// Flushes every `interval` to batch WAL writes. Returns a [`JoinHandle`]
/// for shutdown coordination. Send `true` on `shutdown_rx` to trigger a
/// final flush and exit.
pub fn spawn_flush_task(
    layer: WalLayer,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {
                    layer.flush();
                }
                _ = shutdown_rx.changed() => {
                    layer.flush();
                    break;
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_to_event_type_simple() {
        assert_eq!(message_to_event_type("query complete"), "query_complete");
    }

    #[test]
    fn message_to_event_type_with_colon() {
        assert_eq!(
            message_to_event_type("query failed: bad request"),
            "query_failed"
        );
    }

    #[test]
    fn message_to_event_type_auth() {
        assert_eq!(
            message_to_event_type("auth failed: missing or malformed Authorization header"),
            "auth_failed"
        );
    }

    #[test]
    fn message_to_event_type_empty() {
        assert_eq!(message_to_event_type(""), "");
    }

    #[test]
    fn message_to_event_type_special_chars() {
        assert_eq!(
            message_to_event_type("TLS handshake failed"),
            "tls_handshake_failed"
        );
    }

    #[test]
    fn wal_handle_set_and_get() {
        let handle = WalHandle::new();
        assert!(handle.get().is_none());

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        handle.set(Arc::clone(&writer));

        assert!(handle.get().is_some());
    }

    #[test]
    fn wal_layer_buffers_before_writer_set() {
        use tracing_subscriber::prelude::*;

        let handle = WalHandle::new();
        let layer = WalLayer::new(handle.clone());
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Emit event before writer is available — should be buffered.
        tracing::info!(event_type = "bootstrap", "pre-init event");
        assert!(!layer_ref.inner.buffer.lock().is_empty());

        // Flush without writer — buffer should be retained (not drained).
        layer_ref.flush();
        assert!(!layer_ref.inner.buffer.lock().is_empty());

        // Now inject the writer and flush — buffer should drain.
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();
        handle.set(Arc::clone(&writer));

        layer_ref.flush();
        assert!(layer_ref.inner.buffer.lock().is_empty());

        // Verify the bootstrap event reached the WAL.
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert_eq!(files.len(), 1);
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["event_type"], "bootstrap");
    }

    #[test]
    fn json_visitor_collects_fields() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer));

        let layer = WalLayer::new(handle);
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(
            event_type = "test_fields",
            user = "admin",
            rows = 42u64,
            timed_out = false,
            duration = 1.5f64,
            "fields test"
        );

        layer_ref.flush();

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(parsed["user"], "admin");
        assert_eq!(parsed["rows"], 42);
        assert_eq!(parsed["timed_out"], false);
        assert_eq!(parsed["duration"], 1.5);
    }

    #[test]
    fn wal_layer_buffers_events() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer));

        let layer = WalLayer::new(handle);
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(
            event_type = "test_event",
            user = "alice",
            rows = 42u64,
            "test complete"
        );

        // Buffer should have data now.
        assert!(!layer_ref.inner.buffer.lock().is_empty());

        // Flush to WAL.
        layer_ref.flush();
        assert!(layer_ref.inner.buffer.lock().is_empty());

        // Verify WAL file was written.
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert_eq!(files.len(), 1, "expected exactly one WAL file");

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(parsed["service"], "trawld");
        assert_eq!(parsed["event_type"], "test_event");
        assert_eq!(parsed["user"], "alice");
        assert_eq!(parsed["rows"], 42);
        assert_eq!(parsed["message"], "test complete");
        assert!(parsed["timestamp"].is_string());
        assert!(parsed["level"].is_string());
        assert!(parsed["target"].is_string());
    }

    #[test]
    fn wal_layer_inherits_span_fields() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer));

        let layer = WalLayer::new(handle);
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!(
            "request",
            request_id = "01HZEXAMPLE000000000000000",
            peer_addr = "1.2.3.4"
        );
        let _enter = span.enter();

        tracing::info!(
            event_type = "query_complete",
            rows = 10u64,
            "query complete"
        );

        layer_ref.flush();

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        // Span fields should be inherited.
        assert_eq!(parsed["request_id"], "01HZEXAMPLE000000000000000");
        assert_eq!(parsed["peer_addr"], "1.2.3.4");
        // Event fields should also be present.
        assert_eq!(parsed["rows"], 10);
        assert_eq!(parsed["event_type"], "query_complete");
    }

    #[test]
    fn event_type_explicit_overrides_message() {
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer));

        let layer = WalLayer::new(handle);
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Explicit event_type should win over message-derived.
        tracing::info!(event_type = "custom_type", "some random message");

        layer_ref.flush();

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(parsed["event_type"], "custom_type");
        assert_eq!(parsed["message"], "some random message");
    }

    #[tokio::test]
    async fn flush_publishes_to_event_bus() {
        use crate::bus::{EventBus, EventSubscriber, LocalEventBus};
        use tracing_subscriber::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        writer.ensure_dir().unwrap();

        let handle = WalHandle::new();
        handle.set(Arc::clone(&writer));

        let bus = Arc::new(LocalEventBus::new(16));
        let mut sub = bus.subscribe();

        let layer = WalLayer::new(handle);
        layer.set_bus(Arc::clone(&bus));
        let layer_ref = layer.clone();

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(event_type = "test_bus", user = "alice", "bus test event");

        layer_ref.flush();

        // The batch should arrive on the subscriber.
        let batch = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("timed out waiting for batch")
            .expect("recv failed");

        assert_eq!(batch.service.as_ref(), "trawld");
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["event_type"], "test_bus");
        assert_eq!(batch.events[0]["user"], "alice");
        assert!(batch.byte_size > 0);
        assert!(
            batch.batch_id.starts_with("trawld_"),
            "batch_id should use WAL filename stem: {}",
            batch.batch_id
        );
    }
}
