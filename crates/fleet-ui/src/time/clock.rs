// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The single shared "now" tick every relative timestamp subscribes
//! to (issue #33 D4). One module-level signal, advanced by ONE 30s
//! interval installed once via [`install`] (the
//! [`theme::install`](crate::theme) pattern) — never per-instance
//! timers, so a page of 200 [`When`](super::when::When) labels costs
//! one timer and re-renders in one batch.

use std::cell::RefCell;

use leptos::prelude::*;

thread_local! {
    /// The shared now-ms signal; `None` until [`install`] runs.
    static NOW_MS: RefCell<Option<RwSignal<i64>>> = const { RefCell::new(None) };
}

fn current_ms() -> i64 {
    // chrono's `wasmbind` feature bridges Utc::now to js Date.
    chrono::Utc::now().timestamp_millis()
}

/// Install the shared clock: one `RwSignal<i64>` advanced by a single
/// 30-second interval. Call once at the app root (right after
/// [`theme::install`](crate::theme)); re-entry is a no-op if the clock
/// `is_some` already, so nested roots can't stack timers.
pub fn install() {
    NOW_MS.with_borrow_mut(|slot| {
        if slot.is_some() {
            return;
        }
        let sig = RwSignal::new(current_ms());
        gloo_timers::callback::Interval::new(30_000, move || sig.set(current_ms())).forget();
        *slot = Some(sig);
    });
}

/// The shared now-ms signal. If [`install`] was never called, falls
/// back to a derived one-shot snapshot — labels still render, they
/// just don't tick.
#[must_use]
pub fn now_ms() -> Signal<i64> {
    NOW_MS
        .with_borrow(|slot| slot.map(|sig| sig.read_only()))
        .map_or_else(|| Signal::derive(current_ms), Signal::from)
}
