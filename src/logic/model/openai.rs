use super::error::map_reqwest_error;
use super::message::{normalize_with_system, ChatMessage};
use super::provider::{LlmProvider, LlmRequest, LlmResponse, ToolCall, ToolCallFormat, Usage};
use super::stream::{find_double_newline, StreamChunk};
use crate::common::Result;
use async_trait::async_trait;
use futures::StreamExt;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<serde_json::Value>,
    pub stream: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiMessage {
    pub role: String,
    pub content: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub tool_call_id: Option<String>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub tool_calls: Vec<OpenAiToolCallOut>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiToolCallOut {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiFunctionOut,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiFunctionOut {
    pub name: String,
    pub arguments: String,
}

fn build_openai_request(req: &LlmRequest, stream: bool) -> OpenAiRequest<'_> {
    let normalized = normalize_with_system(req.system.as_deref(), &req.messages);
    let mut messages: Vec<OpenAiMessage> = Vec::new();
    if !normalized.system.is_empty() {
        messages.push(OpenAiMessage {
            role: "system".to_string(),
            content: normalized.system,
            tool_call_id: None,
            tool_calls: vec![],
        });
    }
    for msg in &normalized.messages {
        match msg {
            ChatMessage::User { text } => messages.push(OpenAiMessage {
                role: "user".to_string(),
                content: text.clone(),
                tool_call_id: None,
                tool_calls: vec![],
            }),
            ChatMessage::Assistant { text, tool_calls } => {
                let tool_calls: Vec<OpenAiToolCallOut> = tool_calls
                    .iter()
                    .map(|tc| OpenAiToolCallOut {
                        id: tc.id.clone(),
                        kind: "function".to_string(),
                        function: OpenAiFunctionOut {
                            name: tc.name.clone(),
                            arguments: serde_json::to_string(&tc.arguments).unwrap_or_default(),
                        },
                    })
                    .collect();
                messages.push(OpenAiMessage {
                    role: "assistant".to_string(),
                    content: text.clone(),
                    tool_call_id: None,
                    tool_calls,
                });
            }
            ChatMessage::ToolResult { id, text, .. } => messages.push(OpenAiMessage {
                role: "tool".to_string(),
                content: text.clone(),
                tool_call_id: Some(id.clone()),
                tool_calls: vec![],
            }),
            ChatMessage::System { .. } => unreachable!("normalize 已抽取全部 System"),
        }
    }
    OpenAiRequest {
        model: &req.model,
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_tokens,
        tools: req.tools.clone(),
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
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiToolCallRaw>>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiToolCallRaw {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiFunctionCall,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiFunctionCall {
    pub name: String,

    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

struct OpenAiToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn name(&self) -> &'static str {
        "OpenAI"
    }

    fn tool_call_format(&self) -> ToolCallFormat {
        ToolCallFormat::OpenAI
    }

    async fn call(&self, req: &LlmRequest) -> Result<LlmResponse> {
        if req.api_url.is_empty() {
            return Err(crate::common::AgentError::Llm(
                "openai: req.api_url is empty (model.api_url required)".to_string(),
            ));
        }
        let base_url = req.api_url.clone();
        let api_key_str = req
            .api_key
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| {
                crate::common::AgentError::Llm(
                    "openai: req.api_key is None (model.api_key required)".to_string(),
                )
            })?;
        let url = format!("{}/chat/completions", base_url);
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
            let status = resp.status();
            let body_bytes = resp.bytes().await.unwrap_or_default();
            let body = String::from_utf8_lossy(&body_bytes);
            return Err(crate::common::AgentError::Llm(format!(
                "openai HTTP {status}: {body}"
            )));
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

        let tool_calls: Vec<ToolCall> = parsed
            .choices
            .first()
            .and_then(|c| c.message.tool_calls.as_ref())
            .map(|tcs| {
                tcs.iter()
                    .filter_map(|raw| {
                        match serde_json::from_str(&raw.function.arguments) {
                            Ok(args) => Some(ToolCall {
                                id: raw.id.clone(),
                                name: raw.function.name.clone(),
                                arguments: args,
                            }),
                            Err(e) => {
                                tracing::warn!(
                                    "openai: skipping tool_call {} (id={}): invalid arguments JSON: {e}",
                                    raw.function.name, raw.id
                                );
                                None
                            }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(LlmResponse {
            content,
            tool_calls,
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
        if req.api_url.is_empty() {
            return Err(crate::common::AgentError::Llm(
                "openai: req.api_url is empty (model.api_url required)".to_string(),
            ));
        }
        let base_url = req.api_url.clone();
        let api_key_str = req
            .api_key
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| {
                crate::common::AgentError::Llm(
                    "openai: req.api_key is None (model.api_key required)".to_string(),
                )
            })?;
        let url = format!("{}/chat/completions", base_url);
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
            let status = resp.status();
            let body_bytes = resp.bytes().await.unwrap_or_default();
            let body = String::from_utf8_lossy(&body_bytes);
            return Err(crate::common::AgentError::Llm(format!(
                "openai HTTP {status}: {body}"
            )));
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut accumulated = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_call_builders: HashMap<usize, OpenAiToolCallBuilder> = HashMap::new();
        let mut _first_chunk_received = false;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| map_reqwest_error(e, "openai"))?;
            _first_chunk_received = true;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = find_double_newline(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let event = String::from_utf8_lossy(&event_bytes);
                for line in event.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            return Ok(LlmResponse {
                                content: accumulated,
                                tool_calls,
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
                        if let Some(tcs) = parsed
                            .pointer("/choices/0/delta/tool_calls")
                            .and_then(|v| v.as_array())
                        {
                            for tc in tcs {
                                let index =
                                    tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                                let builder =
                                    tool_call_builders.entry(index).or_insert_with(|| {
                                        OpenAiToolCallBuilder {
                                            id: String::new(),
                                            name: String::new(),
                                            arguments: String::new(),
                                        }
                                    });
                                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                    if builder.id.is_empty() {
                                        builder.id = id.to_string();
                                    }
                                }
                                if let Some(name) =
                                    tc.pointer("/function/name").and_then(|v| v.as_str())
                                {
                                    if builder.name.is_empty() {
                                        builder.name = name.to_string();
                                    }
                                }
                                if let Some(args) =
                                    tc.pointer("/function/arguments").and_then(|v| v.as_str())
                                {
                                    builder.arguments.push_str(args);
                                }
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
            }
        }

        if tool_calls.is_empty() && !tool_call_builders.is_empty() {
            for (_, builder) in tool_call_builders.drain() {
                match serde_json::from_str(&builder.arguments) {
                    Ok(args) => {
                        tool_calls.push(ToolCall {
                            id: builder.id,
                            name: builder.name,
                            arguments: args,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            "openai: failed to parse tool_call arguments for {}: {e}",
                            builder.name
                        );
                    }
                }
            }
        }

        Ok(LlmResponse {
            content: accumulated,
            tool_calls,
            usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::model::message::{ChatMessage, SystemKind};

    fn tool_result_msg(id: &str, text: &str) -> ChatMessage {
        ChatMessage::ToolResult {
            id: id.to_string(),
            name: "cap_x".to_string(),
            text: text.to_string(),
            is_error: false,
        }
    }

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage::User {
            text: text.to_string(),
        }
    }

    #[test]
    fn tool_result_emits_role_tool_with_tool_call_id() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::Assistant {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "cap_x".into(),
                        arguments: serde_json::json!({}),
                    }],
                },
                tool_result_msg("call_1", "plain result"),
            ],
            ..Default::default()
        };
        let body = serde_json::to_value(build_openai_request(&req, false)).unwrap();
        let m = &body["messages"][1];
        assert_eq!(m["role"], "tool");
        assert_eq!(m["content"], "plain result");
        assert_eq!(m["tool_call_id"], "call_1");
    }

    #[test]
    fn assistant_tool_calls_emit_openai_shape() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::Assistant {
                    text: "我来读文件".into(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "cap_file_read".into(),
                        arguments: serde_json::json!({"path": "a.txt"}),
                    }],
                },
                tool_result_msg("call_1", "file body"),
            ],
            ..Default::default()
        };
        let body = serde_json::to_value(build_openai_request(&req, false)).unwrap();
        let a = &body["messages"][0];
        assert_eq!(a["role"], "assistant");
        assert_eq!(a["content"], "我来读文件");
        assert_eq!(a["tool_calls"][0]["id"], "call_1");
        assert_eq!(a["tool_calls"][0]["type"], "function");
        assert_eq!(a["tool_calls"][0]["function"]["name"], "cap_file_read");
        assert_eq!(
            a["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a.txt"}"#
        );
        let t = &body["messages"][1];
        assert_eq!(t["role"], "tool");
        assert_eq!(t["tool_call_id"], "call_1");
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
    fn openai_id_and_name() {
        let p = OpenAiProvider::new();
        assert_eq!(p.id(), "openai");
        assert_eq!(p.name(), "OpenAI");
    }

    #[test]
    fn openai_request_serializes_minimal() {
        let req = OpenAiRequest {
            model: "gpt-4o",
            messages: vec![OpenAiMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
                tool_call_id: None,
                tool_calls: vec![],
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            tools: vec![],
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
                tool_call_id: None,
                tool_calls: vec![],
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            tools: vec![],
            stream: true,
        };
        let j = serde_json::to_string(&req).unwrap();
        assert!(
            j.contains("\"stream\":true"),
            "streaming request body must contain stream:true, got {j}"
        );
    }

    #[test]
    fn openai_request_carries_provider_tools() {
        let request = LlmRequest {
            model: "gpt-4o".to_string(),
            tools: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "cap_echo_1234",
                    "description": "Echo",
                    "parameters": {"type": "object"}
                }
            })],
            ..Default::default()
        };

        let body = serde_json::to_value(build_openai_request(&request, false)).unwrap();
        assert_eq!(body["tools"], serde_json::Value::Array(request.tools));
        assert_eq!(body["tools"][0]["function"]["name"], "cap_echo_1234");
    }
}
