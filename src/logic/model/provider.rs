use super::stream::StreamChunk;
use crate::common::{AgentError, Result};
use crate::data::ModelRow;
use async_trait::async_trait;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,

    #[serde(default)]
    pub system: Option<String>,

    pub messages: Vec<Message>,

    #[serde(default)]
    pub temperature: Option<f32>,

    #[serde(default)]
    pub top_p: Option<f32>,

    #[serde(default)]
    pub max_tokens: Option<u32>,

    #[serde(default)]
    pub tools: Vec<serde_json::Value>,

    #[serde(default)]
    pub stream: bool,

    #[serde(default)]
    pub api_url: String,

    #[serde(skip, default)]
    pub api_key: Option<SecretString>,

    #[serde(default)]
    pub provider_kind: String,

    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

impl LlmRequest {
    pub fn from_model_row(
        model_row: &ModelRow,
        messages: Vec<Message>,
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
            tools: vec![],
            stream: false,
            api_url: model_row.api_url.clone(),
            api_key: Some(api_key),

            provider_kind: model_row.api_type.to_lowercase(),
            config,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        }
    }
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MessageRole {
    type Err = AgentError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "system" => Ok(MessageRole::System),
            "user" => Ok(MessageRole::User),
            "assistant" => Ok(MessageRole::Assistant),
            "tool" => Ok(MessageRole::Tool),
            other => Err(AgentError::Parse(format!("unknown message role: {other}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,

    pub name: String,

    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCallFormat {
    OpenAI,

    Anthropic,

    Ark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,

    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
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

    fn tool_call_format(&self) -> ToolCallFormat {
        ToolCallFormat::OpenAI
    }

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
    async fn stub_provider_returns_not_implemented() {
        let p = StubProvider;
        let req = LlmRequest {
            model: "x".to_string(),
            messages: vec![Message {
                role: MessageRole::User,
                content: "hi".to_string(),
            }],
            ..Default::default()
        };
        let r = p.call(&req).await;
        assert!(matches!(r, Err(AgentError::NotImplemented(_))));
    }

    #[test]
    fn request_serializes_with_optional_fields() {
        let req = LlmRequest {
            model: "gpt-4o".to_string(),
            messages: vec![],
            temperature: Some(0.7),
            max_tokens: Some(1024),
            ..Default::default()
        };
        let j = serde_json::to_string(&req).unwrap();
        assert!(j.contains("gpt-4o"));
        assert!(j.contains("0.7"));
    }

    #[test]
    fn tool_call_struct_construction() {
        let tc = ToolCall {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            arguments: serde_json::json!({"location": "SF"}),
        };
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.name, "get_weather");
        assert_eq!(tc.arguments, serde_json::json!({"location": "SF"}));
    }

    #[test]
    fn tool_call_format_enum_3_variants() {
        let a = ToolCallFormat::OpenAI;
        let b = ToolCallFormat::Anthropic;
        let c = ToolCallFormat::Ark;
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn tool_call_json_round_trip() {
        let tc = ToolCall {
            id: "x".to_string(),
            name: "y".to_string(),
            arguments: serde_json::json!({"k": 1}),
        };
        let j = serde_json::to_string(&tc).unwrap();
        let back: ToolCall = serde_json::from_str(&j).unwrap();
        assert_eq!(tc, back);
    }

    #[tokio::test]
    async fn default_call_stream_returns_not_implemented() {
        let p = StubProvider;
        let req = LlmRequest {
            model: "x".to_string(),
            messages: vec![Message {
                role: MessageRole::User,
                content: "hi".to_string(),
            }],
            ..Default::default()
        };
        let mut on_chunk = |_c: StreamChunk| {};
        let r = p.call_stream(&req, &mut on_chunk).await;
        assert!(matches!(r, Err(AgentError::NotImplemented(_))));
    }
}
