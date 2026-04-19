// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Rail/>` — 52px left navigation rail.
//!
//! Items are mode-aware (Search has Search/History/Schema; Intel has
//! News/IoCs/Feeds; etc.). Active item gets the amber bar + amber color.
//! Help is pinned at the bottom of every mode.

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::use_navigate;

use crate::state::app_mode::AppMode;
use crate::state::section::{RailIcon, items_for};

#[component]
pub fn Rail(
    #[prop(into)] mode: Signal<AppMode>,
    #[prop(into)] section: Signal<String>,
) -> impl IntoView {
    let nav = use_navigate();

    let go = {
        let nav = nav.clone();
        move |target_section: &'static str, current_mode: AppMode| {
            // Preserve mode + the section param; strip everything else
            // (the search workspace is the only mode that has its own
            // params, and switching section there is a fresh page).
            nav(
                &format!(
                    "/search?app={}&section={}",
                    current_mode.as_param(),
                    target_section
                ),
                NavigateOptions::default(),
            );
        }
    };

    view! {
        <nav class="rail">
            {move || {
                let cur_mode = mode.get();
                items_for(cur_mode).iter().copied().map(|item| {
                    let go = go.clone();
                    view! {
                        <div
                            class="it"
                            class:active=move || section.get() == item.id
                            on:click=move |_| go(item.id, cur_mode)
                            title=item.label
                        >
                            <RailIconView icon=item.icon/>
                            <span class="lb">{item.label}</span>
                        </div>
                    }
                }).collect::<Vec<_>>()
            }}
            <div class="bot">
                <div class="it" title="Help — coming soon">
                    <RailIconView icon=RailIcon::Question/>
                    <span class="lb">"Help"</span>
                </div>
            </div>
        </nav>
    }
}

/// Minimal SVG icon set for the rail. Single-stroke 16×16, currentColor.
#[component]
fn RailIconView(icon: RailIcon) -> impl IntoView {
    let path = match icon {
        RailIcon::Search => view! {
            <g><circle cx="7" cy="7" r="4.5"/><path d="m10.5 10.5 3 3"/></g>
        }
        .into_any(),
        RailIcon::Clock => view! {
            <g><circle cx="8" cy="8" r="6"/><path d="M8 5v3l2 1.5"/></g>
        }
        .into_any(),
        RailIcon::Database => view! {
            <g>
                <ellipse cx="8" cy="3.5" rx="5" ry="1.5"/>
                <path d="M3 3.5v9c0 .8 2.2 1.5 5 1.5s5-.7 5-1.5v-9"/>
                <path d="M3 8c0 .8 2.2 1.5 5 1.5s5-.7 5-1.5"/>
            </g>
        }
        .into_any(),
        RailIcon::News => view! {
            <g>
                <rect x="2" y="3" width="11" height="10" rx="1"/>
                <path d="M4.5 6h6M4.5 8.5h6M4.5 11h4"/>
            </g>
        }
        .into_any(),
        RailIcon::Alert => view! {
            <g>
                <path d="M8 2 14 13H2z"/>
                <path d="M8 6.5v3M8 11.5v.01" stroke-linecap="round"/>
            </g>
        }
        .into_any(),
        RailIcon::Link => view! {
            <g>
                <path d="M9 4.5h2.5a2.5 2.5 0 0 1 0 5H9"/>
                <path d="M7 11.5H4.5a2.5 2.5 0 0 1 0-5H7"/>
                <path d="M5.5 8h5"/>
            </g>
        }
        .into_any(),
        RailIcon::Zap => view! {
            <g><path d="M9 1.5 3.5 9.5h4l-1 5L12 6.5h-4z"/></g>
        }
        .into_any(),
        RailIcon::Check => view! {
            <g><path d="m3 8.5 3.5 3L13 4.5"/></g>
        }
        .into_any(),
        RailIcon::Grid => view! {
            <g>
                <rect x="2" y="2" width="5" height="5"/>
                <rect x="9" y="2" width="5" height="5"/>
                <rect x="2" y="9" width="5" height="5"/>
                <rect x="9" y="9" width="5" height="5"/>
            </g>
        }
        .into_any(),
        RailIcon::User => view! {
            <g>
                <circle cx="8" cy="5.5" r="2.5"/>
                <path d="M3 14c0-2.8 2.2-5 5-5s5 2.2 5 5"/>
            </g>
        }
        .into_any(),
        RailIcon::Chart => view! {
            <g><path d="M2 13.5h12M4 11V7.5M7 11V4.5M10 11V8.5M13 11V6"/></g>
        }
        .into_any(),
        RailIcon::Question => view! {
            <g>
                <circle cx="8" cy="8" r="6"/>
                <path d="M6 6.5c0-1.1.9-2 2-2s2 .9 2 2c0 1.5-2 1.5-2 3"/>
                <path d="M8 11.5v.01" stroke-linecap="round"/>
            </g>
        }
        .into_any(),
    };
    view! {
        <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4">
            {path}
        </svg>
    }
}
