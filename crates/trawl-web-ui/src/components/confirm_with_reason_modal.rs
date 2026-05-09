// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;
use leptos::web_sys;
use wasm_bindgen::JsCast;

#[component]
pub fn ConfirmWithReasonModal(
    title: &'static str,
    message: String,
    confirm_label: &'static str,
    reason_placeholder: &'static str,
    #[prop(default = true)] danger: bool,
    on_confirm: Callback<String>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let reason = RwSignal::new(String::new());
    let reason_empty = Memo::new(move |_| reason.get().trim().is_empty());

    let cancel = move || on_cancel.run(());
    let cancel_key = cancel;
    let cancel_scrim = cancel;
    let cancel_btn = cancel;

    let on_keydown = move |e: web_sys::KeyboardEvent| {
        if e.key() == "Escape" {
            e.prevent_default();
            cancel_key();
        }
    };

    let btn_class = if danger { "btn-danger" } else { "btn-pri" };

    view! {
        <div
            class="modal-scrim"
            on:mousedown=move |e: web_sys::MouseEvent| {
                if let Some(target) = e.target()
                    && let Some(el) = target.dyn_ref::<web_sys::Element>()
                    && el.class_name().contains("modal-scrim")
                {
                    cancel_scrim();
                }
            }
            on:keydown=on_keydown
        >
            <div class="modal modal-sm" role="alertdialog" aria-modal="true">
                <div class="m-hd">
                    <span class="t">{title}</span>
                    <span class="x" title="Close (Esc)" on:click=move |_| cancel_btn()>
                        <CloseIcon/>
                    </span>
                </div>

                <div class="m-body">
                    <p style="margin:0; font-size:13px; color:var(--ink-2)">{message}</p>
                    <div class="m-field">
                        <label>"Reason"</label>
                        <textarea
                            class="reason-input"
                            placeholder=reason_placeholder
                            prop:value=move || reason.get()
                            on:input=move |e| {
                                reason.set(event_target_value(&e));
                            }
                        />
                    </div>
                </div>

                <div class="m-ft">
                    <div></div>
                    <button class="btn-sec" on:click=move |_| on_cancel.run(())>"Cancel"</button>
                    <button
                        class=btn_class
                        disabled=reason_empty
                        on:click=move |_| on_confirm.run(reason.get_untracked())
                    >
                        {confirm_label}
                    </button>
                </div>
            </div>
        </div>
    }
}

#[component]
fn CloseIcon() -> impl IntoView {
    view! {
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="m4 4 8 8M12 4l-8 8"/>
        </svg>
    }
}
