// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<SaveAsNetModal/>` — confirm-save dialog for turning a DSL query
//! into a named saved query ("net").

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use wasm_bindgen::JsCast;

use crate::api;
use fleet_ui::{Btn, Field, Icon, Modal, ToastBus, ToastKind, Variant};

#[component]
#[allow(clippy::needless_pass_by_value)]
pub fn SaveAsNetModal(
    query: String,
    /// Search captures an editor buffer; History supplies a recorded query instead.
    #[prop(optional)]
    from_editor: bool,
    /// The bool is `true` on save, `false` on cancel.
    on_close: Callback<bool>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let suggested = suggest_name(&query);
    let name = RwSignal::new(suggested);
    let submitting = RwSignal::new(false);
    // Keep a per-open latch outside the reactive arena. A page-owned close
    // callback can outlive this dialog and must not close its replacement.
    let alive = Arc::new(AtomicBool::new(true));
    let cleanup_alive = Arc::clone(&alive);
    on_cleanup(move || cleanup_alive.store(false, Ordering::Release));

    let input_ref = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if let Some(el) = input_ref.get() {
            let input: web_sys::HtmlInputElement = (*el).clone().unchecked_into();
            let _ = input.focus();
            input.select();
        }
    });

    let q_for_submit = query.clone();
    let do_save = Callback::new(move |()| {
        let draft = name.get_untracked();
        let Ok(n) = trawl_core::saved_name::normalize(&draft) else {
            return;
        };
        let n = n.to_owned();
        if submitting.get_untracked() {
            return;
        }
        submitting.set(true);
        let q = q_for_submit.clone();
        let alive = Arc::clone(&alive);
        spawn_local(async move {
            let result = api::create_saved(&n, &q).await;
            submitting.try_set(false);
            match result {
                Ok(saved) => {
                    bus.push(
                        ToastKind::Success,
                        "Saved as net",
                        Some(format!("'{}' cast. You can haul it anytime.", saved.name)),
                    );
                    if alive.load(Ordering::Acquire) {
                        on_close.run(true);
                    }
                }
                Err(e) => {
                    bus.push(
                        ToastKind::Error,
                        "Couldn't save",
                        Some(crate::api_error::saved_name_refusal(&e)),
                    );
                }
            }
        });
    });

    let cancel = Callback::new(move |()| on_close.run(false));
    let save_disabled = Signal::derive(move || {
        trawl_core::saved_name::normalize(&name.get()).is_err() || submitting.get()
    });

    view! {
        <Modal
            title="Save query as Net"
            icon=Icon::Pin
            on_cancel=cancel
            on_submit=do_save
            footer=Box::new(move || view! {
                <Btn variant=Variant::Secondary on_click=cancel>{move || if submitting.get() { "Close" } else { "Cancel" }}</Btn>
                <Btn variant=Variant::Primary disabled=save_disabled on_click=do_save>
                    {move || if submitting.get() { "Saving…" } else { "Save as Net" }}
                </Btn>
            }.into_any())
        >
            <div class="m-field">
                <label>"Query"</label>
                <div class="preview" title=query.clone()>{query.clone()}</div>
            </div>

            <Show when=move || from_editor>
                <p class="save-scope">"Save captures the editor query text shown above. It omits sidebar filters and the time range control."</p>
                <p class="save-scope">"To share the full browser search state, cancel and use Copy search URL beside the editor. Run any editor changes first."</p>
            </Show>
            <Field id="netName" label="Name" class="m-field">
                <input
                    id="netName"
                    node_ref=input_ref
                    prop:value=move || name.get()
                    on:input=move |e| name.set(event_target_value(&e))
                    aria-invalid=move || trawl_core::saved_name::normalize(&name.get()).is_err().to_string()
                    aria-describedby="netNameError"
                    placeholder="e.g. nginx 5xx by host"
                />
                <span id="netNameError" role="status">{move || trawl_core::saved_name::normalize(&name.get()).err().unwrap_or("")}</span>
            </Field>
        </Modal>
    }
}

fn suggest_name(q: &str) -> String {
    let search = q.split('|').next().unwrap_or("").trim();
    for token in tokenize(search) {
        if let Some((_field, value)) = token.split_once('=')
            && !value.is_empty()
        {
            let cleaned = value.trim_matches(|c: char| c == '"' || c == '\'');
            if let Ok(cleaned) = trawl_core::saved_name::normalize(cleaned) {
                return cleaned.to_string();
            }
        }
    }
    "untitled".to_string()
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote: Option<char> = None;
    for c in s.chars() {
        match (c, in_quote) {
            ('"' | '\'', None) => {
                in_quote = Some(c);
                cur.push(c);
            }
            (c, Some(q)) if c == q => {
                in_quote = None;
                cur.push(c);
            }
            (' ' | '\t' | '\n', None) => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (c, _) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
