use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivationView {
    pub id: String,
    pub derived_object_type: String,
    pub derived_object_id: String,
    pub source_object_type: String,
    pub source_object_id: String,
    pub transformation: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub marking_before: serde_json::Value,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub marking_after: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_decision_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by_principal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalidated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalidation_reason: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AncestryView {
    pub depth: i32,
    #[serde(flatten)]
    pub derivation: DerivationView,
}

#[derive(Debug, Deserialize)]
pub struct CreateDerivationBody {
    pub derived_object_type: String,
    pub derived_object_id: String,
    pub source_object_type: String,
    pub source_object_id: String,
    pub transformation: String,
    pub marking_before: serde_json::Value,
    pub marking_after: serde_json::Value,
    pub redaction_reason: Option<String>,
    pub policy_decision_id: Option<String>,
    pub approved_by_principal_id: Option<String>,
    pub approved_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InvalidateDerivationBody {
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct AncestryQuery {
    pub max_depth: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescendantView {
    pub depth: i32,
    #[serde(flatten)]
    pub derivation: DerivationView,
}

#[derive(Debug, Deserialize)]
pub struct DescendantsQuery {
    pub max_depth: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct RetractSourceBody {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetractSourceResponse {
    pub invalidated_ids: Vec<String>,
    pub requires_review_ids: Vec<String>,
}
