// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use wasm_bindgen_futures::JsFuture;

pub async fn write_clipboard(text: &str) -> Result<(), String> {
    let window = leptos::web_sys::window().ok_or_else(|| "no window".to_string())?;
    let clipboard = window.navigator().clipboard();
    let promise = clipboard.write_text(text);
    JsFuture::from(promise)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}
