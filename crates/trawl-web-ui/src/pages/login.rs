// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/login` — paste API key, submit, redirect to `/search`.

use leptos::ev::SubmitEvent;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

#[component]
pub fn Login() -> impl IntoView {
    let (api_key, set_api_key) = signal(String::new());
    let (error, set_error) = signal::<Option<String>>(None);
    let (submitting, set_submitting) = signal(false);

    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let key = api_key.get();
        if key.trim().is_empty() {
            set_error.set(Some("API key is required".into()));
            return;
        }

        set_submitting.set(true);
        set_error.set(None);

        spawn_local(async move {
            match api::login(&key).await {
                Ok(_) => {
                    if let Some(win) = web_sys::window() {
                        let _ = win.location().set_href("/search");
                    }
                }
                Err(api::ApiError::Unauthorized) => {
                    set_error.set(Some("Invalid API key".into()));
                    set_submitting.set(false);
                }
                Err(e) => {
                    set_error.set(Some(format!("Login failed: {e}")));
                    set_submitting.set(false);
                }
            }
        });
    };

    view! {
        <div class="login-shell">
            <form class="login-card" on:submit=on_submit>
                <h1>"trawl"</h1>
                <p class="subtitle">"sign in with your API key"</p>

                {move || error.get().map(|msg| view! { <div class="error">{msg}</div> })}

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
