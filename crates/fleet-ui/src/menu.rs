// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one menu contract: the panel both fleet-ui menus mount
//! (ADR-0028).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuClose {
    Escape,
    Activate,
    Tab,
    Outside,
}

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

    #[derive(Clone)]
    pub(crate) struct MenuItem {
        pub label: Signal<String>,
        pub danger: bool,
        pub on_activate: Callback<()>,
    }

    /// One explicit choice in a named radio group. Focus does not select it.
    #[derive(Clone)]
    pub(crate) struct MenuRadioItem {
        pub label: &'static str,
        pub checked: Signal<bool>,
        pub on_activate: Callback<()>,
    }

    #[derive(Clone)]
    pub(crate) enum MenuEntry {
        Item(MenuItem),
        RadioGroup {
            label: &'static str,
            items: Vec<MenuRadioItem>,
        },
        Separator,
    }

    fn initial_index(entries: &[MenuEntry]) -> usize {
        let mut index = 0;
        for entry in entries {
            match entry {
                MenuEntry::Item(_) => index += 1,
                MenuEntry::RadioGroup { items, .. } => {
                    for item in items {
                        if item.checked.get_untracked() {
                            return index;
                        }
                        index += 1;
                    }
                }
                MenuEntry::Separator => {}
            }
        }
        0
    }

    /// `panel_class` lands on the wrapper, so the `.user-menu .item` /
    /// `.actions-menu .item` rules keep matching. `wrap_ref` is the
    /// trigger-plus-panel wrapper the outside-mousedown check contains
    /// against, which is what keeps a trigger re-click a clean toggle instead
    /// of a close-then-reopen.
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
        // Seed the roving tab stop before initial focus scans the mounted DOM.
        let focused = RwSignal::new(initial_index(&entries));

        let close = move |cause: MenuClose| {
            open.set(false);
            if restores_trigger(cause)
                && let Some(trigger) = trigger_ref.get_untracked()
            {
                let _ = trigger.focus();
            }
        };

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

        // Bound on the WRAPPER, not the role="menu" node, because an empty
        // menu's fallback focus parks on the wrapper's own tabindex="-1" and
        // a Tab pressed there must still close the menu. Every key handled
        // here stops propagation: a containing modal's window-level Tab trap
        // must not also see the Tab that just closed this menu.
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
            let Ok(items) = menu.query_selector_all(r#"[role="menuitem"], [role="menuitemradio"]"#)
            else {
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

    /// mousedown rather than click so a drag-select that ends outside
    /// doesn't dismiss.
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
                    MenuEntry::RadioGroup { label, items } => {
                        let radios = items.into_iter().map(|item| {
                            let index = next_item;
                            next_item += 1;
                            let checked = item.checked;
                            let cb = item.on_activate;
                            view! {
                                <button
                                    class="item"
                                    type="button"
                                    role="menuitemradio"
                                    aria-checked=move || checked.get().to_string()
                                    tabindex=move || if focused.get() == index { "0" } else { "-1" }
                                    on:focus=move |_| focused.set(index)
                                    on:click=move |e: web_sys::MouseEvent| {
                                        if stop_click_propagation {
                                            e.stop_propagation();
                                        }
                                        close(MenuClose::Activate);
                                        cb.run(());
                                    }
                                >
                                    <span class="menu-choice-mark" aria-hidden="true">
                                        {move || if checked.get() { "✓" } else { "" }}
                                    </span>
                                    {item.label}
                                </button>
                            }
                        }).collect_view();
                        return view! {
                            <div role="group" aria-label=label>
                                <div class="menu-group-label">{label}</div>
                                {radios}
                            </div>
                        }
                        .into_any();
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
pub(crate) use component::{MenuEntry, MenuItem, MenuPanel, MenuRadioItem};

#[cfg(test)]
mod tests {
    use super::{MenuClose, restores_trigger};

    #[test]
    fn every_keyboard_cause_restores_the_trigger_and_an_outside_press_does_not() {
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
