use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceView {
    pub id: String,
    pub name: String,
    pub source_class: String,
    pub state: String,
    pub home_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceDetailView {
    pub id: String,
    pub name: String,
    pub source_class: String,
    pub state: String,
    pub home_url: Option<String>,
    pub health: SourceHealthView,
    pub feeds: Vec<FeedView>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceHealthView {
    pub last_fetch_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
    pub consecutive_errors: u32,
    pub total_fetches: u64,
    pub total_errors: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FeedView {
    pub id: String,
    pub source_id: String,
    pub url: String,
    pub feed_kind: String,
    pub state: String,
    pub poll_interval_seconds: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateSourceRequest {
    pub name: String,
    pub source_class: String,
    pub home_url: Option<String>,
    pub license_policy: Option<serde_json::Value>,
    pub default_markings: Option<serde_json::Value>,
    #[serde(default)]
    pub feeds: Vec<CreateFeedRequest>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateFeedRequest {
    pub url: String,
    pub feed_kind: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: i32,
}

fn default_poll_interval() -> i32 {
    300
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateSourceRequest {
    pub name: Option<String>,
    pub home_url: Option<String>,
    pub license_policy: Option<serde_json::Value>,
    pub default_markings: Option<serde_json::Value>,
}
