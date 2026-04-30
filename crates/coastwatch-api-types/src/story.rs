use serde::{Deserialize, Serialize};

use crate::marking::MarkingView;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoryView {
    pub id: String,
    pub story_class: String,
    pub state: String,
    pub canonical_title: String,
    pub canonical_summary: Option<String>,
    pub parent_story_id: Option<String>,
    pub importance_score: Option<f64>,
    pub markings: Vec<MarkingView>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEventView {
    pub id: String,
    pub delta_type: String,
    pub occurred_at: String,
    pub origin: String,
    pub summary: String,
    pub material: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub source_class: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoryRelationView {
    pub story_a_id: String,
    pub story_b_id: String,
    pub relation: String,
    pub confidence: Option<f64>,
    pub created_at: String,
}
