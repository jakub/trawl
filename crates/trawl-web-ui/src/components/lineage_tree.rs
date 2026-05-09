// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use coastwatch_api_types::derivation::{AncestryView, DerivationView, DescendantView};
use leptos::prelude::*;

use crate::time_fmt::time_ago;

#[derive(Debug, Clone)]
pub struct LineageNode {
    pub depth: i32,
    pub derivation: DerivationView,
}

impl From<AncestryView> for LineageNode {
    fn from(v: AncestryView) -> Self {
        Self {
            depth: v.depth,
            derivation: v.derivation,
        }
    }
}

impl From<DescendantView> for LineageNode {
    fn from(v: DescendantView) -> Self {
        Self {
            depth: v.depth,
            derivation: v.derivation,
        }
    }
}

pub fn can_write_derivations(role: &str) -> bool {
    role == "admin"
}

fn transformation_color(t: &str) -> &'static str {
    match t {
        "redaction" => "--amber",
        "indicator_extraction" => "--blue",
        "summarization" => "--green",
        "aggregation" => "--teal",
        "translation" => "--ink-2",
        _ => "--ink-3",
    }
}

fn render_marking_diff(before: serde_json::Value, after: serde_json::Value) -> impl IntoView {
    if before.is_null() || after.is_null() || before == after {
        return view! { <span></span> }.into_any();
    }

    let before_tlp = before
        .get("tlp")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?")
        .to_uppercase();
    let after_tlp = after
        .get("tlp")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?")
        .to_uppercase();

    if before_tlp != after_tlp {
        let label = format!("TLP:{before_tlp} \u{2192} TLP:{after_tlp}");
        view! {
            <span
                class="intel-badge"
                style="background:var(--amber-wash);color:var(--amber);font-size:9px"
            >
                {label}
            </span>
        }
        .into_any()
    } else {
        view! {
            <span
                class="intel-badge"
                style="background:var(--panel-2);color:var(--ink-3);font-size:9px"
            >
                "marking changed"
            </span>
        }
        .into_any()
    }
}

#[component]
pub fn LineageTree(
    label: &'static str,
    nodes: Vec<LineageNode>,
    can_write: bool,
    #[prop(optional)] on_invalidate: Option<Callback<String>>,
    now_ms: i64,
) -> impl IntoView {
    if nodes.is_empty() {
        return view! {
            <div class="lineage-section">
                <div class="intel-section-hd">{label}</div>
                <p class="mono" style="font-size:11px;color:var(--ink-3);margin:4px 0">
                    "none"
                </p>
            </div>
        }
        .into_any();
    }

    let count = nodes.len();
    let heading = format!("{label} ({count})");

    view! {
        <div class="lineage-section">
            <div class="intel-section-hd">{heading}</div>
            <div class="lineage-tree">
                {nodes.into_iter().map(|node| {
                    let d = node.derivation;
                    let is_invalidated = d.invalidated_at.is_some();
                    let t_color = transformation_color(&d.transformation);
                    let t_label = d.transformation.replace('_', " ");

                    let source_ref = format!("{} {}", d.source_object_type, d.source_object_id);
                    let derived_ref = format!("{} {}", d.derived_object_type, d.derived_object_id);
                    let created = time_ago(&d.created_at, now_ms);
                    let depth = node.depth;
                    let indent = format!("padding-left:{}px", depth * 12);

                    let marking_diff = render_marking_diff(d.marking_before, d.marking_after);

                    let invalidation_title = d.invalidation_reason
                        .unwrap_or_default();

                    let row_class = if is_invalidated {
                        "lineage-node lineage-invalidated"
                    } else {
                        "lineage-node"
                    };

                    let drv_id = d.id;
                    let show_invalidate = can_write
                        && !is_invalidated
                        && on_invalidate.is_some();
                    let on_inv = on_invalidate;

                    view! {
                        <div class=row_class style=indent title=invalidation_title>
                            <span class="lineage-depth">{depth}</span>
                            <span
                                class="intel-badge"
                                style=format!(
                                    "background:var({t_color}-wash,var(--panel-2));color:var({t_color})"
                                )
                            >
                                {t_label}
                            </span>
                            <span class="lineage-ref">
                                {source_ref}" \u{2192} "{derived_ref}
                            </span>
                            {marking_diff}
                            <span class="lineage-time">{created}</span>
                            {show_invalidate.then(|| {
                                let id = drv_id.clone();
                                view! {
                                    <button
                                        class="btn-xs btn-sec"
                                        on:click=move |e| {
                                            e.stop_propagation();
                                            if let Some(cb) = on_inv {
                                                cb.run(id.clone());
                                            }
                                        }
                                    >
                                        "invalidate"
                                    </button>
                                }
                            })}
                            {(!can_write && !is_invalidated && on_invalidate.is_some()).then(|| {
                                view! {
                                    <button
                                        class="btn-xs btn-sec"
                                        disabled=true
                                        title="requires DerivationWrite permission"
                                    >
                                        "invalidate"
                                    </button>
                                }
                            })}
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
    .into_any()
}
