// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/login` — thin wrapper over [`fleet_ui::Login`]: error mapping +
//! hard redirect to `/search` on success. The empty-key check lives in
//! the fleet component.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

#[component]
pub fn Login() -> impl IntoView {
    let (error, set_error) = signal::<Option<String>>(None);
    let (submitting, set_submitting) = signal(false);

    let on_submit = Callback::new(move |key: String| {
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
    });

    // brand_accent="" (not the topbar's "_"): the pre-migration login
    // h1 was plain accent "trawl" with no accent glyph, and zero visual
    // change is the contract. The empty accent span renders nothing.
    view! {
        <fleet_ui::Login
            brand="trawl"
            brand_accent=""
            on_submit=on_submit
            error=error
            submitting=submitting
        />
    }
}
