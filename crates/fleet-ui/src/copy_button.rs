// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<CopyButton/>` — click-to-copy wired to the toast system.
//!
//! The trigger is a secondary [`Btn`](crate::button::Btn) by default;
//! passing `class` renders a bare `<span class=…>` instead — for
//! app-styled inline triggers like trawl's editor `.tool` links,
//! keeping app classes out of fleet-ui. Success and error report
//! through the [`ToastBus`](crate::toast::ToastBus) the `Shell`
//! provides via context, with the canonical "Copied" / "Copy failed"
//! titles; `success_detail` is the optional app-flavored second line.
//!
//! The "which toast fires" decision lives in the pure [`copy_toast`]
//! function so the click → toast outcome is a native `nextest` fact
//! (see this module's tests), mirroring [`crate::toast::stack`], where
//! the toast *stack* transitions are likewise unit-tested off-target.
//! The wasm component below is the DOM glue that runs the async
//! clipboard write and pushes the pure decision onto the bus.

use crate::toast::ToastKind;

/// Map a clipboard-write result to the toast the `CopyButton` fires.
///
/// `Ok` → a `Success` toast titled `"Copied"`, carrying the optional
/// app-flavored `success_detail` as its second line. `Err` → an
/// `Error` toast titled `"Copy failed"`, carrying the browser's reject
/// reason so a failed copy leaves a visible trace instead of nothing.
///
/// Pure and target-agnostic: the wasm component feeds it
/// `write_clipboard(&text).await` and pushes the tuple straight onto
/// the [`ToastBus`](crate::toast::ToastBus), so testing this function
/// against a [`ToastStack`](crate::toast::ToastStack) exercises the
/// exact toast a real copy produces.
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

    /// Click-to-copy trigger. `text` resolves lazily at click time (derive
    /// it for values that change under the trigger, e.g. the current URL);
    /// `children` is the trigger's label/icon slot.
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
            // Bare mode: an app-styled inline trigger. stop_propagation for
            // the same reason as Btn's flag — copy triggers live inside
            // clickable rows/headers.
            Some(cls) => view! {
                <span
                    class=cls
                    on:click=move |e: leptos::web_sys::MouseEvent| {
                        e.stop_propagation();
                        do_copy();
                    }
                >{children()}</span>
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

    /// A successful copy pushes exactly one `Success`/"Copied" toast
    /// onto the bus's stack, carrying the app's success detail line.
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

    /// A rejected clipboard write fires an `Error`/"Copy failed" toast
    /// carrying the browser's reason — a failed copy leaves a trace
    /// rather than silently doing nothing, and the success detail line
    /// must not leak into it.
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
