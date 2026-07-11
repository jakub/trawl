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
use leptos::task::spawn_local;
use leptos::web_sys;

use crate::clipboard::write_clipboard;
use crate::components::editor::DslEditor;
use crate::state::query::{QUICK_RANGES, RangeSpec};
use fleet_ui::{
    Btn, Icon, IconView, Kbd, Segmented, SegmentedOption, Size, ToastBus, ToastKind, Variant,
};

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
    /// Bubbles "save" click to the parent so it can open the save modal.
    on_save: Callback<()>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let on_run = on_submit;
    let format_trigger = RwSignal::new(0_u64);

    let do_share = move |_| {
        let Some(win) = web_sys::window() else { return };
        let Ok(href) = win.location().href() else {
            return;
        };
        spawn_local(async move {
            match write_clipboard(&href).await {
                Ok(()) => bus.push(
                    ToastKind::Success,
                    "Copied",
                    Some("Search URL copied to clipboard.".into()),
                ),
                Err(e) => bus.push(ToastKind::Error, "Copy failed", Some(e)),
            }
        });
    };

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
        <div class="editor-wrap">
            <div class="editor-hd">
                <span class="anch">"❯"</span>
                <span class="title">"Query"</span>
                <span class="sp"></span>
                <span
                    class="tool"
                    on:click=move |_| on_save.run(())
                >"save"</span>
                <span
                    class="tool"
                    on:click=do_share
                >"share"</span>
                <span
                    class="tool"
                    on:click=do_format
                >"format"</span>
            </div>
            <div class="editor-row">
                <DslEditor query=query on_submit=on_submit format_trigger=format_trigger/>
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
                                <span>"Haul"</span>
                                <Kbd inline=true>"⌘⏎"</Kbd>
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
                <IconView icon=Icon::Calendar size=11 stroke_width=1.5/>
                <span>{move || value.get().label()}</span>
                <IconView icon=Icon::Chevron size=10 stroke_width=1.5/>
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

    let apply_absolute = Callback::new(move |()| {
        let f = from.get();
        let t = to.get();
        if f.trim().is_empty() && t.trim().is_empty() {
            close();
            return;
        }
        on_change.run(RangeSpec::Absolute { from: f, to: t });
        close();
    });

    view! {
        // Fullscreen scrim captures outside clicks. Transparent — the
        // popover sits on top of it.
        <div
            class="scrim"
            on:click=move |_| close()
        />
        <div class="dr-pop" on:click=|e: web_sys::MouseEvent| e.stop_propagation()>
            // Popover shell stays app-side; the tab strip composes the
            // fleet Segmented (issue #31) with a two-line id ↔ enum map.
            <Segmented
                size=Size::Sm
                options=vec![
                    SegmentedOption::new("relative", "Relative"),
                    SegmentedOption::new("absolute", "Absolute"),
                    SegmentedOption::new("realtime", "Real-time"),
                ]
                active=Signal::derive(move || tab.get().id().to_string())
                on_change=Callback::new(move |id: String| tab.set(Tab::from_id(&id)))
            />
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
                            <Btn
                                variant=Variant::Secondary
                                on_click=Callback::new(move |()| close())
                            >"Cancel"</Btn>
                            <Btn variant=Variant::Primary on_click=apply_absolute>"Apply"</Btn>
                        </div>
                    </div>
                }.into_any(),
                Tab::RealTime => view! {
                    <div class="rt-hint">
                        <p>"Real-time mode streams events as they arrive."</p>
                        <p class="dim">"Append "<code>"?mode=live"</code>" to the URL to start streaming."</p>
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

impl Tab {
    fn id(self) -> &'static str {
        match self {
            Self::Relative => "relative",
            Self::Absolute => "absolute",
            Self::RealTime => "realtime",
        }
    }

    fn from_id(id: &str) -> Self {
        match id {
            "absolute" => Self::Absolute,
            "realtime" => Self::RealTime,
            _ => Self::Relative,
        }
    }
}
