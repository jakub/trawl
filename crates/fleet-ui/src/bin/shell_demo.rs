// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minimal Shell composition demo — proves the public API can be
//! wired by an app that isn't trawl, including the [`ToastBus`] context
//! contract: a child inside `Shell` (see [`ToastProbe`]) reaches the
//! Shell-owned bus via `expect_context` and fires both a success and an
//! error toast.
//!
//! This is a `src/bin` target (not an `examples/` file) so `cargo check
//! -p fleet-ui --target wasm32-unknown-unknown` — the CI wasm gate —
//! covers it by default (a plain `cargo check` builds bins but skips
//! examples). Compile it standalone with:
//!
//! ```sh
//! cargo build -p fleet-ui --bin shell_demo --target wasm32-unknown-unknown
//! ```
//!
//! For a live rendered preview — the way AC5's "toasts fire via context"
//! is actually observed — the crate ships `index.html` + `Trunk.toml`
//! next to `Cargo.toml`, so `cd crates/fleet-ui && trunk serve` renders
//! it with the real fleet-ui CSS. No backend or auth required: a success
//! and an error toast fire on mount, and buttons re-fire on demand.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // The demo composes wasm-only fleet-ui components. Native build
    // is a no-op so `cargo clippy --workspace --all-targets` (which
    // doesn't cross-compile) stays green.
}

#[cfg(target_arch = "wasm32")]
// TopBar and Rail are exported via fleet-ui but mounted internally by
// Shell — referencing them here would duplicate the chrome.
use fleet_ui::{
    AppLink, Btn, ConfirmWithReasonModal, Drawer, ErrorBanner, Icon, Login, Modal, ModeTab,
    RailItem, Shell, Size, TabItem, Tabs, ToastBus, UserInfo, Variant, install,
};
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

/// Stands in for a page/modal rendered inside `Shell`: it reaches the
/// Shell-owned [`ToastBus`] via `expect_context` (never constructing its
/// own bus or `<Toasts/>` host) and fires both a success and an error
/// toast. This is the "toasts fire via context" contract in miniature —
/// verifiable with `trunk serve` alone, no backend or auth required. One
/// of each fires on mount so a fresh load is self-evident; the buttons
/// re-fire on demand.
#[cfg(target_arch = "wasm32")]
#[component]
fn ToastProbe() -> impl IntoView {
    let bus = expect_context::<ToastBus>();

    Effect::new(move |_| {
        bus.push_success("Saved", Some("net created".into()));
        bus.push_error("Export failed", Some("disk full".into()));
    });

    view! {
        <div style="padding:16px;display:flex;flex-direction:column;gap:8px;align-items:flex-start">
            <p>"hello from the demo shell"</p>
            <div style="display:flex;gap:8px">
                <button
                    class="btn"
                    on:click=move |_| bus.push_success("Saved", Some("net created".into()))
                >
                    "fire success"
                </button>
                <button
                    class="btn"
                    on:click=move |_| bus.push_error("Export failed", Some("disk full".into()))
                >
                    "fire error"
                </button>
            </div>
            <ModalProbe/>
            <DrawerProbe/>
            <ErrorBanner error=Signal::derive(|| Some("demo error banner (role=alert)".to_string()))/>
        </div>
    }
}

/// Mounts the issue-#28 Drawer + Tabs from a non-trawl consumer: a
/// standalone workspace-style strip with a count chip, and a drawer
/// composing the drawer-style strip, title/actions slots, and body
/// panes switched by the active tab.
#[cfg(target_arch = "wasm32")]
#[component]
fn DrawerProbe() -> impl IntoView {
    let show_drawer = RwSignal::new(false);
    let workspace_tab = RwSignal::new("events".to_string());
    let drawer_tab = RwSignal::new("overview".to_string());

    view! {
        <Btn variant=Variant::Secondary on_click=Callback::new(move |()| show_drawer.set(true))>
            "open drawer"
        </Btn>
        <div style="width:420px;border:1px solid var(--line)">
            <Tabs
                items=vec![
                    TabItem::with_count("events", "Events", Signal::derive(|| Some(1287))),
                    TabItem::new("viz", "Visualization"),
                ]
                active=workspace_tab
                on_change=Callback::new(move |id: String| workspace_tab.set(id))
            />
        </div>
        {move || show_drawer.get().then(|| view! {
            <Drawer
                tabs=vec![
                    TabItem::new("overview", "Overview"),
                    TabItem::new("fields", "Fields"),
                ]
                active_tab=drawer_tab
                on_tab_change=Callback::new(move |id: String| drawer_tab.set(id))
                on_close=Callback::new(move |()| show_drawer.set(false))
                meta="1.2k events · 3.4 MB · 12 fields".to_string()
                title=Box::new(|| view! { <span class="name">"demo-service"</span> }.into_any())
                actions=Box::new(move || view! {
                    <Btn variant=Variant::Secondary on_click=Callback::new(|()| {})>
                        "Search this service"
                    </Btn>
                }.into_any())
            >
                {move || if drawer_tab.get() == "fields" {
                    view! { <p>"fields pane"</p> }.into_any()
                } else {
                    view! { <p>"overview pane — Esc or scrim click closes"</p> }.into_any()
                }}
            </Drawer>
        })}
    }
}

/// Mounts the issue-#28 modal family from a non-trawl consumer: the
/// `Modal` shell with icon/footer/Cmd-Ctrl+Enter, and the promoted
/// `ConfirmWithReasonModal`. Also exercises the `Btn` size axis.
#[cfg(target_arch = "wasm32")]
#[component]
fn ModalProbe() -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let show_modal = RwSignal::new(false);
    let show_reason = RwSignal::new(false);

    let submit = Callback::new(move |()| {
        bus.push_success("Submitted", Some("modal primary action ran".into()));
        show_modal.set(false);
    });

    view! {
        <div style="display:flex;gap:8px">
            <Btn variant=Variant::Secondary on_click=Callback::new(move |()| show_modal.set(true))>
                "open modal"
            </Btn>
            <Btn
                variant=Variant::Secondary
                size=Size::Xs
                on_click=Callback::new(move |()| show_reason.set(true))
            >
                "open reason modal"
            </Btn>
        </div>
        {move || show_modal.get().then(|| view! {
            <Modal
                title="Demo dialog"
                icon=Icon::Download
                on_cancel=Callback::new(move |()| show_modal.set(false))
                on_submit=submit
                footer=Box::new(move || view! {
                    <div></div>
                    <Btn
                        variant=Variant::Secondary
                        on_click=Callback::new(move |()| show_modal.set(false))
                    >
                        "Cancel"
                    </Btn>
                    <Btn variant=Variant::Primary on_click=submit>"Submit"</Btn>
                }.into_any())
            >
                <p style="margin:0">"Esc cancels · ⌘/Ctrl+Enter submits · scrim click dismisses."</p>
            </Modal>
        })}
        {move || show_reason.get().then(|| view! {
            <ConfirmWithReasonModal
                title="Retract demo object"
                message="Retract demo-1? This cascades.".to_string()
                confirm_label="Retract"
                reason_placeholder="Reason for retraction"
                on_confirm=Callback::new(move |reason: String| {
                    bus.push_success("Retracted", Some(format!("reason: {reason}")));
                    show_reason.set(false);
                })
                on_cancel=Callback::new(move |()| show_reason.set(false))
            />
        })}
    }
}

#[cfg(target_arch = "wasm32")]
#[component]
fn DemoApp() -> impl IntoView {
    // Theme prefs must be installed from *inside* the component body:
    // `install` registers an `Effect`, and effects can only be spawned
    // once leptos's executor is live (which `mount_to_body` sets up
    // before it renders this component). Calling it from `main` — before
    // mount — panics with "spawn_local before a global executor was
    // initialized". This mirrors trawl-web-ui's `App`, which likewise
    // calls `fleet_ui::install` in its body.
    let _prefs = install("fleet-ui-demo:prefs");

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
                        // Child rendered inside Shell — reaches the
                        // Shell-owned ToastBus via expect_context and fires
                        // success + error toasts (AC5's context contract).
                        <ToastProbe/>
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
    mount_to_body(DemoApp);
}
