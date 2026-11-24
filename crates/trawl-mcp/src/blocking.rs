// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The DSL text trawl-mcp reads itself, read on tokio's blocking pool.
//! A parse is CPU work, so each runs through [`parse_work`] and no tool
//! handler parses on a reactor thread.

pub(crate) async fn parse_work<T, F>(work: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    #[cfg(test)]
    let record = record::current();
    let task = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        record::push(record.as_ref());
        work()
    });
    match task.await {
        Ok(value) => value,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            // The pool cancels a task only while the runtime shuts down.
            Err(error) => panic!("local DSL work did not run: {error}"),
        },
    }
}

#[cfg(test)]
pub(crate) mod record {
    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;

    type Threads = Arc<Mutex<Vec<ThreadId>>>;

    tokio::task_local! {
        static THREADS: Threads;
    }

    pub(crate) async fn threads<F: Future>(future: F) -> (F::Output, Vec<ThreadId>) {
        let threads = Threads::default();
        let output = THREADS.scope(Arc::clone(&threads), future).await;
        let threads = threads.lock().expect("the record is not poisoned").clone();
        (output, threads)
    }

    pub(super) fn current() -> Option<Threads> {
        THREADS.try_with(Arc::clone).ok()
    }

    pub(super) fn push(record: Option<&Threads>) {
        if let Some(record) = record {
            record
                .lock()
                .expect("the record is not poisoned")
                .push(std::thread::current().id());
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use trawl_client::ErrorCode;

    use super::record;
    use crate::support::{Fake, ok, refuse, schema_services};
    use crate::tools::fields;

    /// The test's current-thread runtime polls the handler on this thread.
    #[tokio::test]
    async fn a_tool_parses_off_the_reactor() {
        let backend = Fake {
            catalog_fields: refuse(403, ErrorCode::Forbidden, "insufficient permissions"),
            schema_services: ok(schema_services()),
            ..Fake::default()
        };
        let input = serde_json::from_value(json!({"source": "service", "service": "api"}))
            .expect("the input decodes");
        let (result, threads) = record::threads(fields::run(&backend, input)).await;
        let next = &result.structured_content.as_ref().expect("a mirror")["next_actions"];
        assert_eq!(next[0]["arguments"]["query"], json!("last=1h service=api"));
        let reactor = std::thread::current().id();
        assert_eq!(threads.len(), 1, "{threads:?}");
        assert!(
            threads.iter().all(|thread| *thread != reactor),
            "parsed on the reactor thread {reactor:?}: {threads:?}"
        );
    }
}
