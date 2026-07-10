// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use coastwatch_api_types::derivation::DerivationView;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api;
use crate::api::MeResponse;
use crate::components::lineage_tree::{
    LineageNode, LineageTree, can_write_derivations, render_derivation_meta, render_marking_diff,
    transformation_color,
};
use crate::time_fmt::time_ago;
use fleet_ui::{Btn, ConfirmWithReasonModal, Pager, ToastBus, ToastKind, Variant};

const OBJECT_TYPES: &[&str] = &[
    "story",
    "claim",
    "entity",
    "post",
    "event_fragment",
    "observable",
    "news_item",
];

#[component]
#[allow(clippy::too_many_lines)]
pub fn DerivationsPage() -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let me = use_context::<RwSignal<Option<MeResponse>>>();
    let can_write = Memo::new(move |_| {
        me.and_then(|s| s.get())
            .map(|m| can_write_derivations(&m.role))
            .unwrap_or(false)
    });

    let qm = use_query_map();
    let nav = use_navigate();

    let obj_type = RwSignal::new(String::new());
    let obj_id = RwSignal::new(String::new());

    let ancestors = RwSignal::new(Vec::<LineageNode>::new());
    let descendants = RwSignal::new(Vec::<LineageNode>::new());
    let edges = RwSignal::new(Vec::<DerivationView>::new());
    let edges_cursor = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let loaded_object = RwSignal::new(None::<(String, String)>);

    let invalidate_target = RwSignal::new(None::<String>);
    let retract_modal = RwSignal::new(false);
    let refresh = RwSignal::new(0u64);

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = js_sys::Date::new_0().get_time() as i64;

    // Pre-populate from query params on mount
    Effect::new(move |_| {
        let params = qm.get();
        let t = params.get("type").unwrap_or_default();
        let id = params.get("id").unwrap_or_default();
        if !t.is_empty() && !id.is_empty() {
            obj_type.set(t.clone());
            obj_id.set(id.clone());
            load_lineage(
                t,
                id,
                ancestors,
                descendants,
                edges,
                edges_cursor,
                loading,
                error,
                loaded_object,
            );
        }
    });

    // Re-load when refresh counter bumps
    Effect::new(move |prev: Option<u64>| {
        let r = refresh.get();
        if prev.is_some() && r > 0 {
            if let Some((t, id)) = loaded_object.get_untracked() {
                load_lineage(
                    t,
                    id,
                    ancestors,
                    descendants,
                    edges,
                    edges_cursor,
                    loading,
                    error,
                    loaded_object,
                );
            }
        }
        r
    });

    let nav2 = nav.clone();
    let on_submit_key = move |e: web_sys::KeyboardEvent| {
        if e.key() == "Enter" {
            let t = obj_type.get_untracked();
            let id = obj_id.get_untracked();
            if !t.is_empty() && !id.is_empty() {
                nav(
                    &format!("/intel/derivations?type={t}&id={id}"),
                    NavigateOptions {
                        replace: true,
                        ..Default::default()
                    },
                );
                load_lineage(
                    t,
                    id,
                    ancestors,
                    descendants,
                    edges,
                    edges_cursor,
                    loading,
                    error,
                    loaded_object,
                );
            }
        }
    };
    let on_submit_click = move |_| {
        let t = obj_type.get_untracked();
        let id = obj_id.get_untracked();
        if !t.is_empty() && !id.is_empty() {
            nav2(
                &format!("/intel/derivations?type={t}&id={id}"),
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            );
            load_lineage(
                t,
                id,
                ancestors,
                descendants,
                edges,
                edges_cursor,
                loading,
                error,
                loaded_object,
            );
        }
    };

    let on_invalidate = Callback::new(move |drv_id: String| {
        invalidate_target.set(Some(drv_id));
    });

    let on_load_more_edges = move |_| {
        let Some((t, id)) = loaded_object.get_untracked() else {
            return;
        };
        let Some(cursor) = edges_cursor.get_untracked() else {
            return;
        };
        loading.set(true);
        spawn_local(async move {
            match api::intel::list_object_derivations(&t, &id, Some(&cursor)).await {
                Ok(page) => {
                    edges_cursor.set(page.next_cursor);
                    edges.update(|v| v.extend(page.items));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div style="display:flex;align-items:center;gap:12px">
                    <h1 style="margin:0">"Derivations"</h1>
                </div>
            </div>

            // Search form
            <div style="padding:0 var(--pad);margin-bottom:16px;display:flex;gap:8px;align-items:flex-end;flex-wrap:wrap">
                <div class="m-field">
                    <label>"Object type"</label>
                    <select
                        class="sel"
                        prop:value=move || obj_type.get()
                        on:change=move |e| obj_type.set(event_target_value(&e))
                    >
                        <option value="">"select\u{2026}"</option>
                        {OBJECT_TYPES.iter().map(|t| {
                            let label = t.replace('_', " ");
                            view! { <option value=*t>{label}</option> }
                        }).collect::<Vec<_>>()}
                    </select>
                </div>
                <div class="m-field" style="flex:1;min-width:200px">
                    <label>"Object ID"</label>
                    <input
                        type="text"
                        placeholder="e.g. cla_01JV..."
                        prop:value=move || obj_id.get()
                        on:input=move |e| obj_id.set(event_target_value(&e))
                        on:keydown=on_submit_key
                    />
                </div>
                <Btn variant=Variant::Primary on_click=Callback::new(on_submit_click)>"Load lineage"</Btn>
            </div>

            // Loading / error — the canonical fleet-ui hint class + copy,
            // rendered directly (not via <Loaded/>) because this page
            // shows the indicators ABOVE content that stays visible
            // while a reload is in flight.
            {move || loading.get().then(|| view! {
                <div style="padding:0 var(--pad)">
                    <div class="load-hint">{fleet_ui::loaded::loading_copy(Some("lineage"))}</div>
                </div>
            })}
            {move || error.get().map(|e| view! {
                <div style="padding:0 var(--pad)">
                    <div class="load-hint error">{fleet_ui::loaded::error_copy(Some("lineage"), &e)}</div>
                </div>
            })}

            // Lineage tree
            {move || loaded_object.get().map(|_| {
                let anc = ancestors.get();
                let desc = descendants.get();
                view! {
                    <div style="padding:0 var(--pad);margin-bottom:16px">
                        <LineageTree
                            label="Sources (ancestry)"
                            nodes=anc
                            can_write=can_write.get()
                            on_invalidate=on_invalidate
                            now_ms=now_ms
                        />
                        <LineageTree
                            label="Derived (descendants)"
                            nodes=desc
                            can_write=can_write.get()
                            on_invalidate=on_invalidate
                            now_ms=now_ms
                        />
                    </div>
                }
            })}

            // Derivation edges list
            {move || {
                let items = edges.get();
                let obj = loaded_object.get();
                if obj.is_none() {
                    return ().into_any();
                }
                view! {
                    <div style="padding:0 var(--pad)">
                        <div class="intel-section-hd">
                            {format!("Derivation edges ({})", items.len())}
                        </div>
                        <div class="tbl">
                            <div class="tbl-body">
                                {items.into_iter().map(|d| {
                                    let is_inv = d.invalidated_at.is_some();
                                    let t_color = transformation_color(&d.transformation);
                                    let t_label = d.transformation.replace('_', " ");
                                    let row_style = if is_inv {
                                        "opacity:0.5"
                                    } else {
                                        ""
                                    };
                                    let source = format!("{} {}", d.source_object_type, d.source_object_id);
                                    let derived = format!("{} {}", d.derived_object_type, d.derived_object_id);
                                    let created = time_ago(&d.created_at, now_ms);
                                    let marking = render_marking_diff(d.marking_before, d.marking_after);
                                    let meta = render_derivation_meta(
                                        d.redaction_reason,
                                        d.approved_by_principal_id,
                                        d.approved_at,
                                        now_ms,
                                    );
                                    let inv_reason = d.invalidation_reason.clone().unwrap_or_default();
                                    view! {
                                        <div class="tbl-row" style=row_style title=inv_reason>
                                            <div style="flex:0 0 100px">
                                                <span
                                                    class="intel-badge"
                                                    style=format!(
                                                        "background:var({t_color}-wash,var(--panel-2));color:var({t_color})"
                                                    )
                                                >
                                                    {t_label}
                                                </span>
                                            </div>
                                            <div class="mono" style="flex:1;font-size:10px;color:var(--ink-2);min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">
                                                {source}" \u{2192} "{derived}
                                            </div>
                                            <div style="flex:0 0 auto;display:flex;gap:4px;align-items:center">
                                                {marking}
                                                {meta}
                                            </div>
                                            <div class="mono" style="flex:0 0 80px;font-size:10px;color:var(--ink-4);text-align:right">
                                                {created}
                                            </div>
                                            {is_inv.then(|| view! {
                                                <div style="flex:0 0 70px;text-align:right">
                                                    <span class="intel-badge" style="background:var(--red-wash);color:var(--red)">
                                                        "invalidated"
                                                    </span>
                                                </div>
                                            })}
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                            {move || edges_cursor.get().map(|_| {
                                let is_loading = loading.get();
                                view! {
                                    <Pager summary=String::new()>
                                        <Btn
                                            variant=Variant::Secondary
                                            disabled=is_loading
                                            on_click=Callback::new(on_load_more_edges)
                                        >
                                            {if is_loading { "loading\u{2026}" } else { "load more" }}
                                        </Btn>
                                    </Pager>
                                }
                            })}
                        </div>
                    </div>
                }.into_any()
            }}

            // Retraction section
            {move || {
                let obj = loaded_object.get();
                let _obj = obj.as_ref()?;
                let desc_count = descendants.get().len();
                Some(view! {
                    <div style="padding:0 var(--pad);margin-top:16px">
                        <div class="intel-section-hd">"Retraction"</div>
                        <p style="font-size:12px;color:var(--ink-2);margin:4px 0 8px">
                            {format!(
                                "Retracting this source will cascade to {desc_count} descendant edge(s). \
                                 Mechanical copies are auto-invalidated; synthesized content is routed for review."
                            )}
                        </p>
                        {if can_write.get() {
                            view! {
                                <Btn
                                    variant=Variant::Danger
                                    on_click=Callback::new(move |()| retract_modal.set(true))
                                >
                                    "Retract source object"
                                </Btn>
                            }.into_any()
                        } else {
                            view! {
                                <Btn
                                    variant=Variant::Danger
                                    disabled=true
                                    attr:title="requires DerivationWrite permission"
                                >
                                    "Retract source object"
                                </Btn>
                            }.into_any()
                        }}
                    </div>
                })
            }}

            // Invalidation confirm modal
            {move || invalidate_target.get().map(|drv_id| {
                let bus = bus.clone();
                let id_for_confirm = drv_id.clone();
                view! {
                    <ConfirmWithReasonModal
                        title="Invalidate derivation"
                        message=format!("Invalidate derivation {drv_id}?")
                        confirm_label="Invalidate"
                        reason_placeholder="Reason for invalidation"
                        on_confirm=Callback::new(move |reason: String| {
                            let id = id_for_confirm.clone();
                            let bus = bus.clone();
                            invalidate_target.set(None);
                            spawn_local(async move {
                                match api::intel::invalidate_derivation(&id, &reason).await {
                                    Ok(()) => {
                                        bus.push(ToastKind::Success, format!("invalidated {id}"), None);
                                        refresh.update(|r| *r += 1);
                                    }
                                    Err(e) => bus.push(ToastKind::Error, e.to_string(), None),
                                }
                            });
                        })
                        on_cancel=Callback::new(move |()| invalidate_target.set(None))
                    />
                }
            })}

            // Retraction confirm modal
            {move || retract_modal.get().then(|| {
                let bus = bus.clone();
                let obj = loaded_object.get_untracked();
                let (ot, oid) = obj.unwrap_or_default();
                view! {
                    <ConfirmWithReasonModal
                        title="Retract source"
                        message=format!(
                            "Retract {ot} {oid}? This will cascade invalidation to all descendant edges."
                        )
                        confirm_label="Confirm retraction"
                        reason_placeholder="Reason for retraction"
                        on_confirm=Callback::new(move |reason: String| {
                            let ot = ot.clone();
                            let oid = oid.clone();
                            let bus = bus.clone();
                            retract_modal.set(false);
                            spawn_local(async move {
                                match api::intel::retract_source(&ot, &oid, &reason).await {
                                    Ok(body) => {
                                        let inv = body.data.invalidated_ids.len();
                                        let rev = body.data.requires_review_ids.len();
                                        bus.push(
                                            ToastKind::Success,
                                            format!("retracted: {inv} invalidated, {rev} for review"),
                                            None,
                                        );
                                        refresh.update(|r| *r += 1);
                                    }
                                    Err(e) => bus.push(ToastKind::Error, e.to_string(), None),
                                }
                            });
                        })
                        on_cancel=Callback::new(move |()| retract_modal.set(false))
                    />
                }
            })}
        </div>
    }
}

#[allow(clippy::too_many_arguments)]
fn load_lineage(
    object_type: String,
    object_id: String,
    ancestors: RwSignal<Vec<LineageNode>>,
    descendants: RwSignal<Vec<LineageNode>>,
    edges: RwSignal<Vec<DerivationView>>,
    edges_cursor: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    loaded_object: RwSignal<Option<(String, String)>>,
) {
    ancestors.set(Vec::new());
    descendants.set(Vec::new());
    edges.set(Vec::new());
    edges_cursor.set(None);
    error.set(None);
    loading.set(true);
    loaded_object.set(Some((object_type.clone(), object_id.clone())));

    let ot = object_type.clone();
    let oid = object_id.clone();
    spawn_local(async move {
        let anc_result = api::intel::get_ancestry(&ot, &oid, None).await;
        let desc_result = api::intel::get_descendants(&ot, &oid, None).await;
        let edges_result = api::intel::list_object_derivations(&ot, &oid, None).await;

        match anc_result {
            Ok(body) => ancestors.set(body.items.into_iter().map(LineageNode::from).collect()),
            Err(e) => error.set(Some(format!("ancestry: {e}"))),
        }
        match desc_result {
            Ok(body) => descendants.set(body.items.into_iter().map(LineageNode::from).collect()),
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
        match edges_result {
            Ok(page) => {
                edges_cursor.set(page.next_cursor);
                edges.set(page.items);
            }
            Err(e) => {
                error.update(|existing| {
                    let msg = format!("edges: {e}");
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
}
