// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Login/>` — generic API-key sign-in form.
//!
//! Brand-skinned card with one password field and a submit button.
//! All async work — calling the auth endpoint, surfacing errors,
//! navigating on success — lives in the caller's `on_submit` closure,
//! which receives the key string and the configured
//! `post_login_redirect` path. The component is a pure form; the
//! consumer drives `error` and `submitting` signals to display
//! validation feedback and the spinner state.

use leptos::ev::SubmitEvent;
use leptos::prelude::*;

#[component]
pub fn Login(
    brand: &'static str,
    brand_accent: &'static str,
    post_login_redirect: &'static str,
    on_submit: Callback<(String, &'static str)>,
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into)] submitting: Signal<bool>,
) -> impl IntoView {
    let (api_key, set_api_key) = signal(String::new());
    let (local_error, set_local_error) = signal::<Option<String>>(None);

    let on_form_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let key = api_key.get();
        if key.trim().is_empty() {
            set_local_error.set(Some("API key is required".into()));
            return;
        }
        set_local_error.set(None);
        on_submit.run((key, post_login_redirect));
    };

    let combined_error = Signal::derive(move || error.get().or_else(|| local_error.get()));

    view! {
        <div class="login-shell">
            <form class="login-card" on:submit=on_form_submit>
                <h1>
                    <span>{brand}</span><span class="amber">{brand_accent}</span>
                </h1>
                <p class="subtitle">"sign in with your API key"</p>

                {move || combined_error.get().map(|msg| view! { <div class="error">{msg}</div> })}

                <label class="field">
                    <span>"API key"</span>
                    <input
                        type="password"
                        autocomplete="off"
                        spellcheck="false"
                        prop:value=move || api_key.get()
                        on:input=move |ev| set_api_key.set(event_target_value(&ev))
                    />
                </label>

                <button
                    type="submit"
                    class="btn btn-full"
                    disabled=move || submitting.get()
                >
                    {move || if submitting.get() { "Signing In…" } else { "Sign In" }}
                </button>
            </form>
        </div>
    }
}
