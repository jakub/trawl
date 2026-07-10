// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native compile + contract test for `ToastKind` (issue #27, fleet-ui
//! change 1: "`ToastKind` goes native").
//!
//! This test intentionally runs on the host target: before the
//! toast-module split, `fleet_ui::ToastKind` only existed under
//! `cfg(target_arch = "wasm32")`, forcing coastwatch to re-port the
//! whole toast module just to name the enum in native-compiling code.

use fleet_ui::ToastKind;

#[test]
fn toast_kind_is_native_and_copy() {
    let k = ToastKind::Success;
    let copy = k; // Copy, not move
    assert_eq!(k, copy);
    assert_ne!(ToastKind::Info, ToastKind::Error);
}

#[test]
fn toast_kind_css_class_mapping() {
    // The class suffix rendered by <Toasts/> (`toast {class}`) — the
    // contract fleet-ui.css's `.toast.{info,success,error}` selectors
    // depend on.
    assert_eq!(ToastKind::Info.as_class(), "info");
    assert_eq!(ToastKind::Success.as_class(), "success");
    assert_eq!(ToastKind::Error.as_class(), "error");
}
