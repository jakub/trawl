// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The whole range control, including its trigger and temporary dialog draft.
//! Apps supply presets and accept or refuse each complete transition.

/// A preset id or raw absolute bounds. The app validates their meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeValue {
    Quick(String),
    Absolute { from: String, to: String },
}

/// One app-owned preset. Fleet-ui has no default list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangePreset {
    pub id: String,
    pub label: String,
}

#[cfg(any(target_arch = "wasm32", test))]
fn input_ids() -> (String, String) {
    thread_local! { static NEXT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
    let id = NEXT_ID.with(|next| {
        let id = next
            .get()
            .checked_add(1)
            .expect("range input ids exhausted");
        next.set(id);
        id
    });
    (format!("dr-from-input-{id}"), format!("dr-to-input-{id}"))
}

#[cfg(target_arch = "wasm32")]
mod component {
    use super::{RangePreset, RangeValue, input_ids};
    use crate::{Btn, Icon, IconView, Segmented, SegmentedOption, Size, Variant};
    use leptos::{ev, html::Div, prelude::*, web_sys};
    use leptos_use::{use_event_listener, use_window};
    use wasm_bindgen::JsCast;

    /// A trigger and range-selection dialog. Apps supply the preset labels
    /// and accept or refuse raw selections; live mode is optional.
    #[component]
    pub fn RangeDialog(
        #[prop(into)] value: Signal<RangeValue>,
        presets: Vec<RangePreset>,
        #[prop(into)] reset_key: Signal<String>,
        #[prop(optional)] live_label: String,
        #[prop(optional)] live_description: String,
        on_commit: Callback<RangeValue, Result<(), String>>,
        #[prop(into, optional)] on_live: Option<Callback<(), Result<(), String>>>,
        /// True while the link cannot be read. The trigger still opens — the
        /// popover is also how the current range is READ — but every control
        /// inside it that would navigate is disabled.
        #[prop(into)]
        disabled: Signal<bool>,
    ) -> impl IntoView {
        let open = RwSignal::new(false);
        let presets = StoredValue::new(presets);
        let live_label = StoredValue::new(live_label);
        let live_description = StoredValue::new(live_description);
        let ids = StoredValue::new(input_ids());
        // Only the opaque caller identity triggers this effect. Opening the
        // dialog does not, and a changed URL discards even an equal range.
        Effect::new(move |previous: Option<String>| {
            let current = reset_key.get();
            if previous.is_some_and(|previous| previous != current) {
                open.set(false);
            }
            current
        });

        let trigger_label = move || match value.get() {
            RangeValue::Quick(id) => presets.with_value(|presets| {
                presets
                    .iter()
                    .find(|preset| preset.id == id)
                    .map_or(id.clone(), |preset| preset.label.clone())
            }),
            RangeValue::Absolute { from, to } => format!("{from} → {to}"),
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
                    <RangePanel
                        value=value
                        presets=presets.get_value()
                        ids=ids.get_value()
                        live_label=live_label.get_value()
                        live_description=live_description.get_value()
                        on_commit=on_commit
                        on_live=on_live
                        open=open
                        disabled=disabled
                    />
                </Show>
            </div>
        }
    }

    /// Popover body: Relative (quick presets grid), Absolute (from/to inputs),
    /// Real-time (Live Tail). Click-outside via a fullscreen scrim closes it.
    #[component]
    fn RangePanel(
        #[prop(into)] value: Signal<RangeValue>,
        presets: Vec<RangePreset>,
        ids: (String, String),
        live_label: String,
        live_description: String,
        on_commit: Callback<RangeValue, Result<(), String>>,
        on_live: Option<Callback<(), Result<(), String>>>,
        open: RwSignal<bool>,
        /// True while the link cannot be read: presets, Apply and Live Tail
        /// all navigate, so all three are disabled and their handlers return
        /// before they emit anything (ADR-0027).
        #[prop(into)]
        disabled: Signal<bool>,
    ) -> impl IntoView {
        let live_label = StoredValue::new(live_label);
        let live_description = StoredValue::new(live_description);
        let tab = RwSignal::new(Tab::Relative);
        let presets = StoredValue::new(presets);
        let ids = StoredValue::new(ids);
        let mut tabs = vec![
            SegmentedOption::new("relative", "Relative"),
            SegmentedOption::new("absolute", "Absolute"),
        ];
        if on_live.is_some() {
            tabs.push(SegmentedOption::new("realtime", "Real-time"));
        }
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
        let layer =
            crate::overlay::use_overlay_layer_with(crate::overlay::FocusPolicy::Trap, move || {
                panel_ref.get().map(web_sys::Element::from)
            });
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
            RangeValue::Absolute { from, to } => (from, to),
            RangeValue::Quick(_) => (String::new(), "now".to_string()),
        };
        let from = RwSignal::new(initial_from);
        let to = RwSignal::new(initial_to);
        // One refusal line outside the tab bodies, so range and live errors
        // remain visible while the user reviews or edits the draft.
        let error = RwSignal::new(None::<String>);

        let close = move || open.set(false);

        let commit = Callback::new(move |picked: RangeValue| {
            if disabled.get_untracked() {
                return;
            }
            match on_commit.run(picked) {
                Ok(()) => close(),
                Err(message) => error.set(Some(message)),
            }
        });
        let apply_absolute = Callback::new(move |()| {
            if disabled.get_untracked() {
                return;
            }
            let f = from.get();
            let t = to.get();
            if f.trim().is_empty() && t.trim().is_empty() {
                close();
                return;
            }
            commit.run(RangeValue::Absolute { from: f, to: t });
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
                // The optional callback decides whether the third tab exists.
                <Segmented
                    size=Size::Sm
                    full=true
                    options=tabs
                    active=Signal::derive(move || tab.get().id().to_string())
                    on_change=Callback::new(move |id: String| tab.set(Tab::from_id(&id)))
                />
                {move || match tab.get() {
                    Tab::Relative => view! {
                        <div class="grid">
                            // Buttons rather than divs: a preset navigates,
                            // so an unreadable link has to be able to say so
                            // with the attribute browsers already honour.
                            {presets.get_value().into_iter().map(|preset| {
                                let id = StoredValue::new(preset.id);
                                let is_on = Signal::derive(move || matches!(value.get(), RangeValue::Quick(cur) if cur == id.get_value()));
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
                                        disabled=move || disabled.get()
                                        on:click=move |_| commit.run(RangeValue::Quick(id.get_value()))
                                    >{preset.label}</button>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any(),
                    Tab::Absolute => view! {
                        <div class="cust">
                            <div class="fld">
                                <label class="lb" for=ids.get_value().0>"From"</label>
                                <input
                                    id=ids.get_value().0
                                    class="dr-from"
                                    prop:value=move || from.get()
                                    on:input=move |e| from.set(event_target_value(&e))
                                    placeholder="2026-04-18T00:00:00Z"
                                />
                            </div>
                            <div class="fld">
                                <label class="lb" for=ids.get_value().1>"To"</label>
                                <input
                                    id=ids.get_value().1
                                    class="dr-to"
                                    prop:value=move || to.get()
                                    on:input=move |e| to.set(event_target_value(&e))
                                    placeholder="now"
                                />
                            </div>
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
                                        disabled=disabled
                                        on_click=apply_absolute
                                    >"Apply"</Btn>
                                </span>
                            </div>
                        </div>
                    }.into_any(),
                    Tab::RealTime => view! {
                        <div class="rt-hint">
                            <p>{live_description.get_value()}</p>
                            <Btn
                                variant=Variant::Primary
                                full=true
                                disabled=disabled
                                on_click=Callback::new(move |()| {
                                    if disabled.get_untracked() {
                                        return;
                                    }
                                    if let Some(on_live) = on_live {
                                        match on_live.run(()) {
                                            Ok(()) => close(),
                                            Err(message) => error.set(Some(message)),
                                        }
                                    }
                                })
                            >{live_label.get_value()}</Btn>
                        </div>
                    }.into_any(),
                }}
                <Show when=move || error.get().is_some()>
                    <div class="dr-err">{move || error.get().unwrap_or_default()}</div>
                </Show>
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
}

#[cfg(target_arch = "wasm32")]
pub use component::RangeDialog;

#[cfg(test)]
mod tests {
    #[test]
    fn range_input_ids_are_unique_between_instances_and_controls() {
        let first = super::input_ids();
        let second = super::input_ids();
        assert_ne!(first.0, first.1);
        assert_ne!(first.0, second.0);
        assert_ne!(first.1, second.1);
    }
}
