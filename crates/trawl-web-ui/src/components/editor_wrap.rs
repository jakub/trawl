// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<EditorWrap/>` — the query console's header, the DSL editor, and
//! the date-range picker, Haul button and query tools stacked in a
//! column on its right.
//!
//! The console header says what the box is ("Query") and whether the
//! buffer has been edited since the link ran. That draft state is
//! derived, never stored: it compares the editor buffer against the
//! executed query in the URL and navigates nothing (ADR-0027).
//!
//! The stack order is date range, Haul, then the Save as Net / Copy
//! search URL / Format tool row. Haul mirrors ⌘⏎ in the editor — both
//! call the parent's submit callback.

use leptos::prelude::*;
use leptos::web_sys;

use crate::components::editor::DslEditor;
use crate::state::query::{QUICK_RANGES, RangeSpec};
use fleet_ui::{CopyButton, RangeDialog, RangePreset, RangeValue, ToastBus, ToastKind};

#[component]
pub fn EditorWrap(
    /// Editor buffer — bound to the `DslEditor` document.
    query: RwSignal<String>,
    /// Triggered on ⌘⏎ from the editor and on Run button click.
    #[prop(into)]
    on_submit: Callback<()>,
    /// Currently selected range — read-only here; mutations flow out
    /// via `on_range_change` to the parent, which translates them into
    /// URL navigation.
    #[prop(into)]
    range: Signal<RangeSpec>,
    #[prop(into)] reset_key: Signal<String>,
    /// Accept or refuse a raw range selection from the shared dialog.
    on_range_change: Callback<RangeValue, Result<(), String>>,
    /// True while a query is in flight; flips Run → Hauling…
    #[prop(into)]
    running: Signal<bool>,
    /// True while the link in the address bar cannot be read. Every
    /// control here that would run the query or rewrite the URL is
    /// disabled, so the banner's repair stays the only way forward
    /// (ADR-0027). Typing is unaffected: the editor is a buffer, not a
    /// navigation.
    #[prop(into)]
    blocked: Signal<bool>,
    /// True while the editor buffer differs from the query the current
    /// link executed. Read-only here: the header states it, and only
    /// Haul resolves it.
    #[prop(into)]
    draft_dirty: Signal<bool>,
    /// Bubbles "save" click to the parent so it can open the save modal.
    on_save: Callback<()>,
    /// Fired by the date-range popover's Real-time tab — the parent
    /// re-runs the current query in SSE live mode.
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
        let text = query.get_untracked();
        if text.trim().is_empty() {
            return;
        }
        match trawl_core::format::reformat(&text) {
            None => bus.push(
                ToastKind::Error,
                "Can't format",
                Some("Query has parse errors.".into()),
            ),
            Some(formatted) if formatted == text => {
                bus.push(ToastKind::Info, "Format", Some("Already formatted.".into()));
            }
            Some(formatted) => {
                query.set(formatted);
                format_trigger.update(|v| *v += 1);
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
                <DslEditor query=query on_submit=on_submit format_trigger=format_trigger/>
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
                        // Format acts on click, so a native button makes
                        // it keyboard accessible, ADR-0028.
                        <button
                            type="button"
                            class="tool"
                            on:click=do_format
                        >"Format"</button>
                    </div>
                </div>
            </div>
        </div>
    }
}
