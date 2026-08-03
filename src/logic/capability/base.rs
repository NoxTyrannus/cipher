use crate::common::AgentError;
use serde::{Deserialize, Serialize};

pub type Schema = serde_json::Value;

pub trait BaseCapability: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn execute(&self, input: &Schema) -> Result<Schema, AgentError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseCapabilityMeta {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub cap_type: String,
    pub schema_in: Schema,
    pub schema_out: Schema,
    pub executor: String,
    #[serde(default)]
    pub metadata: Option<Schema>,
}
