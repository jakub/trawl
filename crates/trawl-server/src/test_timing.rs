// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared timing helpers for tests that wait on real worker threads or
//! sockets (#316).
//!
//! Order between two steps comes from something the code publishes: a seam
//! arrival, a join, a channel, a committed row. These helpers only wait for
//! that to happen. They never decide correctness by elapsed time.
//!
//! trawl-web includes this file by `#[path]` for its own unit tests, so it
//! depends on nothing but `std` and `tokio`.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// The one real-clock bound the tests of #316 use.
///
/// It only names a hang: a wait that exceeds it fails instead of blocking
/// the run forever. It never decides correctness, so no assertion may be
/// built on a condition a buggy build could also meet before it expires.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

/// Yield until `cond` holds, panicking with `what` if [`HANG_GUARD`] runs
/// out first.
///
/// A yield loop keeps a paused tokio clock fixed. Tokio auto-advances a
/// paused clock only when the runtime has nothing runnable and parks with
/// no timeout, and a task that always re-wakes itself leaves the runtime
/// with a runnable task, so it parks with a zero limit and the clock never
/// moves while a blocking thread makes real progress. A tokio sleep or
/// timeout in its place would be idle and let the clock jump. The bound is
/// measured on the real clock (`std::time::Instant`) for the same reason.
/// It also works on `multi_thread` runtimes, where the yield just hands
/// the worker back to the scheduler.
pub async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < HANG_GUARD,
            "timed out after {HANG_GUARD:?} waiting for: {what}"
        );
        tokio::task::yield_now().await;
    }
}

/// [`until`] for a test that owns a future (a connection driver, an HTTP
/// request) and has to keep polling it while it waits.
///
/// Each iteration polls `fut` once and then checks `cond`. Returns
/// `Some(output)` if the future completes first and `None` once `cond`
/// holds while it is still pending; the future is left unfinished for the
/// caller to keep polling or drop. Same hang guard and yield as [`until`].
pub async fn until_polling<F: Future + Unpin>(
    what: &str,
    fut: &mut F,
    mut cond: impl FnMut() -> bool,
) -> Option<F::Output> {
    let start = Instant::now();
    loop {
        if let std::task::Poll::Ready(out) =
            std::future::poll_fn(|cx| std::task::Poll::Ready(Pin::new(&mut *fut).poll(cx))).await
        {
            return Some(out);
        }
        if cond() {
            return None;
        }
        assert!(
            start.elapsed() < HANG_GUARD,
            "timed out after {HANG_GUARD:?} waiting for: {what}"
        );
        tokio::task::yield_now().await;
    }
}

/// Drive `op` to completion without letting a paused tokio clock move on
/// its own, panicking with `what` if [`HANG_GUARD`] runs out first.
///
/// Use it around any await on real I/O in a paused test: a TLS request, a
/// Postgres round trip, a `spawn_blocking` result. Awaited bare, such an
/// await leaves the runtime idle, and tokio jumps the clock to the next
/// timer, which fires a deadline the test never advanced to. Here `op` is
/// polled by [`until_polling`] with a condition that never holds, so the
/// runtime always has a runnable task. A `tokio::time::advance` inside
/// `op` still moves the clock by exactly what it asks for. The guard
/// panics in the test's own task, so a hang fails the test at once.
pub async fn frozen<F: Future>(what: &str, op: F) -> F::Output {
    until_polling(what, &mut std::pin::pin!(op), || false)
        .await
        .expect("a condition that never holds cannot end the wait")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn until_leaves_the_paused_clock_alone() {
        let before = tokio::time::Instant::now();
        let worker = tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(50)));
        until("the blocking sleep finished", || worker.is_finished()).await;
        worker.await.unwrap();
        assert_eq!(tokio::time::Instant::now(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn until_polling_reports_which_side_came_first() {
        let mut pending = std::pin::pin!(std::future::pending::<u8>());
        let out = until_polling("the condition", &mut pending, || true).await;
        assert!(out.is_none());

        let mut ready = std::pin::pin!(std::future::ready(7u8));
        let out = until_polling("the future", &mut ready, || false).await;
        assert_eq!(out, Some(7));
    }

    // The peers below run on plain threads. A `spawn_blocking` peer would
    // prove nothing: tokio holds a paused clock itself while a blocking task
    // runs, with or without `frozen`.

    #[tokio::test(start_paused = true)]
    async fn frozen_keeps_the_paused_clock_across_a_loopback_round_trip() {
        use std::io::{Read as _, Write as _};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4];
            sock.read_exact(&mut buf).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            sock.write_all(&buf).unwrap();
        });
        // A pending timer gives an idle runtime somewhere to jump the clock.
        let _timer = tokio::spawn(tokio::time::sleep(Duration::from_secs(3600)));
        tokio::task::yield_now().await;
        let before = tokio::time::Instant::now();
        let echoed = frozen("the loopback round trip", async {
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            client.write_all(b"ping").await.unwrap();
            let mut buf = [0u8; 4];
            client.read_exact(&mut buf).await.unwrap();
            buf
        })
        .await;
        assert_eq!(&echoed, b"ping");
        assert_eq!(tokio::time::Instant::now(), before);
        peer.join().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn frozen_keeps_the_paused_clock_across_a_blocking_sleep() {
        let (done, finished) = tokio::sync::oneshot::channel();
        let sleeper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            done.send(()).unwrap();
        });
        // A pending timer gives an idle runtime somewhere to jump the clock.
        let _timer = tokio::spawn(tokio::time::sleep(Duration::from_secs(3600)));
        tokio::task::yield_now().await;
        let before = tokio::time::Instant::now();
        frozen("the blocking sleep", finished).await.unwrap();
        assert_eq!(tokio::time::Instant::now(), before);
        sleeper.join().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn frozen_moves_the_clock_only_by_an_explicit_advance() {
        let before = tokio::time::Instant::now();
        frozen("the advance", async {
            tokio::time::advance(Duration::from_millis(750)).await;
        })
        .await;
        assert_eq!(
            tokio::time::Instant::now() - before,
            Duration::from_millis(750)
        );
    }
}
