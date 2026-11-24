// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use fleet_ui::ToastKind;

#[test]
fn toast_kind_is_native_and_copy() {
    let k = ToastKind::Success;
    let copy = k;
    assert_eq!(k, copy);
    assert_ne!(ToastKind::Info, ToastKind::Error);
}

#[test]
fn toast_kind_css_class_mapping() {
    assert_eq!(ToastKind::Info.as_class(), "info");
    assert_eq!(ToastKind::Success.as_class(), "success");
    assert_eq!(ToastKind::Error.as_class(), "error");
}
