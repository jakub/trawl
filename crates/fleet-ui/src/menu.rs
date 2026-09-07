// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one menu contract: the panel both fleet-ui menus mount
//! (ADR-0028).
//!
//! The topbar's account menu and a table row's `ActionsMenu` are the
//! same widget with different content, so they are the same component
//! here — the trigger, the entries and the panel class are props, and
//! everything a menu must get right lives in one place.
//!
//! The invariants this module owns:
//!
//! - **One tab stop.** Items are `<button role="menuitem">` with true
//!   roving tabindex: exactly one at `0`, the rest at `-1`. Arrows,
//!   Home and End walk the queried `[role="menuitem"]` list by index
//!   ([`crate::roving`]), so a node that is not an item — the account
//!   menu's identity header — can never take focus. Tab and Shift+Tab close the menu and return to the trigger:
//!   one deterministic keypress beats scanning the document for the
//!   trigger's successor across a panel that unmounts on a microtask.
//! - **The panel is a [`FocusPolicy::None`](crate::overlay::FocusPolicy)
//!   layer**, mounted only while open, and it gates BOTH window
//!   listeners — Escape and outside mousedown — on
//!   [`is_topmost`](crate::overlay::OverlayLayer::is_topmost). `Capture`
//!   would make an open menu the topmost focus owner, and a menu inside
//!   a modal would then switch the modal's Tab trap off while trapping
//!   nothing itself. Initial focus goes through the shared
//!   [`overlay::focus_initial`](crate::overlay::focus_initial) scan
//!   under a once-per-mount latch this component owns, because the
//!   overlay hook's own latch belongs to the focus-owning policies.
//! - **Restore is by cause** ([`restores_trigger`]). Escape, Tab and an
//!   item activation return focus to the trigger; an outside pointer
//!   dismissal restores nothing, because focus belongs to whatever was
//!   clicked. Activation restores BEFORE running the callback, so a
//!   callback that opens a dialog captures the trigger as that dialog's
//!   opener rather than `<body>`.
//! - **The header sits before and outside `role="menu"`.** It is a
//!   separate slot the component renders in the panel wrapper, so the
//!   identity block cannot land among the items — structurally, not by
//!   convention.
//!
//! The panel's roving index follows FOCUS, which is the menu half of
//! the asymmetry [`crate::roving`] documents: a tab strip's follows
//! selection.

/// Why a menu closed. The cause decides where focus goes, so it is
/// named rather than implied by which handler fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuClose {
    /// Escape while the menu is the topmost layer.
    Escape,
    /// An item was activated (click, Enter or Space on the button).
    Activate,
    /// Tab or Shift+Tab inside the menu.
    Tab,
    /// A pointer went down outside the trigger-plus-panel wrapper.
    Outside,
}

/// Whether closing for this cause returns focus to the trigger.
///
/// Every keyboard cause does: the user is in the keyboard's world and
/// dropping focus to `<body>` would lose their place. An outside
/// mousedown does not: focus belongs to whatever the user pressed, and
/// yanking it back would fight the browser's own focus move.
pub(crate) const fn restores_trigger(cause: MenuClose) -> bool {
    !matches!(cause, MenuClose::Outside)
}

#[cfg(target_arch = "wasm32")]
mod component {
    use leptos::ev;
    use leptos::html::{Button, Div};
    use leptos::prelude::*;
    use leptos::web_sys;
    use leptos_use::{use_event_listener, use_window};
    use wasm_bindgen::JsCast;

    use super::{MenuClose, restores_trigger};
    use crate::roving::{next_index, vertical_nav};

    /// One command in a menu. `label` is reactive because the topbar's
    /// theme item renames itself with the theme it would switch to;
    /// `danger` renders the destructive treatment.
    #[derive(Clone)]
    pub(crate) struct MenuItem {
        pub label: Signal<String>,
        pub danger: bool,
        pub on_activate: Callback<()>,
    }

    /// What a menu renders, in order: commands and the rules between
    /// groups of them. A separator is never focusable and never counts
    /// toward the roving index, because the walk indexes the queried
    /// `[role="menuitem"]` list and a separator is not in it.
    #[derive(Clone)]
    pub(crate) enum MenuEntry {
        Item(MenuItem),
        Separator,
    }

    /// The open menu panel. Mounted only while `open` is true — a
    /// closed menu must not sit on the overlay stack shadowing Escape
    /// for the layers beneath it.
    ///
    /// `panel_class` is the caller's positioning/skin class
    /// (`user-menu`, `actions-menu`) and lands on the wrapper, so the
    /// existing `.user-menu .item` / `.actions-menu .item` rules keep
    /// matching; the `role="menu"` node inside carries no class of its
    /// own. `wrap_ref` is the trigger-plus-panel wrapper the
    /// outside-mousedown check contains against, which is what keeps a
    /// trigger re-click a clean toggle instead of a close-then-reopen.
    /// `stop_click_propagation` is for menus nested in a clickable row.
    #[component]
    pub(crate) fn MenuPanel(
        panel_class: &'static str,
        menu_label: &'static str,
        entries: Vec<MenuEntry>,
        #[prop(optional)] header: Option<Children>,
        #[prop(default = false)] stop_click_propagation: bool,
        open: RwSignal<bool>,
        wrap_ref: NodeRef<Div>,
        trigger_ref: NodeRef<Button>,
    ) -> impl IntoView {
        let layer = crate::overlay::use_overlay_layer();
        let panel_ref = NodeRef::<Div>::new();
        let menu_ref = NodeRef::<Div>::new();
        // Which item is the menu's single tab stop. Starts on the first
        // item, which is also where initial focus lands.
        let focused = RwSignal::new(0usize);

        let close = move |cause: MenuClose| {
            open.set(false);
            if restores_trigger(cause)
                && let Some(trigger) = trigger_ref.get_untracked()
            {
                let _ = trigger.focus();
            }
        };

        // Initial focus, once per mount: the panel's first tabbable
        // descendant is the one item at tabindex="0"; a menu with no
        // items falls back to the panel's own tabindex="-1".
        let focused_once = StoredValue::new(false);
        Effect::new(move |_| {
            if focused_once.get_value() {
                return;
            }
            if let Some(panel) = panel_ref.get() {
                crate::overlay::focus_initial(&panel);
                focused_once.set_value(true);
            }
        });

        install_dismissal(layer, wrap_ref, close);

        // Keydown bubbles from the focused item up to the panel
        // wrapper. It is bound on the WRAPPER, not the role="menu"
        // node, because an empty menu's fallback focus parks on the
        // wrapper's own tabindex="-1" and a Tab pressed there must
        // still close the menu rather than walk off it (shadow review
        // 1, #159). Every key handled here stops propagation as well
        // as the default: a containing modal's window-level Tab trap
        // must not also see the Tab that just closed this menu. Escape
        // is deliberately NOT handled here — it belongs to the window
        // listener above, which is the one that knows about topmost.
        let on_keydown = move |e: web_sys::KeyboardEvent| {
            let key = e.key();
            if key == "Tab" {
                e.prevent_default();
                e.stop_propagation();
                close(MenuClose::Tab);
                return;
            }
            let Some(nav) = vertical_nav(&key) else {
                return;
            };
            e.prevent_default();
            e.stop_propagation();
            let Some(menu) = menu_ref.get_untracked() else {
                return;
            };
            let Ok(items) = menu.query_selector_all(r#"[role="menuitem"]"#) else {
                return;
            };
            let len = usize::try_from(items.length()).unwrap_or(0);
            let Some(next) = next_index(focused.get_untracked(), len, nav) else {
                return;
            };
            focused.set(next);
            // Focus moves synchronously off the same event, never from
            // an effect keyed on `focused`: an effect would run a frame
            // later and lose a race with a fast repeat.
            if let Some(el) = u32::try_from(next)
                .ok()
                .and_then(|i| items.get(i))
                .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = el.focus();
            }
        };

        let rendered = render_entries(entries, focused, stop_click_propagation, close);

        view! {
            <div class=panel_class tabindex="-1" node_ref=panel_ref on:keydown=on_keydown>
                {header.map(|h| h())}
                <div role="menu" aria-label=menu_label node_ref=menu_ref>{rendered}</div>
            </div>
        }
    }

    /// The two window-level dismissal listeners, both gated on the
    /// menu being the topmost layer. Escape belongs here rather than on
    /// the menu node so it fires wherever focus is; mousedown rather
    /// than click so a drag-select that ends outside doesn't dismiss,
    /// the same reason Modal's scrim uses mousedown.
    fn install_dismissal(
        layer: crate::overlay::OverlayLayer,
        wrap_ref: NodeRef<Div>,
        close: impl Fn(MenuClose) + Copy + 'static,
    ) {
        let _ = use_event_listener(use_window(), ev::keydown, move |e| {
            if !layer.is_topmost() {
                return;
            }
            if e.key() == "Escape" {
                e.prevent_default();
                close(MenuClose::Escape);
            }
        });

        let _ = use_event_listener(
            use_window(),
            ev::mousedown,
            move |e: web_sys::MouseEvent| {
                if !layer.is_topmost() {
                    return;
                }
                let Some(wrap) = wrap_ref.get_untracked() else {
                    return;
                };
                let Some(target) = e.target() else { return };
                let Some(node) = target.dyn_ref::<web_sys::Node>() else {
                    return;
                };
                if !wrap.contains(Some(node)) {
                    close(MenuClose::Outside);
                }
            },
        );
    }

    /// The panel's entries. Item buttons carry the roving tabindex (one
    /// `0`, the rest `-1`) and sync the index on focus, so a pointer
    /// landing on an item leaves the arrows walking from where the user
    /// actually is. Only items are counted: these indices are the
    /// indices of the queried `[role="menuitem"]` list the walk reads,
    /// which is why a separator can neither be focused nor be skipped
    /// past by an off-by-one.
    fn render_entries(
        entries: Vec<MenuEntry>,
        focused: RwSignal<usize>,
        stop_click_propagation: bool,
        close: impl Fn(MenuClose) + Copy + 'static,
    ) -> Vec<AnyView> {
        let mut next_item = 0usize;
        entries
            .into_iter()
            .map(|entry| {
                let item = match entry {
                    MenuEntry::Separator => {
                        return view! { <div class="sep" role="separator"></div> }.into_any();
                    }
                    MenuEntry::Item(item) => item,
                };
                let index = next_item;
                next_item += 1;
                let class = if item.danger { "item danger" } else { "item" };
                let label = item.label;
                let cb = item.on_activate;
                view! {
                    <button
                        class=class
                        type="button"
                        role="menuitem"
                        tabindex=move || if focused.get() == index { "0" } else { "-1" }
                        on:focus=move |_| focused.set(index)
                        on:click=move |e: web_sys::MouseEvent| {
                            if stop_click_propagation {
                                e.stop_propagation();
                            }
                            // Restore BEFORE the callback: a callback
                            // that opens a dialog must capture the
                            // trigger as that dialog's opener.
                            close(MenuClose::Activate);
                            cb.run(());
                        }
                    >{move || label.get()}</button>
                }
                .into_any()
            })
            .collect()
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use component::{MenuEntry, MenuItem, MenuPanel};

#[cfg(test)]
mod tests {
    use super::{MenuClose, restores_trigger};

    #[test]
    fn every_keyboard_cause_restores_the_trigger_and_an_outside_press_does_not() {
        // The whole table, so a new cause has to state its answer here
        // rather than inheriting one.
        assert!(restores_trigger(MenuClose::Escape));
        assert!(restores_trigger(MenuClose::Activate));
        assert!(restores_trigger(MenuClose::Tab));
        assert!(
            !restores_trigger(MenuClose::Outside),
            "an outside mousedown leaves focus on what was pressed — \
             restoring the trigger would fight the browser's own focus \
             move across the panel's disposal"
        );
    }
}
