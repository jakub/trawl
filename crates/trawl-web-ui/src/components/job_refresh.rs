// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Visible-page polling for Jobs. Reads are serialized; a newer intent makes
//! the pending response obsolete and queues one fresh read. Last good data
//! remains available through transport failures.
//! An eligible 401 goes to the shared sign-in handler, which replaces the
//! page with sign-in carrying its current URL; disposed or superseded
//! reads must pass the lifetime checks first. Polling stops only when
//! navigation started. A declined 401 (an explicit logout is pending, or
//! another redirect started first) keeps polling, so a failed logout
//! still lets the next 401 sign in, and settles on the session-ended
//! sentence.
use crate::api::ApiError;
use crate::auth_return::sign_in_if_unauthorized;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::{cell::Cell, future::Future, rc::Rc};
use wasm_bindgen::{JsCast, closure::Closure};

#[allow(clippy::type_complexity)]
pub fn job_refresh<T, F, Fut>(
    active: Signal<bool>,
    fetch: F,
) -> (
    RwSignal<Option<Result<T, ApiError>>>,
    RwSignal<Option<String>>,
    Callback<()>,
)
where
    T: Clone + Send + Sync + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<T, ApiError>> + 'static,
{
    let data = RwSignal::new(None);
    let error = RwSignal::new(None);
    let revision = RwSignal::new(0u64);
    let alive = Rc::new(Cell::new(true));
    let busy = Rc::new(Cell::new(false));
    let desired = Rc::new(Cell::new(0u64));
    let fetch = Rc::new(fetch);
    let refresh = Callback::new(move |()| {
        revision.update(|r| *r += 1);
    });
    let document = leptos::prelude::document();
    let visible = RwSignal::new(!document.hidden());
    let listener = Closure::<dyn FnMut()>::new(move || {
        visible.set(!leptos::prelude::document().hidden());
        refresh.run(());
    });
    let _ = document
        .add_event_listener_with_callback("visibilitychange", listener.as_ref().unchecked_ref());
    let timer = StoredValue::new_local(None::<gloo_timers::callback::Interval>);
    let timer_busy = busy.clone();
    Effect::new(move |_| {
        let busy = timer_busy.clone();
        timer.update_value(|t| {
            *t = (visible.get() && active.get()).then(|| {
                gloo_timers::callback::Interval::new(5_000, move || {
                    if !busy.get() {
                        refresh.run(());
                    }
                })
            });
        });
    });
    let cleanup = StoredValue::new_local((alive.clone(), document, listener));
    on_cleanup(move || {
        cleanup.with_value(|(live, document, listener)| {
            live.set(false);
            timer.update_value(|t| *t = None);
            let _ = document.remove_event_listener_with_callback(
                "visibilitychange",
                listener.as_ref().unchecked_ref(),
            );
        });
    });
    Effect::new(move |_| {
        revision.track();
        let visible = visible.get() && active.get();
        // Invoke synchronously so page and mutation dependencies are tracked.
        let future = fetch();
        desired.set(desired.get().wrapping_add(1));
        if !visible || busy.replace(true) {
            return;
        }
        // Only successful data is useful while a retry is pending. Clear
        // an initial failure so the canonical loading state replaces Retry.
        if !matches!(data.get_untracked(), Some(Ok(_))) {
            data.set(None);
            error.set(None);
        }
        let generation = desired.get();
        let live = alive.clone();
        let busy = busy.clone();
        let desired = desired.clone();
        spawn_local(async move {
            let result = future.await;
            busy.set(false);
            if !live.get() {
                return;
            }
            if generation != desired.get() {
                refresh.run(());
                return;
            }
            let signing_in = result.as_ref().err().and_then(sign_in_if_unauthorized);
            if signing_in == Some(true) {
                live.set(false);
                timer.update_value(|t| *t = None);
                return;
            }
            match result {
                Ok(value) => {
                    let focused = leptos::prelude::document()
                        .active_element()
                        .filter(|el| el.tag_name() != "BODY")
                        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok());
                    data.set(Some(Ok(value)));
                    error.set(None);
                    restore_focus_after_refresh(focused, live, desired, generation, active);
                }
                Err(e) => {
                    let held = matches!(data.get_untracked(), Some(Ok(_)));
                    error.set(refresh_failure(&e, signing_in == Some(false), held));
                    if data.get_untracked().is_none() {
                        data.set(Some(Err(e)));
                    }
                }
            }
        });
    });
    (data, error, refresh)
}

/// The refresh line after a failed read. `declined` is a 401 the sign-in
/// handler declined; `held` says the page still shows earlier data.
fn refresh_failure(error: &ApiError, declined: bool, held: bool) -> Option<String> {
    match (declined, held) {
        // With no data, the page's own failure state shows the
        // session-ended sentence; once is enough.
        (true, false) => None,
        (true, true) => Some(error.to_string()),
        (false, true) => {
            Some("Refresh failed. Showing the last available data; retrying automatically.".into())
        }
        (false, false) => Some("Could not load data. Retrying automatically.".into()),
    }
}

fn restore_focus_after_refresh(
    focused: Option<web_sys::HtmlElement>,
    live: Rc<Cell<bool>>,
    desired: Rc<Cell<u64>>,
    generation: u64,
    active: Signal<bool>,
) {
    // Keyed rows retain their owners, but insertBefore moves can drop focus.
    // These lists read data directly, so their DOM update is queued before
    // this restoration. No browser user-event task can intervene.
    queue_microtask(move || {
        if !live.get()
            || generation != desired.get()
            || leptos::prelude::document().hidden()
            || active.try_get_untracked() != Some(true)
        {
            return;
        }
        if let Some(focused) = focused
            && focused.is_connected()
            && (focused.offset_width() > 0 || focused.offset_height() > 0)
            && focused.closest("[hidden]").ok().flatten().is_none()
            && leptos::prelude::document()
                .active_element()
                .is_some_and(|active| active.tag_name() == "BODY")
        {
            let options = web_sys::FocusOptions::new();
            options.set_prevent_scroll(true);
            let _ = focused.focus_with_options(&options);
        }
    });
}
