use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedBody<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemBody<T> {
    #[serde(flatten)]
    pub data: T,
    pub request_id: String,
}
