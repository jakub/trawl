// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, HashSet};

use coastwatch_api_types::claim::ClaimEvidenceView;
use coastwatch_api_types::enums::{
    Modality, Polarity, SourceClass, StoryClaimRelationship, StoryRelation,
};
use coastwatch_api_types::marking::MarkingView;
use coastwatch_api_types::pagination::ItemBody;
use coastwatch_api_types::pagination::PaginatedBody;
use coastwatch_api_types::story::{
    StoryClaimView, StoryRelationView, StoryView, TimeRange, TimelineEventView,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;

use crate::api;
use crate::api::{ApiError, MeResponse};
use crate::components::lineage_tree::{LineageNode, LineageTree, can_write_derivations};
use crate::components::linkage_graph::LinkageGraph;
use crate::components::tone_for_var;
use fleet_ui::time::time_ago;
use fleet_ui::{Badge, LoadMore, LoadState, Loaded, Pager, Tone};

use super::stories::{class_label, marking_tone, state_badge};

#[derive(Clone, PartialEq)]
struct HeaderMeta {
    source_names: Vec<String>,
    product_names: Vec<String>,
    has_fix: bool,
}

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

    let all_claims = RwSignal::new(Vec::<StoryClaimView>::new());

    let relations_resource = LocalResource::new(move || {
        let story_id = id.get();
        async move { api::intel::story_relations(&story_id).await }
    });

    let relations_for_graph = Memo::new(move |_| {
        relations_resource
            .get()
            .and_then(Result::ok)
            .map(|body| body.items)
            .unwrap_or_default()
    });

    let header_meta = Memo::new(move |_| {
        let claims = all_claims.get();
        if claims.is_empty() {
            return None;
        }
        Some(derive_header_meta(&claims))
    });

    view! {
        <div class="page">
            <Loaded
                state=Signal::derive(move || {
                    // 404 is Missing (the story doesn't exist — neutral
                    // "story not found" + subtitle), not an error; 503
                    // and friends keep their error copy (issue #33 D5).
                    LoadState::from_resource_with_missing(
                        story_resource.get(),
                        |e| matches!(e, ApiError::Status(404)),
                        |e| match e {
                            ApiError::Status(503) => "intel service unavailable".to_string(),
                            other => other.to_string(),
                        },
                    )
                })
                label="story"
                missing_subtitle="The story may not exist or you may not have permission to view it."
                render=Box::new(move |body: ItemBody<StoryView>| {
                    let story = &body.data;
                    let (state_label, state_color) = state_badge(&story.state);
                    let cls = class_label(&story.story_class);
                    let title = story.canonical_title.clone();
                    let graph_title = title.clone();
                    let summary = story.canonical_summary.clone();
                    let importance = story.importance_score;
                    let markings = story.markings.clone();
                    let parent = story.parent_story_id.clone();
                    let story_id = story.id.clone();
                    let created_at = story.created_at.clone();
                    let updated_at = story.updated_at.clone();

                    view! {
                        <StoryHeader
                            title=title
                            state_label=state_label
                            state_color=state_color
                            class_label=cls
                            importance=importance
                            markings=markings
                            parent_story_id=parent
                            meta=header_meta
                            created_at=created_at.clone()
                            updated_at=updated_at.clone()
                            now_ms=now_ms
                        />
                        <StorySummary summary=summary/>
                        <div class="story-cols">
                            <div class="story-sidebar">
                                <StoryVitals
                                    created_at=created_at
                                    updated_at=updated_at
                                    importance=importance
                                    claims=all_claims
                                    now_ms=now_ms
                                />
                                <VerticalTimeline story_id=story_id.clone() now_ms=now_ms/>
                                <StoryLineage story_id=story_id.clone() now_ms=now_ms/>
                            </div>
                            <div>
                                <AffectedProductsSection claims=all_claims now_ms=now_ms/>
                                <ClaimsSection story_id=story_id.clone() items=all_claims now_ms=now_ms/>
                                <RelationsSection story_id=story_id.clone() relations=relations_resource now_ms=now_ms/>
                            </div>
                        </div>
                        <LinkageGraph
                            story_id=story_id.clone()
                            story_title=graph_title
                            claims=all_claims
                            relations=Signal::derive(move || relations_for_graph.get())
                        />
                    }.into_any()
                })
            />
        </div>
    }
}

// ── Header ─────────────────────────────────────────────────────

#[component]
#[allow(clippy::too_many_lines)]
fn StoryHeader(
    title: String,
    state_label: &'static str,
    state_color: &'static str,
    class_label: &'static str,
    importance: Option<f64>,
    markings: Vec<MarkingView>,
    parent_story_id: Option<String>,
    meta: Memo<Option<HeaderMeta>>,
    created_at: String,
    updated_at: String,
    now_ms: i64,
) -> impl IntoView {
    let score = importance.map(|s| format!("{s:.1}")).unwrap_or_default();
    let created_ago = time_ago(&created_at, now_ms);
    let updated_ago = time_ago(&updated_at, now_ms);

    view! {
        <div class="page-hd compact" style="border-left:2px solid var(--amber)">
            <div style="display:flex;flex-direction:column;gap:6px">
                <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">
                    <h1 style="margin:0">{title}</h1>
                    <Badge tone=tone_for_var(state_color)>{state_label}</Badge>
                    <Badge tone=Tone::Info>{class_label}</Badge>
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
                    {(!score.is_empty()).then(|| view! {
                        <span class="mono" style="color:var(--ink-2);font-size:11px" title="importance score">
                            {format!("\u{25b2} {score}")}
                        </span>
                    })}
                </div>
                {move || {
                    let m = meta.get()?;
                    let mut parts = Vec::new();
                    if !m.product_names.is_empty() {
                        parts.push(m.product_names.join(", "));
                    }
                    if m.source_names.len() == 1 {
                        parts.push(m.source_names[0].clone());
                    } else if m.source_names.len() > 1 {
                        parts.push(format!("{} sources", m.source_names.len()));
                    }
                    if !m.product_names.is_empty() {
                        parts.push(if m.has_fix {
                            "fix available".into()
                        } else {
                            "no fix available".into()
                        });
                    }
                    (!parts.is_empty()).then(|| view! {
                        <p class="sub" style="margin:0;color:var(--ink-2)">
                            {parts.join(" \u{00b7} ")}
                        </p>
                    })
                }}
                <p class="sub" style="margin:0;color:var(--ink-3);font-size:11px">
                    <span title=created_at>{"created "}{created_ago}</span>
                    " \u{00b7} "
                    <span title=updated_at>{"updated "}{updated_ago}</span>
                </p>
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

// ── Vitals card ────────────────────────────────────────────────

#[component]
fn StoryVitals(
    created_at: String,
    updated_at: String,
    importance: Option<f64>,
    claims: RwSignal<Vec<StoryClaimView>>,
    now_ms: i64,
) -> impl IntoView {
    let created_ago = time_ago(&created_at, now_ms);
    let updated_ago = time_ago(&updated_at, now_ms);
    let score_label = importance
        .map(|s| format!("{s:.1}"))
        .unwrap_or_else(|| "\u{2014}".into());

    view! {
        <div class="story-vitals">
            <div class="story-vitals-row">
                <span class="story-vitals-label">"Created"</span>
                <span class="story-vitals-val" title=created_at>{created_ago}</span>
            </div>
            <div class="story-vitals-row">
                <span class="story-vitals-label">"Updated"</span>
                <span class="story-vitals-val" title=updated_at>{updated_ago}</span>
            </div>
            <div class="story-vitals-row">
                <span class="story-vitals-label">"Importance"</span>
                <span class="story-vitals-val">{score_label}</span>
            </div>
            {move || {
                let all = claims.get();
                if all.is_empty() {
                    return ().into_any();
                }
                let claim_count = all.len();
                let source_count = {
                    let mut names = Vec::new();
                    for c in &all {
                        if let Some(ref s) = c.source {
                            if !names.contains(&s.name) {
                                names.push(s.name.clone());
                            }
                        }
                    }
                    names.len()
                };
                view! {
                    <div class="story-vitals-row">
                        <span class="story-vitals-label">"Claims"</span>
                        <span class="story-vitals-val">{claim_count.to_string()}</span>
                    </div>
                    <div class="story-vitals-row">
                        <span class="story-vitals-label">"Sources"</span>
                        <span class="story-vitals-val">{source_count.to_string()}</span>
                    </div>
                }.into_any()
            }}
        </div>
    }
}

// ── Vertical timeline ──────────────────────────────────────────

#[component]
fn VerticalTimeline(story_id: String, now_ms: i64) -> impl IntoView {
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
                    let mut evts = page.items;
                    evts.reverse();
                    items.set(evts);
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
                    items.update(|v| {
                        let mut new_evts = page.items;
                        new_evts.reverse();
                        // prepend older events at the beginning (chrono order)
                        new_evts.append(v);
                        *v = new_evts;
                    });
                    next_cursor.set(page.next_cursor);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };

    view! {
        <div>
            <div class="intel-section-hd" style="margin-top:0">"Timeline"</div>
            <Loaded
                state=Signal::derive(move || {
                    LoadState::from_parts(loading.get() && items.get().is_empty(), error.get(), || {
                        items.get()
                    })
                })
                label="timeline"
                render=Box::new(move |rows: Vec<TimelineEventView>| {
                if rows.is_empty() {
                    return view! {
                        <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"no timeline events yet"</p>
                    }.into_any();
                }
                let has_more = next_cursor.get().is_some();
                let on_load_more = on_load_more.clone();
                view! {
                    <div class="v-timeline">
                        {rows.into_iter().map(|ev| {
                            let when = time_ago(&ev.occurred_at, now_ms);
                            let (delta_label, delta_color) = delta_badge(&ev.delta_type);
                            let material_class = if ev.material { "v-timeline-node material" } else { "v-timeline-node" };
                            let origin_color = match ev.origin.as_str() {
                                "analyst" => "--amber",
                                "backfill" => "--ink-4",
                                _ => "--ink-3",
                            };
                            view! {
                                <div class=material_class>
                                    <div class="v-timeline-dot"
                                        style=format!("background:var({delta_color})")
                                    />
                                    <div style="display:flex;align-items:center;gap:6px;margin-bottom:2px">
                                        <Badge tone=tone_for_var(delta_color)>{delta_label}</Badge>
                                        <span class="v-timeline-when" title=ev.occurred_at.clone()>{when}</span>
                                    </div>
                                    <div class="v-timeline-summary">{ev.summary}</div>
                                    <div class="v-timeline-origin" style=format!("color:var({origin_color})")>
                                        {ev.origin}
                                    </div>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                    // Bare <LoadMore> (no Pager — this is the timeline
                    // column, not a table footer); "load older" stays
                    // the idle label, exhaustion now shows the
                    // canonical "end of list" line (#33 D3).
                    <LoadMore
                        has_more=has_more
                        busy=loading
                        on_load=Callback::new(on_load_more)
                        label="load older"
                        full=true
                        attr:style="margin-top:8px"
                    />
                }.into_any()
            })
            />
        </div>
    }
}

// ── Story lineage (sidebar) ────────────────────────────────────

#[component]
fn StoryLineage(story_id: String, now_ms: i64) -> impl IntoView {
    let me = use_context::<RwSignal<Option<MeResponse>>>();
    let can_write = Memo::new(move |_| {
        me.and_then(|s| s.get())
            .map(|m| can_write_derivations(&m.role))
            .unwrap_or(false)
    });

    let ancestors = RwSignal::new(Vec::<LineageNode>::new());
    let descendants = RwSignal::new(Vec::<LineageNode>::new());
    let loading = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);

    let sid = story_id.clone();
    Effect::new(move |_| {
        let id = sid.clone();
        spawn_local(async move {
            match api::intel::get_ancestry("story", &id, None).await {
                Ok(body) => {
                    ancestors.set(body.items.into_iter().map(LineageNode::from).collect());
                }
                Err(e) => error.set(Some(format!("ancestry: {e}"))),
            }
            match api::intel::get_descendants("story", &id, None).await {
                Ok(body) => {
                    descendants.set(body.items.into_iter().map(LineageNode::from).collect());
                }
                Err(e) => {
                    error.update(|existing| {
                        let msg = format!("descendants: {e}");
                        *existing = Some(
                            existing
                                .as_ref()
                                .map(|prev| format!("{prev}; {msg}"))
                                .unwrap_or(msg),
                        );
                    });
                }
            }
            loading.set(false);
        });
    });

    view! {
        <div style="margin-top:12px">
            <div class="intel-section-hd">"Lineage"</div>
            <Loaded
                state=Signal::derive(move || {
                    LoadState::from_parts(loading.get(), error.get(), || {
                        (ancestors.get(), descendants.get())
                    })
                })
                label="lineage"
                render=Box::new(move |(anc, desc): (Vec<LineageNode>, Vec<LineageNode>)| {
                    if anc.is_empty() && desc.is_empty() {
                        return view! {
                            <p class="mono" style="font-size:11px;color:var(--ink-3);margin:4px 0">"no derivation history"</p>
                        }.into_any();
                    }
                    view! {
                        <LineageTree
                            label="Sources"
                            nodes=anc
                            can_write=can_write.get()
                            now_ms=now_ms
                        />
                        <LineageTree
                            label="Derived"
                            nodes=desc
                            can_write=can_write.get()
                            now_ms=now_ms
                        />
                    }.into_any()
                })
            />
        </div>
    }
}

// ── Affected products ──────────────────────────────────────────

#[component]
fn AffectedProductsSection(claims: RwSignal<Vec<StoryClaimView>>, now_ms: i64) -> impl IntoView {
    view! {
        <div>
            {move || {
                let all = claims.get();
                let products: Vec<_> = all
                    .iter()
                    .filter(|c| is_affected_product(&c.claim_type))
                    .collect();
                if products.is_empty() {
                    return ().into_any();
                }
                view! {
                    <div class="intel-section-hd">"Affected products"</div>
                    <div class="tbl" style="margin-bottom:16px">
                        <div class="tbl-hd">
                            <div style="flex:2">"Product"</div>
                            <div style="flex:0 0 80px">"Severity"</div>
                            <div style="flex:1">"Affected"</div>
                            <div style="flex:1">"Fix"</div>
                            <div style="flex:0 0 100px">"Disclosed"</div>
                            <div style="flex:0 0 120px;text-align:right">"Source"</div>
                        </div>
                        <div class="tbl-body">
                            {products.into_iter().map(|claim| {
                                let product = extract_product_name(claim);
                                let empty = serde_json::Value::Null;
                                let payload = claim.payload.as_ref().unwrap_or(&empty);
                                let (sev_label, sev_color) = format_severity(payload);
                                let versions = format_versions(payload);
                                let (fix_label, fix_known) = format_fix_status(payload);
                                let source = claim
                                    .source
                                    .as_ref()
                                    .map(|s| s.name.clone())
                                    .unwrap_or_else(|| "\u{2014}".into());
                                let disclosed = claim
                                    .disclosed_at
                                    .as_ref()
                                    .map(|d| time_ago(d, now_ms))
                                    .unwrap_or_else(|| "\u{2014}".into());
                                let disclosed_title = claim
                                    .disclosed_at
                                    .clone()
                                    .unwrap_or_default();
                                let row_class = if fix_known == Some(false) {
                                    "products-row-nofx"
                                } else {
                                    ""
                                };
                                let fix_color = match fix_known {
                                    Some(true) => "--green",
                                    Some(false) => "--red",
                                    None => "--ink-2",
                                };
                                view! {
                                    <div class=format!("tbl-row {row_class}") style="cursor:default">
                                        <div style="flex:2;font-weight:500">{product}</div>
                                        <div style="flex:0 0 80px">
                                            <Badge tone=tone_for_var(sev_color)>{sev_label}</Badge>
                                        </div>
                                        <div style="flex:1;color:var(--ink-2)" class="mono">{versions}</div>
                                        <div style="flex:1">
                                            <span style=format!("color:var({fix_color})")>{fix_label}</span>
                                        </div>
                                        <div style="flex:0 0 100px" class="mono" title=disclosed_title>
                                            <span style="color:var(--ink-2)">{disclosed}</span>
                                        </div>
                                        <div style="flex:0 0 120px;text-align:right;color:var(--ink-3)" class="mono">
                                            {source}
                                        </div>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </div>
                }.into_any()
            }}
        </div>
    }
}

// ── Claims ─────────────────────────────────────────────────────

#[component]
#[allow(clippy::too_many_lines)]
fn ClaimsSection(
    story_id: String,
    items: RwSignal<Vec<StoryClaimView>>,
    now_ms: i64,
) -> impl IntoView {
    let next_cursor = RwSignal::new(None::<String>);
    let loading = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);
    let expanded = RwSignal::new(None::<String>);
    let evidence_cache =
        RwSignal::new(HashMap::<String, Result<Vec<ClaimEvidenceView>, String>>::new());
    let lineage_cache = RwSignal::new(HashMap::<
        String,
        Result<(Vec<LineageNode>, Vec<LineageNode>), String>,
    >::new());
    let collapsed_sources = RwSignal::new(HashSet::<String>::new());
    let show_dupes = RwSignal::new(HashSet::<String>::new());

    let id = story_id.clone();
    Effect::new(move |_| {
        items.set(Vec::new());
        evidence_cache.set(HashMap::new());
        lineage_cache.set(HashMap::new());
        expanded.set(None);
        collapsed_sources.set(HashSet::new());
        show_dupes.set(HashSet::new());
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
        <div>
            <div class="intel-section-hd">"Claims"</div>
            <Loaded
                state=Signal::derive(move || {
                    LoadState::from_parts(loading.get() && items.get().is_empty(), error.get(), || {
                        items.get()
                    })
                })
                label="claims"
                render=Box::new(move |rows: Vec<StoryClaimView>| {
                if rows.is_empty() {
                    return view! {
                        <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"no claims yet"</p>
                    }.into_any();
                }
                let groups = group_claims_by_source(rows);
                let collapsed = collapsed_sources.get();
                let dupes_visible = show_dupes.get();
                let has_more = next_cursor.get().is_some();
                let on_load_more = on_load_more.clone();

                view! {
                    <div class="tbl" style="margin-bottom:16px">
                        <div class="tbl-hd">
                            <div style="flex:0 0 88px">"Rel"</div>
                            <div style="flex:0 0 64px">"Polarity"</div>
                            <div style="flex:2;min-width:0">"Type"</div>
                            <div style="flex:0 0 32px;text-align:center">"Mod"</div>
                            <div style="flex:0 0 48px;text-align:right">"Conf"</div>
                        </div>
                        <div class="tbl-body">
                            {groups.into_iter().map(|(source_name, source_class, source_role, group_claims)| {
                                let type_summary = summarize_claim_types(&group_claims);
                                let sc_display = source_class
                                    .parse::<SourceClass>()
                                    .map(|s| s.to_string())
                                    .unwrap_or(source_class);
                                let (role_label, role_color) = source_role_badge(&source_role);

                                let is_collapsed = collapsed.contains(&source_name);

                                let (primary, duplicates): (Vec<_>, Vec<_>) =
                                    group_claims.into_iter().partition(|c| {
                                        c.relationship.parse::<StoryClaimRelationship>().ok()
                                            != Some(StoryClaimRelationship::Duplicate)
                                    });
                                let dup_count = duplicates.len();
                                let has_dupes = dup_count > 0;
                                let dupes_expanded = dupes_visible.contains(&source_name);

                                let toggle_name = source_name.clone();
                                let on_toggle_source = move |_: web_sys::MouseEvent| {
                                    collapsed_sources.update(|s| {
                                        if !s.remove(&toggle_name) {
                                            s.insert(toggle_name.clone());
                                        }
                                    });
                                };

                                let display_name = source_name.clone();
                                view! {
                                    <div>
                                        <div class="source-group-hd" on:click=on_toggle_source>
                                            <span style="font-weight:600">{display_name}</span>
                                            <Badge>{sc_display}</Badge>
                                            {(!role_label.is_empty()).then(|| view! {
                                                <Badge tone=tone_for_var(role_color)>{role_label}</Badge>
                                            })}
                                            <span class="source-group-summary">{type_summary}</span>
                                        </div>
                                        {(!is_collapsed).then(move || {
                                            let dupe_toggle_name = source_name;
                                            let on_toggle_dupes = move |_: web_sys::MouseEvent| {
                                                let n = dupe_toggle_name.clone();
                                                show_dupes.update(|s| {
                                                    if !s.remove(&n) {
                                                        s.insert(n);
                                                    }
                                                });
                                            };

                                            view! {
                                                <div>
                                                    {primary.into_iter().map(|claim| {
                                                        claim_row(claim, false, expanded, evidence_cache, lineage_cache, now_ms)
                                                    }).collect::<Vec<_>>()}
                                                    {has_dupes.then(move || view! {
                                                        <div>
                                                            <div class="dup-badge" style="padding:4px 12px" on:click=on_toggle_dupes>
                                                                {format!("+ {dup_count} corroborating")}
                                                            </div>
                                                            {dupes_expanded.then(move || {
                                                                duplicates.into_iter().map(|claim| {
                                                                    claim_row(claim, true, expanded, evidence_cache, lineage_cache, now_ms)
                                                                }).collect::<Vec<_>>()
                                                            })}
                                                        </div>
                                                    })}
                                                </div>
                                            }
                                        })}
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        // <LoadMore> owns the three-state footer; the
                        // canonical "end of list" line on exhausted
                        // lists is the sanctioned visual delta (#33 D3).
                        <Pager summary=String::new()>
                            <LoadMore
                                has_more=has_more
                                busy=loading
                                on_load=Callback::new(on_load_more)
                            />
                        </Pager>
                    </div>
                }.into_any()
            })
            />
        </div>
    }
}

#[allow(clippy::too_many_lines)]
fn claim_row(
    claim: StoryClaimView,
    dim: bool,
    expanded: RwSignal<Option<String>>,
    evidence_cache: RwSignal<HashMap<String, Result<Vec<ClaimEvidenceView>, String>>>,
    lineage_cache: RwSignal<HashMap<String, Result<(Vec<LineageNode>, Vec<LineageNode>), String>>>,
    now_ms: i64,
) -> impl IntoView {
    let claim_id = claim.claim_id.clone();
    let is_warning = matches!(
        claim.polarity.parse::<Polarity>(),
        Ok(Polarity::Denied | Polarity::Retracted)
    ) || matches!(
        claim.relationship.parse::<StoryClaimRelationship>(),
        Ok(StoryClaimRelationship::Contradiction)
    );

    let mut row_style = String::from("cursor:pointer;");
    if is_warning {
        row_style.push_str("background:var(--red-wash);");
    }
    if claim.material {
        row_style.push_str("border-left:2px solid var(--amber);");
    }

    let dim_class = if dim { "tbl-row claim-dim" } else { "tbl-row" };
    let (rel_label, rel_color) = relationship_badge(&claim.relationship);
    let (pol_label, pol_color) = polarity_badge(&claim.polarity);
    let claim_type = claim.claim_type.replace('_', " ");
    let (mod_icon, mod_color) = modality_icon(&claim.modality);
    let conf_pct = claim.claim_confidence * 100.0;
    let conf_color = conf_bar_color(claim.claim_confidence);

    // Build the meta line items
    let meta_parts = build_claim_meta(&claim, now_ms);
    let claim_markings = claim.markings.clone();

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
                            result.map(|page| page.items).map_err(|e| e.to_string()),
                        );
                    });
                });
            }
            let lineage_cached = lineage_cache.get_untracked();
            if !lineage_cached.contains_key(&cid) {
                let cid2 = cid.clone();
                spawn_local(async move {
                    let anc = api::intel::get_ancestry("claim", &cid2, Some(2)).await;
                    let desc = api::intel::get_descendants("claim", &cid2, Some(2)).await;
                    let result = match (anc, desc) {
                        (Ok(a), Ok(d)) => Ok((
                            a.items.into_iter().map(LineageNode::from).collect(),
                            d.items.into_iter().map(LineageNode::from).collect(),
                        )),
                        (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
                    };
                    lineage_cache.update(|m| {
                        m.insert(cid2, result);
                    });
                });
            }
        }
    };

    let data_id = claim_id.clone();
    let evidence_id = claim_id;

    view! {
        <div data-claim-id=data_id>
            <div class=dim_class style=row_style on:click=on_toggle>
                <div style="flex:0 0 88px">
                    <Badge tone=tone_for_var(rel_color)>{rel_label}</Badge>
                </div>
                <div style="flex:0 0 64px">
                    <Badge tone=tone_for_var(pol_color)>{pol_label}</Badge>
                </div>
                <div style="flex:2;min-width:0">{claim_type}</div>
                <div style="flex:0 0 32px;text-align:center">
                    <span class="modality-icon" style=format!("color:var({mod_color})") title=claim.modality>
                        {mod_icon}
                    </span>
                </div>
                <div style="flex:0 0 48px;text-align:right">
                    <span
                        class="conf-bar"
                        style=format!(
                            "background:linear-gradient(to right, var({conf_color}) {conf_pct:.0}%, var(--panel-2) {conf_pct:.0}%)"
                        )
                        title=format!("{conf_pct:.0}%")
                    />
                </div>
            </div>
            {(!meta_parts.is_empty() || !claim_markings.is_empty()).then(|| {
                view! {
                    <div class="claim-meta">
                        {meta_parts.into_iter().enumerate().map(|(i, part)| {
                            view! {
                                <>
                                    {(i > 0).then(|| view! { <span class="sep">"\u{00b7}"</span> })}
                                    {part}
                                </>
                            }
                        }).collect::<Vec<_>>()}
                        {claim_markings.into_iter().map(|m| {
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
                    </div>
                }
            })}
            {move || {
                let exp = expanded.get();
                if exp.as_deref() != Some(&evidence_id) {
                    return ().into_any();
                }
                let eid_state = evidence_id.clone();
                let eid_render = evidence_id.clone();
                view! {
                    <div class="evidence-panel">
                        <Loaded
                            state=Signal::derive(move || {
                                LoadState::from_resource(evidence_cache.get().get(&eid_state).cloned())
                            })
                            label="evidence"
                            render=Box::new(move |evs: Vec<ClaimEvidenceView>| {
                    if evs.is_empty() {
                        return view! {
                            <span class="mono" style="color:var(--ink-3)">"no evidence records"</span>
                        }.into_any();
                    }
                        let lineage = lineage_cache.get();
                        let lineage_view = match lineage.get(&eid_render) {
                            Some(Ok((anc, desc))) if !anc.is_empty() || !desc.is_empty() => {
                                view! {
                                    <div style="margin-top:8px;padding-top:8px;border-top:1px solid var(--line)">
                                        <LineageTree
                                            label="Sources"
                                            nodes=anc.clone()
                                            can_write=false
                                            now_ms=now_ms
                                        />
                                        <LineageTree
                                            label="Derived"
                                            nodes=desc.clone()
                                            can_write=false
                                            now_ms=now_ms
                                        />
                                    </div>
                                }.into_any()
                            }
                            _ => ().into_any(),
                        };
                        view! {
                            <div>
                                {evs.iter().map(|ev| {
                                    let factual = ev.factual_summary.clone()
                                        .unwrap_or_else(|| "\u{2014}".into());
                                    let claim_s = ev.claim_summary.clone()
                                        .unwrap_or_else(|| "\u{2014}".into());
                                    let ingested = time_ago(&ev.created_at, now_ms);
                                    view! {
                                        <div style="margin-bottom:8px;padding-bottom:8px;border-bottom:1px solid var(--line)">
                                            <div style="font-size:var(--table-fs);margin-bottom:2px">
                                                <strong>"factual: "</strong>{factual}
                                            </div>
                                            <div style="font-size:var(--table-fs);color:var(--ink-2)">
                                                <strong>"claim: "</strong>{claim_s}
                                            </div>
                                            <div class="mono" style="font-size:10px;color:var(--ink-4);margin-top:2px;display:flex;gap:8px;flex-wrap:wrap">
                                                <span>{format!("post {}", ev.post_id)}</span>
                                                <span>{format!("fragment #{} span {}\u{2013}{}", ev.fragment_index, ev.span_start, ev.span_end)}</span>
                                                <span title=ev.created_at.clone()>{format!("ingested {ingested}")}</span>
                                            </div>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                                {lineage_view}
                            </div>
                        }.into_any()
                            })
                        />
                    </div>
                }.into_any()
            }}
        </div>
    }
}

// ── Relations ──────────────────────────────────────────────────

#[component]
fn RelationsSection(
    story_id: String,
    relations: LocalResource<Result<PaginatedBody<StoryRelationView>, ApiError>>,
    now_ms: i64,
) -> impl IntoView {
    view! {
        <div>
            <div class="intel-section-hd">"Related stories"</div>
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(relations.get()))
                label="related stories"
                render=Box::new(move |body: PaginatedBody<StoryRelationView>| {
                    if body.items.is_empty() {
                        return view! {
                            <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"no related stories"</p>
                        }.into_any();
                    }
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
                                    let linked_ago = time_ago(&rel.created_at, now_ms);
                                    view! {
                                        <div class="tbl-row" style="cursor:default">
                                            <div style="flex:0 0 100px">
                                                <Badge>{rel_label}</Badge>
                                            </div>
                                            <div style="flex:1">
                                                <a class="link" href=href>{other_id.clone()}</a>
                                                <span class="mono" style="color:var(--ink-3);font-size:10px">{conf}</span>
                                            </div>
                                            <div style="flex:0 0 80px;text-align:right" class="mono">
                                                <span style="color:var(--ink-4);font-size:10px" title=rel.created_at.clone()>
                                                    {linked_ago}
                                                </span>
                                            </div>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        </div>
                    }.into_any()
                })
            />
        </div>
    }
}

// ── Helpers ────────────────────────────────────────────────────

fn build_claim_meta(claim: &StoryClaimView, now_ms: i64) -> Vec<String> {
    let mut parts = Vec::new();

    // subject → object entities
    let subj = claim
        .subject_entity
        .as_ref()
        .map(|e| e.canonical_name.clone());
    let obj = claim
        .object_entity
        .as_ref()
        .map(|e| e.canonical_name.clone());
    match (subj, obj) {
        (Some(s), Some(o)) => parts.push(format!("{s} \u{2192} {o}")),
        (Some(s), None) => parts.push(s),
        (None, Some(o)) => parts.push(o),
        (None, None) => {}
    }

    if let Some(ref ts) = claim.asserted_at {
        parts.push(format!("asserted {}", time_ago(ts, now_ms)));
    }
    if let Some(ref ts) = claim.disclosed_at {
        parts.push(format!("disclosed {}", time_ago(ts, now_ms)));
    }
    if let Some(ref tr) = claim.event_time_range {
        let label = format_time_range("event", tr, now_ms);
        if !label.is_empty() {
            parts.push(label);
        }
    }
    if let Some(ref tr) = claim.observed_time_range {
        let label = format_time_range("observed", tr, now_ms);
        if !label.is_empty() {
            parts.push(label);
        }
    }
    if let Some(conf) = claim.confidence {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        parts.push(format!("attach: {:.0}%", conf * 100.0));
    }
    if let Some(conf) = claim.attribution_confidence {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        parts.push(format!("attrib: {:.0}%", conf * 100.0));
    }
    if let Some(ref by) = claim.attached_by {
        parts.push(by.clone());
    }

    parts
}

fn format_time_range(prefix: &str, range: &TimeRange, now_ms: i64) -> String {
    match (&range.start, &range.end) {
        (Some(s), Some(e)) => format!(
            "{prefix}: {} \u{2013} {}",
            time_ago(s, now_ms),
            time_ago(e, now_ms)
        ),
        (Some(s), None) => format!("{prefix}: {} \u{2013}", time_ago(s, now_ms)),
        (None, Some(e)) => format!("{prefix}: \u{2013} {}", time_ago(e, now_ms)),
        (None, None) => String::new(),
    }
}

fn derive_header_meta(claims: &[StoryClaimView]) -> HeaderMeta {
    let mut source_names = Vec::new();
    let mut product_names = Vec::new();
    let mut has_fix = false;

    for claim in claims {
        if let Some(ref src) = claim.source {
            if !source_names.contains(&src.name) {
                source_names.push(src.name.clone());
            }
        }
        if is_affected_product(&claim.claim_type) {
            let name = extract_product_name(claim);
            if name != "\u{2014}" && !product_names.contains(&name) {
                product_names.push(name);
            }
            if let Some(status) = claim
                .payload
                .as_ref()
                .and_then(|p| p.get("fix_status"))
                .and_then(serde_json::Value::as_str)
            {
                if status == "patched" {
                    has_fix = true;
                }
            }
        }
    }

    HeaderMeta {
        source_names,
        product_names,
        has_fix,
    }
}

fn is_affected_product(ct: &str) -> bool {
    ct == "AffectedProduct" || ct == "affected_product"
}

fn extract_product_name(claim: &StoryClaimView) -> String {
    if let Some(ref entity) = claim.object_entity {
        return entity.canonical_name.clone();
    }
    let vendor = claim
        .payload
        .as_ref()
        .and_then(|p| p.get("vendor"))
        .and_then(serde_json::Value::as_str);
    let product = claim
        .payload
        .as_ref()
        .and_then(|p| p.get("product"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            claim
                .payload
                .as_ref()
                .and_then(|p| p.get("affected_product"))
                .and_then(serde_json::Value::as_str)
        });
    match (vendor, product) {
        (Some(v), Some(p)) if v != p => format!("{v} {p}"),
        (_, Some(p)) => p.to_string(),
        (Some(v), None) => v.to_string(),
        (None, None) => "\u{2014}".into(),
    }
}

fn format_severity(payload: &serde_json::Value) -> (String, &'static str) {
    let Some(severity) = payload.get("severity").and_then(serde_json::Value::as_str) else {
        return ("\u{2014}".into(), "--ink-3");
    };
    let color = match severity {
        "critical" | "high" => "--red",
        "medium" => "--amber",
        "low" => "--green",
        _ => "--ink-3",
    };
    let label = if let Some(cvss) = payload.get("cvss_v3").and_then(serde_json::Value::as_f64) {
        format!("{severity} ({cvss:.1})")
    } else {
        severity.to_string()
    };
    (label, color)
}

fn format_versions(payload: &serde_json::Value) -> String {
    if let Some(av) = payload.get("affected_versions") {
        if let Some(obj) = av.as_object() {
            let introduced = obj
                .get("introduced")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            return match obj.get("fixed").and_then(serde_json::Value::as_str) {
                Some(f) => format!("{introduced} \u{2013} {f}"),
                None => format!("\u{2265} {introduced}"),
            };
        } else if let Some(s) = av.as_str() {
            return s.to_string();
        }
    }
    if let Some(vr) = payload.get("version_range") {
        if let Some(exact) = vr.get("Exact").and_then(serde_json::Value::as_str) {
            return exact.to_string();
        }
        if let Some(obj) = vr.as_object() {
            if let Some(range) = obj.get("Range") {
                let start = range
                    .get("start")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let end = range
                    .get("end")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                return format!("{start} \u{2013} {end}");
            }
        }
        if let Some(s) = vr.as_str() {
            return s.to_string();
        }
    }
    "\u{2014}".into()
}

fn format_fix_status(payload: &serde_json::Value) -> (String, Option<bool>) {
    let status = payload
        .get("fix_status")
        .and_then(serde_json::Value::as_str);
    let fixed_ver = payload
        .get("affected_versions")
        .and_then(|v| v.get("fixed"))
        .and_then(serde_json::Value::as_str);

    match (status, fixed_ver) {
        (Some("patched"), Some(ver)) => (ver.to_string(), Some(true)),
        (Some("patched"), None) => ("patched".into(), Some(true)),
        (Some("no_fix"), _) => ("no fix".into(), Some(false)),
        (Some("eol"), _) => ("end of life".into(), Some(false)),
        (Some(other), _) => (other.replace('_', " "), None),
        (None, Some(ver)) => (ver.to_string(), Some(true)),
        (None, None) => ("\u{2014}".into(), None),
    }
}

fn delta_badge(delta_type: &str) -> (String, &'static str) {
    match delta_type {
        "initial_report" => ("initial report".into(), "--amber"),
        "exploitation_confirmed" => ("exploited".into(), "--red"),
        "patch_released" => ("patch released".into(), "--green"),
        "correction" => ("correction".into(), "--yellow"),
        "supersession" => ("superseded".into(), "--yellow"),
        "expansion" => ("expansion".into(), "--ink-3"),
        other => (other.replace('_', " "), "--ink-3"),
    }
}

fn modality_icon(modality: &str) -> (&'static str, &'static str) {
    match modality.parse::<Modality>() {
        Ok(Modality::Observed | Modality::Claimed | Modality::Quoted) => ("\u{25c9}", "--ink-2"),
        Ok(Modality::Inferred | Modality::Assessed | Modality::Predicted) => {
            ("\u{25c8}", "--ink-3")
        }
        Ok(Modality::Rumored) => ("\u{25c7}", "--amber"),
        Err(_) => ("?", "--ink-4"),
    }
}

fn conf_bar_color(confidence: f64) -> &'static str {
    if confidence < 0.4 {
        "--red"
    } else if confidence < 0.7 {
        "--amber"
    } else {
        "--green"
    }
}

fn source_role_badge(role: &str) -> (String, &'static str) {
    match role {
        "primary_author" => ("primary".into(), "--green"),
        "corroborating" => ("corroborating".into(), "--ink-3"),
        "aggregator" => ("aggregator".into(), "--blue"),
        "republisher" => ("republisher".into(), "--ink-4"),
        "commentary" => ("commentary".into(), "--ink-3"),
        "" => (String::new(), "--ink-4"),
        other => (other.replace('_', " "), "--ink-3"),
    }
}

fn group_claims_by_source(
    claims: Vec<StoryClaimView>,
) -> Vec<(String, String, String, Vec<StoryClaimView>)> {
    let mut groups: Vec<(String, String, String, Vec<StoryClaimView>)> = Vec::new();
    let mut unknown: Vec<StoryClaimView> = Vec::new();

    for claim in claims {
        let key = claim.source.as_ref().map(|s| s.name.clone());
        match key {
            Some(name) => {
                if let Some(group) = groups.iter_mut().find(|(n, _, _, _)| *n == name) {
                    group.3.push(claim);
                } else {
                    let class = claim
                        .source
                        .as_ref()
                        .map(|s| s.source_class.clone())
                        .unwrap_or_default();
                    let role = claim.source_role.clone().unwrap_or_default();
                    groups.push((name, class, role, vec![claim]));
                }
            }
            None => unknown.push(claim),
        }
    }

    if !unknown.is_empty() {
        groups.push((
            "unknown source".into(),
            String::new(),
            String::new(),
            unknown,
        ));
    }

    groups
}

fn summarize_claim_types(claims: &[StoryClaimView]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for c in claims {
        if let Some(entry) = counts.iter_mut().find(|(t, _)| *t == c.claim_type) {
            entry.1 += 1;
        } else {
            counts.push((c.claim_type.clone(), 1));
        }
    }
    counts
        .iter()
        .map(|(t, n)| format!("{n}\u{00d7} {}", t.replace('_', " ")))
        .collect::<Vec<_>>()
        .join(" \u{00b7} ")
}

fn relationship_badge(s: &str) -> (&'static str, &'static str) {
    match s.parse::<StoryClaimRelationship>() {
        Ok(StoryClaimRelationship::Evidence) => ("evidence", "--green"),
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
