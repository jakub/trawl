// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/login` — thin wrapper over [`fleet_ui::Login`]: error mapping +
//! hard redirect to `/search` on success. The empty-key check lives in
//! the fleet component.
//!
//! The `Atmosphere` mesh-gradient backdrop (ADR-0012) mounts as a
//! sibling above `<Login/>`, matching the fleet-ui workbench
//! arrangement: the backdrop layer is fixed, full-viewport,
//! `z-index: -1`, so the login card composes above it with no stacking
//! work here. That only paints because fleet-ui's `.login-shell`
//! declares no opaque background: an opaque normal-flow block would
//! occlude the z-index:-1 canvas, so don't add one in `main.css`.

use fleet_ui::{Atmosphere, UiPrefs};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

#[component]
pub fn Login() -> impl IntoView {
    let prefs = expect_context::<UiPrefs>();
    let (error, set_error) = signal::<Option<String>>(None);
    let (submitting, set_submitting) = signal(false);
    let (invalid, set_invalid) = signal(false);

    let on_submit = Callback::new(move |key: String| {
        set_submitting.set(true);
        set_error.set(None);
        set_invalid.set(false);

        spawn_local(async move {
            match api::login(&key).await {
                Ok(_) => {
                    if let Some(win) = web_sys::window() {
                        let _ = win.location().set_href("/search");
                    }
                }
                Err(api::ApiError::Unauthorized) => {
                    set_error.try_set(Some("Invalid API key".into()));
                    set_invalid.try_set(true);
                    set_submitting.try_set(false);
                }
                Err(e) => {
                    set_error.try_set(Some(format!("Login failed: {e}")));
                    set_submitting.try_set(false);
                }
            }
        });
    });

    // Empty accent, unlike the command bar's "_": the login wordmark is
    // a plain "trawl", and the empty accent span renders nothing.
    view! {
        <Atmosphere theme=prefs.theme()/>
        <div class="trawl-login">
            <fleet_ui::Login
                brand="trawl"
                brand_accent=""
                heading="Search your logs."
                subtitle="Sign in with your API key to start."
                on_submit=on_submit
                error=error
                submitting=submitting
                invalid=invalid
            />
            <aside class="trawl-login-help" aria-label="Get an API key">
                <p>"Ask your Trawl operator for a personal API key."</p>
                <p>"Setting up Trawl? "<a href="https://trawl.sh/operate/access/#create-roles-and-keys" target="_blank" rel="noopener noreferrer">"Create your first API key"</a>"."</p>
            </aside>
        </div>
    }
}
