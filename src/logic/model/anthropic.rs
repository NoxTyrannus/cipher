use super::error::map_reqwest_error;
#[cfg(test)]
use super::provider::Message;
use super::provider::{
    LlmProvider, LlmRequest, LlmResponse, MessageRole, ToolCall, ToolCallFormat, Usage,
};
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
    system: Option<String>,
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

#[derive(Debug)]
struct ToolEnvelope {
    tool_use_id: String,
    tool_name: String,
    tool_input: serde_json::Value,
    content: String,
}

fn parse_tool_envelope(raw: &str) -> Option<ToolEnvelope> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let id = v.get("tool_use_id")?.as_str()?.to_string();
    let name = v
        .get("tool_name")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let input = v.get("tool_input")?.clone();
    let content = match v.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    Some(ToolEnvelope {
        tool_use_id: id,
        tool_name: name,
        tool_input: input,
        content,
    })
}

fn tool_use_block(env: &ToolEnvelope) -> serde_json::Value {
    serde_json::json!({
        "type": "tool_use",
        "id": env.tool_use_id,
        "name": env.tool_name,
        "input": env.tool_input,
    })
}

fn tool_result_block(env: &ToolEnvelope) -> serde_json::Value {
    serde_json::json!({
        "type": "tool_result",
        "tool_use_id": env.tool_use_id,
        "content": env.content,
    })
}

fn text_block(text: &str) -> serde_json::Value {
    serde_json::json!({"type": "text", "text": text})
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
    let mut system: Option<String> = req.system.clone();
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    let msgs = &req.messages;
    let mut i = 0;
    while i < msgs.len() {
        let m = &msgs[i];
        match m.role {
            MessageRole::System => {
                system = Some(match system {
                    Some(existing) => format!("{existing}\n{}", m.content),
                    None => m.content.clone(),
                });
                i += 1;
            }
            MessageRole::Assistant => {
                let mut j = i + 1;
                let mut tool_uses: Vec<serde_json::Value> = Vec::new();
                let mut tool_results: Vec<serde_json::Value> = Vec::new();
                while j < msgs.len() && msgs[j].role == MessageRole::Tool {
                    match parse_tool_envelope(&msgs[j].content) {
                        Some(env) => {
                            tool_uses.push(tool_use_block(&env));
                            tool_results.push(tool_result_block(&env));
                        }
                        None => {
                            tool_results.push(text_block(&msgs[j].content));
                        }
                    }
                    j += 1;
                }
                let content = if tool_uses.is_empty() {
                    serde_json::Value::String(m.content.clone())
                } else {
                    let mut blocks = Vec::new();
                    if !m.content.is_empty() {
                        blocks.push(text_block(&m.content));
                    }
                    blocks.extend(tool_uses);
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
            MessageRole::Tool => {
                let mut blocks = Vec::new();
                let mut j = i;
                while j < msgs.len() && msgs[j].role == MessageRole::Tool {
                    match parse_tool_envelope(&msgs[j].content) {
                        Some(env) => blocks.push(tool_result_block(&env)),
                        None => blocks.push(text_block(&msgs[j].content)),
                    }
                    j += 1;
                }
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::Value::Array(blocks),
                });
                i = j;
            }
            MessageRole::User => {
                messages.push(AnthropicMessage {
                    role: m.role.to_string(),
                    content: serde_json::Value::String(m.content.clone()),
                });
                i += 1;
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

    #[test]
    fn anthropic_id_and_name() {
        let p = AnthropicProvider::new();
        assert_eq!(p.id(), "anthropic");
        assert_eq!(p.name(), "Anthropic");
    }

    fn tool_msg(content: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: content.to_string(),
        }
    }

    fn assistant_msg(content: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: content.to_string(),
        }
    }

    #[test]
    fn a2_envelope_tool_results_convert_with_tool_use_injection() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![
                assistant_msg("我来读文件"),
                tool_msg(
                    r#"{"tool_use_id":"tu_1","tool_name":"cap_file_read","tool_input":{"path":"a.txt"},"content":"file body"}"#,
                ),
                tool_msg(
                    r#"{"tool_use_id":"tu_2","tool_name":"cap_file_list","tool_input":{"path":"/"},"content":"dir body"}"#,
                ),
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
        assert_eq!(a_blocks[1]["name"], "cap_file_read");
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
    fn a2_bare_output_falls_back_to_user_text_blocks() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![
                assistant_msg(""),
                tool_msg(r#"{"content":"hello from wasm","size":15}"#),
            ],
            ..Default::default()
        };
        let body = serde_json::to_value(build_anthropic_request(&req, false).unwrap()).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);

        assert!(msgs[0]["content"].is_string());

        let u_blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(u_blocks[0]["type"], "text");
        assert!(u_blocks[0]["text"]
            .as_str()
            .unwrap()
            .contains("hello from wasm"));

        assert!(!body.to_string().contains(r#""role":"tool""#));
    }

    #[test]
    fn a2_tool_without_preceding_assistant_merges_to_user() {
        let req = LlmRequest {
            model: "m".into(),
            messages: vec![
                tool_msg(
                    r#"{"tool_use_id":"tu_9","tool_name":"cap_x","tool_input":{},"content":"r1"}"#,
                ),
                tool_msg("plain"),
            ],
            ..Default::default()
        };
        let body = serde_json::to_value(build_anthropic_request(&req, false).unwrap()).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "连续 Tool 合并一条 user: {body}");
        assert_eq!(msgs[0]["role"], "user");
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["type"], "text");
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
            system: Some("be helpful".to_string()),
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
