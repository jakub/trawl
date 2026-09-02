// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ConfirmModal/>` — reusable confirmation dialog (delete,
//! destructive actions).
//!
//! A thin composition over the [`Modal`] shell, which owns the scrim,
//! Escape and outside-click machinery.

use leptos::prelude::*;

use crate::button::{Btn, Variant};
use crate::modal::shell::Modal;

#[component]
pub fn ConfirmModal(
    title: &'static str,
    message: String,
    confirm_label: &'static str,
    #[prop(default = Variant::Danger)] confirm_variant: Variant,
    on_confirm: Callback<()>,
    on_cancel: Callback<()>,
) -> impl IntoView {
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
                <Btn variant=confirm_variant on_click=on_confirm>
                    {confirm_label}
                </Btn>
            }.into_any())
        >
            <p style="margin:0; font-size:13px; color:var(--ink-2)">{message}</p>
        </Modal>
    }
}
