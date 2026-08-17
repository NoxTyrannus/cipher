use super::message::ChatMessage;
use super::stream::StreamChunk;
use crate::common::{AgentError, Result};
use crate::data::ModelRow;
use async_trait::async_trait;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    pub model: String,

    pub system: Option<String>,

    pub messages: Vec<ChatMessage>,

    pub temperature: Option<f32>,

    pub top_p: Option<f32>,

    pub max_tokens: Option<u32>,

    pub response_format: Option<serde_json::Value>,

    pub stream: bool,

    pub api_url: String,

    pub api_key: Option<SecretString>,

    pub provider_kind: String,

    pub config: Option<serde_json::Value>,
}

impl LlmRequest {
    pub fn from_model_row(
        model_row: &ModelRow,
        messages: Vec<ChatMessage>,
        api_key: SecretString,
    ) -> Result<Self> {
        let config = model_row.config.clone();

        let capability = super::capability::resolve_model_capability(model_row);
        let temperature = capability.temperature.or(Some(1.0));
        let top_p = capability.top_p;
        let max_tokens = config
            .as_ref()
            .and_then(|c| c.get("max_output"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
        Ok(Self {
            model: model_row.model_id.clone(),
            system: None,
            messages,
            temperature,
            top_p,
            max_tokens,
            response_format: if model_row.api_type.eq_ignore_ascii_case("openai") {
                Some(serde_json::json!({"type": "json_object"}))
            } else {
                config
                    .as_ref()
                    .and_then(|c| c.get("response_format").cloned())
            },
            stream: false,
            api_url: model_row.api_url.clone(),
            api_key: Some(api_key),

            provider_kind: model_row.api_type.to_lowercase(),
            config,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,

    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &'static str;

    fn name(&self) -> &'static str;

    async fn call(&self, _req: &LlmRequest) -> Result<LlmResponse> {
        Err(AgentError::NotImplemented(format!(
            "{} provider",
            self.id()
        )))
    }

    async fn call_stream(
        &self,
        _req: &LlmRequest,
        _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<LlmResponse> {
        Err(AgentError::NotImplemented(format!(
            "{} provider call_stream",
            self.id()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubProvider;
    #[async_trait]
    impl LlmProvider for StubProvider {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn name(&self) -> &'static str {
            "Stub"
        }
    }

    #[tokio::test]
    async fn provider_default_call_returns_not_implemented() {
        let provider = StubProvider;
        let req = LlmRequest::default();
        let err = provider.call(&req).await.unwrap_err();
        assert!(err.to_string().to_lowercase().contains("not implemented"));
    }
}
