use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalystQueueItemView {
    pub id: String,
    pub item_type: String,
    pub status: String,
    pub source_kind: String,
    pub story_id: String,
    pub claim_id: String,
    pub evidence_packet_id: String,
    pub decision_id: Option<String>,
    pub assigned_to: Option<String>,
    pub priority: i32,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdjudicatorDecisionView {
    pub id: String,
    pub decision_type: String,
    pub target_object_type: String,
    pub target_object_id: String,
    pub decision: String,
    pub confidence: Option<f64>,
    pub reason_codes: serde_json::Value,
    pub evidence_packet_id: String,
    pub created_by: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalystActionResponse {
    pub item_id: String,
    pub decision_id: String,
    pub audit_event_id: String,
    pub new_status: String,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalystActionRequest {
    pub action: String,
    pub reason: Option<String>,
    pub target_story_id: Option<String>,
}
