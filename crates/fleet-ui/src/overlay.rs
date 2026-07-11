// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Topmost-overlay arbitration for window-level Escape (issue #28).
//!
//! [`Modal`](crate::modal::Modal) and [`Drawer`](crate::drawer::Drawer)
//! each bind Escape at the `window` level so it fires regardless of
//! focus. But a drawer doesn't trap focus or make the background inert,
//! so a modal can open on top of a live drawer — and with two
//! unconditional window listeners, one Escape fired *both*, closing the
//! modal *and* invoking the drawer's close callback (navigating away and
//! discarding unsaved edits).
//!
//! This module is the referee. Every overlay pushes a layer on mount
//! ([`push_overlay_with`]) and releases it on unmount
//! ([`OverlayLayer::release`]). The stack is LIFO, so the
//! most-recently-mounted overlay is topmost; each Escape handler guards
//! on [`OverlayLayer::is_topmost`] and the non-topmost layers no-op.
//! One Escape therefore closes exactly one layer.
//!
//! The stack is a pure `thread_local` — wasm is single-threaded, and
//! keeping it free of `leptos` / `web_sys` lets the LIFO arbitration
//! contract be exercised by native `cargo nextest run --workspace`
//! (matching [`theme::prefs`](crate::theme::prefs)). Only the
//! [`use_overlay_layer`] / [`use_overlay_layer_with`] Leptos glue is
//! wasm-only.
//!
//! ## Focus ownership (issue #33)
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
//! background stays tabbable. The ownership queries are pure and
//! native-tested; the DOM glue (element capture, `.focus()`, Tab
//! cycling) lives in [`use_overlay_layer_with`].

use std::cell::RefCell;

/// How an overlay layer participates in focus ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FocusPolicy {
    /// Owns focus while topmost *and* Tab-traps inside its panel —
    /// the modal family (`Modal`, `ConfirmModal`,
    /// `ConfirmWithReasonModal`), which renders `aria-modal="true"`
    /// and must earn it.
    Trap,
    /// Takes initial focus on mount and restores on unmount, but
    /// never traps Tab — the non-modal `Drawer` (background stays
    /// interactive).
    Capture,
    /// Invisible to focus ownership — layers that manage their own
    /// focus (`ActionsMenu`'s roving items + restore-to-trigger).
    #[default]
    None,
}

thread_local! {
    /// LIFO stack of live overlay ids (+ focus policy); the last entry
    /// is topmost.
    static STACK: RefCell<Vec<(u64, FocusPolicy)>> = const { RefCell::new(Vec::new()) };
    /// Monotonic id source so released-and-reused stack slots can't be
    /// confused with a still-live layer.
    static NEXT_ID: RefCell<u64> = const { RefCell::new(0) };
}

/// A registered overlay layer — a cheap `Copy` handle over a stack id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayLayer(u64);

impl OverlayLayer {
    /// True when this layer is the most-recently mounted overlay still
    /// on the stack — the only layer that should react to a
    /// window-level Escape.
    #[must_use]
    pub fn is_topmost(self) -> bool {
        STACK.with_borrow(|s| s.last().map(|&(id, _)| id) == Some(self.0))
    }

    /// True when this layer is the topmost layer whose policy is not
    /// [`FocusPolicy::None`] — the one layer entitled to take initial
    /// focus and to restore focus to its opener on close.
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

    /// True when this layer [`owns_focus`](Self::owns_focus) *and* its
    /// policy is [`FocusPolicy::Trap`] — the gate for the window-level
    /// Tab-cycling listener.
    #[must_use]
    pub fn should_trap(self) -> bool {
        STACK.with_borrow(|s| {
            s.iter()
                .rev()
                .find(|&&(_, policy)| policy != FocusPolicy::None)
                .is_some_and(|&(id, policy)| id == self.0 && policy == FocusPolicy::Trap)
        })
    }

    /// Remove this layer from the arbitration stack. Called from the
    /// owning component's `on_cleanup` on unmount. Idempotent.
    pub fn release(self) {
        STACK.with_borrow_mut(|s| {
            if let Some(pos) = s.iter().rposition(|&(id, _)| id == self.0) {
                s.remove(pos);
            }
        });
    }
}

/// Push a new topmost overlay layer onto the arbitration stack.
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

/// Register an overlay layer for the lifetime of the current reactive
/// owner: pushes on mount and releases via `on_cleanup` on unmount.
/// Returns the `Copy` handle for the component's Escape guard.
///
/// Registers [`FocusPolicy::None`] — the layer arbitrates Escape but
/// stays out of focus ownership (the `ActionsMenu` contract, which
/// manages its own roving focus). Overlays that own focus use
/// [`use_overlay_layer_with`].
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn use_overlay_layer() -> OverlayLayer {
    use leptos::prelude::on_cleanup;
    let layer = push_overlay_with(FocusPolicy::None);
    on_cleanup(move || layer.release());
    layer
}

/// [`use_overlay_layer`] plus focus management. `panel` resolves the
/// overlay's panel element (a closure over the component's `NodeRef`,
/// erasing the `Div` vs `Aside` element type); it is re-queried on
/// every use — never cached — so re-rendered content can't go stale.
///
/// On mount the currently focused element is captured as the opener;
/// once the panel exists, focus moves to its first focusable
/// descendant (falling back to the panel itself, which callers give
/// `tabindex="-1"`). While the layer [`should_trap`
/// ](OverlayLayer::should_trap), a window-level Tab listener cycles
/// focus inside the panel — focusable descendants are re-queried per
/// keypress. On unmount, focus returns to the captured opener only if
/// this layer still [`owns_focus`](OverlayLayer::owns_focus) (a drawer
/// closing beneath a live modal must not steal focus) and the opener
/// is still connected to the document.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn use_overlay_layer_with(
    policy: FocusPolicy,
    panel: impl Fn() -> Option<web_sys::Element> + Clone + 'static,
) -> OverlayLayer {
    use leptos::prelude::{Effect, GetValue, SetValue, StoredValue, on_cleanup, untrack};
    use wasm_bindgen::JsCast;

    let layer = push_overlay_with(policy);

    // The opener: whatever was focused when the overlay mounted.
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
        // Ownership must be read BEFORE release: a drawer unmounting
        // beneath a live modal doesn't own focus and must not restore.
        let owned = layer.owns_focus();
        layer.release();
        if owned
            && policy != FocusPolicy::None
            && let Some(t) = &trigger
            && t.is_connected()
        {
            let _ = t.focus();
        }
    });

    layer
}

/// Selector for tabbable descendants. Re-queried per use — a cached
/// `NodeList` goes stale the moment the panel re-renders (drawer tab
/// switches swap the whole body).
#[cfg(target_arch = "wasm32")]
const FOCUSABLE: &str = "a[href], button:not([disabled]), input:not([disabled]), \
     select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])";

#[cfg(target_arch = "wasm32")]
fn focusable_descendants(panel: &web_sys::Element) -> Vec<web_sys::HtmlElement> {
    use wasm_bindgen::JsCast;
    let mut out = Vec::new();
    if let Ok(list) = panel.query_selector_all(FOCUSABLE) {
        for i in 0..list.length() {
            if let Some(el) = list
                .get(i)
                .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
            {
                out.push(el);
            }
        }
    }
    out
}

/// Move focus to the panel's first focusable descendant, falling back
/// to the panel itself (callers set `tabindex="-1"` so it accepts
/// programmatic focus without joining the tab order).
#[cfg(target_arch = "wasm32")]
fn focus_initial(panel: &web_sys::Element) {
    use wasm_bindgen::JsCast;
    if let Some(first) = focusable_descendants(panel).into_iter().next() {
        let _ = first.focus();
    } else if let Some(el) = panel.dyn_ref::<web_sys::HtmlElement>() {
        let _ = el.focus();
    }
}

/// Tab/Shift+Tab cycling for a trapping layer: wrap at the edges, and
/// pull focus back to the first focusable if it escaped the panel.
#[cfg(target_arch = "wasm32")]
fn cycle_tab(panel: &web_sys::Element, e: &web_sys::KeyboardEvent) {
    use wasm_bindgen::JsCast;

    let focusables = focusable_descendants(panel);
    let Some((first, last)) = focusables.first().zip(focusables.last()) else {
        // Nothing tabbable inside: park focus on the panel itself.
        e.prevent_default();
        if let Some(el) = panel.dyn_ref::<web_sys::HtmlElement>() {
            let _ = el.focus();
        }
        return;
    };

    let active = leptos::prelude::document().active_element();
    let inside = active
        .as_ref()
        .is_some_and(|a| panel.contains(Some(a.unchecked_ref::<web_sys::Node>())));
    let at = |edge: &web_sys::HtmlElement| {
        active
            .as_ref()
            .is_some_and(|a| a.is_same_node(Some(edge.unchecked_ref::<web_sys::Node>())))
    };

    if !inside {
        e.prevent_default();
        let _ = first.focus();
    } else if e.shift_key() && at(first) {
        e.prevent_default();
        let _ = last.focus();
    } else if !e.shift_key() && at(last) {
        e.prevent_default();
        let _ = first.focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests balance every push with a release so they stay robust to
    // any leftover thread-local state when the harness reuses a thread.

    #[test]
    fn topmost_is_last_pushed() {
        let a = push_overlay_with(FocusPolicy::None);
        let b = push_overlay_with(FocusPolicy::None);
        assert!(b.is_topmost(), "last pushed layer is topmost");
        assert!(!a.is_topmost(), "shadowed layer is not topmost");

        // Closing the top (modal over drawer) hands topmost back down.
        b.release();
        assert!(a.is_topmost(), "release exposes the layer beneath");
        a.release();
    }

    #[test]
    fn menu_under_modal_yields_escape_to_the_modal_only() {
        // Issue #31 C5: an open ActionsMenu is an overlay layer like any
        // other. When a ConfirmModal (or Drawer) stacks above it, only
        // the modal is Escape-eligible; when the modal closes, the menu
        // becomes topmost again and takes the next Escape.
        let menu = push_overlay_with(FocusPolicy::None);
        let modal = push_overlay_with(FocusPolicy::None);
        assert!(modal.is_topmost(), "stacked modal takes Escape");
        assert!(!menu.is_topmost(), "shadowed menu must not also close");

        modal.release();
        assert!(menu.is_topmost(), "menu regains Escape after the modal");
        menu.release();
    }

    #[test]
    fn release_is_idempotent_and_order_independent() {
        let a = push_overlay_with(FocusPolicy::None);
        let b = push_overlay_with(FocusPolicy::None);
        let c = push_overlay_with(FocusPolicy::None);

        // Release a middle layer: neither a nor c is disturbed.
        b.release();
        b.release(); // idempotent — no panic, no effect
        assert!(c.is_topmost(), "middle release leaves the top alone");
        assert!(!a.is_topmost());

        c.release();
        assert!(a.is_topmost(), "only the base layer remains");
        a.release();
    }

    // ── focus ownership (issue #33 D1) ─────────────────────────────

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
        // ActionsMenu registers FocusPolicy::None: it manages its own
        // roving focus and must be invisible to focus ownership.
        let menu = push_overlay_with(FocusPolicy::None);
        assert!(!menu.owns_focus(), "None layers never own focus");
        assert!(!menu.should_trap());

        // menu-under-modal: the modal owns focus, the menu stays out.
        let modal = push_overlay_with(FocusPolicy::Trap);
        assert!(modal.owns_focus());
        assert!(modal.should_trap());
        assert!(!menu.owns_focus());

        // A None layer stacked ON TOP of a Trap modal (menu opened from
        // inside a modal) must not break the modal's trap: ownership
        // skips None layers when scanning down from the top.
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
    fn ownership_transitions_track_release_and_re_expose() {
        // drawer → modal → second modal (confirm-over-modal-over-drawer).
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
}
