use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkingView {
    pub scheme: String,
    pub value: String,
}
