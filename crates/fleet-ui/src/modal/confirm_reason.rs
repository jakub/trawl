// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ConfirmWithReasonModal/>` — confirmation dialog with a required
//! reason textarea (audit-trail actions: invalidate, retract,
//! quarantine-resolve).
//!
//! `confirm_variant: Variant` follows the
//! [`ConfirmModal`](super::ConfirmModal) convention. Confirm stays
//! disabled until the reason is non-blank; `on_confirm` receives the
//! entered reason.

use leptos::prelude::*;

use crate::button::{Btn, Variant};
use crate::field::Field;
use crate::modal::shell::Modal;

#[component]
pub fn ConfirmWithReasonModal(
    title: &'static str,
    message: String,
    confirm_label: &'static str,
    reason_placeholder: &'static str,
    #[prop(default = Variant::Danger)] confirm_variant: Variant,
    on_confirm: Callback<String>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let reason = RwSignal::new(String::new());
    let reason_empty = Memo::new(move |_| reason.get().trim().is_empty());

    let confirm = Callback::new(move |()| on_confirm.run(reason.get_untracked()));

    view! {
        <Modal
            title=title
            role="alertdialog"
            narrow=true
            on_cancel=on_cancel
            footer=Box::new(move || view! {
                <div></div>
                <Btn variant=Variant::Secondary on_click=on_cancel>
                    "Cancel"
                </Btn>
                <Btn variant=confirm_variant disabled=reason_empty on_click=confirm>
                    {confirm_label}
                </Btn>
            }.into_any())
        >
            <p style="margin:0; font-size:13px; color:var(--ink-2)">{message}</p>
            <Field label="Reason" class="m-field">
                <textarea
                    class="reason-input"
                    placeholder=reason_placeholder
                    prop:value=move || reason.get()
                    on:input=move |e| {
                        reason.set(event_target_value(&e));
                    }
                />
            </Field>
        </Modal>
    }
}
