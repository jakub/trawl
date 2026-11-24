// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Topmost-overlay arbitration for window-level Escape.
//!
//! [`Modal`](crate::modal::Modal) and [`Drawer`](crate::drawer::Drawer)
//! each bind Escape at the `window` level so it fires regardless of
//! focus. But a drawer doesn't trap focus or make the background inert,
//! so a modal can open on top of a live drawer, and two unconditional
//! window listeners would let one Escape fire both: closing the modal
//! and invoking the drawer's close callback (navigating away and
//! discarding unsaved edits).
//!
//! This module is the referee. Every overlay pushes a layer on mount
//! ([`push_overlay_with`]) and releases it on unmount
//! ([`OverlayLayer::release`]). The stack is LIFO, so the
//! most-recently-mounted overlay is topmost; each Escape handler guards
//! on [`OverlayLayer::is_topmost`] and the non-topmost layers no-op.
//! One Escape therefore closes exactly one layer.
//!
//! ## Focus ownership
//!
//! The same referee arbitrates *focus*: each layer registers a
//! [`FocusPolicy`], and the topmost non-[`FocusPolicy::None`] layer
//! **owns focus** ([`OverlayLayer::owns_focus`]). Owning focus means:
//! take initial focus on mount and restore it to the opener on
//! unmount. A shadowed layer (a drawer beneath a modal) must do
//! neither — its restore would steal focus out of the live modal. A
//! [`FocusPolicy::Trap`] owner additionally Tab-traps
//! ([`OverlayLayer::should_trap`]); [`FocusPolicy::Capture`] (Drawer)
//! deliberately never traps — the drawer is non-modal and the
//! background stays tabbable.

use std::cell::RefCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FocusPolicy {
    Trap,
    Capture,
    #[default]
    None,
}

thread_local! {
    static STACK: RefCell<Vec<(u64, FocusPolicy)>> = const { RefCell::new(Vec::new()) };
    /// Monotonic id source so released-and-reused stack slots can't be
    /// confused with a still-live layer.
    static NEXT_ID: RefCell<u64> = const { RefCell::new(0) };
}

/// Whether any overlay is registered, including layers without focus ownership.
#[must_use]
pub fn has_layers() -> bool {
    STACK.with_borrow(|stack| !stack.is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayLayer(u64);

impl OverlayLayer {
    #[must_use]
    pub fn is_topmost(self) -> bool {
        STACK.with_borrow(|s| s.last().map(|&(id, _)| id) == Some(self.0))
    }

    #[must_use]
    pub fn owns_focus(self) -> bool {
        STACK.with_borrow(|s| {
            s.iter()
                .rev()
                .find(|&&(_, policy)| policy != FocusPolicy::None)
                .map(|&(id, _)| id)
                == Some(self.0)
        })
    }

    #[must_use]
    pub fn should_trap(self) -> bool {
        STACK.with_borrow(|s| {
            s.iter()
                .rev()
                .find(|&&(_, policy)| policy != FocusPolicy::None)
                .is_some_and(|&(id, policy)| id == self.0 && policy == FocusPolicy::Trap)
        })
    }

    pub fn release(self) {
        STACK.with_borrow_mut(|s| {
            if let Some(pos) = s.iter().rposition(|&(id, _)| id == self.0) {
                s.remove(pos);
            }
        });
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[must_use]
pub(crate) fn push_overlay_with(policy: FocusPolicy) -> OverlayLayer {
    let id = NEXT_ID.with_borrow_mut(|n| {
        let id = *n;
        *n += 1;
        id
    });
    STACK.with_borrow_mut(|s| s.push((id, policy)));
    OverlayLayer(id)
}

/// Register a layer *beneath* every layer already on the stack.
///
/// For an overlay that existed before those layers and is only now
/// taking a layer of its own: a docked drawer undocking under a modal
/// that opened over it. Pushing on top would hand that drawer focus
/// and Escape the modal still owns; beneath, it becomes topmost only
/// once everything stacked over it has released.
#[cfg(any(target_arch = "wasm32", test))]
#[must_use]
pub(crate) fn push_overlay_beneath(policy: FocusPolicy) -> OverlayLayer {
    let id = NEXT_ID.with_borrow_mut(|n| {
        let id = *n;
        *n += 1;
        id
    });
    STACK.with_borrow_mut(|s| s.insert(0, (id, policy)));
    OverlayLayer(id)
}

#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn use_overlay_layer() -> OverlayLayer {
    use leptos::prelude::on_cleanup;
    let layer = push_overlay_with(FocusPolicy::None);
    on_cleanup(move || layer.release());
    layer
}

/// `panel` is re-queried on every use — never cached — so re-rendered
/// content can't go stale.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn use_overlay_layer_with(
    policy: FocusPolicy,
    panel: impl Fn() -> Option<web_sys::Element> + Clone + 'static,
) -> OverlayLayer {
    use leptos::prelude::{Effect, GetValue, SetValue, StoredValue, on_cleanup, untrack};
    use wasm_bindgen::JsCast;

    let layer = push_overlay_with(policy);

    let trigger: Option<web_sys::HtmlElement> = leptos::prelude::document()
        .active_element()
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok());

    // Initial focus, once, as soon as the NodeRef populates. Reading
    // `panel()` inside the effect subscribes it to the NodeRef signal,
    // so the effect re-runs from None → Some(el); the `done` latch
    // keeps any later re-run from stealing focus back.
    if policy != FocusPolicy::None {
        let panel_for_focus = panel.clone();
        let done = StoredValue::new(false);
        Effect::new(move |_| {
            if done.get_value() {
                return;
            }
            if let Some(el) = panel_for_focus()
                && layer.owns_focus()
            {
                focus_initial(&el);
                done.set_value(true);
            }
        });
    }

    // Tab trap (Trap policy only), gated per-keypress on should_trap()
    // so a modal shadowed by a second modal stops cycling.
    if policy == FocusPolicy::Trap {
        let panel_for_trap = panel.clone();
        let _ = leptos_use::use_event_listener(
            leptos_use::use_window(),
            leptos::ev::keydown,
            move |e: web_sys::KeyboardEvent| {
                if e.key() != "Tab" || !layer.should_trap() {
                    return;
                }
                if let Some(el) = untrack(&panel_for_trap) {
                    cycle_tab(&el, &e);
                }
            },
        );
    }

    on_cleanup(move || {
        // Ownership must be read before release: a drawer unmounting
        // beneath a live modal doesn't own focus and must not restore.
        let owned = layer.owns_focus();
        layer.release();
        if let Some(t) = &trigger
            && should_restore(owned, policy, t.is_connected())
        {
            let _ = t.focus();
        }
    });

    layer
}

#[cfg(any(target_arch = "wasm32", test))]
fn should_restore(owned: bool, policy: FocusPolicy, opener_connected: bool) -> bool {
    owned && policy != FocusPolicy::None && opener_connected
}

/// Selectors can't see rendering, so candidates hidden via `display:none`
/// (a drawer's inactive tab pane, a collapsed section) still match —
/// [`focusable_descendants`] drops them with the [`is_rendered`] box-metric
/// filter.
#[cfg(any(target_arch = "wasm32", test))]
const FOCUSABLE: &str = "a[href]:not([tabindex='-1']), \
     button:not([disabled]):not([tabindex='-1']), \
     input:not([disabled]):not([tabindex='-1']), \
     select:not([disabled]):not([tabindex='-1']), \
     textarea:not([disabled]):not([tabindex='-1']), \
     [tabindex]:not([tabindex='-1'])";

/// An element inside a `display:none` subtree has zero offset box and
/// zero client rects; the rects leg keeps rendered zero-box elements
/// (inline links, `display:contents` hosts) in the tab order.
#[cfg(any(target_arch = "wasm32", test))]
fn is_rendered(offset_width: i32, offset_height: i32, client_rect_count: u32) -> bool {
    offset_width > 0 || offset_height > 0 || client_rect_count > 0
}

#[cfg(target_arch = "wasm32")]
fn focusable_descendants(panel: &web_sys::Element) -> Vec<web_sys::HtmlElement> {
    use wasm_bindgen::JsCast;
    let mut out = Vec::new();
    if let Ok(list) = panel.query_selector_all(FOCUSABLE) {
        for i in 0..list.length() {
            if let Some(el) = list
                .get(i)
                .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
                && is_rendered(
                    el.offset_width(),
                    el.offset_height(),
                    el.get_client_rects().length(),
                )
            {
                out.push(el);
            }
        }
    }
    out
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InitialFocus {
    First,
    Panel,
}

#[cfg(any(target_arch = "wasm32", test))]
fn initial_focus(has_focusable: bool) -> InitialFocus {
    if has_focusable {
        InitialFocus::First
    } else {
        InitialFocus::Panel
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn focus_initial(panel: &web_sys::Element) {
    use wasm_bindgen::JsCast;
    let focusables = focusable_descendants(panel);
    match initial_focus(!focusables.is_empty()) {
        InitialFocus::First => {
            if let Some(first) = focusables.into_iter().next() {
                let _ = first.focus();
            }
        }
        InitialFocus::Panel => {
            if let Some(el) = panel.dyn_ref::<web_sys::HtmlElement>() {
                let _ = el.focus();
            }
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabWrap {
    Panel,
    First,
    Last,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusPos {
    Empty,
    /// Focus escaped the panel entirely (a background element grabbed it).
    Outside,
    First,
    Last,
    Only,
    /// Inside the panel, between the edges (or on the panel itself).
    Interior,
}

#[cfg(any(target_arch = "wasm32", test))]
// The four booleans are exactly the DOM facts `cycle_tab` reads off the
// document; folding them into enums here would only re-inflate the caller.
#[allow(clippy::fn_params_excessive_bools)]
fn focus_pos(empty: bool, inside: bool, at_first: bool, at_last: bool) -> FocusPos {
    if empty {
        FocusPos::Empty
    } else if !inside {
        FocusPos::Outside
    } else {
        match (at_first, at_last) {
            (true, true) => FocusPos::Only,
            (true, false) => FocusPos::First,
            (false, true) => FocusPos::Last,
            (false, false) => FocusPos::Interior,
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn tab_wrap(pos: FocusPos, shift: bool) -> Option<TabWrap> {
    match (pos, shift) {
        (FocusPos::Empty, _) => Some(TabWrap::Panel),
        (FocusPos::First | FocusPos::Only, true) => Some(TabWrap::Last),
        (FocusPos::Outside, _) | (FocusPos::Last | FocusPos::Only, false) => Some(TabWrap::First),
        (FocusPos::First | FocusPos::Last | FocusPos::Interior, _) => None,
    }
}

#[cfg(target_arch = "wasm32")]
fn cycle_tab(panel: &web_sys::Element, e: &web_sys::KeyboardEvent) {
    use wasm_bindgen::JsCast;

    let focusables = focusable_descendants(panel);
    let active = leptos::prelude::document().active_element();
    let inside = active
        .as_ref()
        .is_some_and(|a| panel.contains(Some(a.unchecked_ref::<web_sys::Node>())));
    let is_active = |edge: &web_sys::HtmlElement| {
        active
            .as_ref()
            .is_some_and(|a| a.is_same_node(Some(edge.unchecked_ref::<web_sys::Node>())))
    };

    let at_first = matches!(focusables.first(), Some(f) if is_active(f));
    let at_last = matches!(focusables.last(), Some(l) if is_active(l));
    let pos = focus_pos(focusables.is_empty(), inside, at_first, at_last);

    let Some(wrap) = tab_wrap(pos, e.shift_key()) else {
        return;
    };
    e.prevent_default();
    let target = match wrap {
        TabWrap::Panel => panel.dyn_ref::<web_sys::HtmlElement>(),
        TabWrap::First => focusables.first(),
        TabWrap::Last => focusables.last(),
    };
    if let Some(el) = target {
        let _ = el.focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests balance every push with a release so they stay robust to
    // any leftover thread-local state when the harness reuses a thread.

    #[test]
    fn command_palette_has_layers_tracks_full_overlay_lifecycle() {
        assert!(!has_layers());
        let menu = push_overlay_with(FocusPolicy::None);
        assert!(has_layers(), "focus-free menus still block the palette");
        let drawer = push_overlay_with(FocusPolicy::Capture);
        let modal = push_overlay_with(FocusPolicy::Trap);
        drawer.release();
        assert!(
            has_layers(),
            "removing a middle layer keeps the stack occupied"
        );
        menu.release();
        menu.release();
        assert!(has_layers(), "a modal alone keeps the stack occupied");
        modal.release();
        assert!(!has_layers());
        modal.release();
        assert!(!has_layers(), "repeated release cannot recreate occupancy");
        let reopened = push_overlay_with(FocusPolicy::Capture);
        assert!(has_layers());
        reopened.release();
        assert!(!has_layers());
    }

    #[test]
    fn topmost_is_last_pushed() {
        let a = push_overlay_with(FocusPolicy::None);
        let b = push_overlay_with(FocusPolicy::None);
        assert!(b.is_topmost(), "last pushed layer is topmost");
        assert!(!a.is_topmost(), "shadowed layer is not topmost");

        b.release();
        assert!(a.is_topmost(), "release exposes the layer beneath");
        a.release();
    }

    #[test]
    fn menu_under_modal_yields_escape_to_the_modal_only() {
        let menu = push_overlay_with(FocusPolicy::None);
        let modal = push_overlay_with(FocusPolicy::None);
        assert!(modal.is_topmost(), "stacked modal takes Escape");
        assert!(!menu.is_topmost(), "shadowed menu must not also close");

        modal.release();
        assert!(menu.is_topmost(), "menu regains Escape after the modal");
        menu.release();
    }

    #[test]
    fn a_layer_pushed_beneath_a_modal_takes_neither_focus_nor_escape() {
        let modal = push_overlay_with(FocusPolicy::Trap);
        let drawer = push_overlay_beneath(FocusPolicy::Capture);
        assert!(modal.is_topmost(), "the modal keeps Escape");
        assert!(modal.owns_focus(), "the modal keeps focus ownership");
        assert!(!drawer.is_topmost());
        assert!(
            !drawer.owns_focus(),
            "an undocking drawer must not steal focus"
        );

        modal.release();
        assert!(
            drawer.is_topmost(),
            "the drawer surfaces once the modal is gone"
        );
        assert!(drawer.owns_focus());
        drawer.release();
    }

    #[test]
    fn release_is_idempotent_and_order_independent() {
        let a = push_overlay_with(FocusPolicy::None);
        let b = push_overlay_with(FocusPolicy::None);
        let c = push_overlay_with(FocusPolicy::None);

        b.release();
        b.release();
        assert!(c.is_topmost(), "middle release leaves the top alone");
        assert!(!a.is_topmost());

        c.release();
        assert!(a.is_topmost(), "only the base layer remains");
        a.release();
    }

    #[test]
    fn drawer_alone_owns_focus_but_does_not_trap() {
        let drawer = push_overlay_with(FocusPolicy::Capture);
        assert!(drawer.owns_focus(), "a lone Capture layer owns focus");
        assert!(
            !drawer.should_trap(),
            "Capture takes initial focus but never Tab-traps — the \
             drawer is non-modal and the background stays tabbable"
        );
        drawer.release();
    }

    #[test]
    fn modal_over_drawer_takes_ownership_and_hands_it_back() {
        let drawer = push_overlay_with(FocusPolicy::Capture);
        let modal = push_overlay_with(FocusPolicy::Trap);

        assert!(modal.owns_focus(), "stacked Trap layer owns focus");
        assert!(modal.should_trap(), "and it Tab-traps");
        assert!(
            !drawer.owns_focus(),
            "the shadowed drawer must NOT own focus while a modal is \
             stacked above it — its on_cleanup restore would otherwise \
             steal focus out of the live modal"
        );
        assert!(!drawer.should_trap());

        modal.release();
        assert!(
            drawer.owns_focus(),
            "closing the modal re-exposes the drawer's ownership"
        );
        assert!(!drawer.should_trap(), "…but a drawer still never traps");
        drawer.release();
    }

    #[test]
    fn none_policy_layers_never_own_focus() {
        let menu = push_overlay_with(FocusPolicy::None);
        assert!(!menu.owns_focus(), "None layers never own focus");
        assert!(!menu.should_trap());

        let modal = push_overlay_with(FocusPolicy::Trap);
        assert!(modal.owns_focus());
        assert!(modal.should_trap());
        assert!(!menu.owns_focus());

        let inner_menu = push_overlay_with(FocusPolicy::None);
        assert!(
            modal.owns_focus() && modal.should_trap(),
            "a None layer above a Trap layer leaves ownership (and the \
             trap) with the Trap layer"
        );
        assert!(!inner_menu.owns_focus());

        inner_menu.release();
        modal.release();
        menu.release();
    }

    #[test]
    fn none_layer_above_trap_keeps_the_trap() {
        let modal = push_overlay_with(FocusPolicy::Trap);
        let menu = push_overlay_with(FocusPolicy::None);

        assert!(menu.is_topmost(), "the menu is topmost for Escape");
        assert!(
            modal.should_trap(),
            "a None layer above a Trap layer leaves the trap switched on"
        );
        assert!(
            modal.owns_focus(),
            "and the Trap layer is still the focus owner"
        );
        assert!(!menu.owns_focus(), "a None layer never owns focus");

        menu.release();
        assert!(modal.should_trap() && modal.owns_focus());
        modal.release();
    }

    #[test]
    fn focusable_selector_excludes_negative_tabindex_on_every_arm() {
        let arms: Vec<&str> = FOCUSABLE.split(',').map(str::trim).collect();
        assert!(
            arms.len() >= 6,
            "expected one arm per tabbable tag family plus the generic \
             [tabindex] arm, got {arms:?}"
        );
        for arm in &arms {
            assert!(
                arm.contains(":not([tabindex='-1'])"),
                "FOCUSABLE arm `{arm}` matches elements the author took \
                 out of the tab order — a roving menu item at -1 would \
                 join a modal's Tab cycle"
            );
        }
    }

    #[test]
    fn ownership_transitions_track_release_and_re_expose() {
        let drawer = push_overlay_with(FocusPolicy::Capture);
        let modal = push_overlay_with(FocusPolicy::Trap);
        let confirm = push_overlay_with(FocusPolicy::Trap);

        assert!(confirm.owns_focus() && confirm.should_trap());
        assert!(!modal.owns_focus() && !modal.should_trap());
        assert!(!drawer.owns_focus());

        confirm.release();
        assert!(modal.owns_focus() && modal.should_trap());

        modal.release();
        assert!(drawer.owns_focus() && !drawer.should_trap());

        drawer.release();
        assert!(
            !drawer.owns_focus(),
            "a released layer no longer owns focus"
        );
    }

    #[test]
    fn focus_pos_empty_wins_over_every_other_bit() {
        assert_eq!(focus_pos(true, false, false, false), FocusPos::Empty);
        assert_eq!(focus_pos(true, true, true, true), FocusPos::Empty);
    }

    #[test]
    fn focus_pos_escaped_focus_is_outside_before_edges() {
        assert_eq!(focus_pos(false, false, false, false), FocusPos::Outside);
        assert_eq!(focus_pos(false, false, true, true), FocusPos::Outside);
    }

    #[test]
    fn focus_pos_maps_the_inside_edge_pairs() {
        assert_eq!(focus_pos(false, true, true, true), FocusPos::Only);
        assert_eq!(focus_pos(false, true, true, false), FocusPos::First);
        assert_eq!(focus_pos(false, true, false, true), FocusPos::Last);
        assert_eq!(focus_pos(false, true, false, false), FocusPos::Interior);
    }

    // `false` = Tab, `true` = Shift+Tab.

    #[test]
    fn tab_forward_at_last_wraps_to_first() {
        assert_eq!(tab_wrap(FocusPos::Last, false), Some(TabWrap::First));
    }

    #[test]
    fn tab_backward_at_first_wraps_to_last() {
        assert_eq!(tab_wrap(FocusPos::First, true), Some(TabWrap::Last));
    }

    #[test]
    fn tab_in_the_interior_passes_through() {
        assert_eq!(tab_wrap(FocusPos::Interior, false), None);
        assert_eq!(tab_wrap(FocusPos::Interior, true), None);
    }

    #[test]
    fn tab_toward_the_far_edge_passes_through() {
        assert_eq!(tab_wrap(FocusPos::First, false), None);
        assert_eq!(tab_wrap(FocusPos::Last, true), None);
    }

    #[test]
    fn tab_recaptures_focus_that_escaped_the_panel() {
        assert_eq!(tab_wrap(FocusPos::Outside, false), Some(TabWrap::First));
        assert_eq!(tab_wrap(FocusPos::Outside, true), Some(TabWrap::First));
    }

    #[test]
    fn tab_in_an_empty_panel_parks_on_the_panel() {
        assert_eq!(tab_wrap(FocusPos::Empty, false), Some(TabWrap::Panel));
        assert_eq!(tab_wrap(FocusPos::Empty, true), Some(TabWrap::Panel));
    }

    #[test]
    fn a_single_focusable_wraps_to_itself_both_directions() {
        assert_eq!(tab_wrap(FocusPos::Only, false), Some(TabWrap::First));
        assert_eq!(tab_wrap(FocusPos::Only, true), Some(TabWrap::Last));
    }

    #[test]
    fn initial_focus_prefers_the_first_focusable() {
        assert_eq!(initial_focus(true), InitialFocus::First);
    }

    #[test]
    fn initial_focus_falls_back_to_the_panel_when_empty() {
        assert_eq!(initial_focus(false), InitialFocus::Panel);
    }

    #[test]
    fn owner_with_a_connected_opener_restores() {
        assert!(should_restore(true, FocusPolicy::Trap, true));
        assert!(should_restore(true, FocusPolicy::Capture, true));
    }

    #[test]
    fn shadowed_layer_does_not_restore() {
        assert!(!should_restore(false, FocusPolicy::Capture, true));
        assert!(!should_restore(false, FocusPolicy::Trap, true));
    }

    #[test]
    fn a_disconnected_opener_is_not_refocused() {
        assert!(!should_restore(true, FocusPolicy::Trap, false));
    }

    #[test]
    fn none_policy_never_restores() {
        assert!(!should_restore(true, FocusPolicy::None, true));
    }

    #[test]
    fn display_none_candidates_are_not_rendered() {
        assert!(!is_rendered(0, 0, 0));
    }

    #[test]
    fn boxed_candidates_are_rendered() {
        assert!(is_rendered(120, 32, 1));
    }

    #[test]
    fn zero_box_but_rect_bearing_candidates_stay_tabbable() {
        assert!(is_rendered(0, 0, 2));
    }
}
