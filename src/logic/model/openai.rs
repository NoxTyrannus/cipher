use super::error::map_reqwest_error;
use super::provider::{
    LlmProvider, LlmRequest, LlmResponse, Message, MessageRole, ToolCall, ToolCallFormat, Usage,
};
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
}

fn parse_tool_envelope(raw: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let id = v.get("tool_use_id")?.as_str()?.to_string();
    let content = match v.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    Some((id, content))
}

fn build_openai_request(req: &LlmRequest, stream: bool) -> OpenAiRequest<'_> {
    let system_msg = req.system.as_ref().map(|s| OpenAiMessage {
        role: "system".to_string(),
        content: s.clone(),
        tool_call_id: None,
    });
    OpenAiRequest {
        model: &req.model,
        messages: system_msg
            .into_iter()
            .chain(req.messages.iter().map(|message: &Message| {
                if message.role == MessageRole::Tool {
                    if let Some((id, content)) = parse_tool_envelope(&message.content) {
                        return OpenAiMessage {
                            role: message.role.to_string(),
                            content,
                            tool_call_id: Some(id),
                        };
                    }
                }
                OpenAiMessage {
                    role: message.role.to_string(),
                    content: message.content.clone(),
                    tool_call_id: None,
                }
            }))
            .collect(),
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
                "openai: req.api_url is empty (model.api_url required, per ADR-131)".to_string(),
            ));
        }
        let base_url = req.api_url.clone();
        let api_key_str = req
            .api_key
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| {
                crate::common::AgentError::Llm(
                    "openai: req.api_key is None (model.api_key required, per ADR-131)".to_string(),
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
                "openai: req.api_url is empty (model.api_url required, per ADR-131)".to_string(),
            ));
        }
        let base_url = req.api_url.clone();
        let api_key_str = req
            .api_key
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| {
                crate::common::AgentError::Llm(
                    "openai: req.api_key is None (model.api_key required, per ADR-131)".to_string(),
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

    #[test]
    fn a2_envelope_tool_content_unwrapped_with_tool_call_id() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![Message {
                role: MessageRole::Tool,
                content: r#"{"tool_use_id":"call_1","tool_name":"cap_x","tool_input":{},"content":"plain result"}"#.into(),
            }],
            ..Default::default()
        };
        let body = serde_json::to_value(build_openai_request(&req, false)).unwrap();
        let m = &body["messages"][0];
        assert_eq!(m["role"], "tool");
        assert_eq!(m["content"], "plain result");
        assert_eq!(m["tool_call_id"], "call_1");
    }

    #[test]
    fn a2_bare_tool_content_passthrough() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![Message {
                role: MessageRole::Tool,
                content: r#"{"stdout":"ok","exit_code":0}"#.into(),
            }],
            ..Default::default()
        };
        let body = serde_json::to_value(build_openai_request(&req, false)).unwrap();
        let m = &body["messages"][0];
        assert_eq!(m["role"], "tool");
        assert!(m["content"].as_str().unwrap().contains("stdout"));
        assert!(
            m.get("tool_call_id").is_none(),
            "裸输出无 tool_call_id 字段"
        );
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
