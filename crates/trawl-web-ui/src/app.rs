// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Top-level `<App/>` component and router.

use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

use crate::pages::{login::Login, search::Search};
use crate::state::theme;

#[component]
pub fn App() -> impl IntoView {
    // Wire UI prefs (theme/density/rowstyle) to <html data-*> + localStorage
    // before any route mounts. provide_context lets descendants pick up
    // the signals without prop-drilling.
    let prefs = theme::install();
    provide_context(prefs);

    view! {
        <Router>
            <Routes fallback=|| view! { <NotFound/> }>
                <Route path=path!("/")       view=RedirectToSearch/>
                <Route path=path!("/login")  view=Login/>
                <Route path=path!("/search") view=Search/>
            </Routes>
        </Router>
    }
}

#[component]
fn RedirectToSearch() -> impl IntoView {
    Effect::new(|_| {
        if let Some(win) = web_sys::window() {
            let _ = win.location().set_href("/search");
        }
    });
    view! { <p>"redirecting…"</p> }
}

#[component]
fn NotFound() -> impl IntoView {
    view! {
        <div class="login-shell">
            <div class="login-card">
                <h1>"404"</h1>
                <p class="subtitle">"that page does not exist."</p>
            </div>
        </div>
    }
}
