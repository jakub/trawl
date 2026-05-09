use serde::{Deserialize, Serialize};

use crate::entity::EntityRef;
use crate::marking::MarkingView;
use crate::redaction::RedactionNotice;
use crate::source::SourceRef;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoryView {
    pub id: String,
    pub story_class: String,
    pub state: String,
    pub canonical_title: String,
    pub canonical_summary: Option<String>,
    pub parent_story_id: Option<String>,
    pub importance_score: Option<f64>,
    pub visible_children_count: Option<u32>,
    pub depth: Option<u8>,
    pub markings: Vec<MarkingView>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct SetParentBody {
    pub parent_story_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEventView {
    pub id: String,
    pub delta_type: String,
    pub occurred_at: String,
    pub origin: String,
    pub summary: String,
    pub material: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoryClaimView {
    pub story_id: String,
    pub claim_id: String,
    pub relationship: String,
    pub material: bool,
    pub confidence: Option<f64>,
    pub attached_by: Option<String>,
    pub claim_type: String,
    pub polarity: String,
    pub modality: String,
    pub claim_confidence: f64,
    pub source_class: Option<String>,
    pub source_role: Option<String>,
    pub attribution_confidence: Option<f64>,
    pub payload: Option<serde_json::Value>,
    pub payload_schema_version: i32,
    pub subject_entity: Option<EntityRef>,
    pub object_entity: Option<EntityRef>,
    pub source: Option<SourceRef>,
    pub asserted_at: Option<String>,
    pub disclosed_at: Option<String>,
    pub event_time_range: Option<TimeRange>,
    pub observed_time_range: Option<TimeRange>,
    pub markings: Vec<MarkingView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction: Option<RedactionNotice>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoryRelationView {
    pub story_a_id: String,
    pub story_b_id: String,
    pub relation: String,
    pub confidence: Option<f64>,
    pub created_at: String,
}
