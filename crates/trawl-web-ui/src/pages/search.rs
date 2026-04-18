// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/search` — hero screen (editor, results).

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::editor::DslEditor;
use crate::components::facet_sidebar::FacetSidebar;
use crate::components::results_table::ResultsTable;
use crate::state::query::{go_to, url_signals};
use crate::state::search_session::rows_resource;

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
    let query_text = RwSignal::new(String::new());

    // URL-driven signals: the executed query + page come from `?q=` and
    // `?page=` on every navigation (including back/forward).
    let (executed_q, page) = url_signals();

    // Keep the editor buffer in sync with the URL on first load and on
    // back/forward — but only when the editor hasn't diverged from the
    // last-executed query (i.e. the user isn't mid-typing). The equality
    // check prevents clobbering in-progress edits while still rehydrating
    // from a pasted/shared URL.
    Effect::new(move |_| {
        let url_q = executed_q.get();
        let buf = query_text.get_untracked();
        if buf.is_empty() || buf == url_q {
            query_text.set(url_q);
        }
    });

    let rows = rows_resource(executed_q, page);

    let on_submit = Callback::new(move |()| {
        // Submit resets to page 0; push to URL — the effect above wires
        // the resource refetch off (executed_q, page).
        go_to(&query_text.get_untracked(), 0, false);
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
                <Show when=move || me.get().is_some() fallback=|| ()>
                    <div class="search-layout">
                        <FacetSidebar rows=rows/>
                        <div class="search-col">
                            <DslEditor query=query_text on_submit=on_submit/>
                            <ResultsTable
                                executed_q=Signal::derive(move || executed_q.get())
                                page=Signal::derive(move || page.get())
                                rows=rows
                            />
                        </div>
                    </div>
                </Show>
            </main>
        </div>
    }
}
