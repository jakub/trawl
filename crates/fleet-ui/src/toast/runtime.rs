// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Toasts/>` host + `ToastBus` push handle (wasm-only runtime).
//!
//! Toasts auto-dismiss after 4.5s via a `gloo_timers::future::TimeoutFuture`
//! launched per push. Border-left color encodes the kind — the class
//! mapping lives on [`ToastKind::as_class`] so it's testable natively.

use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::kinds::ToastKind;
use super::stack::ToastStack;

/// Push handle — clone-and-share. Drives the `<Toasts/>` host.
///
/// # Ownership contract
///
/// In an app composed around [`Shell`](crate::shell::Shell), the Shell
/// owns the bus: it calls `ToastBus::new()`, `provide_context`s it,
/// and mounts the single `<Toasts/>` host. Everything rendered inside
/// the Shell (router `<Outlet/>` content, footer, modals) pushes via
/// `expect_context::<ToastBus>()`. Do NOT construct a second bus or
/// mount a second `<Toasts/>` inside a Shell — that produces two
/// competing toast stacks. Only screens rendered outside a Shell need
/// their own `ToastBus::new()` + `<Toasts bus=bus/>` pair.
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

    /// Push a toast. Auto-dismisses after 4.5s.
    pub fn push(self, kind: ToastKind, title: impl Into<String>, detail: Option<String>) {
        // Single-closure mutation: id allocation and append happen atomically
        // inside `ToastStack::push`, so there's no read/write window for a
        // concurrent push to observe the same counter and produce a duplicate
        // key in `<For key=|t| t.id>`. Wasm is single-threaded today; this is
        // insurance against future `spawn_local` interleaving.
        let id = self
            .stack
            .try_update(|s| s.push(kind, title, detail))
            .unwrap_or(0);
        spawn_local(async move {
            TimeoutFuture::new(4500).await;
            self.stack.update(|s| s.dismiss(id));
        });
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
        self.stack.update(|s| s.dismiss(id));
    }
}

impl Default for ToastBus {
    fn default() -> Self {
        Self::new()
    }
}

#[component]
pub fn Toasts(bus: ToastBus) -> impl IntoView {
    view! {
        <div class="toasts">
            <For
                each=move || bus.stack.with(|s| s.items().to_vec())
                key=|t| t.id
                children=move |t| {
                    let id = t.id;
                    let cls = format!("toast {}", t.kind.as_class());
                    view! {
                        <div class=cls>
                            <div class="body">
                                <div class="title">{t.title}</div>
                                {t.detail.map(|d| view! { <div class="detail">{d}</div> })}
                            </div>
                            <span class="x" on:click=move |_| bus.dismiss(id)>"×"</span>
                        </div>
                    }
                }
            />
        </div>
    }
}
