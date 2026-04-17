// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<DslEditor/>` — `CodeMirror` 6 editor backed by the in-browser trawl-core
//! DSL parser.
//!
//! The parser is the SAME crate the server uses, compiled to wasm. Every
//! keystroke re-parses and produces `Diagnostic` entries that codemirror
//! renders as red squiggles. No network round-trip; zero schema drift
//! between server and browser possible.
//!
//! Autocomplete is fed from `trawl_core::parser::suggest::{KNOWN_PIPE_STAGES,
//! KNOWN_FUNCTIONS}` — compile-time constants shared with the server.

use leptos::prelude::*;
use serde::Serialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::interop::codemirror::{EditorHandle, create_editor};

/// JS-side `Diagnostic` shape expected by the codemirror bundle.
#[derive(Serialize)]
struct Diagnostic {
    from: usize,
    to: usize,
    severity: &'static str,
    message: String,
}

/// JS-side autocomplete entry.
#[derive(Serialize)]
struct Completion {
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'static str>,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct CompletionResult {
    from: usize,
    options: Vec<Completion>,
    filter: bool,
}

/// Emits `CodeMirror` diagnostics for every parse error from trawl-core.
fn lint_document(doc: &str) -> Vec<Diagnostic> {
    match trawl_core::parser::parse(doc) {
        Ok(_) => Vec::new(),
        Err(errors) => errors
            .into_iter()
            .map(|e| {
                let message = match e.hint {
                    Some(hint) => format!("{} — {}", e.message, hint),
                    None => e.message,
                };
                Diagnostic {
                    from: e.span.start,
                    to: e.span.end,
                    severity: "error",
                    message,
                }
            })
            .collect(),
    }
}

/// Autocomplete: offers pipe stages if the cursor is after `|` or at SOL;
/// otherwise offers function names. Good-enough v1 heuristic — full
/// context-aware completion is a later commit.
fn complete_at(doc: &str, pos: usize) -> Option<CompletionResult> {
    let prefix = &doc[..pos];
    let last_word_start = prefix
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(pos, |(i, _)| i);

    let word = &prefix[last_word_start..];
    if word.is_empty() {
        return None;
    }

    let after_pipe = prefix
        .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == ' ' || c == '\t')
        .ends_with('|');

    let options: Vec<Completion> = if after_pipe {
        trawl_core::parser::suggest::KNOWN_PIPE_STAGES
            .iter()
            .filter(|s| s.starts_with(word))
            .map(|s| Completion {
                label: (*s).to_string(),
                detail: Some("pipe stage"),
                kind: "keyword",
            })
            .collect()
    } else {
        trawl_core::parser::suggest::KNOWN_FUNCTIONS
            .iter()
            .filter(|s| s.starts_with(word))
            .map(|s| Completion {
                label: format!("{s}()"),
                detail: Some("function"),
                kind: "function",
            })
            .collect()
    };

    if options.is_empty() {
        return None;
    }

    Some(CompletionResult {
        from: last_word_start,
        options,
        filter: false,
    })
}

/// Convert a non-negative `f64` into `usize`, clamping at 0 on negatives
/// (shouldn't happen for DOM cursor positions but defensive).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn f64_to_usize(f: f64) -> usize {
    if f.is_sign_negative() || !f.is_finite() {
        0
    } else {
        f as usize
    }
}

/// Leptos component that mounts a `CodeMirror` editor into a div. The doc
/// text is pushed into `query` on every change; `on_submit` fires when
/// the user hits ⌘⏎ / ctrl+⏎.
#[component]
pub fn DslEditor(
    #[prop(into)] query: RwSignal<String>,
    #[prop(into)] on_submit: Callback<()>,
) -> impl IntoView {
    let node_ref = NodeRef::<leptos::html::Div>::new();
    let handle: StoredValue<Option<EditorHandle>> = StoredValue::new(None);

    Effect::new(move |_| {
        let Some(element) = node_ref.get() else {
            return;
        };
        let html_el: web_sys::HtmlElement = (*element).clone().unchecked_into();

        // Closures have to outlive the handle; we leak them intentionally
        // for the lifetime of the component (editor lives until navigated
        // away from). A more pedantic solution would store them in
        // StoredValue and invoke destroy() on effect cleanup.
        let on_change_cb = Closure::<dyn Fn(String)>::new(move |doc: String| {
            query.set(doc);
        });

        let on_submit_cb = Closure::<dyn Fn()>::new(move || {
            on_submit.run(());
        });

        let lint_cb = Closure::<dyn Fn(String) -> JsValue>::new(|doc: String| {
            let diags = lint_document(&doc);
            serde_wasm_bindgen::to_value(&diags).unwrap_or(JsValue::UNDEFINED)
        });

        let complete_cb = Closure::<dyn Fn(JsValue) -> JsValue>::new(|ctx: JsValue| {
            // The JS CompletionContext has `state.doc.toString()` and
            // `pos` — we mine both via Reflect. A failure falls back
            // to null (no completions), which is fine.
            let state = js_sys::Reflect::get(&ctx, &JsValue::from_str("state"))
                .unwrap_or(JsValue::UNDEFINED);
            let pos = js_sys::Reflect::get(&ctx, &JsValue::from_str("pos"))
                .ok()
                .and_then(|v| v.as_f64())
                .map_or(0usize, f64_to_usize);
            let doc_obj = js_sys::Reflect::get(&state, &JsValue::from_str("doc"))
                .unwrap_or(JsValue::UNDEFINED);
            let doc_str = js_sys::Reflect::get(&doc_obj, &JsValue::from_str("toString"))
                .ok()
                .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
                .and_then(|f| f.call0(&doc_obj).ok())
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            complete_at(&doc_str, pos).map_or(JsValue::NULL, |result| {
                serde_wasm_bindgen::to_value(&result).unwrap_or(JsValue::NULL)
            })
        });

        // Build the opts object by reflection — matches TS `EditorOpts` shape.
        let opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("onChange"),
            on_change_cb.as_ref().unchecked_ref(),
        );
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("onSubmit"),
            on_submit_cb.as_ref().unchecked_ref(),
        );
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("lint"),
            lint_cb.as_ref().unchecked_ref(),
        );
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("complete"),
            complete_cb.as_ref().unchecked_ref(),
        );

        // Intentionally leak the closures for the component lifetime.
        on_change_cb.forget();
        on_submit_cb.forget();
        lint_cb.forget();
        complete_cb.forget();

        let initial = query.get_untracked();
        let h = create_editor(&html_el, &initial, opts.into());
        handle.set_value(Some(h));
    });

    on_cleanup(move || {
        handle.update_value(|h| {
            if let Some(h) = h.take() {
                h.destroy();
            }
        });
    });

    view! {
        <div node_ref=node_ref class="dsl-editor"></div>
    }
}
