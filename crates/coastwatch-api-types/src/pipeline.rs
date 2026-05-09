use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineSummaryView {
    pub stages: Vec<PipelineStageSummaryView>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineStageSummaryView {
    pub stage: String,
    pub health: String,
    pub ready: i64,
    pub claimed: i64,
    pub completed: i64,
    pub failed: i64,
    pub dead_letter: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineWorkView {
    pub id: String,
    pub stage: String,
    pub status: String,
    pub priority: i32,
    pub ingest_mode: String,
    pub retry_count: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
    pub dedupe_key: Option<String>,
    pub idempotency_key: Option<String>,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<String>,
    pub lease_until: Option<String>,
    pub heartbeat_at: Option<String>,
    pub visible_at: String,
    pub completed_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineRetryResponse {
    pub work_id: String,
    pub new_status: String,
    pub request_id: String,
}
