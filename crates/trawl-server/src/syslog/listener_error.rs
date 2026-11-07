// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The error path the TCP and UDP syslog listeners share (ADR-0054,
//! 2026-11-06 amendment).
//!
//! A failed `accept()` or `recv_from` ends nothing. The listener logs it
//! under a throttle, waits out [`ACCEPT_ERROR_BACKOFF`] and asks its socket
//! again. Shutdown cuts the wait short: an already-set flag, a flag set
//! during the wait and a dropped sender all return at once, with no further
//! accept or receive.
//!
//! The throttle is per listener, over time. A failure logs a line at once
//! only when the listener has logged no line in the last 30 s; otherwise it
//! is counted as unlogged, and the next line carries that count. The first
//! success after a failure episode logs one recovery line, but only when
//! the episode had a logged line; an episode with none closes silently and
//! its count carries into the next line. A flood that alternates errors
//! with successes therefore logs about two lines per 30 s, not two per
//! backoff.
//!
//! Every line uses [`PREAUTH_TRANSPORT_TARGET`]: a sender that can reach
//! the port can provoke them, so they stay out of the corpus.

use std::io;
use std::ops::ControlFlow;
use std::time::Duration;

use tokio::time::Instant;

use crate::shutdown::{ACCEPT_ERROR_BACKOFF, ShutdownRx, shutdown_observed};
use crate::telemetry::PREAUTH_TRANSPORT_TARGET;

/// The shortest gap between two lines one listener logs about its socket
/// errors.
const LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Which listener a [`ListenerErrors`] reports for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Listener {
    Tcp,
    Udp,
}

impl Listener {
    const fn failed(self) -> &'static str {
        match self {
            Self::Tcp => "syslog_tcp_accept_failed",
            Self::Udp => "syslog_udp_recv_failed",
        }
    }

    const fn recovered(self) -> &'static str {
        match self {
            Self::Tcp => "syslog_tcp_accept_recovered",
            Self::Udp => "syslog_udp_recv_recovered",
        }
    }

    const fn operation(self) -> &'static str {
        match self {
            Self::Tcp => "syslog TCP accept",
            Self::Udp => "syslog UDP receive",
        }
    }
}

/// One listener's socket-error state: the throttle and the backoff.
#[derive(Debug)]
pub(super) struct ListenerErrors {
    listener: Listener,
    /// A clone of the listener's receiver, so the backoff can wait on
    /// shutdown from inside the outer select's handler.
    shutdown_rx: ShutdownRx,
    /// Failures since the last success.
    episode_failures: u64,
    /// Whether this episode has logged a line, which is what earns it a
    /// recovery line.
    episode_logged: bool,
    /// Failures since the previous line that got no line of their own,
    /// across episodes.
    unlogged: u64,
    /// When this listener last logged a line about its socket errors.
    last_line: Option<Instant>,
}

impl ListenerErrors {
    pub(super) fn new(listener: Listener, shutdown_rx: ShutdownRx) -> Self {
        Self {
            listener,
            shutdown_rx,
            episode_failures: 0,
            episode_logged: false,
            unlogged: 0,
            last_line: None,
        }
    }

    /// Record a failed accept or receive, then wait out the backoff.
    ///
    /// `Break` means shutdown arrived (or was already set) and the
    /// listener must return without touching its socket again.
    pub(super) async fn back_off(&mut self, error: &io::Error) -> ControlFlow<()> {
        self.record_failure(error, Instant::now());
        tokio::select! {
            biased;

            () = shutdown_observed(&mut self.shutdown_rx) => ControlFlow::Break(()),
            () = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => ControlFlow::Continue(()),
        }
    }

    /// Record a successful accept or receive: the socket works, whatever
    /// the listener then does with the connection or datagram.
    pub(super) fn succeeded(&mut self) {
        if self.episode_failures > 0 {
            self.record_success(Instant::now());
        }
    }

    fn line_due(&self, now: Instant) -> bool {
        self.last_line
            .is_none_or(|at| now.saturating_duration_since(at) >= LOG_INTERVAL)
    }

    fn record_failure(&mut self, error: &io::Error, now: Instant) {
        self.episode_failures = self.episode_failures.saturating_add(1);
        if !self.line_due(now) {
            self.unlogged = self.unlogged.saturating_add(1);
            return;
        }
        tracing::warn!(
            target: PREAUTH_TRANSPORT_TARGET,
            event_type = self.listener.failed(),
            failures = self.episode_failures,
            unlogged = self.unlogged,
            error = %error,
            "{} failed; trying again after a backoff",
            self.listener.operation(),
        );
        self.unlogged = 0;
        self.last_line = Some(now);
        self.episode_logged = true;
    }

    fn record_success(&mut self, now: Instant) {
        if self.episode_failures == 0 {
            return;
        }
        if self.episode_logged {
            tracing::info!(
                target: PREAUTH_TRANSPORT_TARGET,
                event_type = self.listener.recovered(),
                failures = self.episode_failures,
                unlogged = self.unlogged,
                "{} recovered",
                self.listener.operation(),
            );
            self.unlogged = 0;
            self.last_line = Some(now);
        }
        self.episode_failures = 0;
        self.episode_logged = false;
    }
}

/// An injected failure: run once in place of the next accept or receive.
/// It may act first (set the shutdown flag, say) and returns the error the
/// listener sees.
#[cfg(any(test, feature = "test-support"))]
pub(super) type Fault = Box<dyn FnOnce() -> io::Error + Send>;

/// The socket a listener reads, and with `test-support` the queue of
/// faults its next accept or receive calls answer before the socket is
/// asked, and a count of those calls. Both belong to this one listener.
pub(super) struct Incoming<S> {
    pub(super) socket: S,
    #[cfg(any(test, feature = "test-support"))]
    faults: std::collections::VecDeque<Fault>,
    #[cfg(any(test, feature = "test-support"))]
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl<S> Incoming<S> {
    pub(super) fn new(socket: S) -> Self {
        Self {
            socket,
            #[cfg(any(test, feature = "test-support"))]
            faults: std::collections::VecDeque::new(),
            #[cfg(any(test, feature = "test-support"))]
            attempts: std::sync::Arc::default(),
        }
    }

    /// `socket`, with `faults` answering its first calls, and the count of
    /// calls the listener makes.
    #[cfg(test)]
    pub(super) fn with_faults(
        socket: S,
        faults: Vec<Fault>,
    ) -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let mut incoming = Self::new(socket);
        incoming.faults = faults.into();
        let attempts = std::sync::Arc::clone(&incoming.attempts);
        (incoming, attempts)
    }

    /// Start one accept or receive: count it, and answer the next fault if
    /// one is queued. Called from the operation's async body, so a call is
    /// counted when the listener first polls it, not when it builds it.
    // The queue lives on `self` only under `test-support`; the signature
    // stays the same in every build so the listeners carry no cfg.
    #[cfg_attr(
        not(any(test, feature = "test-support")),
        allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)
    )]
    pub(super) fn attempt(&mut self) -> Option<io::Error> {
        #[cfg(any(test, feature = "test-support"))]
        {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(fault) = self.faults.pop_front() {
                return Some(fault());
            }
        }
        None
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::layer::{Context, Layer};

    use super::super::convert::SyslogDoor;
    use super::Fault;

    /// The one door, with the packaged derivation policy and no relays.
    pub(in crate::syslog) fn test_door() -> SyslogDoor {
        SyslogDoor {
            envs: vec!["prod".to_owned()].into(),
            default_env: "prod".into(),
            trusted_relays: Vec::new().into(),
            derivation: Arc::new(crate::ingest::producer::Derivation::defaults()),
            zones: Arc::default(),
        }
    }

    /// `n` plain faults. Nothing classifies errors, so any one will do;
    /// this is the one a process out of descriptors sees.
    pub(in crate::syslog) fn faults(n: usize) -> Vec<Fault> {
        (0..n)
            .map(|_| Box::new(|| std::io::Error::from_raw_os_error(24)) as Fault)
            .collect()
    }

    /// How shutdown reaches the listener in `listener_shutdown_during_backoff`.
    #[derive(Debug, Clone, Copy)]
    pub(in crate::syslog) enum Stop {
        /// The fault sets the flag, so it is already true when the backoff
        /// starts.
        SetAtBackoffEntry,
        /// The flag is set while the listener waits out its backoff.
        SentDuringBackoff,
        /// The last sender drops while the listener waits out its backoff.
        DroppedDuringBackoff,
        /// The last sender drops while the listener waits on its socket.
        DroppedOutsideBackoff,
    }

    pub(in crate::syslog) const STOPS: [Stop; 4] = [
        Stop::SetAtBackoffEntry,
        Stop::SentDuringBackoff,
        Stop::DroppedDuringBackoff,
        Stop::DroppedOutsideBackoff,
    ];

    /// The faults for one [`Stop`] case, given a sender clone the first
    /// case's fault sets the flag with.
    pub(in crate::syslog) fn stop_faults(
        stop: Stop,
        shutdown_tx: &crate::shutdown::ShutdownTx,
    ) -> Vec<Fault> {
        match stop {
            Stop::SetAtBackoffEntry => {
                let tx = shutdown_tx.clone();
                vec![Box::new(move || {
                    tx.send(true).unwrap();
                    std::io::Error::from_raw_os_error(24)
                })]
            }
            Stop::SentDuringBackoff | Stop::DroppedDuringBackoff => faults(1),
            Stop::DroppedOutsideBackoff => Vec::new(),
        }
    }

    /// Drive one [`Stop`] case on the test task, with the clock paused so
    /// the 100 ms backoff cannot run out. Each step is one poll: a listener
    /// that misses the shutdown stays pending and fails at once.
    pub(in crate::syslog) async fn assert_stops<F>(
        stop: Stop,
        serve: F,
        attempts: &std::sync::atomic::AtomicUsize,
        shutdown_tx: crate::shutdown::ShutdownTx,
    ) where
        F: std::future::Future<Output = std::io::Result<()>>,
    {
        let mut serve = std::pin::pin!(serve);
        let mut shutdown_tx = Some(shutdown_tx);
        if !matches!(stop, Stop::SetAtBackoffEntry) {
            assert!(
                crate::test_timing::until_polling("one poll", &mut serve, || true)
                    .await
                    .is_none(),
                "{stop:?}: the listener ended before shutdown"
            );
            assert_eq!(
                attempts.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "{stop:?}"
            );
            match stop {
                Stop::SentDuringBackoff => shutdown_tx.as_ref().unwrap().send(true).unwrap(),
                _ => drop(shutdown_tx.take()),
            }
        }
        let ended = crate::test_timing::until_polling("one poll", &mut serve, || true).await;
        assert!(
            matches!(ended, Some(Ok(()))),
            "{stop:?}: the listener did not return at once: {ended:?}"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "{stop:?}: the listener asked its socket again after shutdown"
        );
    }

    /// Check one burst of `n` faults that the clock drives at one backoff
    /// apiece: the first failure at once, a summary every 30 s (failures
    /// 301 and 601) carrying the 299 failures between, then one recovery
    /// line with the total and the rest.
    pub(in crate::syslog) fn assert_burst_lines(
        lines: &[Line],
        failed: &str,
        recovered: &str,
        n: u64,
    ) {
        let warn = tracing::Level::WARN;
        let line = |level, event_type: &str, failures, unlogged| Line {
            target: crate::telemetry::PREAUTH_TRANSPORT_TARGET.to_owned(),
            level,
            event_type: event_type.to_owned(),
            failures,
            unlogged,
        };
        let mut expected = vec![line(warn, failed, 1, 0)];
        let mut last = 1;
        for at in [301, 601] {
            if at <= n {
                expected.push(line(warn, failed, at, at - last - 1));
                last = at;
            }
        }
        expected.push(line(tracing::Level::INFO, recovered, n, n - last));
        assert_eq!(lines, expected);
    }

    /// One captured socket-error line.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(in crate::syslog) struct Line {
        pub target: String,
        pub level: tracing::Level,
        pub event_type: String,
        pub failures: u64,
        pub unlogged: u64,
    }

    #[derive(Default)]
    struct Fields {
        event_type: String,
        failures: u64,
        unlogged: u64,
    }

    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "event_type" {
                value.clone_into(&mut self.event_type);
            }
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            match field.name() {
                "failures" => self.failures = value,
                "unlogged" => self.unlogged = value,
                _ => {}
            }
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    /// A layer that records the listeners' socket-error lines, with the
    /// target and level from each event's metadata.
    #[derive(Clone, Default)]
    pub(in crate::syslog) struct Capture {
        lines: Arc<Mutex<Vec<Line>>>,
    }

    impl Capture {
        /// Install this layer as the thread's default subscriber until the
        /// guard drops. A current-thread runtime polls every task on this
        /// thread, so spawned tasks report here too.
        pub(in crate::syslog) fn install(&self) -> tracing::subscriber::DefaultGuard {
            use tracing_subscriber::prelude::*;
            tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
        }

        pub(in crate::syslog) fn lines(&self) -> Vec<Line> {
            self.lines.lock().clone()
        }
    }

    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            if !(fields.event_type.ends_with("_failed")
                || fields.event_type.ends_with("_recovered"))
            {
                return;
            }
            self.lines.lock().push(Line {
                target: event.metadata().target().to_owned(),
                level: *event.metadata().level(),
                event_type: fields.event_type,
                failures: fields.failures,
                unlogged: fields.unlogged,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Capture, Line};
    use super::*;
    use crate::test_timing::until_polling;

    fn error() -> io::Error {
        io::Error::from_raw_os_error(24)
    }

    fn line(level: tracing::Level, event_type: &str, failures: u64, unlogged: u64) -> Line {
        Line {
            target: PREAUTH_TRANSPORT_TARGET.to_owned(),
            level,
            event_type: event_type.to_owned(),
            failures,
            unlogged,
        }
    }

    fn errors() -> ListenerErrors {
        let (_tx, rx) = crate::shutdown::shutdown_channel();
        ListenerErrors::new(Listener::Tcp, rx)
    }

    const WARN: tracing::Level = tracing::Level::WARN;
    const INFO: tracing::Level = tracing::Level::INFO;
    const FAILED: &str = "syslog_tcp_accept_failed";
    const RECOVERED: &str = "syslog_tcp_accept_recovered";

    /// One burst: the first failure at once, then a line per 30 s that
    /// counts the failures in between, then one recovery line. A second
    /// success logs nothing.
    #[test]
    fn a_burst_logs_first_summaries_and_one_recovery() {
        let capture = Capture::default();
        let _guard = capture.install();
        let mut errors = errors();
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);

        for ms in [0, 1_000, 29_999, 30_000, 59_999, 60_000] {
            errors.record_failure(&error(), at(ms));
        }
        errors.record_success(at(60_500));
        errors.record_success(at(60_600));

        assert_eq!(
            capture.lines(),
            vec![
                line(WARN, FAILED, 1, 0),
                line(WARN, FAILED, 4, 2),
                line(WARN, FAILED, 6, 1),
                line(INFO, RECOVERED, 6, 0),
            ]
        );
    }

    /// The window is the listener's, not the episode's: a new episode
    /// inside 30 s of the last line logs nothing, closes silently, and its
    /// count rides on the next line.
    #[test]
    fn an_episode_inside_the_window_closes_silently_and_carries_its_count() {
        let capture = Capture::default();
        let _guard = capture.install();
        let mut errors = errors();
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);

        errors.record_failure(&error(), at(0));
        errors.record_success(at(100));
        // Inside the window of the recovery line at 100 ms.
        errors.record_failure(&error(), at(200));
        errors.record_failure(&error(), at(300));
        errors.record_success(at(400));
        // A success with no failure behind it logs nothing either.
        errors.record_success(at(500));
        // The window has passed: this failure logs, carrying the two.
        errors.record_failure(&error(), at(30_100));
        errors.record_success(at(30_200));

        assert_eq!(
            capture.lines(),
            vec![
                line(WARN, FAILED, 1, 0),
                line(INFO, RECOVERED, 1, 0),
                line(WARN, FAILED, 1, 2),
                line(INFO, RECOVERED, 1, 0),
            ]
        );
    }

    /// The flood the throttle exists for: every 100 ms backoff ends in a
    /// success and the next one starts with a failure. Over 100 s of that
    /// one listener logs 8 lines, a failure and its recovery about every
    /// 30 s, where a per-episode reset would log 2 000.
    #[test]
    fn an_alternating_flood_logs_a_bounded_number_of_lines() {
        let capture = Capture::default();
        let _guard = capture.install();
        let mut errors = errors();
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);

        for i in 0..1_000 {
            errors.record_failure(&error(), at(i * 100));
            errors.record_success(at(i * 100 + 50));
        }

        // Lines at 0 / 0.05 s, then the first failure 30 s or more after
        // each recovery line: 30.1 s, 60.2 s and 90.3 s, each carrying the
        // failures of the silent episodes before it.
        assert_eq!(
            capture.lines(),
            vec![
                line(WARN, FAILED, 1, 0),
                line(INFO, RECOVERED, 1, 0),
                line(WARN, FAILED, 1, 300),
                line(INFO, RECOVERED, 1, 0),
                line(WARN, FAILED, 1, 300),
                line(INFO, RECOVERED, 1, 0),
                line(WARN, FAILED, 1, 300),
                line(INFO, RECOVERED, 1, 0),
            ]
        );
    }

    /// The backoff is 100 ms on the clock: still waiting just before, done
    /// at it.
    #[tokio::test(start_paused = true)]
    async fn the_backoff_waits_out_the_shared_constant() {
        let (_tx, rx) = crate::shutdown::shutdown_channel();
        let mut errors = ListenerErrors::new(Listener::Udp, rx);
        let error = error();
        let mut wait = std::pin::pin!(errors.back_off(&error));

        assert!(
            until_polling("one poll", &mut wait, || true)
                .await
                .is_none()
        );
        tokio::time::advance(
            ACCEPT_ERROR_BACKOFF
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        )
        .await;
        assert!(
            until_polling("one poll", &mut wait, || true)
                .await
                .is_none(),
            "done before the backoff"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(
            until_polling("the backoff ends", &mut wait, || false).await,
            Some(ControlFlow::Continue(()))
        );
    }
}
