use super::error::map_reqwest_error;
use super::message::{normalize_with_system, ChatMessage};
use super::provider::{LlmProvider, LlmRequest, LlmResponse, Usage};
use super::stream::{find_double_newline, StreamChunk};
use crate::common::Result;
use async_trait::async_trait;
use futures::StreamExt;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct AnthropicProvider {
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest client build");
        Self { client }
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize)]
pub struct AnthropicRequest<'a> {
    pub model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    pub stream: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicResponse {
    pub content: Option<Vec<AnthropicContentBlock>>,
    pub usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

fn build_anthropic_request(req: &LlmRequest, stream: bool) -> AnthropicRequest<'_> {
    let normalized = normalize_with_system(req.system.as_deref(), &req.messages);
    let messages: Vec<AnthropicMessage> = normalized
        .messages
        .iter()
        .filter_map(|msg| match msg {
            ChatMessage::User { text } => Some(AnthropicMessage {
                role: "user".to_string(),
                content: text.clone(),
            }),
            ChatMessage::Assistant { text } => Some(AnthropicMessage {
                role: "assistant".to_string(),
                content: text.clone(),
            }),
            ChatMessage::System { .. } => None,
        })
        .collect();
    AnthropicRequest {
        model: &req.model,
        system: (!normalized.system.is_empty()).then_some(normalized.system),
        messages,
        max_tokens: req.max_tokens.unwrap_or(4096),
        temperature: req.temperature,
        top_p: req.top_p,
        stream,
    }
}

fn text_from_blocks(blocks: Option<Vec<AnthropicContentBlock>>) -> String {
    blocks
        .unwrap_or_default()
        .into_iter()
        .map(|block| match block {
            AnthropicContentBlock::Text { text } => text,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn name(&self) -> &'static str {
        "Anthropic"
    }

    async fn call(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let (url, api_key) = prepare_url_and_key(req, "anthropic")?;
        let body = build_anthropic_request(req, false);
        let result = self
            .client
            .post(&url)
            .header("x-api-key", api_key.clone())
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await;
        let resp = match result {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("anthropic.call: timeout/connect error, retrying once: {e}");
                let body = build_anthropic_request(req, false);
                self.client
                    .post(&url)
                    .header("x-api-key", api_key.clone())
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| map_reqwest_error(e, "anthropic"))?
            }
            Err(e) => return Err(map_reqwest_error(e, "anthropic")),
        };
        if !resp.status().is_success() {
            return http_error(resp, "anthropic").await;
        }
        let parsed: AnthropicResponse = resp
            .json()
            .await
            .map_err(|e| crate::common::AgentError::Llm(format!("anthropic parse: {e}")))?;
        let content = text_from_blocks(parsed.content);
        let usage = parsed.usage.map(|u| Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
        });
        Ok(LlmResponse { content, usage })
    }

    async fn call_stream(
        &self,
        req: &LlmRequest,
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<LlmResponse> {
        let (url, api_key) = prepare_url_and_key(req, "anthropic")?;
        let body = build_anthropic_request(req, true);
        let result = self
            .client
            .post(&url)
            .header("x-api-key", api_key.clone())
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await;
        let resp = match result {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("anthropic.call_stream: timeout/connect error, retrying once: {e}");
                let body = build_anthropic_request(req, true);
                self.client
                    .post(&url)
                    .header("x-api-key", api_key.clone())
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| map_reqwest_error(e, "anthropic"))?
            }
            Err(e) => return Err(map_reqwest_error(e, "anthropic")),
        };
        if !resp.status().is_success() {
            return http_error(resp, "anthropic").await;
        }

        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        let mut accumulated = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| map_reqwest_error(e, "anthropic"))?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = find_double_newline(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let event = String::from_utf8_lossy(&event_bytes);
                for line in event.lines() {
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let parsed: serde_json::Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            let msg = format!("anthropic SSE parse: {e}");
                            on_chunk(StreamChunk::Error(msg.clone()));
                            return Err(crate::common::AgentError::Llm(msg));
                        }
                    };
                    if parsed.get("type").and_then(|v| v.as_str()) == Some("message_stop") {
                        on_chunk(StreamChunk::Done);
                        return Ok(LlmResponse {
                            content: accumulated,
                            usage: None,
                        });
                    }
                    if let Some(delta) = parsed.pointer("/delta/text").and_then(|v| v.as_str()) {
                        accumulated.push_str(delta);
                        on_chunk(StreamChunk::Delta(delta.to_string()));
                    }
                }
            }
        }
        on_chunk(StreamChunk::Done);
        Ok(LlmResponse {
            content: accumulated,
            usage: None,
        })
    }
}

fn prepare_url_and_key(req: &LlmRequest, name: &str) -> Result<(String, String)> {
    if req.api_url.is_empty() {
        return Err(crate::common::AgentError::Llm(format!(
            "{name}: req.api_url is empty (model.api_url required)"
        )));
    }
    let key = req
        .api_key
        .as_ref()
        .map(|s| s.expose_secret().to_string())
        .ok_or_else(|| {
            crate::common::AgentError::Llm(format!(
                "{name}: req.api_key is None (model.api_key required)"
            ))
        })?;
    Ok((format!("{}/v1/messages", req.api_url), key))
}

async fn http_error(resp: reqwest::Response, name: &str) -> Result<LlmResponse> {
    let status = resp.status();
    let body_bytes = resp.bytes().await.unwrap_or_default();
    let body = String::from_utf8_lossy(&body_bytes);
    Err(crate::common::AgentError::Llm(format!(
        "{name} HTTP {status}: {body}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::model::message::{ChatMessage, SystemKind};

    #[test]
    fn memory_entries_merge_into_system_and_messages_are_text_only() {
        let req = LlmRequest {
            model: "claude".into(),
            system: Some("base".into()),
            messages: vec![
                ChatMessage::System {
                    text: "mem".into(),
                    kind: SystemKind::Memory(crate::logic::model::message::MemoryKind::Attention),
                },
                ChatMessage::User { text: "hi".into() },
                ChatMessage::Assistant { text: "ok".into() },
            ],
            max_tokens: Some(1000),
            ..Default::default()
        };
        let body = serde_json::to_value(build_anthropic_request(&req, false)).unwrap();
        assert!(body["system"].as_str().unwrap().contains("base"));
        assert!(body["system"].as_str().unwrap().contains("mem"));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["role"], "assistant");
    }

    #[test]
    fn anthropic_id_and_name() {
        let p = AnthropicProvider::new();
        assert_eq!(p.id(), "anthropic");
        assert_eq!(p.name(), "Anthropic");
    }
}
