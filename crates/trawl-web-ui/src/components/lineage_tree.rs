// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use coastwatch_api_types::derivation::{AncestryView, DerivationView, DescendantView};
use leptos::prelude::*;

use fleet_ui::{Badge, Btn, Size, Tone, Variant};

use super::{tone_for_var, truncate};
use fleet_ui::time::time_ago;

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

pub(crate) fn transformation_color(t: &str) -> &'static str {
    match t {
        "redaction" => "--amber",
        "indicator_extraction" => "--blue",
        "summarization" => "--green",
        "aggregation" => "--teal",
        "translation" => "--ink-2",
        _ => "--ink-3",
    }
}

fn extract_internal_tags(v: &serde_json::Value) -> Vec<String> {
    v.get("internal")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn is_known_marking_shape(v: &serde_json::Value) -> bool {
    v.as_object()
        .is_some_and(|obj| obj.keys().all(|k| k == "tlp" || k == "internal"))
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn render_marking_diff(
    before: serde_json::Value,
    after: serde_json::Value,
) -> impl IntoView {
    if before.is_null() || after.is_null() || before == after {
        return view! { <span></span> }.into_any();
    }

    if !is_known_marking_shape(&before) || !is_known_marking_shape(&after) {
        let before_json = serde_json::to_string(&before).unwrap_or_default();
        let after_json = serde_json::to_string(&after).unwrap_or_default();
        let full = format!("{before_json} \u{2192} {after_json}");
        let label = format!(
            "{} \u{2192} {}",
            truncate(&before_json, 50),
            truncate(&after_json, 50)
        );
        return view! {
            <Badge attr:title=full>{label}</Badge>
        }
        .into_any();
    }

    let before_tlp_raw = before
        .get("tlp")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let after_tlp_raw = after
        .get("tlp")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");

    let tlp_changed = !before_tlp_raw.eq_ignore_ascii_case(after_tlp_raw);
    let before_tags = extract_internal_tags(&before);
    let after_tags = extract_internal_tags(&after);
    let tags_changed = before_tags != after_tags;

    if !tlp_changed && !tags_changed {
        return view! {
            <Badge>"marking changed"</Badge>
        }
        .into_any();
    }

    let tlp_badge = tlp_changed.then(|| {
        let label = format!(
            "TLP:{} \u{2192} TLP:{}",
            before_tlp_raw.to_uppercase(),
            after_tlp_raw.to_uppercase()
        );
        view! {
            <Badge tone=Tone::Warn>{label}</Badge>
        }
    });

    let tags_badge = tags_changed.then(|| {
        let fmt = |tags: &[String]| {
            if tags.is_empty() {
                "[]".to_string()
            } else {
                format!("[{}]", tags.join(", "))
            }
        };
        let label = format!("{} \u{2192} {}", fmt(&before_tags), fmt(&after_tags));
        view! {
            <Badge tone=Tone::Info>{label}</Badge>
        }
    });

    view! {
        <span style="display:contents">
            {tlp_badge}
            {tags_badge}
        </span>
    }
    .into_any()
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn render_derivation_meta(
    redaction_reason: Option<String>,
    approved_by: Option<String>,
    approved_at: Option<String>,
    now_ms: i64,
) -> impl IntoView {
    if redaction_reason.is_none() && approved_by.is_none() {
        return view! { <span></span> }.into_any();
    }

    let reason_el = redaction_reason.map(|reason| {
        let display = format!("reason: {}", truncate(&reason, 30));
        view! {
            <span
                style="font-size:9px;color:var(--ink-3);font-style:italic"
                title=reason
            >
                {display}
            </span>
        }
    });

    let approval_el = approved_by.map(|principal| {
        let name = principal
            .split_once(':')
            .map_or(principal.as_str(), |(_, name)| name);
        let label = match approved_at {
            Some(ref ts) => format!("{name} {}", time_ago(ts, now_ms)),
            None => name.to_string(),
        };
        view! {
            <span style="font-size:9px;color:var(--ink-3)">
                {label}
            </span>
        }
    });

    let separator = reason_el.is_some() && approval_el.is_some();

    view! {
        <span style="display:contents">
            {reason_el}
            {separator.then(|| view! { <span style="font-size:9px;color:var(--ink-4)">" · "</span> })}
            {approval_el}
        </span>
    }
    .into_any()
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
                    let deriv_meta = render_derivation_meta(
                        d.redaction_reason,
                        d.approved_by_principal_id,
                        d.approved_at,
                        now_ms,
                    );

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
                            <Badge tone=tone_for_var(t_color)>{t_label}</Badge>
                            <span class="lineage-ref">
                                {source_ref}" \u{2192} "{derived_ref}
                            </span>
                            {marking_diff}
                            {deriv_meta}
                            <span class="lineage-time">{created}</span>
                            {show_invalidate.then(|| {
                                let id = drv_id.clone();
                                view! {
                                    <Btn
                                        variant=Variant::Secondary
                                        size=Size::Xs
                                        stop_propagation=true
                                        on_click=Callback::new(move |()| {
                                            if let Some(cb) = on_inv {
                                                cb.run(id.clone());
                                            }
                                        })
                                    >
                                        "invalidate"
                                    </Btn>
                                }
                            })}
                            {(!can_write && !is_invalidated && on_invalidate.is_some()).then(|| {
                                view! {
                                    <Btn
                                        variant=Variant::Secondary
                                        size=Size::Xs
                                        disabled=true
                                        attr:title="requires DerivationWrite permission"
                                    >
                                        "invalidate"
                                    </Btn>
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
