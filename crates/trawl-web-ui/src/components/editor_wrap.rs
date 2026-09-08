// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<EditorWrap/>` — the DSL editor with the date-range picker, Run
//! button, and query tools stacked in a column on its right.
//!
//! The stack order is date range, Run, then the Save / Share / Format
//! tool row. There is no header band: the editor frame is its own label,
//! so the query box gets the full width. Run mirrors ⌘⏎ in the editor —
//! both call the parent's submit callback.

use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use leptos_use::{use_event_listener, use_window};
use wasm_bindgen::JsCast;

use crate::components::editor::DslEditor;
use crate::search_url::{Verdict, decode_range, encode_range, normalize_instant};
use crate::state::query::{QUICK_RANGES, RangeSpec};
use fleet_ui::{
    Btn, CopyButton, Icon, IconView, Kbd, Segmented, SegmentedOption, Size, ToastBus, ToastKind,
    Variant,
};

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
    /// Called with the new range spec on a preset pick or popover Apply.
    on_range_change: Callback<RangeSpec>,
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
    /// Bubbles "save" click to the parent so it can open the save modal.
    on_save: Callback<()>,
    /// Fired by the date-range popover's Real-time tab — the parent
    /// re-runs the current query in SSE live mode.
    on_live: Callback<()>,
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
        <div class="editor-wrap">
            <div class="editor-row">
                <DslEditor query=query on_submit=on_submit format_trigger=format_trigger/>
                <div class="editor-right">
                    <DateRange
                        value=range
                        on_change=on_range_change
                        on_live=on_live
                        blocked=blocked
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
                                <Kbd inline=true>"⌘⏎"</Kbd>
                            }.into_any()
                        }}
                    </button>
                    <div class="editor-tools">
                        // A real button, not a span, because a link that
                        // cannot be read has to be able to disable it:
                        // saving posts the same blanked query an export
                        // would (ADR-0027).
                        <button
                            type="button"
                            class="tool"
                            disabled=move || blocked.get()
                            on:click=move |_| on_save.run(())
                        >"Save"</button>
                        <CopyButton
                            class="tool"
                            text=share_text
                            success_detail="Search URL copied to clipboard."
                        >"Share"</CopyButton>
                        // Same reason as Save above, plus the plain one:
                        // Format acts on click, so it is a button and the
                        // keyboard reaches it (ADR-0028).
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

/// Date-range picker: a single trigger button (grafana-style) opening
/// the popover with Relative / Absolute / Real-time tabs. Its preset
/// grid is the only quick-range control; nothing else on the page picks
/// a range.
#[component]
fn DateRange(
    #[prop(into)] value: Signal<RangeSpec>,
    on_change: Callback<RangeSpec>,
    on_live: Callback<()>,
    /// True while the link cannot be read. The trigger still opens — the
    /// popover is also how the current range is READ — but every control
    /// inside it that would navigate is disabled.
    #[prop(into)]
    blocked: Signal<bool>,
) -> impl IntoView {
    let open = RwSignal::new(false);

    let trigger_label = move || match value.get() {
        RangeSpec::Quick(q) => format!("Last {q}"),
        abs @ RangeSpec::Absolute { .. } => abs.label(),
    };

    view! {
        <div class="daterange">
            // A native button, and the popover's focus restore depends on
            // it: the overlay hook captures document.activeElement as the
            // opener, and only a focusable element is the active one when
            // the click lands.
            <button
                type="button"
                class="dr-trigger"
                class:open=move || open.get()
                aria-haspopup="dialog"
                aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)
            >
                <IconView icon=Icon::Clock size=12 stroke_width=1.5/>
                <span>{trigger_label}</span>
                <IconView icon=Icon::Chevron size=10 stroke_width=1.5/>
            </button>
            <Show when=move || open.get()>
                <DateRangePopover
                    value=value
                    on_change=on_change
                    on_live=on_live
                    open=open
                    blocked=blocked
                />
            </Show>
        </div>
    }
}

/// Popover body: Relative (quick presets grid), Absolute (from/to inputs),
/// Real-time (Live Tail). Click-outside via a fullscreen scrim closes it.
#[component]
fn DateRangePopover(
    #[prop(into)] value: Signal<RangeSpec>,
    on_change: Callback<RangeSpec>,
    on_live: Callback<()>,
    open: RwSignal<bool>,
    /// True while the link cannot be read: presets, Apply and Live Tail
    /// all navigate, so all three are disabled and their handlers return
    /// before they emit anything (ADR-0027).
    #[prop(into)]
    blocked: Signal<bool>,
) -> impl IntoView {
    let tab = RwSignal::new(Tab::Relative);
    let scrim_ref = NodeRef::<Div>::new();
    let panel_ref = NodeRef::<Div>::new();

    // The picker is a modal dialog on fleet-ui's overlay stack, and one
    // causal chain runs through this registration:
    //
    // The hook reads `document.activeElement` at mount and keeps it as
    // the opener, then restores focus to it on unmount. Opening the
    // picker is a click on the trigger, so the opener is whatever the
    // trigger element happens to be — and only a native button is
    // focused by that click. The trigger was a `<div>`, which is not
    // focusable, so the captured opener would have been the editor (or
    // the body) and Escape would have dropped focus somewhere else
    // entirely. Focus restore works because the trigger is now a button.
    //
    // Trap rather than Capture: the scrim already blocks every pointer
    // event outside the panel, so letting Tab walk out into a page the
    // mouse cannot reach is the one combination that strands a keyboard
    // user (ADR-0029). Trap also earns the aria-modal="true" below, and
    // gives the panel its initial focus — the Segmented's first tab.
    let layer = fleet_ui::overlay::use_overlay_layer_with(
        fleet_ui::overlay::FocusPolicy::Trap,
        move || panel_ref.get().map(web_sys::Element::from),
    );
    // Escape at the window, not on the panel: the key has to work before
    // focus has moved anywhere. use_event_listener registers its own
    // on_cleanup, so the discarded handle is deliberate. The topmost
    // guard keeps a modal stacked over the picker from closing both.
    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if !layer.is_topmost() {
            return;
        }
        if e.key() == "Escape" {
            e.prevent_default();
            open.set(false);
        }
    });

    // Seed absolute input state from the current range if it's already
    // absolute; otherwise provide sensible defaults.
    let (initial_from, initial_to) = match value.get_untracked() {
        RangeSpec::Absolute { from, to } => (from, to),
        RangeSpec::Quick(_) => (String::new(), "now".to_string()),
    };
    let from = RwSignal::new(initial_from);
    let to = RwSignal::new(initial_to);
    // What Apply refused, shown under the inputs. The popover stays open
    // while it is set: an instant the app cannot normalize must not
    // reach the URL, where it would come back as an unreadable link
    // (ADR-0027).
    let error = RwSignal::new(None::<&'static str>);

    let close = move || open.set(false);

    let apply_quick = move |q: &'static str| {
        if blocked.get_untracked() {
            return;
        }
        on_change.run(RangeSpec::Quick(q));
        close();
    };

    let apply_absolute = Callback::new(move |()| {
        if blocked.get_untracked() {
            return;
        }
        let f = from.get();
        let t = to.get();
        if f.trim().is_empty() && t.trim().is_empty() {
            close();
            return;
        }
        // Both bounds are normalized to canonical UTC here, so what the
        // URL carries is what the reader accepts back: an offset or a
        // fraction is fine to type and never fine to store.
        let Some(from_utc) = normalize_instant(&f) else {
            error.set(Some(
                "From needs a timestamp like 2026-04-18T00:00:00Z (an offset is fine).",
            ));
            return;
        };
        let to_utc = if t.trim().is_empty() || t.trim() == "now" {
            "now".to_string()
        } else if let Some(normalized) = normalize_instant(&t) {
            normalized
        } else {
            error.set(Some(
                "To needs a timestamp like 2026-04-18T00:00:00Z, or the word now.",
            ));
            return;
        };
        let picked = RangeSpec::Absolute {
            from: from_utc,
            to: to_utc,
        };
        // The picker may only emit a range the URL reader accepts back,
        // so it asks the reader rather than restating its rules. Both
        // bounds are canonical by now, which leaves exactly one way to
        // fail: they are the wrong way round.
        if !matches!(decode_range(&encode_range(&picked)), Verdict::Valid(_)) {
            error.set(Some("To is before From."));
            return;
        }
        error.set(None);
        on_change.run(picked);
        close();
    });

    // Dismiss on mousedown, and only when the press landed on the scrim
    // element itself: an identity compare, never a class-name heuristic
    // (the shape fleet-ui's Modal uses). Gated on topmost so a dialog
    // stacked over the picker is not dismissed through it.
    let on_scrim_mousedown = move |e: web_sys::MouseEvent| {
        if !layer.is_topmost() {
            return;
        }
        let Some(scrim) = scrim_ref.get() else {
            return;
        };
        let Some(target) = e.target() else { return };
        let Some(el) = target.dyn_ref::<web_sys::Element>() else {
            return;
        };
        if el.is_same_node(Some(scrim.as_ref())) {
            // mousedown's default action is the browser's focus fix-up,
            // and it runs AFTER this handler: the scrim is not
            // focusable, so it would clear focus to <body> right after
            // the overlay hook put it back on the trigger. Suppressing
            // it is what makes a scrim dismissal end where Escape does.
            // Nothing else on a transparent full-screen scrim depends on
            // that default (no selection, no drag).
            e.prevent_default();
            close();
        }
    };

    view! {
        // Fullscreen scrim captures outside presses. Transparent — the
        // popover sits on top of it. A sibling BEFORE the panel, and
        // never the panel the focus resolver is given: only .dr-pop is
        // resolved, so the scrim stays out of the Tab cycle.
        <div
            class="scrim"
            node_ref=scrim_ref
            on:mousedown=on_scrim_mousedown
        />
        <div
            class="dr-pop"
            node_ref=panel_ref
            role="dialog"
            aria-modal="true"
            aria-label="Time range"
            tabindex="-1"
        >
            // The popover shell stays app-side; only the tab strip comes
            // from fleet-ui, joined to `Tab` by the id ↔ enum map below.
            <Segmented
                size=Size::Sm
                full=true
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
                        // Buttons rather than divs: a preset navigates,
                        // so an unreadable link has to be able to say so
                        // with the attribute browsers already honour.
                        {QUICK_RANGES.iter().copied().map(|q| {
                            let is_on = Signal::derive(move || matches!(value.get(), RangeSpec::Quick(cur) if cur == q));
                            view! {
                                <button
                                    type="button"
                                    class="opt"
                                    class:on=move || is_on.get()
                                    // The picked preset is a pressed
                                    // state, not a disabled one: the
                                    // grid is a set of toggles and the
                                    // .on class alone says nothing to a
                                    // screen reader.
                                    aria-pressed=move || is_on.get().to_string()
                                    disabled=move || blocked.get()
                                    on:click=move |_| apply_quick(q)
                                >{format!("Last {q}")}</button>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any(),
                Tab::Absolute => view! {
                    <div class="cust">
                        <div class="fld">
                            <label class="lb" for="dr-from-input">"From"</label>
                            <input
                                id="dr-from-input"
                                class="dr-from"
                                prop:value=move || from.get()
                                on:input=move |e| from.set(event_target_value(&e))
                                placeholder="2026-04-18T00:00:00Z"
                            />
                        </div>
                        <div class="fld">
                            <label class="lb" for="dr-to-input">"To"</label>
                            <input
                                id="dr-to-input"
                                class="dr-to"
                                prop:value=move || to.get()
                                on:input=move |e| to.set(event_target_value(&e))
                                placeholder="now"
                            />
                        </div>
                        <Show when=move || error.get().is_some()>
                            <div class="dr-err">{move || error.get().unwrap_or_default()}</div>
                        </Show>
                    </div>
                    <div class="foot">
                        <div class="btns">
                            <Btn
                                variant=Variant::Secondary
                                on_click=Callback::new(move |()| close())
                            >"Cancel"</Btn>
                            // fleet-ui's Btn takes no class prop, so the
                            // browser suite's hook rides a wrapper that
                            // is exactly the button's own box.
                            <span class="dr-apply">
                                <Btn
                                    variant=Variant::Primary
                                    disabled=blocked
                                    on_click=apply_absolute
                                >"Apply"</Btn>
                            </span>
                        </div>
                    </div>
                }.into_any(),
                Tab::RealTime => view! {
                    <div class="rt-hint">
                        <p>"Real-time mode streams events as they arrive."</p>
                        <Btn
                            variant=Variant::Primary
                            full=true
                            disabled=blocked
                            on_click=Callback::new(move |()| {
                                if blocked.get_untracked() {
                                    return;
                                }
                                on_live.run(());
                                close();
                            })
                        >"Live Tail"</Btn>
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
