// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<SaveAsNetModal/>` — confirm-save dialog for turning a DSL query
//! into a named saved query ("net").
//!
//! Single-field modal for now: just the display name. The mockup
//! (`web_ui_mockups/app/pages-schema-history.jsx:67-192`) sketches a
//! richer form with description / folder / tags / pin-to-sidebar /
//! schedule-after-save, but the backend's `CreateSavedRequest` only
//! carries `{ name, query }` — wiring the extra fields would mean
//! growing the API first. Skeleton is structured so those fields can
//! drop in next to `<NameField/>` when that work lands.
//!
//! Behaviour:
//! - Scrim click closes.
//! - `Esc` closes. `Cmd/Ctrl + Enter` saves (if name non-empty).
//! - Name input is autofocused + pre-selected on mount.
//! - Default name derived from the query (first field=value token, or
//!   "untitled").

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys;
use wasm_bindgen::JsCast;

use crate::api;
use crate::components::toast::{ToastBus, ToastKind};

#[component]
#[allow(clippy::too_many_lines)] // single-component dialog tree, not worth splitting further
#[allow(clippy::needless_pass_by_value)] // Leptos component props: easier to pass owned
pub fn SaveAsNetModal(
    /// The DSL query to save. Shown read-only in the preview strip.
    query: String,
    /// Bus for success / error toasts. Cloned in.
    bus: ToastBus,
    /// Called after a successful save OR cancel — parent should clear
    /// whatever made the modal open. The bool is `true` on save, `false`
    /// on cancel; the parent can use that to decide whether to also
    /// clear the row selection, etc.
    on_close: Callback<bool>,
) -> impl IntoView {
    let suggested = suggest_name(&query);
    let name = RwSignal::new(suggested);
    let submitting = RwSignal::new(false);

    let input_ref = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        // First render: focus + select the suggested name so the user
        // can just type to overwrite it.
        if let Some(el) = input_ref.get() {
            let input: web_sys::HtmlInputElement = (*el).clone().unchecked_into();
            let _ = input.focus();
            input.select();
        }
    });

    let q_for_submit = query.clone();
    let do_save = move || {
        let n = name.get_untracked().trim().to_string();
        if n.is_empty() || submitting.get_untracked() {
            return;
        }
        submitting.set(true);
        let q = q_for_submit.clone();
        spawn_local(async move {
            let result = api::create_saved(&n, &q).await;
            submitting.set(false);
            match result {
                Ok(saved) => {
                    bus.push(
                        ToastKind::Success,
                        "Saved as net",
                        Some(format!("'{}' cast. You can haul it anytime.", saved.name)),
                    );
                    on_close.run(true);
                }
                Err(e) => {
                    bus.push(ToastKind::Error, "Couldn't save", Some(e.to_string()));
                    // Leave the modal open so the user can retry / tweak.
                }
            }
        });
    };
    let do_save_click = do_save.clone();
    let do_save_key = do_save.clone();

    let cancel = move || on_close.run(false);

    let on_keydown = move |e: web_sys::KeyboardEvent| match e.key().as_str() {
        "Escape" => {
            e.prevent_default();
            cancel();
        }
        "Enter" if e.meta_key() || e.ctrl_key() => {
            e.prevent_default();
            do_save_key();
        }
        _ => {}
    };

    view! {
        <div
            class="modal-scrim"
            on:mousedown=move |e: web_sys::MouseEvent| {
                // Only close when the click target is the scrim itself,
                // not a descendant — matches the mockup's behaviour and
                // keeps drag-selecting text inside the modal from closing it.
                if let Some(target) = e.target()
                    && let Some(el) = target.dyn_ref::<web_sys::Element>()
                    && el.class_name().contains("modal-scrim")
                {
                    cancel();
                }
            }
            on:keydown=on_keydown
        >
            <div class="modal" role="dialog" aria-modal="true">
                <div class="m-hd">
                    <span class="ic"><PinIcon/></span>
                    <span class="t">"Save query as net"</span>
                    <span class="x" title="Close (Esc)" on:click=move |_| cancel()>
                        <CloseIcon/>
                    </span>
                </div>

                <div class="m-body">
                    <div class="m-field">
                        <label>"Query"</label>
                        <div class="preview" title=query.clone()>{query.clone()}</div>
                    </div>

                    <div class="m-field">
                        <label for="netName">"Name"</label>
                        <input
                            id="netName"
                            node_ref=input_ref
                            prop:value=move || name.get()
                            on:input=move |e| name.set(event_target_value(&e))
                            placeholder="e.g. nginx 5xx by host"
                        />
                    </div>
                </div>

                <div class="m-ft">
                    <div class="hint">
                        <span class="kbd">"⏎"</span>
                        " save"
                        <span style="opacity:.5">"·"</span>
                        <span class="kbd">"Esc"</span>
                        " cancel"
                    </div>
                    <button class="btn-sec" on:click=move |_| cancel()>"Cancel"</button>
                    <button
                        class="btn-pri"
                        disabled=move || name.get().trim().is_empty() || submitting.get()
                        on:click=move |_| do_save_click()
                    >
                        {move || if submitting.get() { "Saving…" } else { "Save net" }}
                    </button>
                </div>
            </div>
        </div>
    }
}

/// Suggest a display name from a DSL query: pick the first bare
/// `field=value` pair we can find. Falls back to `"untitled"` if the
/// query is just a free-text search or we can't spot a field clause.
fn suggest_name(q: &str) -> String {
    // Strip pipeline stages — only look at the search stage.
    let search = q.split('|').next().unwrap_or("").trim();
    for token in tokenize(search) {
        if let Some((_field, value)) = token.split_once('=')
            && !value.is_empty()
        {
            let cleaned = value.trim_matches(|c: char| c == '"' || c == '\'');
            if !cleaned.is_empty() {
                return cleaned.to_string();
            }
        }
    }
    "untitled".to_string()
}

/// Tokenize a search stage respecting quoted strings, so `host="a b"`
/// yields `host="a b"` rather than two tokens. Good enough for name
/// suggestion — not a real DSL parser.
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

#[component]
fn PinIcon() -> impl IntoView {
    view! {
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="M8 1.5v4M5 5.5h6l-1 4H6zM8 9.5v5"/>
        </svg>
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
