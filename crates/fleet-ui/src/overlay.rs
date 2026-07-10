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
//! ([`push_overlay`]) and releases it on unmount
//! ([`OverlayLayer::release`]). The stack is LIFO, so the
//! most-recently-mounted overlay is topmost; each Escape handler guards
//! on [`OverlayLayer::is_topmost`] and the non-topmost layers no-op.
//! One Escape therefore closes exactly one layer.
//!
//! The stack is a pure `thread_local` — wasm is single-threaded, and
//! keeping it free of `leptos` / `web_sys` lets the LIFO arbitration
//! contract be exercised by native `cargo nextest run --workspace`
//! (matching [`theme::prefs`](crate::theme::prefs)). Only the
//! [`use_overlay_layer`] Leptos glue is wasm-only.

use std::cell::RefCell;

thread_local! {
    /// LIFO stack of live overlay ids; the last entry is topmost.
    static STACK: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
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
        STACK.with_borrow(|s| s.last() == Some(&self.0))
    }

    /// Remove this layer from the arbitration stack. Called from the
    /// owning component's `on_cleanup` on unmount. Idempotent.
    pub fn release(self) {
        STACK.with_borrow_mut(|s| {
            if let Some(pos) = s.iter().rposition(|&id| id == self.0) {
                s.remove(pos);
            }
        });
    }
}

/// Push a new topmost overlay layer onto the arbitration stack.
#[cfg(any(target_arch = "wasm32", test))]
#[must_use]
pub(crate) fn push_overlay() -> OverlayLayer {
    let id = NEXT_ID.with_borrow_mut(|n| {
        let id = *n;
        *n += 1;
        id
    });
    STACK.with_borrow_mut(|s| s.push(id));
    OverlayLayer(id)
}

/// Register an overlay layer for the lifetime of the current reactive
/// owner: pushes on mount and releases via `on_cleanup` on unmount.
/// Returns the `Copy` handle for the component's Escape guard.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn use_overlay_layer() -> OverlayLayer {
    use leptos::prelude::on_cleanup;
    let layer = push_overlay();
    on_cleanup(move || layer.release());
    layer
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests balance every push with a release so they stay robust to
    // any leftover thread-local state when the harness reuses a thread.

    #[test]
    fn topmost_is_last_pushed() {
        let a = push_overlay();
        let b = push_overlay();
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
        let menu = push_overlay();
        let modal = push_overlay();
        assert!(modal.is_topmost(), "stacked modal takes Escape");
        assert!(!menu.is_topmost(), "shadowed menu must not also close");

        modal.release();
        assert!(menu.is_topmost(), "menu regains Escape after the modal");
        menu.release();
    }

    #[test]
    fn release_is_idempotent_and_order_independent() {
        let a = push_overlay();
        let b = push_overlay();
        let c = push_overlay();

        // Release a middle layer: neither a nor c is disturbed.
        b.release();
        b.release(); // idempotent — no panic, no effect
        assert!(c.is_topmost(), "middle release leaves the top alone");
        assert!(!a.is_topmost());

        c.release();
        assert!(a.is_topmost(), "only the base layer remains");
        a.release();
    }
}
