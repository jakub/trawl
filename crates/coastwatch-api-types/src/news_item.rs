use serde::{Deserialize, Serialize};

use crate::marking::MarkingView;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsItemView {
    pub id: String,
    pub story_id: String,
    pub timeline_event_id: String,
    pub mode: String,
    pub canonical_title: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_it_matters: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub who_affected: Option<String>,
    pub latest_delta_type: String,
    pub latest_delta_summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    pub source_ids: Vec<String>,
    pub markings: Vec<MarkingView>,
    pub occurred_at: String,
    pub created_at: String,
}
