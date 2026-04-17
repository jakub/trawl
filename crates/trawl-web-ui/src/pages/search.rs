// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/search` — hero screen (editor, results, live-tail).
//!
//! This commit only wires the chrome — the editor, results table, facets,
//! and live-tail come in subsequent commits.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::editor::DslEditor;

#[component]
pub fn Search() -> impl IntoView {
    let me = RwSignal::new(None::<api::MeResponse>);
    let redirect_to_login = RwSignal::new(false);

    // Hoisted out of the view closure so they persist across reactive
    // re-renders. Previously these were constructed *inside* the
    // `move ||` block below — which made them reactive dependents of
    // `me.get()`, so when `/me` resolved (None → Some), leptos tore
    // down the old signal + editor and mounted fresh ones, discarding
    // any query text the user had typed during the in-flight fetch.
    let query = RwSignal::new(String::from(
        "service=nginx level=error last=1h | stats count() by host",
    ));
    let on_submit = Callback::new(move |()| {
        web_sys::console::log_1(&format!("run: {}", query.get()).into());
    });

    // On mount: fetch /me. On 401, redirect to /login.
    Effect::new(move |_| {
        spawn_local(async move {
            match api::me().await {
                Ok(resp) => me.set(Some(resp)),
                Err(_) => redirect_to_login.set(true),
            }
        });
    });

    Effect::new(move |_| {
        if redirect_to_login.get()
            && let Some(win) = web_sys::window()
        {
            let _ = win.location().set_href("/login");
        }
    });

    let on_logout = move |_| {
        spawn_local(async move {
            let _ = api::logout().await;
            if let Some(win) = web_sys::window() {
                let _ = win.location().set_href("/login");
            }
        });
    };

    view! {
        <div class="shell">
            <header class="topbar">
                <h1>"trawl"</h1>
                <div class="spacer"></div>
                <span class="user">
                    {move || me.get().map(|m| format!("{} · {}", m.name, m.role))}
                </span>
                <button class="btn-link" on:click=on_logout>"logout"</button>
            </header>
            <main class="main">
                // `<Show>` only renders children once `me` is `Some`, so the
                // editor doesn't mount during the brief 401 window — AND,
                // critically, doesn't remount when `me` transitions. The
                // hoisted `query` signal is captured by reference, so
                // whatever the user types survives auth resolution.
                <Show when=move || me.get().is_some() fallback=|| ()>
                    <DslEditor query=query on_submit=on_submit/>
                </Show>
            </main>
        </div>
    }
}
