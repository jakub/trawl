use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct PaginatedBody<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_applied: Option<bool>,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ItemBody<T> {
    #[serde(flatten)]
    pub data: T,
    pub request_id: String,
}
