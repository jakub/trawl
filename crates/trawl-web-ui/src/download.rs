// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use wasm_bindgen::JsCast;
use web_sys::{Blob, BlobPropertyBag, HtmlAnchorElement, Url};

pub fn trigger_download(bytes: &[u8], filename: &str, mime: &str) -> Result<(), String> {
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&array.buffer());

    let opts = BlobPropertyBag::new();
    opts.set_type(mime);
    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &opts)
        .map_err(|e| format!("{e:?}"))?;

    let url = Url::create_object_url_with_blob(&blob).map_err(|e| format!("{e:?}"))?;

    let document = leptos::web_sys::window()
        .ok_or("no window")?
        .document()
        .ok_or("no document")?;
    let a: HtmlAnchorElement = document
        .create_element("a")
        .map_err(|e| format!("{e:?}"))?
        .unchecked_into();

    a.set_href(&url);
    a.set_download(filename);
    let _ = a.style().set_property("display", "none");
    document
        .body()
        .ok_or("no body")?
        .append_child(&a)
        .map_err(|e| format!("{e:?}"))?;
    a.click();
    let _ = document.body().ok_or("no body")?.remove_child(&a);

    let _ = Url::revoke_object_url(&url);
    Ok(())
}
