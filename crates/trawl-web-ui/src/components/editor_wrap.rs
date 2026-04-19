// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<EditorWrap/>` — header (anchor + title + tools) wrapped around the
//! DSL editor, with the date-range picker and Run button on the right.
//!
//! The right column is a stack: date range above, Run button below.
//! Run mirrors ⌘⏎ in the editor — both call the parent's submit
//! callback.

use leptos::prelude::*;

use crate::components::editor::DslEditor;

/// Range options shown as quick buttons. Visual-only in v1 — does not
/// inject `last=X` into the query yet (user still writes time clauses
/// inline like `last=15m`).
const RANGES: &[&str] = &["5m", "15m", "1h", "4h", "24h", "7d"];

#[component]
pub fn EditorWrap(
    /// Editor buffer — bound to the DslEditor's textarea.
    query: RwSignal<String>,
    /// Triggered on ⌘⏎ from the editor and on Run button click.
    #[prop(into)]
    on_submit: Callback<()>,
    /// Currently selected range (purely visual for v1).
    range: RwSignal<&'static str>,
    /// True while a query is in flight; flips Run → Hauling…
    #[prop(into)]
    running: Signal<bool>,
    /// Toast push for the visual-only tools (save/share/format/syntax).
    #[prop(into)]
    on_toast: Callback<(&'static str, &'static str)>,
) -> impl IntoView {
    let on_run = on_submit;
    let toast_save = on_toast;
    let toast_share = on_toast;
    let toast_format = on_toast;
    let toast_syntax = on_toast;

    view! {
        <div class="editor-wrap">
            <div class="editor-hd">
                <span class="anch">"❯"</span>
                <span class="title">"Query"</span>
                <span class="dim">"·"</span>
                <span class="dim">"pipe DSL"</span>
                <span class="sp"></span>
                <span
                    class="tool"
                    on:click=move |_| toast_save.run(("Save", "Saving nets is coming soon."))
                >"save"</span>
                <span
                    class="tool"
                    on:click=move |_| toast_share.run(("Share", "Sharing search URLs is coming soon."))
                >"share"</span>
                <span
                    class="tool"
                    on:click=move |_| toast_format.run(("Format", "Auto-format is coming soon."))
                >"format"</span>
                <span
                    class="tool"
                    on:click=move |_| toast_syntax.run(("Syntax", "Syntax help is coming soon."))
                >"syntax"</span>
            </div>
            <div class="editor-row">
                <DslEditor query=query on_submit=on_submit/>
                <div class="editor-right">
                    <DateRange value=range/>
                    <button
                        class="run"
                        class:running=move || running.get()
                        disabled=move || running.get()
                        on:click=move |_| on_run.run(())
                    >
                        {move || if running.get() {
                            view! { <span>"Hauling…"</span> }.into_any()
                        } else {
                            view! {
                                <span>"Run search"</span>
                                <span class="kbd-inline">"⌘⏎"</span>
                            }.into_any()
                        }}
                    </button>
                </div>
            </div>
        </div>
    }
}

#[component]
fn DateRange(value: RwSignal<&'static str>) -> impl IntoView {
    view! {
        <div class="daterange">
            <div class="quick">
                {RANGES.iter().copied().map(|r| view! {
                    <div
                        class="q"
                        class:on=move || value.get() == r
                        on:click=move |_| value.set(r)
                    >{r}</div>
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
}
