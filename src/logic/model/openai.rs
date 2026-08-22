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

pub struct OpenAiProvider {
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest client build");
        Self { client }
    }
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize)]
pub struct OpenAiRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
    pub stream: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiMessage {
    pub role: String,
    pub content: String,
}

fn build_openai_request(req: &LlmRequest, stream: bool) -> OpenAiRequest<'_> {
    let normalized = normalize_with_system(req.system.as_deref(), &req.messages);
    let mut messages: Vec<OpenAiMessage> = Vec::new();
    if !normalized.system.is_empty() {
        messages.push(OpenAiMessage {
            role: "system".to_string(),
            content: normalized.system,
        });
    }
    for msg in &normalized.messages {
        match msg {
            ChatMessage::User { text } => messages.push(OpenAiMessage {
                role: "user".to_string(),
                content: text.clone(),
            }),
            ChatMessage::Assistant { text } => messages.push(OpenAiMessage {
                role: "assistant".to_string(),
                content: text.clone(),
            }),
            // Meta 等 System 段保序输出 role=system（不合并进主 system，保序语义）。
            ChatMessage::System { text, .. } => messages.push(OpenAiMessage {
                role: "system".to_string(),
                content: text.clone(),
            }),
        }
    }
    OpenAiRequest {
        model: &req.model,
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_tokens,
        response_format: req.response_format.clone(),
        stream,
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenAiResponse {
    pub choices: Vec<OpenAiChoice>,
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiChoice {
    pub message: OpenAiMessageOut,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiMessageOut {
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn name(&self) -> &'static str {
        "OpenAI"
    }

    async fn call(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let (url, api_key_str) = prepare_url_and_key(req, "openai")?;
        let body = build_openai_request(req, false);

        let result = self
            .client
            .post(&url)
            .bearer_auth(&api_key_str)
            .json(&body)
            .send()
            .await;

        let resp = match result {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("openai.call: timeout/connect error, retrying once: {e}");
                let body = build_openai_request(req, false);
                self.client
                    .post(&url)
                    .bearer_auth(&api_key_str)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| map_reqwest_error(e, "openai"))?
            }
            Err(e) => return Err(map_reqwest_error(e, "openai")),
        };

        if !resp.status().is_success() {
            return http_error(resp, "openai").await;
        }

        let parsed: OpenAiResponse = resp
            .json()
            .await
            .map_err(|e| crate::common::AgentError::Llm(format!("openai parse: {e}")))?;

        let content = parsed
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();

        Ok(LlmResponse {
            content,
            usage: parsed.usage.map(|u| Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            }),
        })
    }

    async fn call_stream(
        &self,
        req: &LlmRequest,
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<LlmResponse> {
        let (url, api_key_str) = prepare_url_and_key(req, "openai")?;
        let body = build_openai_request(req, true);

        let result = self
            .client
            .post(&url)
            .bearer_auth(&api_key_str)
            .json(&body)
            .header("accept", "text/event-stream")
            .send()
            .await;

        let resp = match result {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("openai.call_stream: timeout/connect error, retrying once: {e}");
                let body = build_openai_request(req, true);
                self.client
                    .post(&url)
                    .bearer_auth(&api_key_str)
                    .json(&body)
                    .header("accept", "text/event-stream")
                    .send()
                    .await
                    .map_err(|e| map_reqwest_error(e, "openai"))?
            }
            Err(e) => return Err(map_reqwest_error(e, "openai")),
        };

        if !resp.status().is_success() {
            return http_error(resp, "openai").await;
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut accumulated = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| map_reqwest_error(e, "openai"))?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = find_double_newline(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let event = String::from_utf8_lossy(&event_bytes);
                for line in event.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            on_chunk(StreamChunk::Done);
                            return Ok(LlmResponse {
                                content: accumulated,
                                usage: None,
                            });
                        }
                        let parsed: serde_json::Value = match serde_json::from_str(data) {
                            Ok(v) => v,
                            Err(e) => {
                                let err_msg = format!("openai SSE parse: {e}");
                                on_chunk(StreamChunk::Error(err_msg.clone()));
                                return Err(crate::common::AgentError::Llm(err_msg));
                            }
                        };
                        if let Some(s) = parsed
                            .pointer("/choices/0/delta/content")
                            .and_then(|v| v.as_str())
                        {
                            if !s.is_empty() {
                                accumulated.push_str(s);
                                on_chunk(StreamChunk::Delta(s.to_string()));
                            }
                        }
                    }
                }
            }
        }

        if accumulated.is_empty() && !buf.is_empty() {
            if let Ok(parsed) = serde_json::from_slice::<OpenAiResponse>(&buf) {
                accumulated = parsed
                    .choices
                    .first()
                    .and_then(|c| c.message.content.clone())
                    .unwrap_or_default();
                if !accumulated.is_empty() {
                    on_chunk(StreamChunk::Delta(accumulated.clone()));
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
    Ok((format!("{}/chat/completions", req.api_url), key))
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

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage::User {
            text: text.to_string(),
        }
    }

    #[test]
    fn memory_entries_merge_into_single_system_message() {
        let req = LlmRequest {
            model: "m".into(),
            system: Some("你是助手".into()),
            messages: vec![
                ChatMessage::System {
                    text: "[ATTENTION] focus: x".into(),
                    kind: SystemKind::Memory(crate::logic::model::message::MemoryKind::Attention),
                },
                user_msg("hi"),
            ],
            ..Default::default()
        };
        let body = serde_json::to_value(build_openai_request(&req, false)).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "system 恰为 1 条: {body}");
        assert_eq!(msgs[0]["role"], "system");
        let sys_text = msgs[0]["content"].as_str().unwrap();
        assert!(sys_text.contains("你是助手"));
        assert!(sys_text.contains("## 注意力"));
        assert!(sys_text.contains("[ATTENTION] focus: x"));
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn meta_system_serialized_in_order_as_system_role() {
        let req = LlmRequest {
            model: "m".into(),
            system: Some("你是助手".into()),
            messages: vec![
                user_msg("a"),
                ChatMessage::System {
                    text: "[Think Engine output]\nthink".into(),
                    kind: SystemKind::Meta,
                },
                user_msg("b"),
            ],
            ..Default::default()
        };
        let body = serde_json::to_value(build_openai_request(&req, false)).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["system", "user", "system", "user"], "{body}");
        assert_eq!(msgs[2]["content"], "[Think Engine output]\nthink");
    }

    #[test]
    fn openai_id_and_name() {
        let p = OpenAiProvider::new();
        assert_eq!(p.id(), "openai");
        assert_eq!(p.name(), "OpenAI");
    }

    #[test]
    fn openai_request_serializes_minimal_without_tools() {
        let req = OpenAiRequest {
            model: "gpt-4o",
            messages: vec![OpenAiMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            response_format: None,
            stream: false,
        };
        let j = serde_json::to_string(&req).unwrap();
        assert!(j.contains("gpt-4o"));
        assert!(j.contains("hi"));
        assert!(!j.contains("temperature"));
        assert!(!j.contains("max_tokens"));
    }

    #[test]
    fn openai_stream_request_serializes_stream_true() {
        let req = OpenAiRequest {
            model: "doubao-seed-2.0-pro",
            messages: vec![OpenAiMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            response_format: None,
            stream: true,
        };
        let j = serde_json::to_string(&req).unwrap();
        assert!(j.contains("\"stream\":true"), "got {j}");
    }
}
