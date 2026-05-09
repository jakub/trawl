// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<LinkageGraph/>` — cytoscape.js node diagram showing the linkages
//! between a story's claims, entities, sources, and related stories.

use std::collections::HashSet;

use coastwatch_api_types::story::{StoryClaimView, StoryRelationView};

use super::truncate;
use leptos::prelude::*;
use serde::Serialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::interop::cytoscape::{GraphHandle, create_graph};

// ── Cytoscape element types ───────���───────────────────────────

#[derive(Serialize)]
struct GraphElement {
    group: &'static str,
    data: GraphData,
    #[serde(skip_serializing_if = "Option::is_none")]
    classes: Option<String>,
}

#[derive(Serialize)]
struct GraphData {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_type: Option<String>,
}

fn node(id: String, label: String, node_type: &str, classes: &str) -> GraphElement {
    GraphElement {
        group: "nodes",
        data: GraphData {
            id,
            source: None,
            target: None,
            label,
            node_type: Some(node_type.to_string()),
        },
        classes: Some(classes.to_string()),
    }
}

fn edge(source: &str, target: &str, label: &str, classes: &str) -> GraphElement {
    GraphElement {
        group: "edges",
        data: GraphData {
            id: format!("{source}->{target}"),
            source: Some(source.to_string()),
            target: Some(target.to_string()),
            label: label.to_string(),
            node_type: None,
        },
        classes: Some(classes.to_string()),
    }
}

fn relationship_class(rel: &str) -> &'static str {
    match rel.to_lowercase().as_str() {
        "evidence" => "edge-evidence",
        "contradiction" => "edge-contradiction",
        "supersession" => "edge-supersession",
        "correction" => "edge-correction",
        "evolution" => "edge-evolution",
        _ => "",
    }
}

fn claim_class(rel: &str) -> String {
    let suffix = rel.to_lowercase().replace(' ', "_");
    format!("claim claim-{suffix}")
}

// ── Graph builder ─────────────────────────────────────────────

fn build_graph_elements(
    story_id: &str,
    story_title: &str,
    claims: &[StoryClaimView],
    relations: &[StoryRelationView],
) -> Vec<GraphElement> {
    let mut elements = Vec::new();
    let mut seen_entities = HashSet::new();
    let mut seen_sources = HashSet::new();

    // Central story node
    elements.push(node(
        story_id.to_string(),
        truncate(story_title, 30),
        "story",
        "story-central",
    ));

    // Claims + their entity/source edges
    for claim in claims {
        let claim_node_id = format!("claim-{}", claim.claim_id);
        let rel_lower = claim.relationship.to_lowercase();
        let claim_label = claim.claim_type.replace('_', " ");

        elements.push(node(
            claim_node_id.clone(),
            truncate(&claim_label, 20),
            "claim",
            &claim_class(&rel_lower),
        ));

        // Story → Claim edge
        let edge_class = relationship_class(&claim.relationship);
        let edge_label = rel_lower.replace('_', " ");
        elements.push(edge(story_id, &claim_node_id, &edge_label, edge_class));

        // Claim → Subject entity
        if let Some(ref ent) = claim.subject_entity {
            let ent_id = format!("ent-{}", ent.id);
            if seen_entities.insert(ent.id.clone()) {
                elements.push(node(
                    ent_id.clone(),
                    truncate(&ent.canonical_name, 24),
                    "entity",
                    "entity",
                ));
            }
            elements.push(edge(&claim_node_id, &ent_id, "subject", "edge-subject"));
        }

        // Claim → Object entity
        if let Some(ref ent) = claim.object_entity {
            let ent_id = format!("ent-{}", ent.id);
            if seen_entities.insert(ent.id.clone()) {
                elements.push(node(
                    ent_id.clone(),
                    truncate(&ent.canonical_name, 24),
                    "entity",
                    "entity",
                ));
            }
            elements.push(edge(&claim_node_id, &ent_id, "object", "edge-object"));
        }

        // Claim → Source
        if let Some(ref src) = claim.source {
            let src_id = format!("src-{}", src.id);
            if seen_sources.insert(src.id.clone()) {
                elements.push(node(
                    src_id.clone(),
                    truncate(&src.name, 20),
                    "source",
                    "source",
                ));
            }
            elements.push(edge(&claim_node_id, &src_id, "", "edge-source"));
        }
    }

    // Related stories
    for rel in relations {
        let other_id = if rel.story_a_id == story_id {
            &rel.story_b_id
        } else {
            &rel.story_a_id
        };
        let rel_node_id = format!("rel-{other_id}");
        let short_id = truncate(other_id, 12);
        elements.push(node(
            rel_node_id.clone(),
            short_id,
            "story",
            "story-related",
        ));
        let label = rel.relation.to_lowercase().replace('_', " ");
        elements.push(edge(story_id, &rel_node_id, &label, "edge-story-relation"));
    }

    elements
}

// ── Lifecycle ─────────────────────────��───────────────────────

struct GraphLifecycle {
    handle: GraphHandle,
    _on_click: Closure<dyn Fn(String, String)>,
}

impl Drop for GraphLifecycle {
    fn drop(&mut self) {
        self.handle.destroy();
    }
}

// ── Component ──��──────────────────────────────────────────────

#[component]
#[allow(clippy::needless_pass_by_value)]
pub fn LinkageGraph(
    story_id: String,
    story_title: String,
    claims: RwSignal<Vec<StoryClaimView>>,
    #[prop(into)] relations: Signal<Vec<StoryRelationView>>,
) -> impl IntoView {
    let node_ref = NodeRef::<leptos::html::Div>::new();
    let lifecycle: StoredValue<Option<GraphLifecycle>, leptos::prelude::LocalStorage> =
        StoredValue::new_local(None);

    let sid = story_id.clone();
    let stitle = story_title.clone();

    Effect::new(move |_| {
        let Some(element) = node_ref.get() else {
            return;
        };
        let c = claims.get();
        let r = relations.get();

        if c.is_empty() && r.is_empty() {
            return;
        }

        let elements = build_graph_elements(&sid, &stitle, &c, &r);
        let elements_js = serde_wasm_bindgen::to_value(&elements).unwrap_or(JsValue::UNDEFINED);
        let html_el: web_sys::HtmlElement = (*element).clone().unchecked_into();

        // Build opts
        let width = f64::from(html_el.client_width()).max(400.0);
        let opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&opts, &"width".into(), &JsValue::from_f64(width));
        let _ = js_sys::Reflect::set(&opts, &"height".into(), &JsValue::from_f64(480.0));

        lifecycle.update_value(|slot| {
            if let Some(existing) = slot.as_ref() {
                existing.handle.set_elements(elements_js);
            } else {
                let handle = create_graph(&html_el, elements_js, opts.into());

                let on_click =
                    Closure::<dyn Fn(String, String)>::new(move |id: String, node_type: String| {
                        handle_node_click(&id, &node_type);
                    });
                handle.on_node_click(on_click.as_ref().unchecked_ref());

                *slot = Some(GraphLifecycle {
                    handle,
                    _on_click: on_click,
                });
            }
        });
    });

    on_cleanup(move || {
        lifecycle.update_value(|slot| {
            drop(slot.take());
        });
    });

    view! {
        <div class="linkage-section">
            <div class="intel-section-hd">"Linkage graph"</div>
            <div class="linkage-canvas" node_ref=node_ref></div>
        </div>
    }
}

fn handle_node_click(id: &str, node_type: &str) {
    let window = web_sys::window().expect("no window");
    match node_type {
        "story" => {
            // Related story — navigate. Strip the "rel-" prefix if present.
            let story_id = id.strip_prefix("rel-").unwrap_or(id);
            let href = format!("/intel/stories/{story_id}");
            let _ = window.location().set_href(&href);
        }
        "entity" => {
            let entity_id = id.strip_prefix("ent-").unwrap_or(id);
            let href = format!("/intel/entities/{entity_id}");
            let _ = window.location().set_href(&href);
        }
        "claim" => {
            // Scroll to the claim in the claims section above.
            let claim_id = id.strip_prefix("claim-").unwrap_or(id);
            let doc = window.document().expect("no document");
            let selector = format!("[data-claim-id=\"{claim_id}\"]");
            if let Ok(Some(el)) = doc.query_selector(&selector) {
                let opts = web_sys::ScrollIntoViewOptions::new();
                opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                opts.set_block(web_sys::ScrollLogicalPosition::Center);
                el.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        }
        _ => {}
    }
}
