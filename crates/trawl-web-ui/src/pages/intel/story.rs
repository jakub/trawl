// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;

use coastwatch_api_types::claim::ClaimEvidenceView;
use coastwatch_api_types::enums::{Polarity, SourceClass, StoryClaimRelationship, StoryRelation};
use coastwatch_api_types::marking::MarkingView;
use coastwatch_api_types::story::{StoryClaimView, TimelineEventView};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;

use crate::api;
use crate::api::ApiError;
use crate::time_fmt::time_ago;

use super::stories::{class_label, state_badge};

#[component]
#[allow(clippy::too_many_lines)]
pub fn StoryPage() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.get().get("id").unwrap_or_default());

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = js_sys::Date::new_0().get_time() as i64;

    let story_resource = LocalResource::new(move || {
        let story_id = id.get();
        async move { api::intel::get_story(&story_id).await }
    });

    view! {
        <div class="page">
            {move || match story_resource.get() {
                None => view! {
                    <div class="page-hd compact">
                        <div><h1 class="mono" style="color:var(--ink-3)">"loading…"</h1></div>
                    </div>
                }.into_any(),
                Some(Err(ref e)) => {
                    let msg = match e {
                        ApiError::Status(404) => "story not found".to_string(),
                        ApiError::Status(503) => "intel service unavailable".to_string(),
                        other => format!("error: {other}"),
                    };
                    view! {
                        <div class="page-hd compact">
                            <div>
                                <h1 style="color:var(--red)">{msg}</h1>
                                <p class="sub">"The story may not exist or you may not have permission to view it."</p>
                            </div>
                        </div>
                    }.into_any()
                }
                Some(Ok(ref body)) => {
                    let story = &body.data;
                    let (state_label, state_color) = state_badge(&story.state);
                    let cls = class_label(&story.story_class);
                    let title = story.canonical_title.clone();
                    let summary = story.canonical_summary.clone();
                    let importance = story.importance_score;
                    let markings = story.markings.clone();
                    let parent = story.parent_story_id.clone();
                    let story_id = story.id.clone();

                    view! {
                        <StoryHeader
                            title=title
                            state_label=state_label
                            state_color=state_color
                            class_label=cls
                            importance=importance
                            markings=markings
                            parent_story_id=parent
                        />
                        <StorySummary summary=summary/>
                        <TimelineSection story_id=story_id.clone() now_ms=now_ms/>
                        <ClaimsSection story_id=story_id.clone()/>
                        <RelationsSection story_id=story_id/>
                    }.into_any()
                }
            }}
        </div>
    }
}

#[component]
fn StoryHeader(
    title: String,
    state_label: &'static str,
    state_color: &'static str,
    class_label: &'static str,
    importance: Option<f64>,
    markings: Vec<MarkingView>,
    parent_story_id: Option<String>,
) -> impl IntoView {
    let score = importance.map(|s| format!("{s:.1}")).unwrap_or_default();

    view! {
        <div class="page-hd compact">
            <div style="display:flex;flex-direction:column;gap:6px">
                <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">
                    <h1 style="margin:0">{title}</h1>
                    <span
                        class="intel-badge"
                        style=format!("background:var({state_color}-wash,var(--panel-2));color:var({state_color})")
                    >{state_label}</span>
                    <span class="intel-badge" style="background:var(--panel-2);color:var(--blue)">{class_label}</span>
                    {markings.into_iter().map(|m| {
                        let (bg, fg) = marking_colors(&m);
                        let label = if m.scheme.eq_ignore_ascii_case("TLP") {
                            format!("TLP:{}", m.value.to_uppercase())
                        } else {
                            format!("{}:{}", m.scheme, m.value)
                        };
                        view! {
                            <span class="intel-badge" style=format!("background:{bg};color:{fg};font-weight:600")>
                                {label}
                            </span>
                        }
                    }).collect::<Vec<_>>()}
                    {(!score.is_empty()).then(|| view! {
                        <span class="mono" style="color:var(--ink-2);font-size:11px" title="importance score">
                            {format!("\u{25b2} {score}")}
                        </span>
                    })}
                </div>
                {parent_story_id.map(|pid| {
                    let href = format!("/intel/stories/{pid}");
                    view! {
                        <p class="sub" style="margin:0">
                            "parent: "
                            <a class="link" href=href>{pid}</a>
                        </p>
                    }
                })}
            </div>
        </div>
    }
}

#[component]
fn StorySummary(summary: Option<String>) -> impl IntoView {
    let color = if summary.is_some() {
        "--ink"
    } else {
        "--ink-3"
    };
    let text = summary.unwrap_or_else(|| "no summary available".into());
    view! {
        <div style="padding:0 var(--pad);margin-bottom:16px">
            <p style=format!("color:var({color}); margin:0; line-height:1.5")>
                {text}
            </p>
        </div>
    }
}

#[component]
fn TimelineSection(story_id: String, now_ms: i64) -> impl IntoView {
    let items = RwSignal::new(Vec::<TimelineEventView>::new());
    let next_cursor = RwSignal::new(None::<String>);
    let loading = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);

    let id = story_id.clone();
    Effect::new(move |_| {
        let id = id.clone();
        spawn_local(async move {
            match api::intel::story_timeline(&id, None).await {
                Ok(page) => {
                    items.set(page.items);
                    next_cursor.set(page.next_cursor);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    });

    let load_id = story_id;
    let on_load_more = move |_| {
        let cursor = next_cursor.get_untracked();
        let id = load_id.clone();
        loading.set(true);
        spawn_local(async move {
            match api::intel::story_timeline(&id, cursor.as_deref()).await {
                Ok(page) => {
                    items.update(|v| v.extend(page.items));
                    next_cursor.set(page.next_cursor);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };

    view! {
        <div style="padding:0 var(--pad)">
            <div class="intel-section-hd">"Timeline"</div>
            {move || {
                if loading.get() && items.get().is_empty() {
                    return view! {
                        <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"loading…"</p>
                    }.into_any();
                }
                if let Some(ref e) = error.get() {
                    return view! {
                        <p class="mono" style="color:var(--red);font-size:var(--table-fs)">{e.clone()}</p>
                    }.into_any();
                }
                let rows = items.get();
                if rows.is_empty() {
                    return view! {
                        <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"no timeline events yet"</p>
                    }.into_any();
                }
                let has_more = next_cursor.get().is_some();
                let on_load_more = on_load_more.clone();
                view! {
                    <div class="tbl" style="margin-bottom:16px">
                        <div class="tbl-body">
                            {rows.into_iter().map(|ev| {
                                let when = time_ago(&ev.occurred_at, now_ms);
                                let delta = ev.delta_type.replace('_', " ");
                                let origin_color = match ev.origin.as_str() {
                                    "analyst" => "--amber",
                                    "backfill" => "--ink-4",
                                    _ => "--ink-3",
                                };
                                view! {
                                    <div class="tbl-row" style="cursor:default">
                                        <div style="flex:0 0 72px;color:var(--ink-3)" class="mono" title=ev.occurred_at.clone()>
                                            {when}
                                        </div>
                                        <div style="flex:0 0 120px;font-weight:500">{delta}</div>
                                        <div style="flex:3;min-width:0;color:var(--ink-2)">{ev.summary}</div>
                                        <div style="flex:0 0 72px;text-align:right">
                                            <span class="mono" style=format!("color:var({origin_color});font-size:10px")>
                                                {ev.origin}
                                            </span>
                                        </div>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        {has_more.then(|| {
                            let is_loading = loading.get();
                            view! {
                                <div class="tbl-foot">
                                    <span></span>
                                    <button class="btn-sec" disabled=is_loading on:click=on_load_more>
                                        {if is_loading { "loading\u{2026}" } else { "load more" }}
                                    </button>
                                </div>
                            }
                        })}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn ClaimsSection(story_id: String) -> impl IntoView {
    let items = RwSignal::new(Vec::<StoryClaimView>::new());
    let next_cursor = RwSignal::new(None::<String>);
    let loading = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);
    let expanded = RwSignal::new(None::<String>);
    let evidence_cache =
        RwSignal::new(HashMap::<String, Result<Vec<ClaimEvidenceView>, String>>::new());

    let id = story_id.clone();
    Effect::new(move |_| {
        let id = id.clone();
        spawn_local(async move {
            match api::intel::story_claims(&id, None).await {
                Ok(page) => {
                    items.set(page.items);
                    next_cursor.set(page.next_cursor);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    });

    let load_id = story_id;
    let on_load_more = move |_| {
        let cursor = next_cursor.get_untracked();
        let id = load_id.clone();
        loading.set(true);
        spawn_local(async move {
            match api::intel::story_claims(&id, cursor.as_deref()).await {
                Ok(page) => {
                    items.update(|v| v.extend(page.items));
                    next_cursor.set(page.next_cursor);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };

    view! {
        <div style="padding:0 var(--pad)">
            <div class="intel-section-hd">"Claims"</div>
            {move || {
                if loading.get() && items.get().is_empty() {
                    return view! {
                        <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"loading…"</p>
                    }.into_any();
                }
                if let Some(ref e) = error.get() {
                    return view! {
                        <p class="mono" style="color:var(--red);font-size:var(--table-fs)">{e.clone()}</p>
                    }.into_any();
                }
                let rows = items.get();
                if rows.is_empty() {
                    return view! {
                        <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"no claims yet"</p>
                    }.into_any();
                }
                let has_more = next_cursor.get().is_some();
                let on_load_more = on_load_more.clone();
                view! {
                    <div class="tbl" style="margin-bottom:16px">
                        <div class="tbl-hd">
                            <div style="flex:0 0 88px">"Relationship"</div>
                            <div style="flex:0 0 64px">"Polarity"</div>
                            <div style="flex:2;min-width:0">"Type"</div>
                            <div style="flex:0 0 72px">"Modality"</div>
                            <div style="flex:0 0 48px;text-align:right">"Conf"</div>
                            <div style="flex:0 0 80px;text-align:right">"Source"</div>
                        </div>
                        <div class="tbl-body">
                            {rows.into_iter().map(|claim| {
                                let claim_id = claim.claim_id.clone();
                                let is_warning = matches!(
                                    claim.polarity.parse::<Polarity>(),
                                    Ok(Polarity::Denied | Polarity::Retracted)
                                ) || matches!(
                                    claim.relationship.parse::<StoryClaimRelationship>(),
                                    Ok(StoryClaimRelationship::Contradiction)
                                );
                                let row_bg = if is_warning { "background:var(--red-wash)" } else { "" };
                                let (rel_label, rel_color) = relationship_badge(&claim.relationship);
                                let (pol_label, pol_color) = polarity_badge(&claim.polarity);
                                let claim_type = claim.claim_type.replace('_', " ");
                                let conf = format!("{:.0}%", claim.claim_confidence * 100.0);
                                let source = claim.source_class.parse::<SourceClass>()
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|_| claim.source_class.clone());

                                let toggle_id = claim_id.clone();
                                let on_toggle = move |e: web_sys::MouseEvent| {
                                    e.stop_propagation();
                                    let cid = toggle_id.clone();
                                    let currently = expanded.get_untracked();
                                    if currently.as_deref() == Some(&cid) {
                                        expanded.set(None);
                                    } else {
                                        expanded.set(Some(cid.clone()));
                                        let cached = evidence_cache.get_untracked();
                                        if !cached.contains_key(&cid) {
                                            let cid2 = cid.clone();
                                            spawn_local(async move {
                                                let result = api::intel::claim_evidence(&cid2, None).await;
                                                evidence_cache.update(|m| {
                                                    m.insert(
                                                        cid2,
                                                        result
                                                            .map(|page| page.items)
                                                            .map_err(|e| e.to_string()),
                                                    );
                                                });
                                            });
                                        }
                                    }
                                };

                                let evidence_id = claim_id.clone();
                                view! {
                                    <div>
                                        <div class="tbl-row" style=format!("cursor:pointer;{row_bg}") on:click=on_toggle>
                                            <div style="flex:0 0 88px">
                                                <span class="intel-badge" style=format!("background:var({rel_color}-wash,var(--panel-2));color:var({rel_color})")>
                                                    {rel_label}
                                                </span>
                                            </div>
                                            <div style="flex:0 0 64px">
                                                <span class="intel-badge" style=format!("background:var({pol_color}-wash,var(--panel-2));color:var({pol_color})")>
                                                    {pol_label}
                                                </span>
                                            </div>
                                            <div style="flex:2;min-width:0">{claim_type}</div>
                                            <div style="flex:0 0 72px;color:var(--ink-3)" class="mono">{claim.modality}</div>
                                            <div style="flex:0 0 48px;text-align:right" class="mono">{conf}</div>
                                            <div style="flex:0 0 80px;text-align:right;color:var(--ink-3)" class="mono">{source}</div>
                                        </div>
                                        {move || {
                                            let exp = expanded.get();
                                            if exp.as_deref() != Some(&evidence_id) {
                                                return ().into_any();
                                            }
                                            let cached = evidence_cache.get();
                                            match cached.get(&evidence_id) {
                                                None => view! {
                                                    <div class="evidence-panel">
                                                        <span class="mono" style="color:var(--ink-3)">"loading evidence…"</span>
                                                    </div>
                                                }.into_any(),
                                                Some(Err(msg)) => view! {
                                                    <div class="evidence-panel">
                                                        <span class="mono" style="color:var(--red)">{format!("error: {msg}")}</span>
                                                    </div>
                                                }.into_any(),
                                                Some(Ok(evs)) if evs.is_empty() => view! {
                                                    <div class="evidence-panel">
                                                        <span class="mono" style="color:var(--ink-3)">"no evidence records"</span>
                                                    </div>
                                                }.into_any(),
                                                Some(Ok(evs)) => view! {
                                                    <div class="evidence-panel">
                                                        {evs.iter().map(|ev| {
                                                            let factual = ev.factual_summary.clone()
                                                                .unwrap_or_else(|| "\u{2014}".into());
                                                            let claim_s = ev.claim_summary.clone()
                                                                .unwrap_or_else(|| "\u{2014}".into());
                                                            view! {
                                                                <div style="margin-bottom:8px;padding-bottom:8px;border-bottom:1px solid var(--line)">
                                                                    <div style="font-size:var(--table-fs);margin-bottom:2px">
                                                                        <strong>"factual: "</strong>{factual}
                                                                    </div>
                                                                    <div style="font-size:var(--table-fs);color:var(--ink-2)">
                                                                        <strong>"claim: "</strong>{claim_s}
                                                                    </div>
                                                                    <div class="mono" style="font-size:10px;color:var(--ink-4);margin-top:2px">
                                                                        {format!("fragment {} span {}–{}", ev.fragment_id, ev.span_start, ev.span_end)}
                                                                    </div>
                                                                </div>
                                                            }
                                                        }).collect::<Vec<_>>()}
                                                    </div>
                                                }.into_any(),
                                            }
                                        }}
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        {has_more.then(|| {
                            let is_loading = loading.get();
                            view! {
                                <div class="tbl-foot">
                                    <span></span>
                                    <button class="btn-sec" disabled=is_loading on:click=on_load_more>
                                        {if is_loading { "loading\u{2026}" } else { "load more" }}
                                    </button>
                                </div>
                            }
                        })}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn RelationsSection(story_id: String) -> impl IntoView {
    let resource = LocalResource::new({
        let id = story_id.clone();
        move || {
            let id = id.clone();
            async move { api::intel::story_relations(&id).await }
        }
    });

    view! {
        <div style="padding:0 var(--pad)">
            <div class="intel-section-hd">"Related stories"</div>
            {move || match resource.get() {
                None => view! {
                    <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"loading…"</p>
                }.into_any(),
                Some(Err(ref e)) => view! {
                    <p class="mono" style="color:var(--red);font-size:var(--table-fs)">{e.to_string()}</p>
                }.into_any(),
                Some(Ok(ref body)) if body.items.is_empty() => view! {
                    <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"no related stories"</p>
                }.into_any(),
                Some(Ok(ref body)) => {
                    let current_id = story_id.clone();
                    view! {
                        <div class="tbl" style="margin-bottom:16px">
                            <div class="tbl-body">
                                {body.items.iter().map(|rel| {
                                    let other_id = if rel.story_a_id == current_id {
                                        &rel.story_b_id
                                    } else {
                                        &rel.story_a_id
                                    };
                                    let rel_label = rel.relation.parse::<StoryRelation>()
                                        .map(|r| r.to_string())
                                        .unwrap_or_else(|_| rel.relation.clone());
                                    let href = format!("/intel/stories/{other_id}");
                                    let conf = rel.confidence
                                        .map(|c| format!(" ({:.0}%)", c * 100.0))
                                        .unwrap_or_default();
                                    view! {
                                        <div class="tbl-row" style="cursor:default">
                                            <div style="flex:0 0 100px">
                                                <span class="intel-badge" style="background:var(--panel-2);color:var(--ink-2)">
                                                    {rel_label}
                                                </span>
                                            </div>
                                            <div style="flex:1">
                                                <a class="link" href=href>{other_id.clone()}</a>
                                                <span class="mono" style="color:var(--ink-3);font-size:10px">{conf}</span>
                                            </div>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}

fn marking_colors(m: &MarkingView) -> (&'static str, &'static str) {
    if m.scheme.eq_ignore_ascii_case("TLP") {
        match m.value.to_uppercase().as_str() {
            "RED" => ("var(--red-wash)", "var(--red)"),
            "AMBER" | "AMBER+STRICT" => ("var(--amber-wash)", "var(--amber)"),
            "GREEN" => ("rgba(74,125,63,.10)", "var(--green)"),
            _ => ("var(--panel-2)", "var(--ink-3)"),
        }
    } else {
        ("var(--panel-2)", "var(--ink-2)")
    }
}

fn relationship_badge(s: &str) -> (&'static str, &'static str) {
    match s.parse::<StoryClaimRelationship>() {
        Ok(StoryClaimRelationship::Contradiction) => ("contradiction", "--red"),
        Ok(StoryClaimRelationship::Supersession) => ("supersession", "--yellow"),
        Ok(StoryClaimRelationship::Correction) => ("correction", "--yellow"),
        Ok(StoryClaimRelationship::Evolution) => ("evolution", "--amber"),
        Ok(StoryClaimRelationship::Related) => ("related", "--ink-2"),
        Ok(StoryClaimRelationship::Background) => ("background", "--ink-3"),
        Ok(StoryClaimRelationship::Duplicate) => ("duplicate", "--ink-4"),
        Ok(StoryClaimRelationship::NewStory) => ("new story", "--teal"),
        Err(_) => ("unknown", "--ink-3"),
    }
}

fn polarity_badge(s: &str) -> (&'static str, &'static str) {
    match s.parse::<Polarity>() {
        Ok(Polarity::Affirmed) => ("affirmed", "--green"),
        Ok(Polarity::Denied) => ("denied", "--red"),
        Ok(Polarity::Retracted) => ("retracted", "--red"),
        Ok(Polarity::Corrected) => ("corrected", "--yellow"),
        Ok(Polarity::Superseded) => ("superseded", "--ink-3"),
        Ok(Polarity::Unknown) => ("unknown", "--ink-4"),
        Err(_) => ("???", "--ink-3"),
    }
}
