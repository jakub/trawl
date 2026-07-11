// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<CopyButton/>` — click-to-copy wired to the toast system (issue
//! #33 D6). Unifies trawl's two hand-rolled copies (results-table raw
//! event, editor share URL); coastwatch wants the same affordance.
//!
//! The trigger is a secondary [`Btn`](crate::button::Btn) by default;
//! passing `class` renders a bare `<span class=…>` instead — for
//! app-styled inline triggers like trawl's editor `.tool` links,
//! keeping app classes out of fleet-ui. Success and error report
//! through the [`ToastBus`](crate::toast::ToastBus) the `Shell`
//! provides via context, with the canonical "Copied" / "Copy failed"
//! titles; `success_detail` is the optional app-flavored second line.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::button::{Btn, Variant};
use crate::clipboard::write_clipboard;
use crate::toast::{ToastBus, ToastKind};

/// Click-to-copy trigger. `text` resolves lazily at click time (derive
/// it for values that change under the trigger, e.g. the current URL);
/// `children` is the trigger's label/icon slot.
#[component]
pub fn CopyButton(
    #[prop(into)] text: Signal<String>,
    #[prop(optional)] class: Option<&'static str>,
    #[prop(optional, into)] success_detail: Option<String>,
    children: Children,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();

    let do_copy = move || {
        let value = text.get_untracked();
        let detail = success_detail.clone();
        spawn_local(async move {
            match write_clipboard(&value).await {
                Ok(()) => bus.push(ToastKind::Success, "Copied", detail),
                Err(msg) => bus.push(ToastKind::Error, "Copy failed", Some(msg)),
            }
        });
    };

    match class {
        // Bare mode: an app-styled inline trigger. stop_propagation for
        // the same reason as Btn's flag — copy triggers live inside
        // clickable rows/headers.
        Some(cls) => view! {
            <span
                class=cls
                on:click=move |e: leptos::web_sys::MouseEvent| {
                    e.stop_propagation();
                    do_copy();
                }
            >{children()}</span>
        }
        .into_any(),
        None => view! {
            <Btn
                variant=Variant::Secondary
                stop_propagation=true
                on_click=Callback::new(move |()| do_copy())
            >{children()}</Btn>
        }
        .into_any(),
    }
}
