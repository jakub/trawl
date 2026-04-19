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
    /// Success/Error variants are wired by callers in subsequent
    /// commits (save/share/export will use Success; query failures
    /// will use Error). Reserve them now so consumers don't need to
    /// add the variant alongside their first usage.
    #[allow(dead_code)]
    Success,
    #[allow(dead_code)]
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
#[derive(Clone, Copy)]
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
        let id = self.next_id.with_value(|n| n + 1);
        self.next_id.set_value(id);
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

    /// Convenience for the (title, detail) tuple shape passed through
    /// the editor toolbar's `on_toast` callback.
    pub fn info_tuple(self, t: (&'static str, &'static str)) {
        self.push(ToastKind::Info, t.0, Some(t.1.to_owned()));
    }

    fn dismiss(self, id: u64) {
        self.items.update(|v| v.retain(|t| t.id != id));
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
