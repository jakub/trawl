use serde::{Deserialize, Serialize};

use crate::marking::MarkingView;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRef {
    pub id: String,
    pub entity_type: String,
    pub canonical_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EntityView {
    pub id: String,
    pub entity_type: String,
    pub canonical_name: String,
    pub aliases: Vec<EntityAliasView>,
    pub markings: Vec<MarkingView>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EntityAliasView {
    pub id: String,
    pub alias: String,
    pub alias_kind: String,
    pub source: String,
    pub confidence: Option<f64>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EntityRelationView {
    pub entity_a_id: String,
    pub entity_b_id: String,
    pub relation: String,
    pub confidence: Option<f64>,
    pub created_at: String,
}
