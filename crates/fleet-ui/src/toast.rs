// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Toasts/>` host + `ToastBus` push handle.
//!
//! Toasts auto-dismiss after 4.5s via a `gloo_timers::future::TimeoutFuture`
//! launched per push. Border-left color encodes the kind.

use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
}

/// Push handle — clone-and-share. Drives the `<Toasts/>` host.
///
/// Construct once at app boot (typically inside `<App/>` so the
/// `RwSignal` lives in the reactive root), then either pass it as a
/// prop to `<Toasts bus=bus/>` or stash it with `provide_context(bus)`
/// for descendants to pick up via `use_context`.
#[derive(Debug, Clone, Copy)]
pub struct ToastBus {
    items: RwSignal<Vec<Toast>>,
    next_id: StoredValue<u64>,
}

impl ToastBus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: RwSignal::new(Vec::new()),
            next_id: StoredValue::new(0),
        }
    }

    /// Push a toast. Auto-dismisses after 4.5s.
    pub fn push(self, kind: ToastKind, title: impl Into<String>, detail: Option<String>) {
        // Single-closure mutation: no read/write window for a concurrent push to
        // observe the same `n` and produce a duplicate key in `<For key=|t| t.id>`.
        // Wasm is single-threaded today; this is insurance against future `spawn_local`
        // interleaving and the cheapest fix.
        let id = self
            .next_id
            .try_update_value(|n| {
                *n += 1;
                *n
            })
            .unwrap_or(0);
        let toast = Toast {
            id,
            kind,
            title: title.into(),
            detail,
        };
        self.items.update(|v| v.push(toast));
        spawn_local(async move {
            TimeoutFuture::new(4500).await;
            self.items.update(|v| v.retain(|t| t.id != id));
        });
    }

    fn dismiss(self, id: u64) {
        self.items.update(|v| v.retain(|t| t.id != id));
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
                each=move || bus.items.get()
                key=|t| t.id
                children=move |t| {
                    let id = t.id;
                    let cls = match t.kind {
                        ToastKind::Info => "toast info",
                        ToastKind::Success => "toast success",
                        ToastKind::Error => "toast error",
                    };
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
