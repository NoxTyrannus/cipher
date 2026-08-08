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
struct AnthropicRequest<'a> {
    model: &'a str,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicSystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,

    content: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    kind: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<serde_json::Value>,
}

fn text_block(text: &str) -> serde_json::Value {
    serde_json::json!({"type": "text", "text": text})
}

fn tool_use_block(id: &str, name: &str, input: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input,
    })
}

fn tool_result_block(id: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": content,
    })
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

fn build_anthropic_tools(tools: &[serde_json::Value]) -> Result<Vec<AnthropicTool>> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            if tool.get("type").and_then(serde_json::Value::as_str) != Some("function") {
                return Err(crate::common::AgentError::Parse(format!(
                    "anthropic tool definition {index} is not a function"
                )));
            }
            let function = tool
                .get("function")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    crate::common::AgentError::Parse(format!(
                        "anthropic tool definition {index} has no function object"
                    ))
                })?;
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    crate::common::AgentError::Parse(format!(
                        "anthropic tool definition {index} has no name"
                    ))
                })?;
            let description = function
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let input_schema = function.get("parameters").cloned().ok_or_else(|| {
                crate::common::AgentError::Parse(format!(
                    "anthropic tool definition {index} has no parameters schema"
                ))
            })?;

            Ok(AnthropicTool {
                name: name.to_string(),
                description: description.to_string(),
                input_schema,
            })
        })
        .collect()
}

fn build_anthropic_request(req: &LlmRequest, stream: bool) -> Result<AnthropicRequest<'_>> {
    let normalized = normalize_with_system(req.system.as_deref(), &req.messages);
    let system = if normalized.system.is_empty() {
        None
    } else {
        Some(vec![AnthropicSystemBlock {
            kind: "text".to_string(),
            text: normalized.system,
            cache_control: Some(serde_json::json!({"type": "ephemeral"})),
        }])
    };
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    let msgs = &normalized.messages;
    let mut i = 0;
    while i < msgs.len() {
        match &msgs[i] {
            ChatMessage::System { .. } => unreachable!("normalize 已抽取全部 System"),
            ChatMessage::User { text } => {
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::Value::String(text.clone()),
                });
                i += 1;
            }
            ChatMessage::Assistant { text, tool_calls } => {
                let mut j = i + 1;
                let mut tool_results: Vec<serde_json::Value> = Vec::new();
                while j < msgs.len() {
                    if let ChatMessage::ToolResult { id, text, .. } = &msgs[j] {
                        tool_results.push(tool_result_block(id, text));
                        j += 1;
                    } else {
                        break;
                    }
                }
                let content = if tool_calls.is_empty() {
                    serde_json::Value::String(text.clone())
                } else {
                    let mut blocks = Vec::new();
                    if !text.is_empty() {
                        blocks.push(text_block(text));
                    }
                    for tc in tool_calls {
                        blocks.push(tool_use_block(&tc.id, &tc.name, &tc.arguments));
                    }
                    serde_json::Value::Array(blocks)
                };
                messages.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content,
                });
                if !tool_results.is_empty() {
                    messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: serde_json::Value::Array(tool_results),
                    });
                }
                i = j;
            }
            ChatMessage::ToolResult { .. } => {
                let mut blocks = Vec::new();
                let mut j = i;
                while j < msgs.len() {
                    if let ChatMessage::ToolResult { id, text, .. } = &msgs[j] {
                        blocks.push(tool_result_block(id, text));
                        j += 1;
                    } else {
                        break;
                    }
                }
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::Value::Array(blocks),
                });
                i = j;
            }
        }
    }

    Ok(AnthropicRequest {
        model: &req.model,
        messages,
        system,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_tokens.unwrap_or(1024),
        tools: build_anthropic_tools(&req.tools)?,
        stream,
    })
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,

    #[serde(default)]
    input: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

struct ToolUseBuilder {
    id: String,
    name: String,
    partial_json: String,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn name(&self) -> &'static str {
        "Anthropic"
    }

    fn tool_call_format(&self) -> ToolCallFormat {
        ToolCallFormat::Anthropic
    }

    async fn call(&self, req: &LlmRequest) -> Result<LlmResponse> {
        if req.api_url.is_empty() {
            return Err(crate::common::AgentError::Llm(
                "anthropic: req.api_url is empty (model.api_url required, per ADR-131)".to_string(),
            ));
        }
        let base_url = req.api_url.clone();
        let api_key_str = req
            .api_key
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| {
                crate::common::AgentError::Llm(
                    "anthropic: req.api_key is None (model.api_key required, per ADR-131)"
                        .to_string(),
                )
            })?;
        let url = format!("{}/v1/messages", base_url);
        let body = build_anthropic_request(req, false)?;

        let result = self
            .client
            .post(&url)
            .header("x-api-key", &api_key_str)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await;

        let resp = match result {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("anthropic.call: timeout/connect error, retrying once: {e}");
                let body = build_anthropic_request(req, false)?;
                self.client
                    .post(&url)
                    .header("x-api-key", &api_key_str)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| map_reqwest_error(e, "anthropic"))?
            }
            Err(e) => return Err(map_reqwest_error(e, "anthropic")),
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body_bytes = resp.bytes().await.unwrap_or_default();
            let body = String::from_utf8_lossy(&body_bytes);
            return Err(crate::common::AgentError::Llm(format!(
                "anthropic HTTP {status}: {body}"
            )));
        }

        let parsed: AnthropicResponse = resp
            .json()
            .await
            .map_err(|e| crate::common::AgentError::Llm(format!("anthropic parse: {e}")))?;

        let mut content_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for block in &parsed.content {
            if block.kind == "text" {
                if let Some(t) = &block.text {
                    content_parts.push(t.clone());
                }
            } else if block.kind == "tool_use" {
                if let (Some(id), Some(name)) = (&block.id, &block.name) {
                    tool_calls.push(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: block.input.clone().unwrap_or(serde_json::Value::Null),
                    });
                }
            }
        }
        let content = content_parts.join("");

        Ok(LlmResponse {
            content,
            tool_calls,
            usage: parsed.usage.map(|u| Usage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                total_tokens: u.input_tokens + u.output_tokens,
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
                "anthropic: req.api_url is empty (model.api_url required, per ADR-131)".to_string(),
            ));
        }
        let base_url = req.api_url.clone();
        let api_key_str = req
            .api_key
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| {
                crate::common::AgentError::Llm(
                    "anthropic: req.api_key is None (model.api_key required, per ADR-131)"
                        .to_string(),
                )
            })?;
        let url = format!("{}/v1/messages", base_url);
        let body = build_anthropic_request(req, true)?;

        let result = self
            .client
            .post(&url)
            .header("x-api-key", &api_key_str)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .header("accept", "text/event-stream")
            .send()
            .await;

        let resp = match result {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("anthropic.call_stream: timeout/connect error, retrying once: {e}");
                let body = build_anthropic_request(req, true)?;
                self.client
                    .post(&url)
                    .header("x-api-key", &api_key_str)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .header("accept", "text/event-stream")
                    .send()
                    .await
                    .map_err(|e| map_reqwest_error(e, "anthropic"))?
            }
            Err(e) => return Err(map_reqwest_error(e, "anthropic")),
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body_bytes = resp.bytes().await.unwrap_or_default();
            let body = String::from_utf8_lossy(&body_bytes);
            return Err(crate::common::AgentError::Llm(format!(
                "anthropic HTTP {status}: {body}"
            )));
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut accumulated = String::new();
        let mut completion_tokens: u32 = 0;
        let mut prompt_tokens: u32 = 0;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_use_builders: HashMap<usize, ToolUseBuilder> = HashMap::new();
        let mut _first_chunk_received = false;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| map_reqwest_error(e, "anthropic"))?;
            _first_chunk_received = true;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = find_double_newline(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let event_str = String::from_utf8_lossy(&event_bytes);
                let mut event_type = String::new();
                let mut data = String::new();
                for line in event_str.lines() {
                    if let Some(t) = line.strip_prefix("event: ") {
                        event_type = t.to_string();
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data = d.to_string();
                    }
                }
                if event_type == "message_start"
                    || event_type == "content_block_delta"
                    || event_type == "message_delta"
                    || event_type == "content_block_start"
                    || event_type == "content_block_stop"
                {
                    let parsed: serde_json::Value = match serde_json::from_str(&data) {
                        Ok(v) => v,
                        Err(e) => {
                            let err_msg = format!("anthropic SSE parse: {e}");
                            on_chunk(StreamChunk::Error(err_msg.clone()));
                            return Err(crate::common::AgentError::Llm(err_msg));
                        }
                    };
                    if event_type == "message_start" {
                        if let Some(n) = parsed
                            .pointer("/message/usage/input_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            prompt_tokens = n as u32;
                        }
                    } else if event_type == "content_block_start" {
                        if let Some(cb) = parsed.get("content_block") {
                            if cb.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                                let index =
                                    parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0)
                                        as usize;
                                let id = cb
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = cb
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                tool_use_builders.insert(
                                    index,
                                    ToolUseBuilder {
                                        id,
                                        name,
                                        partial_json: String::new(),
                                    },
                                );
                            }
                        }
                    } else if event_type == "content_block_delta" {
                        if let Some(s) = parsed.pointer("/delta/text").and_then(|v| v.as_str()) {
                            if !s.is_empty() {
                                accumulated.push_str(s);
                                on_chunk(StreamChunk::Delta(s.to_string()));
                            }
                        }
                        if let Some(partial) = parsed
                            .pointer("/delta/partial_json")
                            .and_then(|v| v.as_str())
                        {
                            let index =
                                parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            if let Some(builder) = tool_use_builders.get_mut(&index) {
                                builder.partial_json.push_str(partial);
                            }
                        }
                    } else if event_type == "content_block_stop" {
                        let index =
                            parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        if let Some(builder) = tool_use_builders.remove(&index) {
                            match serde_json::from_str(&builder.partial_json) {
                                Ok(args) => {
                                    tool_calls.push(ToolCall {
                                        id: builder.id,
                                        name: builder.name,
                                        arguments: args,
                                    });
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "anthropic: failed to parse tool_use JSON for {}: {e}",
                                        builder.name
                                    );
                                }
                            }
                        }
                    } else if event_type == "message_delta" {
                        if let Some(n) = parsed
                            .pointer("/usage/output_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            completion_tokens = n as u32;
                        }
                    }
                } else if event_type == "message_stop" {
                    return Ok(LlmResponse {
                        content: accumulated,
                        tool_calls,
                        usage: Some(Usage {
                            prompt_tokens,
                            completion_tokens,
                            total_tokens: prompt_tokens + completion_tokens,
                        }),
                    });
                }
            }
        }
        Ok(LlmResponse {
            content: accumulated,
            tool_calls,
            usage: Some(Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::model::message::{ChatMessage, SystemKind};

    #[test]
    fn anthropic_id_and_name() {
        let p = AnthropicProvider::new();
        assert_eq!(p.id(), "anthropic");
        assert_eq!(p.name(), "Anthropic");
    }

    fn tool_result_msg(id: &str, content: &str) -> ChatMessage {
        ChatMessage::ToolResult {
            id: id.to_string(),
            name: "cap_x".to_string(),
            text: content.to_string(),
            is_error: false,
        }
    }

    fn assistant_with_calls(text: &str, ids: &[&str]) -> ChatMessage {
        ChatMessage::Assistant {
            text: text.to_string(),
            tool_calls: ids
                .iter()
                .map(|id| ToolCall {
                    id: id.to_string(),
                    name: format!("cap_{id}"),
                    arguments: serde_json::json!({}),
                })
                .collect(),
        }
    }

    #[test]
    fn tool_results_convert_with_tool_use_injection() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![
                assistant_with_calls("我来读文件", &["tu_1", "tu_2"]),
                tool_result_msg("tu_1", "file body"),
                tool_result_msg("tu_2", "dir body"),
            ],
            ..Default::default()
        };
        let body = serde_json::to_value(build_anthropic_request(&req, false).unwrap()).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "assistant + 合并 user 共 2 条: {body}");

        let a = &msgs[0];
        assert_eq!(a["role"], "assistant");
        let a_blocks = a["content"].as_array().unwrap();
        assert_eq!(a_blocks[0]["type"], "text");
        assert_eq!(a_blocks[1]["type"], "tool_use");
        assert_eq!(a_blocks[1]["id"], "tu_1");
        assert_eq!(a_blocks[1]["name"], "cap_tu_1");
        assert_eq!(a_blocks[2]["id"], "tu_2");

        let u = &msgs[1];
        assert_eq!(u["role"], "user");
        let u_blocks = u["content"].as_array().unwrap();
        assert_eq!(u_blocks.len(), 2);
        assert_eq!(u_blocks[0]["type"], "tool_result");
        assert_eq!(u_blocks[0]["tool_use_id"], "tu_1");
        assert_eq!(u_blocks[0]["content"], "file body");
        assert_eq!(u_blocks[1]["tool_use_id"], "tu_2");
        assert_eq!(u_blocks[1]["content"], "dir body");
    }

    #[test]
    fn assistant_without_tool_calls_emits_string_content() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![ChatMessage::Assistant {
                text: "plain reply".into(),
                tool_calls: vec![],
            }],
            ..Default::default()
        };
        let body = serde_json::to_value(build_anthropic_request(&req, false).unwrap()).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0]["content"].is_string());
        assert_eq!(msgs[0]["content"], "plain reply");
    }

    #[test]
    fn orphan_tool_result_merges_to_user_with_synthesized_error() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![tool_result_msg("tu_9", "raw")],
            ..Default::default()
        };
        let body = serde_json::to_value(build_anthropic_request(&req, false).unwrap()).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "孤儿 ToolResult 合成一条 user: {body}");
        assert_eq!(msgs[0]["role"], "user");
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "tu_9");
        assert!(
            blocks[0]["content"]
                .as_str()
                .unwrap()
                .contains("孤儿工具结果合成"),
            "{body}"
        );
        assert!(!body.to_string().contains(r#""role":"tool""#));
    }

    #[test]
    fn anthropic_system_is_block_array_with_cache_control() {
        let req = LlmRequest {
            model: "claude-3-5-sonnet".to_string(),
            system: Some("be helpful".to_string()),
            messages: vec![ChatMessage::User {
                text: "hi".to_string(),
            }],
            ..Default::default()
        };
        let body = serde_json::to_value(build_anthropic_request(&req, false).unwrap()).unwrap();
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "be helpful");
        assert_eq!(
            sys[0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn anthropic_request_promotes_system_to_top_level() {
        let req = AnthropicRequest {
            model: "claude-3-5-sonnet",
            temperature: None,
            top_p: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("hi".to_string()),
            }],
            system: Some(vec![AnthropicSystemBlock {
                kind: "text".to_string(),
                text: "be helpful".to_string(),
                cache_control: None,
            }]),
            max_tokens: 1024,
            tools: vec![],
            stream: false,
        };
        let j = serde_json::to_string(&req).unwrap();
        assert!(j.contains("claude-3-5-sonnet"));
        assert!(j.contains("be helpful"));
        assert!(j.contains("1024"));
    }

    #[test]
    fn anthropic_memory_entries_merge_into_single_system_block() {
        let req = LlmRequest {
            model: "claude-3-5-sonnet".to_string(),
            messages: vec![
                ChatMessage::System {
                    text: "[EXPERIENCE] fixed: x".into(),
                    kind: SystemKind::Memory(crate::logic::model::message::MemoryKind::Experience),
                },
                ChatMessage::User {
                    text: "hi".to_string(),
                },
            ],
            ..Default::default()
        };
        let body = serde_json::to_value(build_anthropic_request(&req, false).unwrap()).unwrap();
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 1, "system 恰为 1 个 block: {body}");
        let sys_text = sys[0]["text"].as_str().unwrap();
        assert!(sys_text.contains("## 经验"));
        assert!(sys_text.contains("[EXPERIENCE] fixed: x"));
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn anthropic_request_converts_provider_tools() {
        let request = LlmRequest {
            model: "claude-3-5-sonnet".to_string(),
            tools: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "cap_echo_1234",
                    "description": "Echo",
                    "parameters": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}}
                    }
                }
            })],
            ..Default::default()
        };

        let body = serde_json::to_value(build_anthropic_request(&request, false).unwrap()).unwrap();
        assert_eq!(body["tools"][0]["name"], "cap_echo_1234");
        assert_eq!(body["tools"][0]["description"], "Echo");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn anthropic_request_rejects_malformed_provider_tool() {
        let request = LlmRequest {
            model: "claude-3-5-sonnet".to_string(),
            tools: vec![serde_json::json!({"type": "function"})],
            ..Default::default()
        };

        assert!(matches!(
            build_anthropic_request(&request, false),
            Err(crate::common::AgentError::Parse(_))
        ));
    }
}
