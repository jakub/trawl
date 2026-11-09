//! Test-only capture of the tracing events a closure emits, with each
//! event's level, for the tests that pin what default-level events may
//! carry (ADR-0040, amended 2026-11-08).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One captured event: its level and every field, the message among them.
#[derive(Debug)]
pub(crate) struct LoggedEvent {
    pub(crate) level: tracing::Level,
    pub(crate) fields: HashMap<String, String>,
}

impl LoggedEvent {
    /// Whether the event is emitted at a default level: INFO, WARN or ERROR.
    pub(crate) fn is_default_level(&self) -> bool {
        self.level <= tracing::Level::INFO
    }

    /// Every field value, joined, for a marker search.
    pub(crate) fn text(&self) -> String {
        self.fields.values().cloned().collect::<Vec<_>>().join(" ")
    }
}

struct EventFields<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for EventFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

#[derive(Clone, Default)]
struct EventLog(Arc<Mutex<Vec<LoggedEvent>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLog {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        event.record(&mut EventFields(&mut fields));
        self.0.lock().unwrap().push(LoggedEvent {
            level: *event.metadata().level(),
            fields,
        });
    }
}

/// Every event `work` emits on this thread, at every level.
pub(crate) fn events_of(work: impl FnOnce()) -> Vec<LoggedEvent> {
    use tracing_subscriber::layer::SubscriberExt;

    let log = EventLog::default();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(log.clone()), work);
    std::mem::take(&mut *log.0.lock().unwrap())
}
