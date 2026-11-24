// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<EditorWrap/>` — the query console's header, the DSL editor, and
//! the date-range picker, Haul button and query tools stacked in a
//! column on its right.

use leptos::prelude::*;
use leptos::web_sys;

use crate::components::editor::DslEditor;
use crate::query_error::{DraftDiagnostic, DraftKind, draft_diagnostic, omitted_advice_note};
use crate::state::query::{QUICK_RANGES, RangeSpec};
use fleet_ui::{CopyButton, RangeDialog, RangePreset, RangeValue, ToastBus, ToastKind};

#[component]
pub fn EditorWrap(
    query: RwSignal<String>,
    #[prop(into)] on_submit: Callback<()>,
    #[prop(into)] range: Signal<RangeSpec>,
    #[prop(into)] reset_key: Signal<String>,
    on_range_change: Callback<RangeValue, Result<(), String>>,
    #[prop(into)] running: Signal<bool>,
    #[prop(into)] blocked: Signal<bool>,
    /// True while the editor buffer differs from the query the current
    /// link executed. Read-only here: the header states it, and only
    /// Haul resolves it.
    #[prop(into)]
    draft_dirty: Signal<bool>,
    on_save: Callback<()>,
    on_live: Callback<(), Result<(), String>>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let on_run = on_submit;
    let format_trigger = RwSignal::new(0_u64);

    // Current-URL text for the share <CopyButton> — derived so the
    // href resolves at click time, not at render time.
    let share_text = Signal::derive(move || {
        web_sys::window()
            .and_then(|w| w.location().href().ok())
            .unwrap_or_default()
    });

    let do_format = move |_| {
        // Every click is a Format request, whatever Format then does with
        // the text: the editor closes its completion popup on each one.
        // Bumped before `query` is set; the editor reads the signal in a
        // later effect run, after this handler has returned.
        format_trigger.update(|v| *v += 1);
        let text = query.get_untracked();
        if text.trim().is_empty() {
            return;
        }
        let Ok(parsed) = trawl_core::parser::parse(&text) else {
            bus.push(
                ToastKind::Error,
                "Can't format",
                Some("Query has parse errors.".into()),
            );
            return;
        };
        // A refusal (ADR-0061) never reaches `query`, so the document and
        // its CodeMirror undo history stay as they were.
        match trawl_core::format::checked_format_query(&parsed) {
            Err(refusal) => {
                bus.push(ToastKind::Error, "Can't format", Some(refusal.to_string()));
            }
            Ok(formatted) if formatted == text => {
                bus.push(ToastKind::Info, "Format", Some("Already formatted.".into()));
            }
            Ok(formatted) => {
                query.set(formatted);
                bus.push(ToastKind::Success, "Formatted", None);
            }
        }
    };

    view! {
        // Header and editor are siblings inside the console frame, so the
        // frame's own padding is not paid twice down the left edge.
        <div class="console-hd">
            <span class="console-lb">"Query"</span>
            <Show when=move || draft_dirty.get()>
                <span class="draft dirty">"Edited"</span>
            </Show>
        </div>
        <div class="editor-wrap" id="search-query" tabindex="-1">
            <div class="editor-row">
                <EditorField query=query on_submit=on_submit format_trigger=format_trigger/>
                <div class="editor-right">
                    <RangeDialog
                        value=Signal::derive(move || match range.get() {
                            RangeSpec::Quick(id) => RangeValue::Quick(id.to_string()),
                            RangeSpec::Absolute { from, to } => RangeValue::Absolute { from, to },
                        })
                        presets=QUICK_RANGES.iter().map(|id| RangePreset {
                            id: (*id).to_string(), label: format!("Last {id}"),
                        }).collect()
                        on_commit=on_range_change
                        on_live=on_live
                        live_label="Live Tail".to_string()
                        live_description="Real-time mode streams events as they arrive.".to_string()
                        reset_key=reset_key
                        disabled=blocked
                    />
                    <button
                        type="button"
                        class="run"
                        class:running=move || running.get()
                        disabled=move || running.get() || blocked.get()
                        on:click=move |_| on_run.run(())
                    >
                        {move || if running.get() {
                            view! { <span>"Hauling…"</span> }.into_any()
                        } else {
                            view! {
                                <span>"Haul"</span>
                            }.into_any()
                        }}
                    </button>
                    <div class="editor-tools">
                        // Malformed URL state disables Save even though it
                        // captures the editor buffer, ADR-0027.
                        <button
                            type="button"
                            class="tool"
                            disabled=move || blocked.get()
                            on:click=move |_| on_save.run(())
                        >"Save as Net"</button>
                        <CopyButton
                            class="tool"
                            text=share_text
                            success_detail="Search URL copied to clipboard."
                        >"Copy search URL"</CopyButton>
                        <button
                            type="button"
                            class="tool"
                            on:click=do_format
                        >"Format"</button>
                    </div>
                </div>
            </div>
            <DraftDiagnosticLine query=query/>
        </div>
    }
}

#[component]
fn EditorField(
    query: RwSignal<String>,
    on_submit: Callback<()>,
    format_trigger: RwSignal<u64>,
) -> impl IntoView {
    let ua = window().navigator().user_agent().unwrap_or_default();
    let chord = if fleet_ui::kbd::apple_keyboard(&ua) {
        "⌘⏎"
    } else {
        "Ctrl+Enter"
    };
    view! {
        <div class="editor-field">
            <DslEditor query=query on_submit=on_submit format_trigger=format_trigger/>
            <Show when=move || query.with(String::is_empty)>
                <span class="editor-hint" aria-hidden="true">
                    "Type a query, for example "
                    <code>"service=api _severity>=warn"</code>
                    ". "
                    {chord}
                    " hauls."
                </span>
            </Show>
        </div>
    }
}

fn located(first: String) -> AnyView {
    match first.split_once(" — ") {
        Some((at, message)) if at.starts_with("Line ") => {
            let (at, message) = (at.to_owned(), message.to_owned());
            view! { <span class="draft-diagnostic-at">{at}</span>" — "{message} }.into_any()
        }
        _ => first.into_any(),
    }
}

#[component]
fn DraftDiagnosticLine(query: RwSignal<String>) -> impl IntoView {
    let diagnostic = Memo::new(move |_| draft_diagnostic(&query.get()));
    // The disclosure answers for the draft it was opened on: any edit
    // closes it, even one that leaves the diagnostic text unchanged.
    let more = NodeRef::<leptos::html::Details>::new();
    Effect::new(move |_| {
        query.track();
        if let Some(more) = more.get_untracked() {
            // Removing an attribute from a live element cannot fail.
            let _ = more.remove_attribute("open");
        }
    });

    move || {
        diagnostic.get().map(|DraftDiagnostic { kind, first, rest, omitted }| {
            let advice = kind == DraftKind::Advice;
            view! {
                <div class="draft-diagnostic" class:draft-diagnostic-advice=advice>
                    <p class="draft-diagnostic-first">{located(first)}</p>
                    {(!rest.is_empty()).then(|| view! {
                        <details class="draft-diagnostic-more" node_ref=more>
                            <summary>{format!("+{} more", rest.len())}</summary>
                            <ul>
                                {rest.into_iter().map(|line| view! { <li>{line}</li> }).collect_view()}
                            </ul>
                        </details>
                    })}
                    {(omitted > 0).then(|| view! {
                        <p class="draft-diagnostic-omitted">{omitted_advice_note(omitted)}</p>
                    })}
                </div>
            }
        })
    }
}
