// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minimal Shell composition demo — proves the public API can be
//! wired by an app that isn't trawl.
//!
//! This is a `src/bin` target (not an `examples/` file) so `cargo check
//! -p fleet-ui --target wasm32-unknown-unknown` — the CI wasm gate —
//! covers it by default (a plain `cargo check` builds bins but skips
//! examples).
//!
//! For a live rendered preview the crate ships `index.html` +
//! `Trunk.toml` next to `Cargo.toml`, so `cd crates/fleet-ui && trunk
//! serve` renders it with the real fleet-ui CSS.

#[cfg(not(target_arch = "wasm32"))]
fn main() {}

#[cfg(target_arch = "wasm32")]
use fleet_ui::{
    Btn, ConfirmWithReasonModal, Drawer, ErrorBanner, Icon, Login, Modal, RailItem, Shell,
    SidebarGroup, Size, TabItem, Tabs, Theme, ToastBus, UserInfo, Variant, install,
};
#[cfg(target_arch = "wasm32")]
use leptos::prelude::*;
#[cfg(target_arch = "wasm32")]
use leptos_router::components::{Route, Router, Routes};
#[cfg(target_arch = "wasm32")]
use leptos_router::path;

#[cfg(all(target_arch = "wasm32", feature = "atmosphere"))]
fn backdrop(theme: impl Into<Signal<Theme>>) -> impl IntoView {
    use fleet_ui::Atmosphere;
    view! { <Atmosphere theme=theme/> }
}

#[cfg(all(target_arch = "wasm32", not(feature = "atmosphere")))]
fn backdrop(_theme: impl Into<Signal<Theme>>) -> impl IntoView {}

#[cfg(target_arch = "wasm32")]
fn sidebar_groups() -> Vec<SidebarGroup> {
    vec![
        SidebarGroup {
            label: None,
            items: vec![
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
            ],
        },
        SidebarGroup {
            label: Some("Operations".into()),
            items: vec![RailItem {
                id: "alerts".into(),
                label: "Alerts".into(),
                icon: Icon::Alert,
                path: "/alerts".into(),
                badge: Some(3),
            }],
        },
    ]
}

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
                label="Demo workspace"
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
                tabs_label="Demo service"
                label="demo-service"
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
    // initialized".
    let prefs = install("fleet-ui-demo:prefs");
    // TopBar's theme choices reach them via `use_context::<UiPrefs>()`;
    // missing context hides the choices and warns.
    provide_context(prefs);

    let groups = Signal::derive(sidebar_groups);
    let sidebar_active = Signal::derive(|| "home".to_string());
    let identity_present = RwSignal::new(true);
    let user = Signal::derive(move || {
        identity_present.get().then(|| UserInfo {
            name: "demo user".into(),
            detail: "admin".into(),
        })
    });

    view! {
        <Router>
            <Routes fallback=|| view! { <p>"…"</p> }>
                <Route path=path!("/login") view=move || view! {
                    {backdrop(prefs.theme())}
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
                        sidebar_groups=groups
                        sidebar_active=sidebar_active
                        user=user
                        on_logout=Callback::new(|()| {})
                        footer=Box::new(|| view! {
                            <div class="statusbar">"demo footer"</div>
                        }.into_any())
                        sidebar_bottom=ViewFn::from(|| view! {
                            <a class="it" title="Pinned — demo" href="/">
                                <span class="lb">"Pinned"</span>
                            </a>
                        })
                    >
                        <ToastProbe/>
                        <RangeProbe/>
                        <span hidden=true>{format!("{:?}", Icon::Question)}</span>
                    </Shell>
                }/>
            </Routes>
            <div role="group" aria-label="Demo identity" style="position:fixed;left:12px;bottom:52px;z-index:10">
                <button class="btn" type="button" disabled=move || !identity_present.get()
                    on:click=move |_| identity_present.set(false)>"Clear demo identity"</button>
                <button class="btn" type="button" disabled=move || identity_present.get()
                    on:click=move |_| identity_present.set(true)>"Restore demo identity"</button>
            </div>
            // Always-visible theme choices, outside <Routes> so they stay
            // mounted on /login too (the real Shell's TopBar choices only
            // exist on shell routes). That is what lets a preference change
            // re-color the mounted backdrop canvas in place, without a
            // remount, while /login is on screen.
            <div
                role="group"
                aria-label="Demo theme"
                style="position:fixed;right:12px;bottom:12px;z-index:10"
            >
                {[
                    ("Light", fleet_ui::ThemePreference::Light),
                    ("Dark", fleet_ui::ThemePreference::Dark),
                    ("System", fleet_ui::ThemePreference::System),
                ].into_iter().map(move |(label, preference)| view! {
                    <button
                        class="btn"
                        type="button"
                        aria-pressed=move || (prefs.theme_preference().get() == preference).to_string()
                        on:click=move |_| prefs.select_theme(preference)
                    >{label}</button>
                }).collect_view()}
            </div>
        </Router>
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    mount_to_body(DemoHost);
}

#[cfg(target_arch = "wasm32")]
#[component]
fn DemoHost() -> impl IntoView {
    let installed = RwSignal::new(true);
    view! {
        <Show when=move || installed.get()>
            <DemoApp/>
        </Show>
        <div role="group" aria-label="Demo lifecycle" style="position:fixed;left:12px;bottom:12px;z-index:10">
            <button class="btn" type="button" disabled=move || installed.get()
                on:click=move |_| installed.set(true)>"Install demo"</button>
            <button class="btn" type="button" disabled=move || !installed.get()
                on:click=move |_| installed.set(false)>"Dispose demo"</button>
        </div>
    }
}

#[cfg(target_arch = "wasm32")]
#[component]
fn RangeProbe() -> impl IntoView {
    use fleet_ui::{RangeDialog, RangePreset, RangeValue};
    view! {
        <section style="padding:16px;max-width:360px">
            <h2>"Range dialog"</h2>
            <RangeDialog
                value=Signal::from(RangeValue::Quick("30m".into()))
                presets=vec![
                    RangePreset { id: "30m".into(), label: "Past half hour".into() },
                    RangePreset { id: "6h".into(), label: "Past six hours".into() },
                ]
                reset_key={Signal::<String>::from("workbench".to_string())}
                disabled=Signal::from(false)
                on_commit=Callback::new(|_| Err("Workbench refuses this range.".into()))
            />
        </section>
    }
}
