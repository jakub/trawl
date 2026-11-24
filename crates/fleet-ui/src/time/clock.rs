// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One module-level signal, advanced by one 30s interval, never per-instance
//! timers: a page of 200 [`When`](super::when::When) labels costs one timer.

use std::cell::RefCell;

use leptos::prelude::*;

thread_local! {
    static NOW_MS: RefCell<Option<RwSignal<i64>>> = const { RefCell::new(None) };
}

fn current_ms() -> i64 {
    // chrono's `wasmbind` feature bridges Utc::now to js Date.
    chrono::Utc::now().timestamp_millis()
}

/// Call once at the app root; re-entry is a no-op, so nested roots can't
/// stack timers.
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

#[must_use]
pub fn now_ms() -> Signal<i64> {
    NOW_MS
        .with_borrow(|slot| slot.map(|sig| sig.read_only()))
        .map_or_else(
            || {
                #[cfg(debug_assertions)]
                leptos::logging::warn!(
                    "fleet-ui: time::clock::install() was never called — relative timestamps will not tick"
                );
                Signal::derive(current_ms)
            },
            Signal::from,
        )
}
