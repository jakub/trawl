// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin garbage collection: reclaiming catalog slots no live field holds.
//!
//! A pin is spent permanently on the ingest path and the cap
//! ([`crate::store::MAX_PINNED_FIELDS`]) is install-wide, so a typo'd
//! field name or a decommissioned sender keeps a slot forever. This module
//! holds the pure half of the reclaim: how long "dead" is, and whether one
//! pin qualifies on the observation axis.
//!
//! Two independent proofs make a pin collectable, and this module owns the
//! first one only. The observation axis is the field catalog's own
//! `field_services` history, which says nothing has written the field
//! recently. The metadata axis is the standing corpus itself, where no
//! parquet footer under a live env declares the column. A candidate here is a candidate to be
//! disproved by a footer, never a decision to delete.
//!
//! Pure by construction: no clock, no pool, no filesystem. The caller
//! samples one `decided_at` for the whole run and hands the derived cutoff
//! down, so the report, the SQL comparisons and the audit events all agree
//! on when "now" was.

use std::time::Duration;

use chrono::{DateTime, Utc};
use trawl_core::schema::is_contract_typed;

/// How long a field must go unobserved before gc will consider it dead,
/// when the request names no window of its own.
///
/// A month covers the shapes that legitimately go quiet without being
/// gone: a monthly batch job, a service parked for a sprint, a host out
/// for a long repair. Shorter windows start collecting fields that were
/// only sleeping, and re-pinning is cheap but the conflict evidence and
/// per-service history that die with the pin are not.
pub const DEFAULT_DEAD_WINDOW: Duration = Duration::from_hours(24 * 30);

/// The window one gc run applies, and the numbers behind it.
///
/// The formula lives here and only here. The CLI and the SPA render
/// `requested`/`floor`/`effective` as the server reported them; if a
/// client recomputed the floor it would eventually disagree with the run
/// that actually happened, and an operator would be reading a window no
/// deletion ever used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadWindow {
    /// The requested window in seconds, after the server default applied.
    pub requested_secs: u64,
    /// The retention floor in seconds, when age retention is enabled.
    /// Reported whenever it exists, whether or not it bound the answer.
    pub floor_secs: Option<u64>,
    /// The window applied: the larger of the request and the floor.
    pub effective_secs: u64,
}

impl DeadWindow {
    /// The effective window as a [`Duration`].
    #[must_use]
    pub fn effective(&self) -> Duration {
        Duration::from_secs(self.effective_secs)
    }
}

/// Decide the window one gc run applies.
///
/// `older_than` is the operator's request, `None` meaning
/// [`DEFAULT_DEAD_WINDOW`]. `floor_secs` is the retention floor from
/// [`crate::retention::maximum_enabled_age_secs`]: `None` when age
/// retention is disabled, which imposes no floor at all.
///
/// The rule is one `max`. A window shorter than the corpus trawl still
/// keeps would call a field dead while its data sits on disk under a live
/// env, so retention raises the request; nothing ever lowers it. A
/// requested `0` is honoured literally, because the footer scan is the
/// proof that makes that safe, and an operator cleaning up after a bad shipper should
/// not have to wait out a window for a field the corpus has never carried.
#[must_use]
pub fn effective_dead_window(older_than: Option<Duration>, floor_secs: Option<u64>) -> DeadWindow {
    let requested_secs = older_than.unwrap_or(DEFAULT_DEAD_WINDOW).as_secs();
    let effective_secs = floor_secs.map_or(requested_secs, |floor| requested_secs.max(floor));
    DeadWindow {
        requested_secs,
        floor_secs,
        effective_secs,
    }
}

/// One pin's verdict on the observation axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidacy {
    /// Unobserved through the whole window: a candidate, pending the
    /// footer scan.
    Candidate,
    /// A field whose type is trawl's to declare, not an operator's to
    /// reclaim ([`is_contract_typed`]).
    ContractTyped,
    /// Observed at or after the cutoff, so something still writes it.
    ObservedInWindow,
}

/// Judge one pin on the observation axis.
///
/// `last_seen` is the newest `field_services` observation, `cutoff` the
/// run's `decided_at` minus the effective window.
///
/// Three rules, in this order:
///
/// 1. [`is_contract_typed`] wins over everything. The ten envelope slots
///    are trawl's own contract; an envelope field nobody has sent lately
///    is still the field the partition path, the peer fill and the
///    severity ladder are built on, and its seeded pin must survive a
///    corpus that never carried it.
/// 2. `last_seen >= cutoff` is alive. At-the-instant counts as observed:
///    the boundary belongs to the retained side, matching the `last_seen`
///    window the schema routes apply.
/// 3. Everything else, including a pin never observed at all, is a
///    candidate.
///
/// Rule 3 is a deliberate divergence from `/api/v1/schema`'s listing rule,
/// where a never-observed pin is always shown rather than windowed out
/// (a pin with no observation has no `last_seen` to compare, and hiding it
/// would make the seeded envelope invisible). The listing errs toward
/// showing; gc errs toward reclaiming, because a pin with no observation
/// and no carrier file is exactly the accidental slot this exists to
/// free: a `curl` typo pinned once, never written, never seen again. The
/// envelope stays safe through rule 1, not through the never-observed
/// case, and the metadata axis still has to agree before anything is
/// deleted.
#[must_use]
pub fn candidacy(
    field: &str,
    last_seen: Option<DateTime<Utc>>,
    cutoff: DateTime<Utc>,
) -> Candidacy {
    if is_contract_typed(field) {
        return Candidacy::ContractTyped;
    }
    match last_seen {
        Some(seen) if seen >= cutoff => Candidacy::ObservedInWindow,
        _ => Candidacy::Candidate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;
    use trawl_core::schema::ENVELOPE_TYPES;

    const DAY: u64 = 86_400;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid instant")
    }

    #[test]
    fn dead_window_truth_table() {
        // No request, no retention: the packaged default.
        let w = effective_dead_window(None, None);
        assert_eq!(w.requested_secs, 30 * DAY);
        assert_eq!(w.floor_secs, None);
        assert_eq!(w.effective_secs, 30 * DAY);
        assert_eq!(w.effective(), DEFAULT_DEAD_WINDOW);

        // 7 days requested under 90 days of retention: the floor wins.
        let w = effective_dead_window(Some(Duration::from_secs(7 * DAY)), Some(90 * DAY));
        assert_eq!(w.requested_secs, 7 * DAY);
        assert_eq!(w.floor_secs, Some(90 * DAY));
        assert_eq!(w.effective_secs, 90 * DAY);

        // A request longer than the floor stands, and the floor is still
        // reported so the operator can see it did not bind.
        let w = effective_dead_window(Some(Duration::from_secs(120 * DAY)), Some(90 * DAY));
        assert_eq!(w.effective_secs, 120 * DAY);
        assert_eq!(w.floor_secs, Some(90 * DAY));

        // Retention disabled: the request is the answer, default included.
        let w = effective_dead_window(Some(Duration::from_secs(DAY)), None);
        assert_eq!(w.effective_secs, DAY);
        assert_eq!(effective_dead_window(None, None).effective_secs, 30 * DAY);

        // Zero is a window, not a missing value.
        let w = effective_dead_window(Some(Duration::ZERO), None);
        assert_eq!(w.requested_secs, 0);
        assert_eq!(w.effective_secs, 0);

        // ...and it is still floored when retention names one.
        let w = effective_dead_window(Some(Duration::ZERO), Some(90 * DAY));
        assert_eq!(w.effective_secs, 90 * DAY);
    }

    /// The envelope is trawl's contract, so no window and no absence of
    /// observations can reclaim its seeded pins. `ENVELOPE_TYPES` growing
    /// a slot must not need this test edited.
    #[test]
    fn every_envelope_seed_survives_gc() {
        let cutoff = at(4_000_000_000);
        let ancient = at(0);

        for (field, _) in ENVELOPE_TYPES {
            assert_eq!(
                candidacy(field, None, cutoff),
                Candidacy::ContractTyped,
                "{field} (never observed) escaped the envelope refusal"
            );
            assert_eq!(
                candidacy(field, Some(ancient), cutoff),
                Candidacy::ContractTyped,
                "{field} (last seen in 1970) escaped the envelope refusal"
            );
        }

        // The four sender-asserted bare names carry no `_` prefix, so
        // they are the half a prefix predicate alone would miss.
        for field in ["env", "service", "host", "message"] {
            assert_eq!(candidacy(field, None, cutoff), Candidacy::ContractTyped);
        }
    }

    #[test]
    fn observation_exactly_at_the_cutoff_is_alive() {
        let cutoff = at(1_000_000);
        assert_eq!(
            candidacy("duration", Some(cutoff), cutoff),
            Candidacy::ObservedInWindow
        );
        assert_eq!(
            candidacy("duration", Some(at(1_000_001)), cutoff),
            Candidacy::ObservedInWindow
        );
        assert_eq!(
            candidacy("duration", Some(at(999_999)), cutoff),
            Candidacy::Candidate
        );
    }

    /// The divergence from the schema listing, asserted so a later "make
    /// gc match /schema" cleanup has to argue with a test.
    #[test]
    fn a_never_observed_pin_is_a_candidate() {
        assert_eq!(candidacy("typoed_feild", None, at(0)), Candidacy::Candidate);
    }
}
