// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<CopyButton/>` — click-to-copy wired to the toast system.
//!
//! Passing `class` renders a bare `<button class=…>`, and the caller's
//! class owns the ENTIRE look, including the button reset: fleet-ui ships
//! no rule for a class it does not name, so a bare trigger whose class only
//! sets colour and font arrives with the browser's grey fill and bevel.

use crate::toast::ToastKind;

pub fn copy_toast(
    result: Result<(), String>,
    success_detail: Option<String>,
) -> (ToastKind, &'static str, Option<String>) {
    match result {
        Ok(()) => (ToastKind::Success, "Copied", success_detail),
        Err(msg) => (ToastKind::Error, "Copy failed", Some(msg)),
    }
}

#[cfg(target_arch = "wasm32")]
mod component {
    use leptos::prelude::*;
    use leptos::task::spawn_local;

    use super::copy_toast;
    use crate::button::{Btn, Variant};
    use crate::clipboard::write_clipboard;
    use crate::toast::ToastBus;

    /// `text` resolves lazily at click time (derive it for values that change
    /// under the trigger, e.g. the current URL).
    #[component]
    pub fn CopyButton(
        #[prop(into)] text: Signal<String>,
        #[prop(optional)] class: Option<&'static str>,
        #[prop(optional, into)] success_detail: Option<String>,
        children: Children,
    ) -> impl IntoView {
        let bus = expect_context::<ToastBus>();

        let do_copy = move || {
            let value = text.get_untracked();
            let detail = success_detail.clone();
            spawn_local(async move {
                let (kind, title, detail) = copy_toast(write_clipboard(&value).await, detail);
                bus.push(kind, title, detail);
            });
        };

        match class {
            // The click stops here: a consumer may nest this trigger inside an
            // ancestor that handles pointer events.
            Some(cls) => view! {
                <button
                    type="button"
                    class=cls
                    on:click=move |e: leptos::web_sys::MouseEvent| {
                        e.stop_propagation();
                        do_copy();
                    }
                >{children()}</button>
            }
            .into_any(),
            None => view! {
                <Btn
                    variant=Variant::Secondary
                    stop_propagation=true
                    on_click=Callback::new(move |()| do_copy())
                >{children()}</Btn>
            }
            .into_any(),
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use component::CopyButton;

#[cfg(test)]
mod tests {
    use super::copy_toast;
    use crate::toast::{ToastKind, ToastStack};

    #[test]
    fn copy_success_fires_a_copied_toast() {
        let (kind, title, detail) = copy_toast(Ok(()), Some("share URL copied".into()));
        let mut stack = ToastStack::new();
        stack.push(kind, title, detail);

        let items = stack.items();
        assert_eq!(items.len(), 1, "a copy fires exactly one toast");
        assert_eq!(items[0].kind(), ToastKind::Success);
        assert_eq!(items[0].title(), "Copied");
        assert_eq!(items[0].detail(), Some("share URL copied"));
    }

    #[test]
    fn copy_failure_fires_an_error_toast_carrying_the_reason() {
        let (kind, title, detail) = copy_toast(
            Err("NotAllowedError: no permission".into()),
            Some("share URL copied".into()),
        );
        let mut stack = ToastStack::new();
        stack.push(kind, title, detail);

        let items = stack.items();
        assert_eq!(items[0].kind(), ToastKind::Error);
        assert_eq!(items[0].title(), "Copy failed");
        assert_eq!(items[0].detail(), Some("NotAllowedError: no permission"));
    }
}
