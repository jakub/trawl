// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wasm-only `<Login/>` component — the leptos wiring around the pure
//! [`validate`](super::validate) helpers.
//!
//! Brand-skinned card with one password field and a submit button.
//! All async work — calling the auth endpoint, surfacing errors,
//! navigating on success — lives in the caller's `on_submit` closure,
//! which receives just the entered API key. The consumer captures its
//! own post-login redirect target. The component is a pure form; the
//! consumer drives `error` and `submitting` signals to display
//! validation feedback and the spinner state.

use leptos::ev::SubmitEvent;
use leptos::prelude::*;

use super::validate::{combined, validate_key};
use crate::button::{Btn, Variant};
use crate::error_banner::ErrorBanner;
use crate::field::Field;

#[component]
pub fn Login(
    #[prop(into)] brand: String,
    #[prop(into)] brand_accent: String,
    on_submit: Callback<String>,
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into)] submitting: Signal<bool>,
    /// Credential rejection, distinct from service or network errors.
    #[prop(optional, into)]
    invalid: Signal<bool>,
) -> impl IntoView {
    let (api_key, set_api_key) = signal(String::new());
    let (local_error, set_local_error) = signal::<Option<String>>(None);
    let input_ref = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if (local_error.get().is_some() || invalid.get())
            && let Some(input) = input_ref.get()
        {
            let _ = input.focus();
        }
    });

    let on_form_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let key = api_key.get();
        if let Err(msg) = validate_key(&key) {
            set_local_error.set(Some(msg.into()));
            return;
        }
        set_local_error.set(None);
        on_submit.run(key);
    };

    // Local validation wins over the external error signal: if the user
    // hits submit with an empty key after a prior failed attempt, they
    // need to see "API key is required", not the stale server message.
    let combined_error = Signal::derive(move || combined(local_error.get(), error.get()));

    view! {
        <div class="login-shell">
            <form class="login-card" on:submit=on_form_submit>
                <h1>
                    <span>{brand}</span><span class="accent">{brand_accent}</span>
                </h1>
                <p class="subtitle">"Sign in with your API key"</p>

                // ErrorBanner carries the role="alert" strip. Field's
                // wrap mode renders label.field > span > input, whose
                // unstyled <span> caption is the point: the default
                // Field's styled <label> would visibly restyle it.
                <ErrorBanner error=combined_error id="fleet-login-error"/>

                <Field label="API key" wrap=true>
                    <input
                        node_ref=input_ref
                        type="password"
                        autocomplete="off"
                        spellcheck="false"
                        aria-invalid=move || (local_error.get().is_some() || invalid.get()).to_string()
                        aria-describedby=move || combined_error.get().map(|_| "fleet-login-error")
                        prop:value=move || api_key.get()
                        on:input=move |ev| {
                            set_api_key.set(event_target_value(&ev));
                            set_local_error.set(None);
                        }
                    />
                </Field>

                // No on_click — a <button> inside a <form> defaults to
                // type=submit, so the form's on:submit drives the flow.
                <Btn variant=Variant::Form full=true disabled=submitting>
                    {move || if submitting.get() { "Signing In…" } else { "Sign In" }}
                </Btn>
            </form>
        </div>
    }
}
