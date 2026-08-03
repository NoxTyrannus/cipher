use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeNode {
    pub id: String,
    pub base_capability: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeCapabilityMeta {
    pub id: String,
    pub name: String,
    pub dag: serde_json::Value,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}
