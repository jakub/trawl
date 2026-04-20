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
use leptos::web_sys;

use crate::components::editor::DslEditor;
use crate::state::query::{QUICK_RANGES, RangeSpec};

#[component]
pub fn EditorWrap(
    /// Editor buffer — bound to the DslEditor's textarea.
    query: RwSignal<String>,
    /// Triggered on ⌘⏎ from the editor and on Run button click.
    #[prop(into)]
    on_submit: Callback<()>,
    /// Currently selected range — read-only here; mutations flow out
    /// via `on_range_change` to the parent, which translates them into
    /// URL navigation.
    #[prop(into)]
    range: Signal<RangeSpec>,
    /// Called with the new range spec on quick-pill click or popover Apply.
    on_range_change: Callback<RangeSpec>,
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
                    <DateRange value=range on_change=on_range_change/>
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

/// Date-range picker: quick pills + custom popover with Relative /
/// Absolute / Real-time tabs.
///
/// The quick pills (5m … 7d) mirror the mockup's strip and directly
/// write `RangeSpec::Quick(label)` on click. The "Custom…" pill opens
/// a popover for absolute windows and the full preset grid.
#[component]
fn DateRange(
    #[prop(into)] value: Signal<RangeSpec>,
    on_change: Callback<RangeSpec>,
) -> impl IntoView {
    let open = RwSignal::new(false);

    view! {
        <div class="daterange">
            <div class="quick">
                {QUICK_RANGES.iter().copied().map(|r| {
                    let is_on = Signal::derive(move || matches!(value.get(), RangeSpec::Quick(q) if q == r));
                    view! {
                        <div
                            class="q"
                            class:on=move || is_on.get()
                            on:click=move |_| on_change.run(RangeSpec::Quick(r))
                        >{r}</div>
                    }
                }).collect::<Vec<_>>()}
            </div>
            <div class="custom" on:click=move |_| open.update(|o| *o = !*o)>
                <CalendarIcon/>
                <span>{move || value.get().label()}</span>
                <ChevIcon/>
            </div>
            <Show when=move || open.get()>
                <DateRangePopover value=value on_change=on_change open=open/>
            </Show>
        </div>
    }
}

/// Popover body: Relative (quick presets grid), Absolute (from/to inputs),
/// Real-time (stub). Click-outside via a fullscreen scrim closes it.
#[component]
fn DateRangePopover(
    #[prop(into)] value: Signal<RangeSpec>,
    on_change: Callback<RangeSpec>,
    open: RwSignal<bool>,
) -> impl IntoView {
    let tab = RwSignal::new(Tab::Relative);

    // Seed absolute input state from the current range if it's already
    // absolute; otherwise provide sensible defaults.
    let (initial_from, initial_to) = match value.get_untracked() {
        RangeSpec::Absolute { from, to } => (from, to),
        RangeSpec::Quick(_) => (String::new(), "now".to_string()),
    };
    let from = RwSignal::new(initial_from);
    let to = RwSignal::new(initial_to);

    let close = move || open.set(false);

    let apply_quick = move |q: &'static str| {
        on_change.run(RangeSpec::Quick(q));
        close();
    };

    let apply_absolute = move |_| {
        let f = from.get();
        let t = to.get();
        if f.trim().is_empty() && t.trim().is_empty() {
            close();
            return;
        }
        on_change.run(RangeSpec::Absolute { from: f, to: t });
        close();
    };

    view! {
        // Fullscreen scrim captures outside clicks. Transparent — the
        // popover sits on top of it.
        <div
            class="scrim"
            on:click=move |_| close()
        />
        <div class="dr-pop" on:click=|e: web_sys::MouseEvent| e.stop_propagation()>
            <div class="tabs">
                <div
                    class="t"
                    class:on=move || tab.get() == Tab::Relative
                    on:click=move |_| tab.set(Tab::Relative)
                >"Relative"</div>
                <div
                    class="t"
                    class:on=move || tab.get() == Tab::Absolute
                    on:click=move |_| tab.set(Tab::Absolute)
                >"Absolute"</div>
                <div
                    class="t"
                    class:on=move || tab.get() == Tab::RealTime
                    on:click=move |_| tab.set(Tab::RealTime)
                >"Real-time"</div>
            </div>
            {move || match tab.get() {
                Tab::Relative => view! {
                    <div class="grid">
                        {QUICK_RANGES.iter().copied().map(|q| {
                            let is_on = Signal::derive(move || matches!(value.get(), RangeSpec::Quick(cur) if cur == q));
                            view! {
                                <div
                                    class="opt"
                                    class:on=move || is_on.get()
                                    on:click=move |_| apply_quick(q)
                                >
                                    <span>{format!("last {q}")}</span>
                                    <span class="dim">{q}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any(),
                Tab::Absolute => view! {
                    <div class="cust">
                        <div class="fld">
                            <div class="lb">"From"</div>
                            <input
                                prop:value=move || from.get()
                                on:input=move |e| from.set(event_target_value(&e))
                                placeholder="2026-04-18T00:00:00Z"
                            />
                        </div>
                        <div class="fld">
                            <div class="lb">"To"</div>
                            <input
                                prop:value=move || to.get()
                                on:input=move |e| to.set(event_target_value(&e))
                                placeholder="now"
                            />
                        </div>
                    </div>
                    <div class="foot">
                        <div class="summary">"bucket: " <span class="amber">"auto · 1m"</span></div>
                        <div class="btns">
                            <button
                                class="btn-sec"
                                on:click=move |_| close()
                            >"Cancel"</button>
                            <button
                                class="btn-pri"
                                on:click=apply_absolute
                            >"Apply"</button>
                        </div>
                    </div>
                }.into_any(),
                Tab::RealTime => view! {
                    <div class="rt-hint">
                        <p>"Real-time mode streams events as they arrive."</p>
                        <p class="dim">"Use the "<code>"?mode=live"</code>" URL to enable live-tail — the popover toggle lands in a follow-up."</p>
                    </div>
                }.into_any(),
            }}
        </div>
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Relative,
    Absolute,
    RealTime,
}

#[component]
fn CalendarIcon() -> impl IntoView {
    view! {
        <svg width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <rect x="2" y="3" width="12" height="11" rx="1"/>
            <path d="M2 6h12M5 1.5v3M11 1.5v3"/>
        </svg>
    }
}

#[component]
fn ChevIcon() -> impl IntoView {
    view! {
        <svg width="10" height="10" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="m4 6 4 4 4-4"/>
        </svg>
    }
}
