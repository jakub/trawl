use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct ModelReleaseView {
    pub id: String,
    pub model: String,
    pub prompt_version: String,
    pub eval_report_id: String,
    pub approved_by: String,
    pub approved_at: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateModelReleaseRequest {
    pub model: String,
    pub prompt_version: String,
    pub eval_report_id: String,
}

#[derive(Debug, Serialize)]
pub struct EvalReportView {
    pub id: String,
    pub model: String,
    pub prompt_version: String,
    pub run_by: String,
    pub ran_at: String,
    pub total_cases: i32,
    pub passed_cases: i32,
    pub failed_cases: i32,
    pub pass_rate: f64,
    pub case_results: serde_json::Value,
    pub created_at: String,
}
