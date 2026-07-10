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
use crate::time_fmt::time_ago;
use fleet_ui::{Badge, Btn, LoadState, Loaded, Pager, ToastBus, ToastKind, Tone, Variant};

use crate::components::lineage_tree::tone_for_var;

#[component]
pub fn StoriesPage() -> impl IntoView {
    let bus = expect_context::<ToastBus>();
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

            <Loaded
                state=Signal::derive(move || {
                    if let Some(e) = error.get() {
                        return LoadState::Error(match e {
                            ApiError::Status(503) => "intel service unavailable \u{2014} configure web.coastwatch_url in trawld.toml".to_string(),
                            other => other.to_string(),
                        });
                    }
                    if loading.get() && items.get().is_empty() {
                        return LoadState::Loading;
                    }
                    LoadState::Ready(items.get())
                })
                label="stories"
                render=Box::new(move |rows: Vec<StoryView>| {
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
                        <div class="tbl-body">
                            {rows.into_iter().map(|story| {
                                let id = story.id.clone();
                                let on_click = on_row_click.clone();
                                let (state_label, state_color) = state_badge(&story.state);
                                let cls = class_label(&story.story_class);
                                let score = story.importance_score
                                    .map(|s| format!("\u{25b2} {s:.1}"))
                                    .unwrap_or_default();
                                let updated = time_ago(&story.updated_at, now_ms);
                                let created = time_ago(&story.created_at, now_ms);
                                let summary = story.canonical_summary.clone();
                                let markings = story.markings.clone();
                                let has_parent = story.parent_story_id.is_some();

                                view! {
                                    <div
                                        class="story-list-row"
                                        on:click=move |_| on_click(id.clone())
                                    >
                                        <div class="story-list-main">
                                            <div style="display:flex;align-items:center;gap:6px;flex-wrap:wrap;min-width:0;flex:1">
                                                <Badge tone=tone_for_var(state_color)>{state_label}</Badge>
                                                <Badge tone=Tone::Info>{cls}</Badge>
                                                {markings.into_iter().map(|m| {
                                                    let tone = marking_tone(&m);
                                                    let label = if m.scheme.eq_ignore_ascii_case("TLP") {
                                                        format!("TLP:{}", m.value.to_uppercase())
                                                    } else {
                                                        format!("{}:{}", m.scheme, m.value)
                                                    };
                                                    view! {
                                                        <Badge tone=tone>{label}</Badge>
                                                    }
                                                }).collect::<Vec<_>>()}
                                                {has_parent.then(|| view! {
                                                    <Badge>"\u{2934}"</Badge>
                                                })}
                                                <span class="story-list-title">{story.canonical_title}</span>
                                            </div>
                                            <div style="display:flex;align-items:center;gap:10px;flex-shrink:0">
                                                {(!score.is_empty()).then(|| view! {
                                                    <span class="mono" style="color:var(--ink-2);font-size:11px">
                                                        {score}
                                                    </span>
                                                })}
                                            </div>
                                        </div>
                                        <div class="story-list-sub">
                                            {match summary {
                                                Some(s) => view! {
                                                    <span class="story-list-summary">{s}</span>
                                                }.into_any(),
                                                None => view! {
                                                    <span class="story-list-summary" style="color:var(--ink-4);font-style:italic">
                                                        "no summary yet"
                                                    </span>
                                                }.into_any(),
                                            }}
                                            <span class="story-list-times">
                                                <span title=story.updated_at>{format!("updated {updated}")}</span>
                                                " \u{00b7} "
                                                <span title=story.created_at>{format!("created {created}")}</span>
                                            </span>
                                        </div>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        {if has_more {
                            let is_loading = loading.get();
                            view! {
                                <Pager summary=String::new()>
                                    <Btn
                                        variant=Variant::Secondary
                                        disabled=is_loading
                                        on_click=Callback::new(on_load_more)
                                    >
                                        {if is_loading { "loading\u{2026}" } else { "load more" }}
                                    </Btn>
                                </Pager>
                            }.into_any()
                        } else {
                            ().into_any()
                        }}
                    </div>
                }.into_any()
            })
            />
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

/// Map a marking onto the closed badge tone set: TLP colors keep their
/// severity reading (RED→Danger, AMBER→Warn, GREEN→Success); anything
/// else is Neutral.
pub(crate) fn marking_tone(m: &coastwatch_api_types::marking::MarkingView) -> Tone {
    if m.scheme.eq_ignore_ascii_case("TLP") {
        match m.value.to_uppercase().as_str() {
            "RED" => Tone::Danger,
            "AMBER" | "AMBER+STRICT" => Tone::Warn,
            "GREEN" => Tone::Success,
            _ => Tone::Neutral,
        }
    } else {
        Tone::Neutral
    }
}
