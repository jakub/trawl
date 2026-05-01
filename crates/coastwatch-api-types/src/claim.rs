use serde::{Deserialize, Serialize};

use crate::marking::MarkingView;

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaimView {
    pub id: String,
    pub claim_type: String,
    pub subject_entity_id: Option<String>,
    pub object_entity_id: Option<String>,
    pub polarity: String,
    pub modality: String,
    pub claim_confidence: f64,
    pub source_class: String,
    pub asserted_at: Option<String>,
    pub markings: Vec<MarkingView>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaimRelationView {
    pub claim_a_id: String,
    pub claim_b_id: String,
    pub relation: String,
    pub confidence: Option<f64>,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaimEvidenceView {
    pub fragment_id: String,
    pub post_id: String,
    pub fragment_index: i32,
    pub span_start: i32,
    pub span_end: i32,
    pub factual_summary: Option<String>,
    pub claim_summary: Option<String>,
    pub created_at: String,
}
