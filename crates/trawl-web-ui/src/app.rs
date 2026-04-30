// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Top-level `<App/>` component and router.

use leptos::prelude::*;
use leptos_router::components::{ParentRoute, Route, Router, Routes};
use leptos_router::path;

use crate::pages::history::HistoryPage;
use crate::pages::layout::{NotFound, RedirectTo, Shell};
use crate::pages::login::Login;
use crate::pages::nets::NetsPage;
use crate::pages::placeholder::{IntelPlaceholder, SettingsPlaceholder};
use crate::pages::runs::RunsPage;
use crate::pages::schema::SchemaPage;
use crate::pages::search::Search;
use crate::state::theme;

#[component]
pub fn App() -> impl IntoView {
    let prefs = theme::install();
    provide_context(prefs);

    view! {
        <Router>
            <Routes fallback=|| view! { <NotFound/> }>
                <Route path=path!("/login") view=Login/>
                <ParentRoute path=path!("") view=Shell>
                    <Route path=path!("/") view=|| view! { <RedirectTo path="/search"/> }/>
                    <Route path=path!("/search") view=Search/>
                    <Route path=path!("/search/history") view=HistoryPage/>
                    <Route path=path!("/search/schema") view=SchemaPage/>
                    <Route path=path!("/jobs") view=|| view! { <RedirectTo path="/jobs/nets"/> }/>
                    <Route path=path!("/jobs/nets") view=NetsPage/>
                    <Route path=path!("/jobs/runs") view=RunsPage/>
                    <Route path=path!("/intel") view=|| view! { <RedirectTo path="/intel/stories"/> }/>
                    <Route path=path!("/intel/stories") view=IntelPlaceholder/>
                    <Route path=path!("/intel/stories/:id") view=IntelPlaceholder/>
                    <Route path=path!("/intel/queue") view=IntelPlaceholder/>
                    <Route path=path!("/intel/entities") view=IntelPlaceholder/>
                    <Route path=path!("/intel/sources") view=IntelPlaceholder/>
                    <Route path=path!("/settings") view=SettingsPlaceholder/>
                </ParentRoute>
            </Routes>
        </Router>
    }
}
