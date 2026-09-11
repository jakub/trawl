// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Toasts/>` host + `ToastBus` push handle (wasm-only runtime).
//!
//! Errors stay until dismissed. Other toasts expire after 4.5s without
//! pointer or keyboard interaction. Border-left color encodes the kind — the class
//! mapping lives on [`ToastKind::as_class`] so it's testable natively.
//!
//! The title+detail wrapper is `toast-body`, not `body`: `.body` is a
//! reserved Shell chrome class (the rail+main row) whose `display: flex`
//! would flatten a toast's title and detail onto one line.

use leptos::leptos_dom::helpers::TimeoutHandle;
use leptos::prelude::*;
use std::time::Duration;

use super::kinds::ToastKind;
use super::stack::{Toast, ToastStack};

/// Push handle — clone-and-share. Drives the `<Toasts/>` host.
///
/// # Ownership contract
///
/// [`Shell`](crate::shell::Shell) owns the bus: it calls
/// `ToastBus::new()`, `provide_context`s it, and mounts the single
/// `<Toasts/>` host. Everything rendered inside the Shell (router
/// `<Outlet/>` content, footer, modals) pushes via
/// `expect_context::<ToastBus>()`; a second bus or a second
/// `<Toasts/>` inside a Shell means two competing stacks. Only screens
/// rendered outside a Shell need their own `ToastBus::new()` +
/// `<Toasts bus=bus/>` pair.
#[derive(Debug, Clone, Copy)]
pub struct ToastBus {
    stack: RwSignal<ToastStack>,
}

impl ToastBus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stack: RwSignal::new(ToastStack::new()),
        }
    }

    /// Push a toast. The mounted item owns expiry; errors never expire.
    pub fn push(self, kind: ToastKind, title: impl Into<String>, detail: Option<String>) {
        // Single-closure mutation: id allocation and append happen atomically
        // inside `ToastStack::push`, so there's no read/write window for a
        // concurrent push to observe the same counter and produce a duplicate
        // key in `<For key=|t| t.id>`. Wasm is single-threaded today; this is
        // insurance against future `spawn_local` interleaving.
        //
        // `try_update` returns `None` only if the signal was disposed. In that
        // case no toast was appended, so log the swallow — toasts are the app's error-surfacing primitive,
        // so a silently-dropped push (especially an error toast) would leave the
        // failure with zero trace.
        if self
            .stack
            .try_update(|s| s.push(kind, title, detail))
            .is_none()
        {
            web_sys::console::warn_1(&"toast dropped: bus signal disposed".into());
        }
    }

    /// Variant-encoded sugar over [`Self::push`]. Use these at call
    /// sites — `bus.push_error("save failed", None)` reads better than
    /// `bus.push(ToastKind::Error, "save failed", None)`.
    pub fn push_info(self, title: impl Into<String>, detail: Option<String>) {
        self.push(ToastKind::Info, title, detail);
    }

    pub fn push_success(self, title: impl Into<String>, detail: Option<String>) {
        self.push(ToastKind::Success, title, detail);
    }

    pub fn push_error(self, title: impl Into<String>, detail: Option<String>) {
        self.push(ToastKind::Error, title, detail);
    }

    /// Programmatically dismiss a toast by id (e.g. when an in-flight
    /// retry succeeds before the auto-dismiss timeout). No-op if the id
    /// has already been removed.
    pub fn dismiss(self, id: u64) {
        let _ = self.stack.try_update(|s| s.dismiss(id));
    }
}

impl Default for ToastBus {
    fn default() -> Self {
        Self::new()
    }
}

#[component]
pub fn Toasts(bus: ToastBus) -> impl IntoView {
    let host = NodeRef::<leptos::html::Div>::new();
    let hovered = RwSignal::new(false);
    let focused = RwSignal::new(false);
    let paused = Signal::derive(move || hovered.get() || focused.get());
    Effect::new(move |_| {
        // Follow additions after the keyed list has rendered. While someone
        // reads or operates the stack, keep its scroll position and pause all
        // routine expiry, including notices outside the visible scroll area.
        let _latest = bus
            .stack
            .with(|stack| stack.items().last().map(|toast| toast.id));
        if !paused.get() {
            request_animation_frame(move || {
                if paused.try_get_untracked() == Some(false)
                    && let Some(Some(element)) = host.try_get_untracked()
                {
                    element.set_scroll_top(element.scroll_height());
                }
            });
        }
    });
    view! {
        // Routine operation outcomes are polite. The region is mounted
        // empty before the first push; additions announce only new items.
        <div node_ref=host class="toasts"
            on:pointerenter=move |_| hovered.set(true)
            on:pointerleave=move |_| hovered.set(false)
            on:focusin=move |_| focused.set(true)
            on:focusout=move |_| focused.set(false)
            role="status" aria-label="Notifications" aria-live="polite" aria-atomic="false" aria-relevant="additions">
            <For
                each=move || bus.stack.with(|s| s.items().to_vec())
                key=|t| t.id
                children=move |toast| view! { <ToastItem toast=toast bus=bus paused=paused/> }
            />
        </div>
    }
}

#[component]
fn ToastItem(toast: Toast, bus: ToastBus, paused: Signal<bool>) -> impl IntoView {
    let id = toast.id;
    let persistent = toast.kind == ToastKind::Error;
    let timer = StoredValue::new(None::<TimeoutHandle>);
    Effect::new(move |_| {
        timer.update_value(|timer| {
            if let Some(timer) = timer.take() {
                timer.clear();
            }
        });
        if !persistent
            && !paused.get()
            && let Ok(handle) =
                set_timeout_with_handle(move || bus.dismiss(id), Duration::from_millis(4500))
        {
            timer.set_value(Some(handle));
        }
    });
    on_cleanup(move || {
        if let Some(timer) = timer.get_value() {
            timer.clear();
        }
    });
    view! {
        <div
            class=format!("toast {}", toast.kind.as_class())
        >
            <div class="toast-body">
                <div class="title">{toast.title}</div>
                {toast.detail.map(|d| view! { <div class="detail">{d}</div> })}
            </div>
            // The multiplication sign is decoration; the button's name
            // comes from aria-label, not a spoken "times" glyph.
            <button
                type="button"
                class="x"
                aria-label="Dismiss notification"
                on:click=move |_| bus.dismiss(id)
            ><span aria-hidden="true">"×"</span></button>
        </div>
    }
}
