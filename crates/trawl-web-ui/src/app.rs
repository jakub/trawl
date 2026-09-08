// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Top-level `<App/>` component and router.

use leptos::prelude::*;
use leptos_router::components::{ParentRoute, Route, Router, Routes};
use leptos_router::path;

use crate::pages::health::HealthPage;
use crate::pages::history::HistoryPage;
use crate::pages::layout::{AuthShell, NotFound, RedirectTo};
use crate::pages::login::Login;
use crate::pages::nets::NetsPage;
use crate::pages::placeholder::SettingsPlaceholder;
use crate::pages::runs::RunsPage;
use crate::pages::schema::SchemaPage;
use crate::pages::search::Search;

#[component]
pub fn App() -> impl IntoView {
    let prefs = fleet_ui::install("trawl.ui");
    provide_context(prefs);

    // The shared 30s "now" tick every relative label subscribes to; installed
    // once at the root so a page of labels costs one timer, not one each.
    fleet_ui::time::clock::install();

    view! {
        <Router>
            <Routes fallback=|| view! { <NotFound/> }>
                <Route path=path!("/login") view=Login/>
                <ParentRoute path=path!("") view=AuthShell>
                    <Route path=path!("/") view=|| view! { <RedirectTo path="/search"/> }/>
                    <Route path=path!("/search") view=Search/>
                    <Route path=path!("/search/history") view=HistoryPage/>
                    <Route path=path!("/search/schema") view=SchemaPage/>
                    <Route path=path!("/jobs") view=|| view! { <RedirectTo path="/jobs/nets"/> }/>
                    <Route path=path!("/jobs/nets") view=NetsPage/>
                    <Route path=path!("/jobs/runs") view=RunsPage/>
                    <Route path=path!("/settings/health") view=HealthPage/>
                    <Route path=path!("/settings") view=SettingsPlaceholder/>
                </ParentRoute>
            </Routes>
        </Router>
    }
}
