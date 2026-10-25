// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Capture admission and quotas (ADR-0057).
//!
//! [`CaptureGate::admit`] decides whether one capture may run, before the
//! capture takes a query id or asks for an executor permit. It runs four
//! steps, in this order:
//!
//! 1. **Quota pre-check.** The key's and the installation's committed
//!    usage, plus the reservations of captures still running, plus this
//!    capture's charge (one result and `max_file_bytes`), against the
//!    `[capture]` quotas. A key that is already full is refused here, so it
//!    never waits in a queue only to be refused at the end. The check is
//!    not binding: step 4 is.
//! 2. **The key's lane.** `per_key_running` captures run per key. When none
//!    is free, up to `queue_depth` callers wait inside their own deadline;
//!    the next caller is refused at once and never queues.
//! 3. **The installation.** `installation_running` captures run at once.
//!    A capture that finds none free is refused, not queued: its lane did
//!    the queueing.
//! 4. **Binding reservation.** Under the pass lock: expire every due
//!    temporary capture and unlink its file, so an expired capture frees
//!    its disk at the next reservation instead of at the next sweep;
//!    then read committed usage again, add every reservation in the book,
//!    and record this capture's charge or refuse.
//!
//! Every refusal says when to try again, in whole seconds and never less
//! than one. A full lane or installation names the time until the first
//! running capture's deadline. A full quota names the time until the first
//! temporary result in the full scope expires, capped at `ttl_secs`, or
//! `ttl_secs` when nothing there will expire. A deadline that expires
//! during admission names one second: the capture never started.
//!
//! An [`Admission`] holds the lane slot, the installation slot and the
//! [`Reservation`]. It rides the executor slot through
//! [`crate::pool::WorkContext::holding`] ([`Admission::into_holds`]), so all
//! three stay charged until the work physically stops, however the request
//! ends.
//!
//! # The book
//!
//! The book holds the charges of admitted captures whose rows the table
//! does not count yet. A charge is *done* when it settles, because its row
//! is in the table, or when its last handle drops, because the capture
//! ended without a row. A done charge stays in the book until the next
//! binding pass removes it, under the pass lock and before that pass reads
//! usage. So every pass reads the table after the rows of the charges it
//! removed were inserted, and counts a capture that commits during its
//! reads either in the table or in the book, never in neither. Removing a
//! settled charge at once would let a pass that read usage just before the
//! row was inserted miss that capture in both places, and admit past the
//! quota. The cost is a capture counted twice for the length of one pass.
//!
//! The pre-check counts only charges that are not done: it is not binding,
//! and a capture it lets through still meets step 4.
//!
//! # Evidence
//!
//! An attached capture (ADR-0059) is kept from birth, so it never charges
//! the temporary quota: [`CaptureGate::admit_attached`] runs the lane and
//! the installation as for any capture, but its pre-check and its binding
//! reservation charge `max_file_bytes` against the evidence budgets
//! instead, the investigation's and the installation's, read from the
//! kept rows ([`crate::store::InvestigationStore::evidence_usage`]) plus the
//! book's evidence charges. A promotion of an existing result
//! ([`CaptureGate::reserve_evidence`]) takes no slot and charges the
//! result's own file bytes by the same binding pass. A charge's scope,
//! temporary or evidence, decides which sums count it; the purge and the
//! settle are the same for both.
//!
//! A pass awaits Postgres, so the lock that serializes passes is an async
//! one; the book itself is a short synchronous lock, so a settle or a drop
//! never waits for a pass.
//!
//! A lane is made the first time one of a key's captures passes the
//! pre-check, and is kept for the life of the process: a few dozen bytes
//! per key, and keys are made by an administrator.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use trawl_api::investigations::InvestigationId;

use super::ResultsClock;
use super::root::ResultsRoot;
use crate::config::{CaptureConfig, InvestigationConfig};
use crate::deadline::{Deadline, Expired};
use crate::store::{InvestigationStore, ResultsStore, StoreError, Usage};

/// Why a capture was not admitted.
#[derive(Debug)]
pub enum Refusal {
    /// 429 `capture_busy`: the key's lane and its queue are full, the
    /// installation is at `installation_running`, or the deadline expired
    /// while the capture waited.
    Busy {
        /// Whole seconds until a slot should free, at least 1.
        retry_after_secs: u64,
    },
    /// 429 `capture_quota_exceeded`: the key or the installation is at its
    /// result or byte quota.
    QuotaExceeded {
        /// Whole seconds until the first temporary result in the full scope
        /// expires, at least 1 and at most `ttl_secs`.
        retry_after_secs: u64,
    },
    /// 409 `evidence_budget_exceeded`: the investigation's or the
    /// installation's evidence budget cannot hold the charge.
    EvidenceBudgetExceeded {
        /// Evidence bytes already kept or reserved in the full scope.
        used: u64,
        /// That scope's budget, in bytes.
        limit: u64,
    },
    /// The metadata store failed while admission read usage. Nothing was
    /// admitted; a server fault, like any other [`StoreError`].
    Store(StoreError),
}

/// The evidence budgets a kept result is charged against (ADR-0059):
/// `[investigation] evidence_per_investigation_bytes` and
/// `evidence_installation_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceBudget {
    /// The most evidence bytes one investigation keeps.
    pub per_investigation: u64,
    /// The most evidence bytes every investigation together keeps.
    pub installation: u64,
}

impl EvidenceBudget {
    /// The budgets `config` sets.
    #[must_use]
    pub fn of(config: &InvestigationConfig) -> Self {
        Self {
            per_investigation: config.evidence_per_investigation_bytes,
            installation: config.evidence_installation_bytes,
        }
    }
}

/// What a capture holds from admission until its work stops: its lane
/// slot, its installation slot and its quota reservation.
#[derive(Debug)]
pub struct Admission {
    // Fields drop in this order. The reservation goes first. The
    // installation slot frees before the lane slot, so the key's next
    // waiter, which the lane slot wakes, finds this capture's installation
    // slot already free.
    reservation: Reservation,
    _installation: Slot,
    _lane: Slot,
}

impl Admission {
    /// The capture's quota reservation. A clone keeps the charge past this
    /// admission, so the caller that inserts the row can settle it.
    #[must_use]
    pub fn reservation(&self) -> &Reservation {
        &self.reservation
    }

    /// The admission as the executor slot's holds
    /// ([`crate::pool::WorkContext::holding`]): everything it holds is
    /// released when the slot drops, after the executor permit.
    #[must_use]
    pub fn into_holds(self) -> Arc<dyn Any + Send + Sync> {
        Arc::new(self)
    }
}

/// One admitted capture's binding charge against its key's and the
/// installation's quotas: one result and `max_file_bytes`.
///
/// A handle: clones share one charge. The charge is released by
/// [`Self::settle`] once the capture's row is in the table, or when the
/// last handle drops, whichever comes first.
#[derive(Debug, Clone)]
pub struct Reservation(Arc<Charge>);

impl Reservation {
    /// The capture's row is inserted, so the table counts it from now on:
    /// stop counting the reservation, without waiting for the executor slot
    /// to drop. Call it once the row is inserted, by the insert or its
    /// fence, or read after the grace. A capture whose commit is in doubt
    /// keeps the reservation through its grace instead
    /// ([`super::service::COMMIT_GRACE`]). A second call does nothing.
    pub fn settle(&self) {
        self.0.book.lock().done(self.0.ticket);
    }
}

/// The shared part of a [`Reservation`]: its entry in the book.
#[derive(Debug)]
struct Charge {
    ticket: u64,
    book: Arc<Mutex<Book>>,
}

impl Drop for Charge {
    /// The capture ended, with its row settled or without one.
    fn drop(&mut self) {
        self.book.lock().done(self.ticket);
    }
}

/// Admission for captures: the per-key lanes, the installation's slots and
/// the book of quota reservations. One per process, in
/// [`super::ResultsState`].
#[derive(Debug)]
pub struct CaptureGate {
    capture: CaptureConfig,
    store: ResultsStore,
    /// The kept rows' evidence usage, on the same pool.
    investigations: InvestigationStore,
    root: Arc<ResultsRoot>,
    clock: ResultsClock,
    lanes: DashMap<i64, Arc<Lane>>,
    installation: Arc<Slots>,
    /// Serializes binding passes; held across their Postgres reads.
    pass: tokio::sync::Mutex<()>,
    book: Arc<Mutex<Book>>,
    /// One ticket per admission attempt, naming its slots and its charge.
    tickets: AtomicU64,
}

impl CaptureGate {
    /// A gate enforcing `capture`'s running limits and quotas, reading
    /// usage from `store` at `clock`'s time and unlinking expired files
    /// from `root`.
    #[must_use]
    pub fn new(
        capture: &CaptureConfig,
        store: ResultsStore,
        root: Arc<ResultsRoot>,
        clock: ResultsClock,
    ) -> Self {
        Self {
            installation: Slots::new(capture.installation_running),
            capture: capture.clone(),
            investigations: store.investigations(),
            store,
            root,
            clock,
            lanes: DashMap::new(),
            pass: tokio::sync::Mutex::new(()),
            book: Arc::default(),
            tickets: AtomicU64::new(0),
        }
    }

    /// Admit one capture by `key_id`, waiting no later than `deadline`: the
    /// quota pre-check, the key's lane, the installation, then the binding
    /// reservation (module docs).
    pub async fn admit(&self, key_id: i64, deadline: Deadline) -> Result<Admission, Refusal> {
        self.pre_check(key_id, deadline).await?;
        let ticket = self.tickets.fetch_add(1, Ordering::Relaxed);
        let lane = self.lane(key_id);
        let lane = lane
            .enter(ticket, deadline, self.capture.queue_depth)
            .await?;
        let installation = self
            .installation
            .try_hold(ticket, deadline)
            .ok_or_else(|| Refusal::Busy {
                retry_after_secs: self.installation.retry_after_secs(),
            })?;
        let reservation = self.reserve(key_id, ticket, deadline).await?;
        Ok(Admission {
            reservation,
            _installation: installation,
            _lane: lane,
        })
    }

    /// Step 1: refuse a key or an installation that is already full,
    /// counting the charges not yet done.
    async fn pre_check(&self, key_id: i64, deadline: Deadline) -> Result<(), Refusal> {
        let now = self.clock.now();
        let key = within(deadline, self.store.usage(key_id, now)).await?;
        let installation = within(deadline, self.store.installation_usage(now)).await?;
        let full = {
            let book = self.book.lock();
            self.full(
                plus(key, book.pending(Some(key_id))),
                plus(installation, book.pending(None)),
            )
        };
        match full {
            None => Ok(()),
            Some(scope) => Err(self.quota_exceeded(scope, key_id, now, deadline).await),
        }
    }

    /// Step 4: record this capture's charge, or refuse it, against usage
    /// read under the pass lock (module docs, "The book").
    async fn reserve(
        &self,
        key_id: i64,
        ticket: u64,
        deadline: Deadline,
    ) -> Result<Reservation, Refusal> {
        let pass = deadline.run(self.pass.lock()).await.map_err(|_| busy())?;
        self.book.lock().purge();
        let now = self.clock.now();
        self.expire_due(now, deadline).await?;
        let key = within(deadline, self.store.usage(key_id, now)).await?;
        let installation = within(deadline, self.store.installation_usage(now)).await?;

        let full = {
            let mut book = self.book.lock();
            let full = self.full(
                plus(key, book.counted(Some(key_id))),
                plus(installation, book.counted(None)),
            );
            if full.is_none() {
                book.entries.insert(
                    ticket,
                    Entry {
                        scope: Charged::Temporary { key_id },
                        bytes: self.capture.max_file_bytes,
                        done: false,
                    },
                );
            }
            full
        };
        drop(pass);
        match full {
            None => Ok(Reservation(Arc::new(Charge {
                ticket,
                book: Arc::clone(&self.book),
            }))),
            Some(scope) => Err(self.quota_exceeded(scope, key_id, now, deadline).await),
        }
    }

    /// Admit one capture by `key_id` attached to `investigation`, waiting
    /// no later than `deadline`: the evidence pre-check, the key's lane,
    /// the installation, then the binding evidence reservation of
    /// `max_file_bytes` (module docs, "Evidence"). The temporary quota is
    /// neither checked nor charged.
    pub async fn admit_attached(
        &self,
        key_id: i64,
        investigation: &InvestigationId,
        budget: EvidenceBudget,
        deadline: Deadline,
    ) -> Result<Admission, Refusal> {
        let bytes = self.capture.max_file_bytes;
        self.evidence_pre_check(investigation, bytes, budget, deadline)
            .await?;
        let ticket = self.tickets.fetch_add(1, Ordering::Relaxed);
        let lane = self.lane(key_id);
        let lane = lane
            .enter(ticket, deadline, self.capture.queue_depth)
            .await?;
        let installation = self
            .installation
            .try_hold(ticket, deadline)
            .ok_or_else(|| Refusal::Busy {
                retry_after_secs: self.installation.retry_after_secs(),
            })?;
        let reservation = self
            .reserve_evidence_ticket(investigation, ticket, bytes, budget, deadline)
            .await?;
        Ok(Admission {
            reservation,
            _installation: installation,
            _lane: lane,
        })
    }

    /// Charge `bytes` of an existing result promoted into `investigation`
    /// against the evidence budgets, by a binding pass that waits no later
    /// than `deadline`. Takes no slot: a promotion runs no query.
    pub async fn reserve_evidence(
        &self,
        investigation: &InvestigationId,
        bytes: u64,
        budget: EvidenceBudget,
        deadline: Deadline,
    ) -> Result<Reservation, Refusal> {
        let ticket = self.tickets.fetch_add(1, Ordering::Relaxed);
        self.reserve_evidence_ticket(investigation, ticket, bytes, budget, deadline)
            .await
    }

    /// The evidence step 1: refuse a charge of `bytes` that an evidence
    /// budget already cannot hold, counting the charges not yet done.
    async fn evidence_pre_check(
        &self,
        investigation: &InvestigationId,
        bytes: u64,
        budget: EvidenceBudget,
        deadline: Deadline,
    ) -> Result<(), Refusal> {
        let (used, installation) = self.evidence_usage(investigation, deadline).await?;
        let book = self.book.lock();
        evidence_full(
            used.saturating_add(book.evidence_pending(Some(investigation))),
            installation.saturating_add(book.evidence_pending(None)),
            bytes,
            budget,
        )
        .map_or(Ok(()), Err)
    }

    /// The evidence step 4: record a charge of `bytes` against
    /// `investigation`'s and the installation's evidence, or refuse it,
    /// against usage read under the pass lock (module docs, "The book").
    async fn reserve_evidence_ticket(
        &self,
        investigation: &InvestigationId,
        ticket: u64,
        bytes: u64,
        budget: EvidenceBudget,
        deadline: Deadline,
    ) -> Result<Reservation, Refusal> {
        let pass = deadline.run(self.pass.lock()).await.map_err(|_| busy())?;
        self.book.lock().purge();
        let (used, installation) = self.evidence_usage(investigation, deadline).await?;
        let full = {
            let mut book = self.book.lock();
            let full = evidence_full(
                used.saturating_add(book.evidence_counted(Some(investigation))),
                installation.saturating_add(book.evidence_counted(None)),
                bytes,
                budget,
            );
            if full.is_none() {
                book.entries.insert(
                    ticket,
                    Entry {
                        scope: Charged::Evidence {
                            investigation: investigation.clone(),
                        },
                        bytes,
                        done: false,
                    },
                );
            }
            full
        };
        drop(pass);
        match full {
            None => Ok(Reservation(Arc::new(Charge {
                ticket,
                book: Arc::clone(&self.book),
            }))),
            Some(refusal) => Err(refusal),
        }
    }

    /// The committed evidence bytes of `investigation` and of the
    /// installation.
    async fn evidence_usage(
        &self,
        investigation: &InvestigationId,
        deadline: Deadline,
    ) -> Result<(u64, u64), Refusal> {
        let used = within(deadline, self.investigations.evidence_usage(investigation)).await?;
        let installation =
            within(deadline, self.investigations.installation_evidence_usage()).await?;
        Ok((used, installation))
    }

    /// Expire every temporary capture due at `now` and unlink its file.
    /// Usage never counts a due row, so this frees disk, not quota: a
    /// failure is logged and left to the next sweep, which retries both.
    async fn expire_due(&self, now: DateTime<Utc>, deadline: Deadline) -> Result<(), Refusal> {
        let expired = match deadline.run(self.store.expire_due(now)).await {
            Ok(Ok(expired)) => expired,
            Ok(Err(error)) => {
                warn_expiry("expiring due captures", &error);
                return Ok(());
            }
            Err(Expired) => return Err(busy()),
        };
        if expired.is_empty() {
            return Ok(());
        }
        // A zero-column capture has no file.
        let files: Vec<String> = expired
            .iter()
            .filter(|file| file.file_bytes > 0)
            .map(|file| file.id.clone())
            .collect();
        let root = Arc::clone(&self.root);
        let unlinked = tokio::task::spawn_blocking(move || {
            let mut outcome = (0_usize, 0_usize);
            for id in &files {
                match root.unlink(id) {
                    Ok(true) => outcome.0 += 1,
                    Ok(false) => {}
                    Err(error) => {
                        outcome.1 += 1;
                        warn_expiry("unlinking an expired capture's file", &error);
                    }
                }
            }
            outcome
        })
        .await;
        match unlinked {
            Ok((unlinked, errors)) => tracing::debug!(
                event_type = "results_expire_at_reservation",
                expired = expired.len(),
                unlinked,
                errors,
                "due captures expired at a capture reservation"
            ),
            Err(error) => warn_expiry("unlinking expired captures' files", &error),
        }
        Ok(())
    }

    /// Which scope this capture's charge would overfill: the key's, checked
    /// first, or the installation's.
    fn full(&self, key: Usage, installation: Usage) -> Option<Scope> {
        let charge = self.capture.max_file_bytes;
        let over = |used: Usage, results: u64, bytes: u64| {
            used.results.saturating_add(1) > results || used.bytes.saturating_add(charge) > bytes
        };
        if over(
            key,
            self.capture.per_key_results,
            self.capture.per_key_bytes,
        ) {
            Some(Scope::Key)
        } else if over(
            installation,
            self.capture.installation_results,
            self.capture.installation_bytes,
        ) {
            Some(Scope::Installation)
        } else {
            None
        }
    }

    /// The quota refusal for `scope`, naming when its first temporary
    /// result expires. A full key frees no earlier than its own first
    /// expiry, and that expiry frees the installation by one too, so a key
    /// and an installation both full name the key's.
    async fn quota_exceeded(
        &self,
        scope: Scope,
        key_id: i64,
        now: DateTime<Utc>,
        deadline: Deadline,
    ) -> Refusal {
        let earliest = match scope {
            Scope::Key => within(deadline, self.store.earliest_expiry(key_id, now)).await,
            Scope::Installation => {
                within(deadline, self.store.installation_earliest_expiry(now)).await
            }
        };
        match earliest {
            Ok(at) => Refusal::QuotaExceeded {
                retry_after_secs: until_expiry(at, now, self.capture.ttl_secs),
            },
            Err(refusal) => refusal,
        }
    }

    /// The key's lane, made on its first capture.
    fn lane(&self, key_id: i64) -> Arc<Lane> {
        Arc::clone(
            self.lanes
                .entry(key_id)
                .or_insert_with(|| Arc::new(Lane::new(self.capture.per_key_running)))
                .value(),
        )
    }
}

#[cfg(any(test, feature = "test-support"))]
impl CaptureGate {
    /// Callers of `key_id` waiting in its lane for a running slot, so a
    /// test can fill the queue before it sends the caller the queue
    /// refuses.
    #[must_use]
    pub fn queued(&self, key_id: i64) -> u64 {
        self.lanes
            .get(&key_id)
            .map_or(0, |lane| lane.waiting.load(Ordering::Acquire))
    }

    /// The evidence bytes of every charge not yet done, so a test can see a
    /// charge held and then settled.
    #[must_use]
    pub fn evidence_reserved(&self) -> u64 {
        self.book.lock().evidence_pending(None)
    }
}

/// The quota a refusal names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Key,
    Installation,
}

/// One key's running slots and its queue.
#[derive(Debug)]
struct Lane {
    slots: Arc<Slots>,
    /// Callers waiting for a slot.
    waiting: AtomicU64,
}

impl Lane {
    fn new(running: u64) -> Self {
        Self {
            slots: Slots::new(running),
            waiting: AtomicU64::new(0),
        }
    }

    /// Take a slot: at once when one is free, else by waiting until
    /// `deadline` as one of at most `depth` waiters. A caller that finds
    /// `depth` already waiting is refused at once.
    async fn enter(&self, ticket: u64, deadline: Deadline, depth: u64) -> Result<Slot, Refusal> {
        if let Some(slot) = self.slots.try_hold(ticket, deadline) {
            return Ok(slot);
        }
        if self
            .waiting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |waiting| {
                (waiting < depth).then_some(waiting + 1)
            })
            .is_err()
        {
            return Err(Refusal::Busy {
                retry_after_secs: self.slots.retry_after_secs(),
            });
        }
        let _queued = Queued(&self.waiting);
        match deadline
            .run(Arc::clone(&self.slots.semaphore).acquire_owned())
            .await
        {
            Ok(Ok(permit)) => Ok(self.slots.hold(permit, ticket, deadline)),
            // The gate never closes a semaphore, so only the deadline ends
            // this wait without a permit.
            Ok(Err(_)) | Err(Expired) => Err(busy()),
        }
    }
}

/// A place in a lane's queue, given up however the wait ends, a dropped
/// caller included.
struct Queued<'a>(&'a AtomicU64);

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A semaphore's slots, with the deadline each holder runs under, so a
/// refusal can name when the first should free.
#[derive(Debug)]
struct Slots {
    semaphore: Arc<Semaphore>,
    deadlines: Mutex<HashMap<u64, Instant>>,
}

impl Slots {
    fn new(permits: u64) -> Arc<Self> {
        let permits = usize::try_from(permits)
            .unwrap_or(usize::MAX)
            .min(Semaphore::MAX_PERMITS);
        Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(permits)),
            deadlines: Mutex::default(),
        })
    }

    /// A free slot for `ticket`, running until `deadline`, or `None`.
    fn try_hold(self: &Arc<Self>, ticket: u64, deadline: Deadline) -> Option<Slot> {
        let permit = Arc::clone(&self.semaphore).try_acquire_owned().ok()?;
        Some(self.hold(permit, ticket, deadline))
    }

    fn hold(
        self: &Arc<Self>,
        permit: OwnedSemaphorePermit,
        ticket: u64,
        deadline: Deadline,
    ) -> Slot {
        self.deadlines.lock().insert(ticket, deadline.instant());
        Slot {
            slots: Arc::clone(self),
            ticket,
            _permit: permit,
        }
    }

    /// Whole seconds until the first holder's deadline, at least 1; 1 when
    /// no holder is recorded.
    fn retry_after_secs(&self) -> u64 {
        self.deadlines.lock().values().min().map_or(1, |first| {
            ceil_secs(first.saturating_duration_since(Instant::now()))
        })
    }
}

/// One held slot. Dropping it forgets its deadline, then frees the permit.
#[derive(Debug)]
struct Slot {
    slots: Arc<Slots>,
    ticket: u64,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.slots.deadlines.lock().remove(&self.ticket);
    }
}

/// Reservations the table does not count yet, by ticket (module docs).
#[derive(Debug, Default)]
struct Book {
    entries: HashMap<u64, Entry>,
}

#[derive(Debug)]
struct Entry {
    scope: Charged,
    bytes: u64,
    /// Settled, or every handle dropped.
    done: bool,
}

/// What a charge counts against.
#[derive(Debug)]
enum Charged {
    /// The key's and the installation's temporary quotas: one result and
    /// its bytes.
    Temporary { key_id: i64 },
    /// The investigation's and the installation's evidence budgets: bytes.
    Evidence { investigation: InvestigationId },
}

impl Book {
    fn done(&mut self, ticket: u64) {
        if let Some(entry) = self.entries.get_mut(&ticket) {
            entry.done = true;
        }
    }

    /// Forget every done charge. Only a binding pass calls this, under the
    /// pass lock and before it reads usage.
    fn purge(&mut self) {
        self.entries.retain(|_, entry| !entry.done);
    }

    /// The charges not done, of `key_id` or of every key: the pre-check's
    /// count.
    fn pending(&self, key_id: Option<i64>) -> Usage {
        self.sum(key_id, |entry| !entry.done)
    }

    /// Every charge in the book, of `key_id` or of every key: a binding
    /// pass's count, after its purge.
    fn counted(&self, key_id: Option<i64>) -> Usage {
        self.sum(key_id, |_| true)
    }

    fn sum(&self, key_id: Option<i64>, include: impl Fn(&Entry) -> bool) -> Usage {
        self.entries
            .values()
            .filter(|entry| {
                matches!(entry.scope, Charged::Temporary { key_id: of }
                    if key_id.is_none_or(|key| of == key))
                    && include(entry)
            })
            .fold(Usage::default(), |usage, entry| Usage {
                results: usage.results.saturating_add(1),
                bytes: usage.bytes.saturating_add(entry.bytes),
            })
    }

    /// The evidence bytes not done, of `investigation` or of every one: the
    /// evidence pre-check's count.
    fn evidence_pending(&self, investigation: Option<&InvestigationId>) -> u64 {
        self.evidence_sum(investigation, |entry| !entry.done)
    }

    /// Every evidence charge's bytes, of `investigation` or of every one: a
    /// binding pass's count, after its purge.
    fn evidence_counted(&self, investigation: Option<&InvestigationId>) -> u64 {
        self.evidence_sum(investigation, |_| true)
    }

    fn evidence_sum(
        &self,
        investigation: Option<&InvestigationId>,
        include: impl Fn(&Entry) -> bool,
    ) -> u64 {
        self.entries
            .values()
            .filter(|entry| {
                matches!(&entry.scope, Charged::Evidence { investigation: of }
                    if investigation.is_none_or(|id| of == id))
                    && include(entry)
            })
            .fold(0, |bytes, entry| bytes.saturating_add(entry.bytes))
    }
}

/// The refusal for a charge of `bytes` on top of `used` evidence in the
/// investigation and `installation` in the installation, if either budget
/// cannot hold it: the investigation's checked first.
fn evidence_full(
    used: u64,
    installation: u64,
    bytes: u64,
    budget: EvidenceBudget,
) -> Option<Refusal> {
    if used.saturating_add(bytes) > budget.per_investigation {
        Some(Refusal::EvidenceBudgetExceeded {
            used,
            limit: budget.per_investigation,
        })
    } else if installation.saturating_add(bytes) > budget.installation {
        Some(Refusal::EvidenceBudgetExceeded {
            used: installation,
            limit: budget.installation,
        })
    } else {
        None
    }
}

fn plus(a: Usage, b: Usage) -> Usage {
    Usage {
        results: a.results.saturating_add(b.results),
        bytes: a.bytes.saturating_add(b.bytes),
    }
}

/// Await `work` within `deadline`. An expiry is [`busy`]; a store failure
/// is [`Refusal::Store`].
async fn within<T>(
    deadline: Deadline,
    work: impl Future<Output = Result<T, StoreError>>,
) -> Result<T, Refusal> {
    match deadline.run(work).await {
        Ok(result) => result.map_err(Refusal::Store),
        Err(Expired) => Err(busy()),
    }
}

/// The refusal for a deadline that expired during admission: the capture
/// never started, so it is busy, and may try again in a second.
fn busy() -> Refusal {
    Refusal::Busy {
        retry_after_secs: 1,
    }
}

/// `span` in whole seconds, rounded up, at least 1.
fn ceil_secs(span: Duration) -> u64 {
    (span.as_secs() + u64::from(span.subsec_nanos() > 0)).max(1)
}

/// Whole seconds from `now` until `at`, at least 1 and at most `ttl_secs`;
/// `ttl_secs` when nothing will expire.
fn until_expiry(at: Option<DateTime<Utc>>, now: DateTime<Utc>, ttl_secs: u64) -> u64 {
    at.map_or(ttl_secs, |at| {
        (at - now).to_std().map_or(1, ceil_secs).min(ttl_secs)
    })
    .max(1)
}

fn warn_expiry(step: &str, error: &dyn std::fmt::Display) {
    tracing::warn!(
        event_type = "results_expire_at_reservation_error",
        step,
        error = %error,
        "expiring due captures at a reservation failed; the next sweep retries it"
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::SeqCst;

    use chrono::{TimeDelta, TimeZone as _};
    use sqlx::PgPool;
    use trawl_api::results::{
        ColumnFacts, ResultColumn, ResultShape, ResultSource, ResultState, StoredAs,
    };
    use trawl_api::value::ColumnType;

    use super::*;
    use crate::results::root::Published;
    use crate::store::{CaptureInsert, NewCapture};

    const KEY: i64 = 7;

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 23, 10, 0, 0).unwrap()
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn id(n: u32) -> String {
        format!("{n:032x}")
    }

    /// A gate on a real store and results root, reading a fixed clock that
    /// starts at [`start`].
    struct Fixture {
        gate: Arc<CaptureGate>,
        root: Arc<ResultsRoot>,
        store: ResultsStore,
        clock: ResultsClock,
        capture: CaptureConfig,
        _dir: tempfile::TempDir,
    }

    fn fixture(pool: PgPool, capture: CaptureConfig) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root =
            Arc::new(ResultsRoot::open_apart_from(&dir.path().join("data.results"), &[]).unwrap());
        let store = ResultsStore::new(pool);
        let clock = ResultsClock::fixed(start());
        let gate = Arc::new(CaptureGate::new(
            &capture,
            store.clone(),
            Arc::clone(&root),
            clock.clone(),
        ));
        Fixture {
            gate,
            root,
            store,
            clock,
            capture,
            _dir: dir,
        }
    }

    impl Fixture {
        /// Record a temporary capture of `key_id` made now, expiring
        /// `ttl_secs` later, with `file` or no file.
        async fn insert(&self, n: u32, key_id: i64, file: Option<Published>) {
            let created_at = self.clock.now();
            let row = NewCapture {
                id: id(n),
                key_id,
                created_at,
                expires_at: created_at
                    + TimeDelta::seconds(i64::try_from(self.capture.ttl_secs).unwrap()),
                query: "_service=nginx".into(),
                executed_query: "_service=nginx | sort -_time, -_ingested".into(),
                source: ResultSource::Live,
                shape: ResultShape::Events,
                earliest: Some(created_at - TimeDelta::minutes(15)),
                latest: Some(created_at),
                columns: vec![ResultColumn {
                    name: "_time".into(),
                    ty: ColumnType::Timestamp,
                    stored_as: StoredAs::Native,
                }],
                column_facts: vec![ColumnFacts {
                    nulls: 0,
                    distinct: 0,
                    distinct_capped: false,
                    values: None,
                    min: None,
                    max: None,
                }],
                captured_rows: 0,
                total_rows: 0,
                wire_bytes: 0,
                file_bytes: file.map_or(0, |file| file.file_bytes),
                digest: file.map(|file| file.digest),
                format_version: 1,
                investigation_id: None,
            };
            assert_eq!(
                self.store.insert(&row).await.unwrap(),
                CaptureInsert::Inserted
            );
        }

        fn admit(
            &self,
            key_id: i64,
            budget: u64,
        ) -> impl Future<Output = Result<Admission, Refusal>> + use<> {
            let gate = Arc::clone(&self.gate);
            async move { gate.admit(key_id, Deadline::after(secs(budget))).await }
        }

        fn waiting(&self, key_id: i64) -> u64 {
            self.gate
                .lanes
                .get(&key_id)
                .map_or(0, |lane| lane.waiting.load(SeqCst))
        }

        fn lane_free(&self, key_id: i64) -> usize {
            self.gate
                .lanes
                .get(&key_id)
                .unwrap()
                .slots
                .semaphore
                .available_permits()
        }

        fn installation_free(&self) -> usize {
            self.gate.installation.semaphore.available_permits()
        }

        fn pending(&self, key_id: Option<i64>) -> Usage {
            self.gate.book.lock().pending(key_id)
        }

        fn charge(&self, results: u64) -> Usage {
            Usage {
                results,
                bytes: results * self.capture.max_file_bytes,
            }
        }
    }

    /// Publish a file of `len` bytes as result `n`.
    fn publish(root: &ResultsRoot, n: u32, len: usize) -> Published {
        let staged = root.stage().unwrap();
        std::fs::write(staged.path(), vec![b'x'; len]).unwrap();
        root.publish(staged, &id(n)).unwrap()
    }

    /// Poll `done` until it holds, failing after a generous bound.
    async fn wait_until(what: &str, done: impl Fn() -> bool) {
        let give_up = std::time::Instant::now() + secs(30);
        while !done() {
            assert!(std::time::Instant::now() < give_up, "never saw {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[sqlx::test]
    async fn the_fifth_caller_is_refused_busy_while_four_wait(pool: PgPool) {
        let f = fixture(pool, CaptureConfig::default());
        let holder = f.admit(KEY, 30).await.unwrap();
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let admit = f.admit(KEY, 60);
                tokio::spawn(async move { admit.await.map(drop) })
            })
            .collect();
        wait_until("four waiters", || f.waiting(KEY) == 4).await;

        let refused = f.admit(KEY, 60).await;
        let Err(Refusal::Busy { retry_after_secs }) = refused else {
            panic!("expected capture_busy, got {refused:?}");
        };
        // The holder's deadline: 30 s from its admission, moments ago.
        assert!((21..=30).contains(&retry_after_secs), "{retry_after_secs}");
        assert_eq!(f.waiting(KEY), 4, "the refused caller never queued");

        // Each waiter takes the lane in turn once the holder lets go.
        drop(holder);
        for waiter in waiters {
            waiter.await.unwrap().unwrap();
        }
        assert_eq!(f.waiting(KEY), 0);
        assert_eq!(f.lane_free(KEY), 1);
        assert_eq!(f.installation_free(), 2);
    }

    #[sqlx::test]
    async fn a_third_running_capture_is_refused_busy_until_the_first_deadline(pool: PgPool) {
        let f = fixture(pool, CaptureConfig::default());
        let _late = f.admit(1, 1_000).await.unwrap();
        let _early = f.admit(2, 100).await.unwrap();

        let refused = f.admit(3, 60).await;
        let Err(Refusal::Busy { retry_after_secs }) = refused else {
            panic!("expected capture_busy, got {refused:?}");
        };
        assert!((91..=100).contains(&retry_after_secs), "{retry_after_secs}");
        // The refused capture gave its lane back and reserved nothing.
        assert_eq!(f.lane_free(3), 1);
        assert_eq!(f.waiting(3), 0);
        assert_eq!(f.pending(None), f.charge(2));
    }

    #[sqlx::test]
    async fn two_captures_racing_for_a_keys_last_result_admit_exactly_one(pool: PgPool) {
        let f = fixture(
            pool,
            CaptureConfig {
                per_key_results: 1,
                per_key_running: 2,
                ..CaptureConfig::default()
            },
        );
        // Hold the pass lock so both racers pass the pre-check, take their
        // slots and meet at the binding reservation.
        let pass = f.gate.pass.lock().await;
        let racers: Vec<_> = (0..2).map(|_| tokio::spawn(f.admit(KEY, 60))).collect();
        wait_until("both racers at the reservation", || {
            f.installation_free() == 0
        })
        .await;
        drop(pass);

        let mut admitted = Vec::new();
        let mut refused = Vec::new();
        for racer in racers {
            match racer.await.unwrap() {
                Ok(admission) => admitted.push(admission),
                Err(refusal) => refused.push(refusal),
            }
        }
        assert_eq!(admitted.len(), 1);
        // No result of the key will expire: the refusal names the TTL.
        assert!(
            matches!(
                refused.as_slice(),
                [Refusal::QuotaExceeded {
                    retry_after_secs: 900
                }]
            ),
            "{refused:?}"
        );
        // The loser gave back both slots; the winner holds its own.
        assert_eq!(f.installation_free(), 1);
        assert_eq!(f.lane_free(KEY), 1);
        assert_eq!(f.pending(Some(KEY)), f.charge(1));
    }

    #[sqlx::test]
    async fn a_dropped_admission_releases_the_lane_the_installation_and_the_book(pool: PgPool) {
        let f = fixture(
            pool,
            CaptureConfig {
                per_key_results: 1,
                ..CaptureConfig::default()
            },
        );
        let admission = f.admit(KEY, 60).await.unwrap();
        assert_eq!(f.lane_free(KEY), 0);
        assert_eq!(f.installation_free(), 1);
        assert_eq!(f.pending(Some(KEY)), f.charge(1));

        drop(admission);
        assert_eq!(f.lane_free(KEY), 1);
        assert_eq!(f.installation_free(), 2);
        assert_eq!(f.pending(Some(KEY)), Usage::default());

        // The key's only result is free again, and the executor slot's
        // holds release everything the same way.
        let holds = f.admit(KEY, 60).await.unwrap().into_holds();
        assert_eq!(
            f.gate.book.lock().entries.len(),
            1,
            "the pass forgot the done charge"
        );
        assert_eq!(f.lane_free(KEY), 0);
        assert_eq!(f.installation_free(), 1);
        drop(holds);
        assert_eq!(f.lane_free(KEY), 1);
        assert_eq!(f.installation_free(), 2);
        assert_eq!(f.pending(Some(KEY)), Usage::default());
    }

    #[sqlx::test]
    async fn settle_releases_the_reservation_while_the_slots_stay(pool: PgPool) {
        let f = fixture(
            pool,
            CaptureConfig {
                per_key_results: 2,
                per_key_running: 2,
                ..CaptureConfig::default()
            },
        );
        let first = f.admit(KEY, 60).await.unwrap();
        let reservation = first.reservation().clone();
        let holds = first.into_holds();

        // The first capture's row commits. Until it settles it counts
        // twice, which refuses the key's second capture.
        f.insert(1, KEY, None).await;
        let refused = f.admit(KEY, 60).await;
        assert!(
            matches!(
                refused,
                Err(Refusal::QuotaExceeded {
                    retry_after_secs: 900
                })
            ),
            "{refused:?}"
        );

        reservation.settle();
        assert_eq!(f.pending(Some(KEY)), Usage::default());
        assert_eq!(f.lane_free(KEY), 1, "the first capture keeps its lane slot");
        assert_eq!(f.installation_free(), 1, "and its installation slot");

        let second = f.admit(KEY, 60).await.unwrap();
        assert_eq!(f.installation_free(), 0);
        reservation.settle();
        drop((reservation, holds, second));
        assert_eq!(f.installation_free(), 2);
        assert_eq!(f.pending(None), Usage::default());
    }

    #[sqlx::test]
    async fn an_expired_capture_frees_its_file_and_its_bytes_at_the_next_reservation(pool: PgPool) {
        // One capture's charge fits the key's bytes; the stored 200-byte
        // file and a charge do not.
        let f = fixture(
            pool,
            CaptureConfig {
                max_file_bytes: 1_024,
                per_key_bytes: 1_124,
                ..CaptureConfig::default()
            },
        );
        let published = publish(&f.root, 1, 200);
        f.insert(1, KEY, Some(published)).await;

        // Full until the capture expires 900 s from now, and each refusal
        // says how long that is, never less than a second.
        let retry = |refused: Result<Admission, Refusal>| match refused {
            Err(Refusal::QuotaExceeded { retry_after_secs }) => retry_after_secs,
            other => panic!("expected capture_quota_exceeded, got {other:?}"),
        };
        assert_eq!(retry(f.admit(KEY, 60).await), 900);
        f.clock.set(start() + TimeDelta::seconds(600));
        assert_eq!(retry(f.admit(KEY, 60).await), 300);
        f.clock.set(start() + TimeDelta::microseconds(899_999_999));
        assert_eq!(retry(f.admit(KEY, 60).await), 1);
        assert!(
            f.root.open_file(&id(1)).unwrap().is_some(),
            "a pre-check unlinks nothing"
        );

        f.clock.set(start() + TimeDelta::seconds(900));
        let _admission = f.admit(KEY, 60).await.unwrap();
        assert!(
            f.root.open_file(&id(1)).unwrap().is_none(),
            "the expired capture's file is unlinked"
        );
        assert_eq!(
            f.store.get(&id(1), KEY).await.unwrap().unwrap().state,
            ResultState::Expired
        );
    }

    #[sqlx::test]
    async fn the_pre_check_refuses_a_full_key_without_queueing(pool: PgPool) {
        let f = fixture(
            pool,
            CaptureConfig {
                per_key_results: 2,
                ..CaptureConfig::default()
            },
        );
        f.insert(1, KEY, None).await;
        let _holder = f.admit(KEY, 60).await.unwrap();

        // The lane is busy and the key is full with the holder's charge:
        // the refusal comes at once, without a place in the queue.
        let refused = tokio::time::timeout(secs(30), f.admit(KEY, 60))
            .await
            .expect("the pre-check refuses without waiting for the lane");
        assert!(
            matches!(
                refused,
                Err(Refusal::QuotaExceeded {
                    retry_after_secs: 900
                })
            ),
            "{refused:?}"
        );
        assert_eq!(f.waiting(KEY), 0);

        // A key full before its first capture never gets a lane.
        f.insert(2, 8, None).await;
        f.insert(3, 8, None).await;
        assert!(matches!(
            f.admit(8, 60).await,
            Err(Refusal::QuotaExceeded { .. })
        ));
        assert!(f.gate.lanes.get(&8).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_fifth_caller_names_the_holders_remaining_deadline() {
        let lane = Arc::new(Lane::new(1));
        let holder = lane.enter(0, Deadline::after(secs(30)), 4).await.unwrap();
        tokio::time::advance(secs(10)).await;
        let waiters: Vec<_> = (1..=4)
            .map(|ticket| {
                let lane = Arc::clone(&lane);
                tokio::spawn(async move {
                    lane.enter(ticket, Deadline::after(secs(60)), 4)
                        .await
                        .map(drop)
                })
            })
            .collect();
        while lane.waiting.load(SeqCst) < 4 {
            tokio::task::yield_now().await;
        }

        let refused = lane.enter(5, Deadline::after(secs(60)), 4).await;
        assert!(
            matches!(
                refused,
                Err(Refusal::Busy {
                    retry_after_secs: 20
                })
            ),
            "{refused:?}"
        );
        assert_eq!(lane.waiting.load(SeqCst), 4);

        drop(holder);
        for waiter in waiters {
            waiter.await.unwrap().unwrap();
        }
        assert_eq!(lane.waiting.load(SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deadline_that_expires_in_the_queue_is_busy_for_a_second() {
        let lane = Lane::new(1);
        let _holder = lane.enter(0, Deadline::after(secs(30)), 4).await.unwrap();
        let refused = lane.enter(1, Deadline::after(secs(2)), 4).await;
        assert!(
            matches!(
                refused,
                Err(Refusal::Busy {
                    retry_after_secs: 1
                })
            ),
            "{refused:?}"
        );
        assert_eq!(lane.waiting.load(SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_whose_caller_goes_away_leaves_the_queue() {
        let lane = Arc::new(Lane::new(1));
        let _holder = lane.enter(0, Deadline::after(secs(30)), 4).await.unwrap();
        let waiter = {
            let lane = Arc::clone(&lane);
            tokio::spawn(async move { lane.enter(1, Deadline::after(secs(60)), 4).await.map(drop) })
        };
        while lane.waiting.load(SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(lane.waiting.load(SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_is_never_zero() {
        assert_eq!(ceil_secs(Duration::ZERO), 1);
        assert_eq!(ceil_secs(Duration::from_nanos(1)), 1);
        assert_eq!(ceil_secs(secs(20)), 20);
        assert_eq!(ceil_secs(Duration::from_millis(20_001)), 21);

        let now = start();
        assert_eq!(
            until_expiry(Some(now + TimeDelta::microseconds(1)), now, 900),
            1
        );
        assert_eq!(until_expiry(Some(now), now, 900), 1);
        assert_eq!(until_expiry(Some(now - TimeDelta::seconds(5)), now, 900), 1);
        assert_eq!(until_expiry(Some(now + TimeDelta::hours(2)), now, 900), 900);
        assert_eq!(until_expiry(None, now, 900), 900);
        assert_eq!(until_expiry(None, now, 0), 1);

        // The first holder's deadline; one second once it has passed and
        // the holder is still stopping, and when nothing is recorded.
        let slots = Slots::new(2);
        assert_eq!(slots.retry_after_secs(), 1);
        let _late = slots.try_hold(0, Deadline::after(secs(100))).unwrap();
        let early = slots.try_hold(1, Deadline::after(secs(40))).unwrap();
        assert!(slots.try_hold(2, Deadline::after(secs(10))).is_none());
        assert_eq!(slots.retry_after_secs(), 40);
        tokio::time::advance(secs(41)).await;
        assert_eq!(slots.retry_after_secs(), 1);
        drop(early);
        assert_eq!(slots.retry_after_secs(), 59);
    }
}
