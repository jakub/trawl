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
//! The stack is a pure `thread_local` — wasm is single-threaded, and
//! keeping it free of `leptos` / `web_sys` lets the LIFO arbitration
//! contract be exercised by native `cargo nextest run --workspace`
//! (matching [`theme::prefs`](crate::theme::prefs)). Only the
//! [`use_overlay_layer`] / [`use_overlay_layer_with`] Leptos glue is
//! wasm-only.
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
//! background stays tabbable. The ownership queries are pure and
//! native-tested, as are the two runtime decisions beside them: where
//! initial focus lands ([`initial_focus`]) and whether a closing layer
//! restores to its opener ([`should_restore`]).
//! Only the irreducible platform calls (element capture, `.focus()`,
//! `query_selector_all`) stay wasm-only, inside
//! [`use_overlay_layer_with`].

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

/// Whether any overlay is registered, including layers without focus ownership.
/// Shell uses this to leave global palette chords inert behind another overlay.
#[must_use]
pub fn has_layers() -> bool {
    STACK.with_borrow(|stack| !stack.is_empty())
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
        // Ownership must be read before release: a drawer unmounting
        // beneath a live modal doesn't own focus and must not restore.
        // The restore predicate itself is the native-tested
        // `should_restore`; only `.focus()` stays glue.
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

/// Whether a closing overlay should restore focus to its captured
/// opener — the pure predicate behind [`use_overlay_layer_with`]'s
/// `on_cleanup`, so the restore contract is native-tested. Restore only
/// when this layer still owned focus at cleanup (a drawer closing
/// beneath a live modal does not — restoring would yank focus out of
/// the modal), its policy participates in focus ([`FocusPolicy::None`]
/// layers never restore), and the opener is still connected to the
/// document.
#[cfg(any(target_arch = "wasm32", test))]
fn should_restore(owned: bool, policy: FocusPolicy, opener_connected: bool) -> bool {
    owned && policy != FocusPolicy::None && opener_connected
}

/// Selector for tabbable descendants. Re-queried per use — a cached
/// `NodeList` goes stale the moment the panel re-renders (drawer tab
/// switches swap the whole body). Selectors can't see rendering, so
/// candidates hidden via `display:none` (a drawer's inactive tab pane,
/// a collapsed section) still match — [`focusable_descendants`] drops
/// them with the [`is_rendered`] box-metric filter, otherwise Tab-wrap
/// could park focus on an invisible element and it would silently vanish.
///
/// Every arm carries `:not([tabindex='-1'])`, not just the generic
/// `[tabindex]` one: `tabindex="-1"` takes an element OUT of the tab
/// order whatever its tag is, so `button:not([disabled])` alone matched
/// the roving menu items a menu keeps at `-1` and a modal's Tab cycle
/// walked every one of them (ADR-0028). The selector is compiled on
/// native test builds too so the string itself is a native fact.
#[cfg(any(target_arch = "wasm32", test))]
const FOCUSABLE: &str = "a[href]:not([tabindex='-1']), \
     button:not([disabled]):not([tabindex='-1']), \
     input:not([disabled]):not([tabindex='-1']), \
     select:not([disabled]):not([tabindex='-1']), \
     textarea:not([disabled]):not([tabindex='-1']), \
     [tabindex]:not([tabindex='-1'])";

/// Whether an element's box metrics say it is actually rendered — the
/// pure predicate behind [`focusable_descendants`]'s visibility filter.
/// An element inside a `display:none` subtree has zero offset box and
/// zero client rects; the rects leg keeps rendered zero-box elements
/// (inline links, `display:contents` hosts) in the tab order. Deliberate
/// boundary: `visibility:hidden` elements still have boxes and pass —
/// catching those needs a per-candidate computed-style read, not worth
/// it until a real layout uses `visibility` to hide focusables.
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

/// Where an owning overlay's initial focus should land — the pure
/// decision split out of [`focus_initial`] so it is native-tested: the
/// first focusable descendant if the panel has any, else the panel
/// itself (which callers give `tabindex="-1"`, so it accepts
/// programmatic focus without joining the tab order).
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InitialFocus {
    /// Focus the panel's first focusable descendant.
    First,
    /// No focusable descendants — park focus on the panel itself.
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

/// Move focus to the panel's first focusable descendant, falling back
/// to the panel itself. The target decision is the native-tested
/// [`initial_focus`]; this shell only reads the DOM and applies it.
///
/// Crate-visible because the menus own their initial focus rather than
/// delegating it to [`use_overlay_layer_with`]: they register as
/// [`FocusPolicy::None`] layers, so the hook's own latch (which belongs
/// to the focus-owning policies) never runs for them, and the shared
/// menu panel calls this under a once-per-mount latch of its own.
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

/// Where one Tab keypress should send focus inside a trapping panel —
/// the pure edge-wrap decision, split from the DOM so the trap's
/// cycling logic is native-testable. `None` means let the browser move
/// focus naturally (no `prevent_default`).
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabWrap {
    /// Nothing tabbable inside — park focus on the panel itself.
    Panel,
    /// Wrap to (or recapture on) the first focusable.
    First,
    /// Wrap to the last focusable.
    Last,
}

/// Where focus sits relative to a trapping panel's focusables when Tab is
/// pressed — the DOM state [`cycle_tab`] classifies before handing off to
/// the pure [`tab_wrap`] decision.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusPos {
    /// No focusable descendants at all.
    Empty,
    /// Focus escaped the panel entirely (a background element grabbed it).
    Outside,
    /// On the first focusable (and not also the last).
    First,
    /// On the last focusable (and not also the first).
    Last,
    /// The sole focusable — simultaneously first and last.
    Only,
    /// Inside the panel, between the edges (or on the panel itself).
    Interior,
}

/// Classify the DOM focus state [`cycle_tab`] reads into a [`FocusPos`].
/// Precedence matters: an empty panel is `Empty` whatever else holds, and
/// focus that escaped the panel is `Outside` before the edge bits are even
/// consulted. Inside the panel, the `(at_first, at_last)` pair maps to the
/// sole/first/last/interior positions.
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

/// Resolve a Tab keypress against panel state. Mirrors [`cycle_tab`]'s
/// branches exactly: an empty panel parks; focus that escaped the panel
/// is pulled back to the first focusable; Shift+Tab at the first wraps to
/// the last; Tab at the last wraps to the first; anything else passes
/// through (`None`, meaning let the browser move focus naturally).
#[cfg(any(target_arch = "wasm32", test))]
fn tab_wrap(pos: FocusPos, shift: bool) -> Option<TabWrap> {
    match (pos, shift) {
        (FocusPos::Empty, _) => Some(TabWrap::Panel),
        (FocusPos::First | FocusPos::Only, true) => Some(TabWrap::Last),
        // Recapture escaped focus, or wrap forward off the last edge —
        // both land on the first focusable.
        (FocusPos::Outside, _) | (FocusPos::Last | FocusPos::Only, false) => Some(TabWrap::First),
        // First+forward, Last+backward, Interior: interior moves the
        // browser handles without wrapping.
        (FocusPos::First | FocusPos::Last | FocusPos::Interior, _) => None,
    }
}

/// Tab/Shift+Tab cycling for a trapping layer: wrap at the edges, and
/// pull focus back to the first focusable if it escaped the panel. The
/// edge arithmetic lives in [`tab_wrap`] (native-tested); this shell only
/// reads DOM state and applies the decision.
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
        // Interior move: let the browser shift focus naturally.
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

        // Closing the top (modal over drawer) hands topmost back down.
        b.release();
        assert!(a.is_topmost(), "release exposes the layer beneath");
        a.release();
    }

    #[test]
    fn menu_under_modal_yields_escape_to_the_modal_only() {
        // An open ActionsMenu is an overlay layer like any other.
        // When a ConfirmModal (or Drawer) stacks above it, only
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

    // ── focus ownership ────────────────────────────────────────────

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

        // A None layer stacked on top of a Trap modal (menu opened from
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
    fn none_layer_above_trap_keeps_the_trap() {
        // A menu opened from inside a modal pushes a None layer above
        // the modal's Trap layer. The menu is topmost for Escape, but
        // the modal must keep BOTH focus ownership and its Tab trap:
        // handing the trap to a layer that traps nothing would let Tab
        // walk straight out of an aria-modal="true" dialog (ADR-0028's
        // reason menus stay FocusPolicy::None).
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
        // `tabindex="-1"` removes an element from the tab order whatever
        // its tag is. Before ADR-0028 only the generic `[tabindex]` arm
        // said so, so `button:not([disabled])` matched a menu's roving
        // items (all but one sit at -1) and a modal's Tab cycle walked
        // every one of them. Every arm carries the guard.
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

    // ── focus classification ───────────────────────────────────────
    // `cycle_tab`'s DOM shell reads four booleans off the document and
    // hands them to `focus_pos`; these pin the empty/outside precedence
    // and the 2×2 edge mapping natively, so the classification feeding
    // `tab_wrap` is tested off-target rather than only in a browser.

    #[test]
    fn focus_pos_empty_wins_over_every_other_bit() {
        // An empty panel is `Empty` regardless of inside/edge bits — the
        // first branch short-circuits before they are consulted.
        assert_eq!(focus_pos(true, false, false, false), FocusPos::Empty);
        assert_eq!(focus_pos(true, true, true, true), FocusPos::Empty);
    }

    #[test]
    fn focus_pos_escaped_focus_is_outside_before_edges() {
        // Not empty and not inside → `Outside`, whatever the edge bits
        // (stale from a previous position) happen to say.
        assert_eq!(focus_pos(false, false, false, false), FocusPos::Outside);
        assert_eq!(focus_pos(false, false, true, true), FocusPos::Outside);
    }

    #[test]
    fn focus_pos_maps_the_inside_edge_pairs() {
        // Inside the panel, the (at_first, at_last) 2×2 selects the sole,
        // first, last, and interior positions.
        assert_eq!(focus_pos(false, true, true, true), FocusPos::Only);
        assert_eq!(focus_pos(false, true, true, false), FocusPos::First);
        assert_eq!(focus_pos(false, true, false, true), FocusPos::Last);
        assert_eq!(focus_pos(false, true, false, false), FocusPos::Interior);
    }

    // ── Tab-cycle edge arithmetic ──────────────────────────────────
    // `cycle_tab`'s DOM shell classifies focus into a `FocusPos` and
    // defers the wrap decision to `tab_wrap`; these pin the runtime
    // trap's cycling behavior natively. `false` = Tab, `true` =
    // Shift+Tab.

    #[test]
    fn tab_forward_at_last_wraps_to_first() {
        // Tab on the last focusable → override, focus the first.
        assert_eq!(tab_wrap(FocusPos::Last, false), Some(TabWrap::First));
    }

    #[test]
    fn tab_backward_at_first_wraps_to_last() {
        // Shift+Tab on the first focusable → override, focus the last.
        assert_eq!(tab_wrap(FocusPos::First, true), Some(TabWrap::Last));
    }

    #[test]
    fn tab_in_the_interior_passes_through() {
        // Between the edges → let the browser move focus naturally, both
        // directions.
        assert_eq!(tab_wrap(FocusPos::Interior, false), None);
        assert_eq!(tab_wrap(FocusPos::Interior, true), None);
    }

    #[test]
    fn tab_toward_the_far_edge_passes_through() {
        // Forward at the first / backward at the last are interior moves
        // the browser handles — no wrap.
        assert_eq!(tab_wrap(FocusPos::First, false), None);
        assert_eq!(tab_wrap(FocusPos::Last, true), None);
    }

    #[test]
    fn tab_recaptures_focus_that_escaped_the_panel() {
        // Focus left the panel (a background element grabbed it) → pull
        // it back to the first focusable, regardless of direction.
        assert_eq!(tab_wrap(FocusPos::Outside, false), Some(TabWrap::First));
        assert_eq!(tab_wrap(FocusPos::Outside, true), Some(TabWrap::First));
    }

    #[test]
    fn tab_in_an_empty_panel_parks_on_the_panel() {
        // No focusable descendants → park focus on the panel itself
        // (which carries tabindex="-1"), whatever the direction.
        assert_eq!(tab_wrap(FocusPos::Empty, false), Some(TabWrap::Panel));
        assert_eq!(tab_wrap(FocusPos::Empty, true), Some(TabWrap::Panel));
    }

    #[test]
    fn a_single_focusable_wraps_to_itself_both_directions() {
        // The sole focusable is simultaneously first and last: Tab and
        // Shift+Tab both override and re-focus it (First and Last resolve
        // to the same node in `cycle_tab`).
        assert_eq!(tab_wrap(FocusPos::Only, false), Some(TabWrap::First));
        assert_eq!(tab_wrap(FocusPos::Only, true), Some(TabWrap::Last));
    }

    // ── initial-focus target ───────────────────────────────────────
    // `focus_initial`'s DOM shell queries focusable descendants and then
    // defers *where focus lands* to `initial_focus`; these pin that
    // decision natively, so "takes initial focus on open" is a tested
    // claim rather than a browser-only one.

    #[test]
    fn initial_focus_prefers_the_first_focusable() {
        assert_eq!(initial_focus(true), InitialFocus::First);
    }

    #[test]
    fn initial_focus_falls_back_to_the_panel_when_empty() {
        // Nothing tabbable inside → park on the panel itself (tabindex=-1).
        assert_eq!(initial_focus(false), InitialFocus::Panel);
    }

    // ── restore-to-opener predicate ────────────────────────────────
    // `use_overlay_layer_with`'s on_cleanup reads DOM connectivity and
    // ownership, then defers the restore decision to `should_restore`;
    // these pin the "restores focus to the opener on close" guarantee —
    // and, critically, the negative case that keeps a drawer closing
    // beneath a live modal from stealing focus.

    #[test]
    fn owner_with_a_connected_opener_restores() {
        // Both focus-participating policies restore when they still own
        // focus and the opener is live — the modal-closes and
        // drawer-closes-alone happy paths.
        assert!(should_restore(true, FocusPolicy::Trap, true));
        assert!(should_restore(true, FocusPolicy::Capture, true));
    }

    #[test]
    fn shadowed_layer_does_not_restore() {
        // A drawer unmounting beneath a live modal no longer owns focus;
        // restoring to its opener would yank focus out of the modal.
        assert!(!should_restore(false, FocusPolicy::Capture, true));
        assert!(!should_restore(false, FocusPolicy::Trap, true));
    }

    #[test]
    fn a_disconnected_opener_is_not_refocused() {
        // The opener left the document while the overlay was open —
        // focusing a detached node is a no-op at best, so skip it.
        assert!(!should_restore(true, FocusPolicy::Trap, false));
    }

    #[test]
    fn none_policy_never_restores() {
        // ActionsMenu (None) manages its own restore-to-trigger; the
        // overlay layer must not double-restore on its behalf.
        assert!(!should_restore(true, FocusPolicy::None, true));
    }

    // ── rendered-candidate filter ──────────────────────────────────
    // `focusable_descendants` drops selector matches that aren't
    // rendered, so Tab-wrap can't park focus on a `display:none`
    // element (hidden tab pane, collapsed section) where it would
    // silently vanish; `is_rendered` is that filter's pure predicate.

    #[test]
    fn display_none_candidates_are_not_rendered() {
        // display:none (self or ancestor) zeroes the offset box and
        // yields no client rects.
        assert!(!is_rendered(0, 0, 0));
    }

    #[test]
    fn boxed_candidates_are_rendered() {
        assert!(is_rendered(120, 32, 1));
    }

    #[test]
    fn zero_box_but_rect_bearing_candidates_stay_tabbable() {
        // Inline links and display:contents hosts can report a zero
        // offset box while still painting client rects — they are
        // visible and must stay in the tab order.
        assert!(is_rendered(0, 0, 2));
    }
}
