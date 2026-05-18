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

/// A single toast notification. Construction is sealed: instances only
/// arise from [`ToastBus::push`] (and its kind-specific helpers), so the
/// monotonic `id` allocated by the bus is the only one in circulation —
/// preventing a third-party `Toast { id: 0, ... }` from colliding with
/// keys the `<For>` loop relies on.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Toast {
    pub(crate) id: u64,
    pub(crate) kind: ToastKind,
    pub(crate) title: String,
    pub(crate) detail: Option<String>,
}

impl Toast {
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub fn kind(&self) -> ToastKind {
        self.kind
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
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
