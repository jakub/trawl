// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minimal Shell composition demo — proves the public API can be
//! wired by an app that isn't trawl. Builds under wasm32 via:
//!
//! ```sh
//! cargo build -p fleet-ui --example shell_demo --target wasm32-unknown-unknown
//! ```
//!
//! For an actual rendered preview, a consumer would wire this into
//! a `Trunk.toml` with `index.html` and `trunk serve`. The CI gate
//! is the wasm compile; visual verification is manual.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // The demo composes wasm-only fleet-ui components. Native build
    // is a no-op so `cargo clippy --workspace --all-targets` (which
    // doesn't cross-compile) stays green.
}

#[cfg(target_arch = "wasm32")]
// TopBar and Rail are exported via fleet-ui but mounted internally by
// Shell — referencing them here would duplicate the chrome.
use fleet_ui::{AppLink, Icon, Login, ModeTab, RailItem, Shell, UserInfo, install};
#[cfg(target_arch = "wasm32")]
use leptos::prelude::*;
#[cfg(target_arch = "wasm32")]
use leptos_router::components::{Route, Router, Routes};
#[cfg(target_arch = "wasm32")]
use leptos_router::path;

#[cfg(target_arch = "wasm32")]
fn rail_items() -> Vec<RailItem> {
    vec![
        RailItem {
            id: "home".into(),
            label: "Home".into(),
            icon: Icon::Grid,
            path: "/".into(),
            badge: None,
        },
        RailItem {
            id: "search".into(),
            label: "Search".into(),
            icon: Icon::Search,
            path: "/search".into(),
            badge: None,
        },
        RailItem {
            id: "alerts".into(),
            label: "Alerts".into(),
            icon: Icon::Alert,
            path: "/alerts".into(),
            // Count chip — the slot coastwatch previously smuggled into
            // the label text ("Editions (N)").
            badge: Some(3),
        },
    ]
}

#[cfg(target_arch = "wasm32")]
fn app_links() -> Vec<AppLink> {
    vec![
        AppLink {
            label: "trawl".into(),
            href: "https://trawl.example/".into(),
            active: false,
        },
        AppLink {
            label: "demo".into(),
            href: "/".into(),
            active: true,
        },
    ]
}

#[cfg(target_arch = "wasm32")]
fn modes() -> Vec<ModeTab> {
    vec![
        ModeTab {
            id: "logs".into(),
            label: "Logs".into(),
            path: "/".into(),
            active: true,
        },
        ModeTab {
            id: "settings".into(),
            label: "Settings".into(),
            path: "/settings".into(),
            active: false,
        },
    ]
}

#[cfg(target_arch = "wasm32")]
#[component]
fn DemoApp() -> impl IntoView {
    let rail_items_sig = Signal::derive(rail_items);
    let app_links_sig = Signal::derive(app_links);
    let rail_active = Signal::derive(|| "home".to_string());
    let modes_sig = Signal::derive(modes);
    let user = Signal::derive(|| {
        Some(UserInfo {
            name: "demo user".into(),
            detail: "admin".into(),
        })
    });

    view! {
        <Router>
            <Routes fallback=|| view! { <p>"…"</p> }>
                <Route path=path!("/login") view=move || view! {
                    <Login
                        brand="demo"
                        brand_accent="·"
                        on_submit=Callback::new(|_key: String| {})
                        error=Signal::derive(|| Option::<String>::None)
                        submitting=Signal::derive(|| false)
                    />
                }/>
                <Route path=path!("/*any") view=move || view! {
                    <Shell
                        brand="demo"
                        brand_accent="·"
                        rail_items=rail_items_sig
                        rail_active=rail_active
                        modes=modes_sig
                        user=user
                        app_links=app_links_sig
                        on_logout=Callback::new(|()| {})
                        // footer is #[prop(optional)] now — a footer-less app
                        // simply omits it. The demo passes one to exercise the
                        // slot (and the `auto` grid row sizing).
                        footer=Box::new(|| view! {
                            <div class="statusbar">"demo footer"</div>
                        }.into_any())
                        // bottom-pinned rail slot (trawl's "Help — coming soon").
                        rail_bottom=Box::new(|| view! {
                            <div class="it" title="Pinned — demo">
                                <span class="lb">"Pinned"</span>
                            </div>
                        }.into_any())
                    >
                        <p style="padding:16px">"hello from the demo shell"</p>
                        // Hidden export sentinel — proves Icon is in scope
                        // without re-mounting TopBar/Rail (which Shell
                        // already renders internally).
                        <span hidden=true>{format!("{:?}", Icon::Question)}</span>
                    </Shell>
                }/>
            </Routes>
        </Router>
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    let _prefs = install("fleet-ui-demo:prefs");
    mount_to_body(DemoApp);
}
