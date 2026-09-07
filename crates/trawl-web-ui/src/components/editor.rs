// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<DslEditor/>` — `CodeMirror` 6 editor backed by the in-browser trawl-core
//! DSL parser.
//!
//! The parser is the same crate the server uses, compiled to wasm. Every
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
use crate::offset::{utf8_to_utf16, utf16_to_utf8};

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
///
/// `doc` is the current document text. Parser spans are UTF-8 byte
/// offsets; `CodeMirror` expects UTF-16 code-unit offsets — we translate
/// so squiggles line up under the correct characters for non-ASCII
/// input.
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
                    from: utf8_to_utf16(doc, e.span.start),
                    to: utf8_to_utf16(doc, e.span.end),
                    severity: "error",
                    message,
                }
            })
            .collect(),
    }
}

/// Autocomplete: offers pipe stages when the word being typed follows a
/// `|`, otherwise function names. A heuristic, not a context-aware
/// completion over the parse tree.
///
/// `pos_utf16` is the cursor position in UTF-16 code units (as `CodeMirror`
/// reports). We translate to a UTF-8 byte offset before slicing.
fn complete_at(doc: &str, pos_utf16: usize) -> Option<CompletionResult> {
    let pos_utf8 = utf16_to_utf8(doc, pos_utf16);
    // `utf16_to_utf8` is supposed to land on a char boundary, but if
    // CodeMirror ever hands us a cursor that sits between two UTF-16
    // surrogate units we'd slice mid-codepoint and panic across the
    // wasm boundary. `.get()` returns None on a non-boundary slice,
    // and the `?` bails out of completion entirely — an acceptable
    // "no completions" fallback for the niche case.
    let prefix = doc.get(..pos_utf8)?;
    let last_word_start_utf8 = prefix
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(pos_utf8, |(i, _)| i);

    let word = &prefix[last_word_start_utf8..];
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

    // `CompletionResult::from` must be in UTF-16 code units.
    Some(CompletionResult {
        from: utf8_to_utf16(doc, last_word_start_utf8),
        options,
        filter: false,
    })
}

/// Convert an `f64` into `usize`, clamping negatives and non-finite
/// values to 0 (neither should reach here from a DOM cursor position).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn f64_to_usize(f: f64) -> usize {
    if f.is_sign_negative() || !f.is_finite() {
        0
    } else {
        f as usize
    }
}

/// Bundle of everything that must outlive the editor mount.
///
/// `EditorHandle` is the `CodeMirror` view; this struct's `Drop` calls
/// `destroy` on it. The four closures are installed into that JS view as
/// callbacks, so they need to stay alive at least as long as the view.
/// Keeping them in one struct drops them together on component cleanup,
/// in the right order: view first, then closures.
///
/// Nothing here is `Closure::forget()`-ed: a parent re-render re-mounts
/// this component, and forgotten closures would pile up unreferenceable
/// JS functions in the GC roots.
struct EditorLifecycle {
    handle: EditorHandle,
    // The `dyn Fn` types differ per closure, so a Vec is impossible.
    // Field order is drop order: the handle goes first, so CodeMirror
    // has stopped firing callbacks before the closures are released.
    _on_change: Closure<dyn Fn(String)>,
    _on_submit: Closure<dyn Fn()>,
    _lint: Closure<dyn Fn(String) -> JsValue>,
    _complete: Closure<dyn Fn(JsValue) -> JsValue>,
}

impl Drop for EditorLifecycle {
    fn drop(&mut self) {
        // Destroy the view before the field drops release the closures:
        // CodeMirror's destroy() removes the view and tears down the
        // listeners that reference those callbacks, so none can fire on
        // a dangling closure.
        self.handle.destroy();
    }
}

/// Make the document follow `query` when something OTHER than typing
/// moves it — a Back navigation rewriting `?q=`, a saved query being
/// opened. Without this the editor kept whatever was mounted into it and
/// showed a query that was not the one on screen (ADR-0027's
/// Back/Forward contract).
///
/// A keystroke never round-trips: `on_change` records the document
/// before it sets the signal, so `push` finds them equal and does
/// nothing. The format trigger is now a second route to the same push —
/// the parent's contract is to set `query` and then bump the counter,
/// and the first half alone is enough.
fn follow_query_signal(
    query: RwSignal<String>,
    format_trigger: Option<RwSignal<u64>>,
    push: impl Fn(&str) + Copy + 'static,
) {
    Effect::new(move |_| {
        let text = query.get();
        push(&text);
    });

    if let Some(fmt) = format_trigger {
        Effect::new(move |_| {
            if fmt.get() == 0 {
                return;
            }
            push(&query.get_untracked());
        });
    }
}

/// Leptos component that mounts a `CodeMirror` editor into a div. The doc
/// text is pushed into `query` on every change; `on_submit` fires when
/// the user hits ⌘⏎ / ctrl+⏎.
#[component]
pub fn DslEditor(
    #[prop(into)] query: RwSignal<String>,
    #[prop(into)] on_submit: Callback<()>,
    /// Counter signal for external format requests. Parent sets `query`
    /// to the formatted text and then increments this; the Effect pushes
    /// the new value into CodeMirror via `set_doc`.
    #[prop(optional, into)]
    format_trigger: Option<RwSignal<u64>>,
) -> impl IntoView {
    let node_ref = NodeRef::<leptos::html::Div>::new();
    // `StoredValue::new_local`, not `::new`: `Closure<dyn Fn...>` is
    // neither `Send` nor `Sync`, so the default `SyncStorage` rejects
    // it. A CSR app runs on the main JS thread, so the single-threaded
    // `LocalStorage` variant is the right one.
    let lifecycle: StoredValue<Option<EditorLifecycle>, leptos::prelude::LocalStorage> =
        StoredValue::new_local(None);
    // What CodeMirror's document currently holds, as last seen by this
    // component. The editor is the source of truth while the user types
    // and the follower when the signal moves under it, and this is how
    // the two are told apart without asking JS for the document.
    let editor_doc: StoredValue<String, leptos::prelude::LocalStorage> =
        StoredValue::new_local(String::new());
    // Guarded: a push that would not change the document is skipped, so
    // the effects below can be unconditional.
    let push_doc = move |text: &str| {
        if editor_doc.with_value(|doc| doc == text) {
            return;
        }
        let pushed = lifecycle.with_value(|slot| {
            slot.as_ref().map(|lc| {
                lc.handle.set_doc(text);
            })
        });
        if pushed.is_some() {
            editor_doc.set_value(text.to_string());
        }
    };

    Effect::new(move |_| {
        let Some(element) = node_ref.get() else {
            return;
        };
        let html_el: web_sys::HtmlElement = (*element).clone().unchecked_into();

        let on_change_cb = Closure::<dyn Fn(String)>::new(move |doc: String| {
            editor_doc.set_value(doc.clone());
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
        // `as_ref().unchecked_ref()` reads the closure's JS function
        // *without* consuming it; the closure is then moved into
        // `EditorLifecycle` below to keep it alive.
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

        let initial = query.get_untracked();
        editor_doc.set_value(initial.clone());
        let handle = create_editor(&html_el, &initial, opts.into());

        lifecycle.set_value(Some(EditorLifecycle {
            handle,
            _on_change: on_change_cb,
            _on_submit: on_submit_cb,
            _lint: lint_cb,
            _complete: complete_cb,
        }));
    });

    follow_query_signal(query, format_trigger, push_doc);

    on_cleanup(move || {
        // Dropping the `EditorLifecycle` is what tears the view down.
        lifecycle.update_value(|v| {
            let _ = v.take();
        });
    });

    view! {
        <div node_ref=node_ref class="dsl-editor"></div>
    }
}
