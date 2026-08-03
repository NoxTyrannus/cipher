use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageMethodMeta {
    pub id: String,
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub examples: Option<serde_json::Value>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}
