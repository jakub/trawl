// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Async clipboard write (wasm-only — there is no clipboard off the
//! browser). [`crate::copy_button::CopyButton`] is the toast-wired UI
//! over it; the bare helper stays public for programmatic copies.

use wasm_bindgen_futures::JsFuture;

/// Write `text` to the system clipboard via the async Clipboard API.
///
/// # Errors
///
/// Returns the stringified JS error when the browser rejects the
/// write (no permission, no focus, no secure context).
pub async fn write_clipboard(text: &str) -> Result<(), String> {
    let window = leptos::web_sys::window().ok_or_else(|| "no window".to_string())?;
    let clipboard = window.navigator().clipboard();
    let promise = clipboard.write_text(text);
    JsFuture::from(promise)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}
