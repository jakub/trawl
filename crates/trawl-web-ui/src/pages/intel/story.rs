// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, HashSet};

use coastwatch_api_types::claim::ClaimEvidenceView;
use coastwatch_api_types::enums::{
    Modality, Polarity, SourceClass, StoryClaimRelationship, StoryRelation,
};
use coastwatch_api_types::marking::MarkingView;
use coastwatch_api_types::story::{StoryClaimView, TimelineEventView};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;

use crate::api;
use crate::api::ApiError;
use crate::time_fmt::time_ago;

use super::stories::{class_label, state_badge};

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

    let header_meta = Memo::new(move |_| {
        let claims = all_claims.get();
        if claims.is_empty() {
            return None;
        }
        Some(derive_header_meta(&claims))
    });

    view! {
        <div class="page">
            {move || match story_resource.get() {
                None => view! {
                    <div class="page-hd compact">
                        <div><h1 class="mono" style="color:var(--ink-3)">"loading\u{2026}"</h1></div>
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
                            meta=header_meta
                        />
                        <StorySummary summary=summary/>
                        <AffectedProductsSection claims=all_claims/>
                        <TimelineSection story_id=story_id.clone() now_ms=now_ms/>
                        <ClaimsSection story_id=story_id.clone() items=all_claims/>
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
    meta: Memo<Option<HeaderMeta>>,
) -> impl IntoView {
    let score = importance.map(|s| format!("{s:.1}")).unwrap_or_default();

    view! {
        <div class="page-hd compact" style="border-left:2px solid var(--amber)">
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

// ── Affected products ───────────────────────────────────────────

#[component]
fn AffectedProductsSection(claims: RwSignal<Vec<StoryClaimView>>) -> impl IntoView {
    view! {
        <div style="padding:0 var(--pad)">
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
                            <div style="flex:0 0 140px;text-align:right">"Source"</div>
                        </div>
                        <div class="tbl-body">
                            {products.into_iter().map(|claim| {
                                let product = extract_product_name(claim);
                                let (sev_label, sev_color) = format_severity(&claim.payload);
                                let versions = format_versions(&claim.payload);
                                let (fix_label, fix_known) = format_fix_status(&claim.payload);
                                let source = claim
                                    .source
                                    .as_ref()
                                    .map(|s| s.name.clone())
                                    .unwrap_or_else(|| "\u{2014}".into());
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
                                            <span class="intel-badge"
                                                style=format!("background:var({sev_color}-wash,var(--panel-2));color:var({sev_color})")
                                            >{sev_label}</span>
                                        </div>
                                        <div style="flex:1;color:var(--ink-2)" class="mono">{versions}</div>
                                        <div style="flex:1">
                                            <span style=format!("color:var({fix_color})")>{fix_label}</span>
                                        </div>
                                        <div style="flex:0 0 140px;text-align:right;color:var(--ink-3)" class="mono">
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

// ── Timeline ────────────────────────────────────────────────────

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
                        <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"loading\u{2026}"</p>
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
                                let (delta_label, delta_color) = delta_badge(&ev.delta_type);
                                let material_style = if ev.material {
                                    "border-left:2px solid var(--amber);"
                                } else {
                                    ""
                                };
                                let origin_color = match ev.origin.as_str() {
                                    "analyst" => "--amber",
                                    "backfill" => "--ink-4",
                                    _ => "--ink-3",
                                };
                                view! {
                                    <div class="tbl-row" style=format!("cursor:default;{material_style}")>
                                        <div style="flex:0 0 72px;color:var(--ink-3)" class="mono" title=ev.occurred_at.clone()>
                                            {when}
                                        </div>
                                        <div style="flex:0 0 140px;display:flex;align-items:center;gap:6px">
                                            <span class="intel-badge"
                                                style=format!("background:var({delta_color}-wash,var(--panel-2));color:var({delta_color})")
                                            >{delta_label}</span>
                                        </div>
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

// ── Claims ──────────────────────────────────────────────────────

#[component]
#[allow(clippy::too_many_lines)]
fn ClaimsSection(story_id: String, items: RwSignal<Vec<StoryClaimView>>) -> impl IntoView {
    let next_cursor = RwSignal::new(None::<String>);
    let loading = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);
    let expanded = RwSignal::new(None::<String>);
    let evidence_cache =
        RwSignal::new(HashMap::<String, Result<Vec<ClaimEvidenceView>, String>>::new());
    let collapsed_sources = RwSignal::new(HashSet::<String>::new());
    let show_dupes = RwSignal::new(HashSet::<String>::new());

    let id = story_id.clone();
    Effect::new(move |_| {
        items.set(Vec::new());
        evidence_cache.set(HashMap::new());
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
        <div style="padding:0 var(--pad)">
            <div class="intel-section-hd">"Claims"</div>
            {move || {
                if loading.get() && items.get().is_empty() {
                    return view! {
                        <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"loading\u{2026}"</p>
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
                            {groups.into_iter().map(|(source_name, source_class, group_claims)| {
                                let type_summary = summarize_claim_types(&group_claims);
                                let sc_display = source_class
                                    .parse::<SourceClass>()
                                    .map(|s| s.to_string())
                                    .unwrap_or(source_class);

                                let is_collapsed = collapsed.contains(&source_name);

                                let (primary, duplicates): (Vec<_>, Vec<_>) =
                                    group_claims.into_iter().partition(|c| {
                                        c.relationship.parse::<StoryClaimRelationship>()
                                            != Ok(StoryClaimRelationship::Duplicate)
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

                                view! {
                                    <div>
                                        <div class="source-group-hd" on:click=on_toggle_source>
                                            <span style="font-weight:600">{&source_name}</span>
                                            <span class="intel-badge" style="background:var(--panel-2);color:var(--ink-3)">
                                                {sc_display}
                                            </span>
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
                                                        claim_row(claim, false, expanded, evidence_cache)
                                                    }).collect::<Vec<_>>()}
                                                    {has_dupes.then(move || view! {
                                                        <div>
                                                            <div class="dup-badge" style="padding:4px 12px" on:click=on_toggle_dupes>
                                                                {format!("+ {dup_count} corroborating")}
                                                            </div>
                                                            {dupes_expanded.then(move || {
                                                                duplicates.into_iter().map(|claim| {
                                                                    claim_row(claim, true, expanded, evidence_cache)
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

#[allow(clippy::too_many_lines)]
fn claim_row(
    claim: StoryClaimView,
    dim: bool,
    expanded: RwSignal<Option<String>>,
    evidence_cache: RwSignal<HashMap<String, Result<Vec<ClaimEvidenceView>, String>>>,
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
        }
    };

    let evidence_id = claim_id;

    view! {
        <div>
            <div class=dim_class style=row_style on:click=on_toggle>
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
            {move || {
                let exp = expanded.get();
                if exp.as_deref() != Some(&evidence_id) {
                    return ().into_any();
                }
                let cached = evidence_cache.get();
                match cached.get(&evidence_id) {
                    None => view! {
                        <div class="evidence-panel">
                            <span class="mono" style="color:var(--ink-3)">"loading evidence\u{2026}"</span>
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
                                            {format!("fragment {} span {}\u{2013}{}", ev.fragment_id, ev.span_start, ev.span_end)}
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
}

// ── Relations ───────────────────────────────────────────────────

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
                    <p class="mono" style="color:var(--ink-3);font-size:var(--table-fs)">"loading\u{2026}"</p>
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

// ── Helpers ─────────────────────────────────────────────────────

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
            if let Some(ref entity) = claim.object_entity {
                if !product_names.contains(&entity.canonical_name) {
                    product_names.push(entity.canonical_name.clone());
                }
            }
            if let Some(status) = claim
                .payload
                .get("fix_status")
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
    claim
        .object_entity
        .as_ref()
        .map(|e| e.canonical_name.clone())
        .or_else(|| {
            claim
                .payload
                .get("affected_product")
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        })
        .unwrap_or_else(|| "\u{2014}".into())
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
    let Some(av) = payload.get("affected_versions") else {
        return "\u{2014}".into();
    };
    if let Some(obj) = av.as_object() {
        let introduced = obj
            .get("introduced")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        match obj.get("fixed").and_then(serde_json::Value::as_str) {
            Some(f) => format!("{introduced} \u{2013} {f}"),
            None => format!("\u{2265} {introduced}"),
        }
    } else if let Some(s) = av.as_str() {
        s.to_string()
    } else {
        "\u{2014}".into()
    }
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

fn group_claims_by_source(
    claims: Vec<StoryClaimView>,
) -> Vec<(String, String, Vec<StoryClaimView>)> {
    let mut groups: Vec<(String, String, Vec<StoryClaimView>)> = Vec::new();
    let mut unknown: Vec<StoryClaimView> = Vec::new();

    for claim in claims {
        let key = claim.source.as_ref().map(|s| s.name.clone());
        match key {
            Some(name) => {
                if let Some(group) = groups.iter_mut().find(|(n, _, _)| *n == name) {
                    group.2.push(claim);
                } else {
                    let class = claim
                        .source
                        .as_ref()
                        .map(|s| s.source_class.clone())
                        .unwrap_or_default();
                    groups.push((name, class, vec![claim]));
                }
            }
            None => unknown.push(claim),
        }
    }

    if !unknown.is_empty() {
        groups.push(("unknown source".into(), String::new(), unknown));
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
