//! Event bus for real-time fanout from ingest to subscribers.
//!
//! The bus sits between the ingest handler and downstream consumers
//! (hot buffer, live streaming). It uses `tokio::sync::broadcast` for
//! lock-free fanout to multiple subscribers.
//!
//! The trait boundary (`EventBus` / `EventSubscriber`) exists to keep the
//! door open for future multi-node backends (NATS, Redis Streams) without
//! requiring any consumer changes.

use std::sync::Arc;

/// A batch of events published to the bus after a successful WAL write.
///
/// The `batch_id` is the WAL filename stem (e.g. `nginx_1739000000000_abcd`),
/// which doubles as the coordination key for hot buffer draining after
/// compaction.
#[derive(Debug, Clone)]
pub struct IngestBatch {
    /// WAL filename stem — unique identifier for this batch.
    pub batch_id: Arc<str>,
    /// Service name for these events.
    pub service: Arc<str>,
    /// Parsed event objects ready for in-memory filtering.
    pub events: Vec<serde_json::Map<String, serde_json::Value>>,
}

/// Error returned when receiving from a subscriber.
#[derive(Debug, thiserror::Error)]
pub enum RecvError {
    /// The subscriber fell behind and missed `n` messages.
    /// The bus continued without blocking — missed events are still
    /// in the WAL and will become queryable after compaction.
    #[error("subscriber lagged behind by {0} messages")]
    Lagged(u64),

    /// The bus has been dropped (server shutting down).
    #[error("event bus closed")]
    Closed,
}

/// Trait for publishing events to the bus.
///
/// Implementors handle fanout to all active subscribers.
pub trait EventBus: Send + Sync + 'static {
    /// The subscriber type returned by [`subscribe`](Self::subscribe).
    type Subscriber: EventSubscriber;

    /// Publish a batch of events to all subscribers.
    ///
    /// Returns the number of subscribers that received the batch.
    /// A return of 0 is normal (no active subscribers).
    fn publish(&self, batch: Arc<IngestBatch>) -> usize;

    /// Create a new subscriber that receives future published batches.
    fn subscribe(&self) -> Self::Subscriber;
}

/// Trait for receiving events from the bus.
pub trait EventSubscriber: Send + 'static {
    /// Receive the next batch, waiting if necessary.
    ///
    /// Returns `Err(RecvError::Lagged(n))` if this subscriber fell behind
    /// and missed `n` messages. The next call will return the oldest
    /// available message.
    fn recv(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Arc<IngestBatch>, RecvError>> + Send;
}

/// In-process event bus backed by `tokio::sync::broadcast`.
#[derive(Debug)]
pub struct LocalEventBus {
    tx: tokio::sync::broadcast::Sender<Arc<IngestBatch>>,
}

impl LocalEventBus {
    /// Create a new bus with the given channel capacity.
    ///
    /// When the channel is full, the oldest message is dropped and slow
    /// subscribers receive `RecvError::Lagged` on their next recv.
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(capacity);
        Self { tx }
    }
}

impl EventBus for LocalEventBus {
    type Subscriber = LocalSubscriber;

    fn publish(&self, batch: Arc<IngestBatch>) -> usize {
        // send() returns Err only if there are zero receivers, which is fine.
        self.tx.send(batch).unwrap_or(0)
    }

    fn subscribe(&self) -> Self::Subscriber {
        LocalSubscriber {
            rx: self.tx.subscribe(),
        }
    }
}

/// Subscriber for the local in-process event bus.
#[derive(Debug)]
pub struct LocalSubscriber {
    rx: tokio::sync::broadcast::Receiver<Arc<IngestBatch>>,
}

impl EventSubscriber for LocalSubscriber {
    async fn recv(&mut self) -> Result<Arc<IngestBatch>, RecvError> {
        match self.rx.recv().await {
            Ok(batch) => Ok(batch),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => Err(RecvError::Lagged(n)),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => Err(RecvError::Closed),
        }
    }
}

/// Default event bus channel capacity.
pub const DEFAULT_EVENT_BUS_CAPACITY: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_and_receive() {
        let bus = LocalEventBus::new(16);
        let mut sub = bus.subscribe();

        let batch = Arc::new(IngestBatch {
            batch_id: "test_batch_001".into(),
            service: "nginx".into(),
            events: vec![{
                let mut m = serde_json::Map::new();
                m.insert("message".into(), serde_json::Value::String("hello".into()));
                m
            }],
        });

        let receivers = bus.publish(Arc::clone(&batch));
        assert_eq!(receivers, 1);

        let received = sub.recv().await.unwrap();
        assert_eq!(received.batch_id.as_ref(), "test_batch_001");
        assert_eq!(received.events.len(), 1);
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = LocalEventBus::new(16);
        let mut sub1 = bus.subscribe();
        let mut sub2 = bus.subscribe();

        let batch = Arc::new(IngestBatch {
            batch_id: "multi_001".into(),
            service: "test".into(),
            events: vec![],
        });

        let receivers = bus.publish(Arc::clone(&batch));
        assert_eq!(receivers, 2);

        let r1 = sub1.recv().await.unwrap();
        let r2 = sub2.recv().await.unwrap();
        assert_eq!(r1.batch_id.as_ref(), "multi_001");
        assert_eq!(r2.batch_id.as_ref(), "multi_001");
    }

    #[tokio::test]
    async fn lagged_subscriber() {
        // Channel capacity of 2 — publishing 3 messages should cause lag.
        let bus = LocalEventBus::new(2);
        let mut sub = bus.subscribe();

        for i in 0..3 {
            let batch = Arc::new(IngestBatch {
                batch_id: format!("lag_{i}").into(),
                service: "test".into(),
                events: vec![],
            });
            bus.publish(batch);
        }

        // First recv should report lag.
        let result = sub.recv().await;
        assert!(
            matches!(result, Err(RecvError::Lagged(_))),
            "expected Lagged, got {result:?}"
        );

        // Subsequent recv should succeed with the oldest available message.
        let batch = sub.recv().await.unwrap();
        assert_eq!(batch.batch_id.as_ref(), "lag_1");
    }

    #[tokio::test]
    async fn closed_bus() {
        let mut sub = {
            let bus = LocalEventBus::new(16);
            bus.subscribe()
            // bus drops here
        };

        let result = sub.recv().await;
        assert!(
            matches!(result, Err(RecvError::Closed)),
            "expected Closed, got {result:?}"
        );
    }

    #[test]
    fn publish_with_no_subscribers() {
        let bus = LocalEventBus::new(16);
        let batch = Arc::new(IngestBatch {
            batch_id: "orphan".into(),
            service: "test".into(),
            events: vec![],
        });
        // Should not panic, returns 0.
        let receivers = bus.publish(batch);
        assert_eq!(receivers, 0);
    }
}
