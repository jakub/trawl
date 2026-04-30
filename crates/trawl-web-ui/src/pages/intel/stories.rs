// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use coastwatch_api_types::enums::StoryState;
use coastwatch_api_types::story::StoryView;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::api::ApiError;
use crate::components::toast::{ToastBus, ToastKind};
use crate::time_fmt::time_ago;

#[component]
pub fn StoriesPage() -> impl IntoView {
    let bus = use_context::<ToastBus>().expect("ToastBus context");
    let items = RwSignal::new(Vec::<StoryView>::new());
    let next_cursor = RwSignal::new(None::<String>);
    let loading = RwSignal::new(true);
    let error = RwSignal::new(None::<ApiError>);

    Effect::new(move |_| {
        spawn_local(async move {
            match api::intel::list_stories(None).await {
                Ok(page) => {
                    items.set(page.items);
                    next_cursor.set(page.next_cursor);
                }
                Err(e) => error.set(Some(e)),
            }
            loading.set(false);
        });
    });

    let nav = use_navigate();
    let on_row_click = move |id: String| {
        nav(&format!("/intel/stories/{id}"), Default::default());
    };

    let on_load_more = move |_| {
        let cursor = next_cursor.get_untracked();
        loading.set(true);
        spawn_local(async move {
            match api::intel::list_stories(cursor.as_deref()).await {
                Ok(page) => {
                    items.update(|v| v.extend(page.items));
                    next_cursor.set(page.next_cursor);
                }
                Err(e) => {
                    bus.push(ToastKind::Error, "Load more failed", Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = js_sys::Date::new_0().get_time() as i64;

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Intel stories"</h1>
                    <p class="sub">"Threat intelligence narratives assembled from source claims."</p>
                </div>
            </div>

            {move || {
                if loading.get() && items.get().is_empty() {
                    return view! {
                        <div class="tbl">
                            <div class="tbl-body">
                                <div class="tbl-row" style="cursor:default">
                                    <span class="mono" style="color:var(--ink-3)">"loading…"</span>
                                </div>
                            </div>
                        </div>
                    }.into_any();
                }

                if let Some(ref e) = error.get() {
                    let msg = match e {
                        ApiError::Status(503) => "intel service unavailable — configure web.coastwatch_url in trawld.toml".to_string(),
                        other => format!("couldn't load stories: {other}"),
                    };
                    return view! {
                        <div class="tbl">
                            <div class="tbl-body">
                                <div class="tbl-row" style="cursor:default">
                                    <span class="mono" style="color:var(--red)">{msg}</span>
                                </div>
                            </div>
                        </div>
                    }.into_any();
                }

                let rows = items.get();
                if rows.is_empty() {
                    return view! {
                        <div class="tbl">
                            <div class="tbl-body">
                                <div class="tbl-row" style="cursor:default">
                                    <span class="mono" style="color:var(--ink-3)">"no stories yet"</span>
                                </div>
                            </div>
                        </div>
                    }.into_any();
                }

                let has_more = next_cursor.get().is_some();
                let on_load_more = on_load_more.clone();

                view! {
                    <div class="tbl">
                        <div class="tbl-hd">
                            <div style="flex:0 0 72px">"State"</div>
                            <div style="flex:0 0 80px">"Class"</div>
                            <div style="flex:3; min-width:0">"Title"</div>
                            <div style="flex:0 0 56px; text-align:right">"Score"</div>
                            <div style="flex:0 0 72px; text-align:right">"Updated"</div>
                        </div>
                        <div class="tbl-body">
                            {rows.into_iter().map(|story| {
                                let id = story.id.clone();
                                let on_click = on_row_click.clone();
                                let (state_label, state_color) = state_badge(&story.state);
                                let class_label = class_label(&story.story_class);
                                let score = story.importance_score
                                    .map(|s| format!("{s:.1}"))
                                    .unwrap_or_else(|| "\u{2014}".into());
                                let updated = time_ago(&story.updated_at, now_ms);

                                view! {
                                    <div class="tbl-row" on:click=move |_| on_click(id.clone())>
                                        <div style="flex:0 0 72px">
                                            <span
                                                class="intel-badge"
                                                style=format!("background:var({state_color}-wash,var(--panel-2));color:var({state_color})")
                                            >{state_label}</span>
                                        </div>
                                        <div style="flex:0 0 80px">
                                            <span class="intel-badge" style="background:var(--panel-2);color:var(--blue)">
                                                {class_label}
                                            </span>
                                        </div>
                                        <div style="flex:3; min-width:0" class="path">{story.canonical_title}</div>
                                        <div style="flex:0 0 56px; text-align:right" class="mono">{score}</div>
                                        <div style="flex:0 0 72px; text-align:right; color:var(--ink-3)" class="mono">{updated}</div>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        {if has_more {
                            view! {
                                <div class="tbl-foot">
                                    <span></span>
                                    <button class="btn-sec" on:click=on_load_more>"load more"</button>
                                </div>
                            }.into_any()
                        } else {
                            ().into_any()
                        }}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

pub(crate) fn state_badge(s: &str) -> (&'static str, &'static str) {
    match s.parse::<StoryState>() {
        Ok(StoryState::Emerging) => ("emerging", "--amber"),
        Ok(StoryState::Active) => ("active", "--green"),
        Ok(StoryState::Monitoring) => ("monitoring", "--teal"),
        Ok(StoryState::Closed) => ("closed", "--ink-3"),
        Ok(StoryState::Superseded) => ("superseded", "--ink-4"),
        Ok(StoryState::Debunked) => ("debunked", "--red"),
        Err(_) => ("unknown", "--ink-3"),
    }
}

pub(crate) fn class_label(s: &str) -> &'static str {
    use coastwatch_api_types::enums::StoryClass;
    match s.parse::<StoryClass>() {
        Ok(StoryClass::Vulnerability) => "VULN",
        Ok(StoryClass::Campaign) => "CAMP",
        Ok(StoryClass::ActorProfile) => "ACTOR",
        Ok(StoryClass::MalwareProfile) => "MALWARE",
        Ok(StoryClass::Incident) => "INCIDENT",
        Ok(StoryClass::Policy) => "POLICY",
        Ok(StoryClass::General) => "GENERAL",
        Err(_) => "???",
    }
}
