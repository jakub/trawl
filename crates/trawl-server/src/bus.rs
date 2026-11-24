// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Event bus for real-time fanout from ingest to subscribers.

use std::sync::Arc;

/// A batch of events published to the bus after a successful WAL write.
#[derive(Debug)]
pub struct IngestBatch {
    pub batch_id: Arc<str>,
    pub service: Arc<str>,
    pub events: Vec<serde_json::Map<String, serde_json::Value>>,
    /// Approximate size in bytes (serialized ndjson byte count).
    /// This is the on-wire/on-disk size, not the in-memory representation
    /// size.
    pub byte_size: usize,
}

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

pub trait EventBus: Send + Sync + 'static {
    type Subscriber: EventSubscriber;

    fn publish(&self, batch: Arc<IngestBatch>) -> usize;

    fn subscribe(&self) -> Self::Subscriber;
}

pub trait EventSubscriber: Send + 'static {
    /// Returns `Err(RecvError::Lagged(n))` if this subscriber fell behind
    /// and missed `n` messages. The next call will return the oldest
    /// available message.
    fn recv(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Arc<IngestBatch>, RecvError>> + Send;

    /// The next batch if one has already arrived, without waiting: `None`
    /// when none has. Otherwise as [`Self::recv`].
    fn try_recv(&mut self) -> Option<Result<Arc<IngestBatch>, RecvError>>;
}

#[derive(Debug)]
pub struct LocalEventBus {
    tx: tokio::sync::broadcast::Sender<Arc<IngestBatch>>,
}

impl LocalEventBus {
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

    fn try_recv(&mut self) -> Option<Result<Arc<IngestBatch>, RecvError>> {
        use tokio::sync::broadcast::error::TryRecvError;
        match self.rx.try_recv() {
            Ok(batch) => Some(Ok(batch)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Lagged(n)) => Some(Err(RecvError::Lagged(n))),
            Err(TryRecvError::Closed) => Some(Err(RecvError::Closed)),
        }
    }
}

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
            byte_size: 0,
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
            byte_size: 0,
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
        let bus = LocalEventBus::new(2);
        let mut sub = bus.subscribe();

        for i in 0..3 {
            let batch = Arc::new(IngestBatch {
                batch_id: format!("lag_{i}").into(),
                service: "test".into(),
                events: vec![],
                byte_size: 0,
            });
            bus.publish(batch);
        }

        let result = sub.recv().await;
        assert!(
            matches!(result, Err(RecvError::Lagged(_))),
            "expected Lagged, got {result:?}"
        );

        let batch = sub.recv().await.unwrap();
        assert_eq!(batch.batch_id.as_ref(), "lag_1");
    }

    #[test]
    fn try_recv_takes_only_what_has_arrived() {
        let bus = LocalEventBus::new(2);
        let mut sub = bus.subscribe();
        assert!(sub.try_recv().is_none(), "nothing has arrived");

        for i in 0..3 {
            bus.publish(Arc::new(IngestBatch {
                batch_id: format!("try_{i}").into(),
                service: "test".into(),
                events: vec![],
                byte_size: 0,
            }));
        }
        assert!(matches!(sub.try_recv(), Some(Err(RecvError::Lagged(1)))));
        let next = |sub: &mut LocalSubscriber| match sub.try_recv() {
            Some(Ok(batch)) => batch.batch_id.to_string(),
            other => panic!("expected a batch, got {other:?}"),
        };
        assert_eq!(next(&mut sub), "try_1");
        assert_eq!(next(&mut sub), "try_2");
        assert!(sub.try_recv().is_none(), "everything was taken");

        drop(bus);
        assert!(matches!(sub.try_recv(), Some(Err(RecvError::Closed))));
    }

    #[tokio::test]
    async fn closed_bus() {
        let mut sub = {
            let bus = LocalEventBus::new(16);
            bus.subscribe()
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
            byte_size: 0,
        });
        let receivers = bus.publish(batch);
        assert_eq!(receivers, 0);
    }
}
