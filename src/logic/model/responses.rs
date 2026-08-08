use super::error::map_reqwest_error;
use super::message::{normalize_with_system, ChatMessage};
use super::provider::{LlmProvider, LlmRequest, LlmResponse, ToolCallFormat, Usage};
use super::stream::{find_double_newline, StreamChunk};
use crate::common::{AgentError, Result};
use async_trait::async_trait;
use futures::StreamExt;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const PROVIDER_NAME: &str = "responses";

pub struct ResponsesProvider {
    client: reqwest::Client,
}

impl ResponsesProvider {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest client build");
        Self { client }
    }
}

impl Default for ResponsesProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesInputItem {
    #[serde(rename = "type")]
    item_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
}

fn build_responses_request(req: &LlmRequest, stream: bool) -> ResponsesRequest<'_> {
    let normalized = normalize_with_system(req.system.as_deref(), &req.messages);
    let instructions = if normalized.system.is_empty() {
        None
    } else {
        Some(normalized.system)
    };
    let mut input: Vec<ResponsesInputItem> = Vec::new();

    for msg in &normalized.messages {
        match msg {
            ChatMessage::User { text } => input.push(ResponsesInputItem {
                item_type: "message".to_string(),
                role: Some("user".to_string()),
                content: Some(text.clone()),
                call_id: None,
                output: None,
            }),
            ChatMessage::Assistant { text, .. } => input.push(ResponsesInputItem {
                item_type: "message".to_string(),
                role: Some("assistant".to_string()),
                content: Some(text.clone()),
                call_id: None,
                output: None,
            }),
            ChatMessage::ToolResult { id, text, .. } => input.push(ResponsesInputItem {
                item_type: "function_call_output".to_string(),
                role: None,
                content: None,
                call_id: Some(id.clone()),
                output: Some(text.clone()),
            }),
            ChatMessage::System { .. } => unreachable!("normalize 已抽取全部 System"),
        }
    }

    ResponsesRequest {
        model: &req.model,
        instructions,
        input,
        temperature: req.temperature,
        top_p: req.top_p,
        max_output_tokens: req.max_tokens,
        tools: req.tools.clone(),
        stream,
    }
}

fn extract_message_content(output: &Option<Vec<ResponsesOutputItem>>) -> String {
    let Some(items) = output else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        if item.kind != "message" {
            continue;
        }
        let Some(blocks) = &item.content else {
            continue;
        };
        let mut text_parts: Vec<String> = Vec::new();
        for block in blocks {
            if block.kind == "output_text" {
                if let Some(t) = &block.text {
                    text_parts.push(t.clone());
                }
            }
        }
        if text_parts.is_empty() {
            for block in blocks {
                if block.kind == "text" {
                    if let Some(t) = &block.text {
                        text_parts.push(t.clone());
                    }
                }
            }
        }
        parts.append(&mut text_parts);
    }
    parts.join("")
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResponsesApiResponse {
    output: Option<Vec<ResponsesOutputItem>>,
    usage: Option<ResponsesUsage>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResponsesOutputItem {
    #[serde(rename = "type")]
    kind: String,
    role: Option<String>,
    content: Option<Vec<ResponsesContentBlock>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResponsesContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    annotations: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ResponsesUsage {
    input_tokens: u32,
    output_tokens: u32,
    total_tokens: u32,
}

fn extract_stream_error(json: &serde_json::Value, event_type: &str) -> String {
    let msg = json
        .pointer("/response/error/message")
        .and_then(|v| v.as_str())
        .or_else(|| json.pointer("/error/message").and_then(|v| v.as_str()))
        .or_else(|| json.pointer("/response/error").and_then(|v| v.as_str()))
        .or_else(|| json.pointer("/error").and_then(|v| v.as_str()))
        .or_else(|| json.pointer("/message").and_then(|v| v.as_str()));
    msg.map(str::to_string)
        .unwrap_or_else(|| format!("responses stream event '{event_type}' failed"))
}

/// 处理一行 `data: ...` 的 SSE 事件。返回 Continue 表示继续读流，
/// Stop 表示正常结束（[DONE] / response.completed），出错返回 Err。
fn process_sse_data(
    data: &str,
    accumulated: &mut String,
    usage: &mut Option<Usage>,
    on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
) -> Result<SseEventOutcome> {
    if data == "[DONE]" {
        on_chunk(StreamChunk::Done);
        return Ok(SseEventOutcome::Stop);
    }

    let json: serde_json::Value = serde_json::from_str(data).map_err(|e| {
        let msg = format!("responses SSE parse: {e}");
        on_chunk(StreamChunk::Error(msg.clone()));
        AgentError::Llm(msg)
    })?;

    let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "response.output_text.delta" => {
            if let Some(delta) = json.get("delta").and_then(|v| v.as_str()) {
                if !delta.is_empty() {
                    accumulated.push_str(delta);
                    on_chunk(StreamChunk::Delta(delta.to_string()));
                }
            }
            Ok(SseEventOutcome::Continue)
        }
        "response.completed" => {
            if let Some(u) = json.pointer("/response/usage") {
                match serde_json::from_value::<ResponsesUsage>(u.clone()) {
                    Ok(ru) => {
                        *usage = Some(Usage {
                            prompt_tokens: ru.input_tokens,
                            completion_tokens: ru.output_tokens,
                            total_tokens: ru.total_tokens,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("responses: failed to parse stream usage: {e}");
                    }
                }
            }
            on_chunk(StreamChunk::Done);
            Ok(SseEventOutcome::Stop)
        }
        "response.failed" | "response.error" | "response.incomplete" => {
            let msg = extract_stream_error(&json, event_type);
            on_chunk(StreamChunk::Error(msg.clone()));
            Err(AgentError::Llm(msg))
        }
        _ => {
            let is_function_call = event_type.starts_with("response.function_call_arguments")
                || (matches!(
                    event_type,
                    "response.output_item.added" | "response.output_item.done"
                ) && json.pointer("/item/type").and_then(|v| v.as_str())
                    == Some("function_call"));
            if is_function_call {
                tracing::warn!(
                    "responses: function_call events are not supported in this stage, ignoring"
                );
            }
            Ok(SseEventOutcome::Continue)
        }
    }
}

#[derive(Debug)]
enum SseEventOutcome {
    Continue,
    Stop,
}

#[async_trait]
impl LlmProvider for ResponsesProvider {
    fn id(&self) -> &'static str {
        "responses"
    }

    fn name(&self) -> &'static str {
        "Responses API"
    }

    fn tool_call_format(&self) -> ToolCallFormat {
        ToolCallFormat::OpenAI
    }

    async fn call(&self, req: &LlmRequest) -> Result<LlmResponse> {
        if req.api_url.is_empty() {
            return Err(crate::common::AgentError::Llm(
                "ResponsesProvider: api_url is empty".to_string(),
            ));
        }
        let api_key = req
            .api_key
            .as_ref()
            .ok_or_else(|| {
                crate::common::AgentError::Llm("ResponsesProvider: api_key is required".to_string())
            })?
            .expose_secret()
            .clone();

        let url = format!("{}/v1/responses", req.api_url.trim_end_matches('/'));
        let body = build_responses_request(req, false);

        let resp = match self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("responses.call: timeout/connect error, retrying once: {e}");
                let body = build_responses_request(req, false);
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {api_key}"))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| map_reqwest_error(e, PROVIDER_NAME))?
            }
            Err(e) => return Err(map_reqwest_error(e, PROVIDER_NAME)),
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::common::AgentError::Llm(format!(
                "Responses API error (HTTP {status}): {text}"
            )));
        }

        let api_resp: ResponsesApiResponse = resp.json().await.map_err(|e| {
            crate::common::AgentError::Llm(format!("Responses API parse error: {e}"))
        })?;

        let content = extract_message_content(&api_resp.output);

        let usage = api_resp.usage.map(|u| Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
        });

        Ok(LlmResponse {
            content,
            tool_calls: vec![],
            usage,
        })
    }

    async fn call_stream(
        &self,
        req: &LlmRequest,
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<LlmResponse> {
        if req.api_url.is_empty() {
            return Err(crate::common::AgentError::Llm(
                "ResponsesProvider: api_url is empty".to_string(),
            ));
        }
        let api_key = req
            .api_key
            .as_ref()
            .ok_or_else(|| {
                crate::common::AgentError::Llm("ResponsesProvider: api_key is required".to_string())
            })?
            .expose_secret()
            .clone();

        let url = format!("{}/v1/responses", req.api_url.trim_end_matches('/'));
        let body = build_responses_request(req, true);

        let stream_resp = match self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                tracing::warn!("responses.call_stream: timeout/connect error, retrying once: {e}");
                let body = build_responses_request(req, true);
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {api_key}"))
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| map_reqwest_error(e, PROVIDER_NAME))?
            }
            Err(e) => return Err(map_reqwest_error(e, PROVIDER_NAME)),
        };

        if !stream_resp.status().is_success() {
            let status = stream_resp.status();
            let text = stream_resp.text().await.unwrap_or_default();
            return Err(crate::common::AgentError::Llm(format!(
                "Responses API stream error (HTTP {status}): {text}"
            )));
        }

        let mut accumulated = String::new();
        let mut stream_usage: Option<Usage> = None;
        let mut buf: Vec<u8> = Vec::new();

        let mut byte_stream = stream_resp.bytes_stream();
        'stream: while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.map_err(|e| map_reqwest_error(e, PROVIDER_NAME))?;
            buf.extend_from_slice(&chunk);

            while let Some(pos) = find_double_newline(&buf) {
                let event_bytes = buf[..pos].to_vec();
                buf = buf[pos + 2..].to_vec();
                let event_str = String::from_utf8_lossy(&event_bytes);

                for line in event_str.lines() {
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    if data.is_empty() {
                        continue;
                    }
                    match process_sse_data(data, &mut accumulated, &mut stream_usage, on_chunk)? {
                        SseEventOutcome::Continue => {}
                        SseEventOutcome::Stop => break 'stream,
                    }
                }
            }
        }

        Ok(LlmResponse {
            content: accumulated,
            tool_calls: vec![],
            usage: stream_usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::model::message::{ChatMessage, SystemKind};

    fn make_request(messages: Vec<ChatMessage>) -> LlmRequest {
        LlmRequest {
            model: "gpt-5".to_string(),
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn build_request_folds_system_into_instructions() {
        let req = make_request(vec![ChatMessage::User {
            text: "hi".to_string(),
        }]);
        let req = LlmRequest {
            system: Some("sys from req".to_string()),
            ..req
        };
        let body = serde_json::to_value(build_responses_request(&req, false)).unwrap();
        assert_eq!(body["instructions"], "sys from req");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"], "hi");
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn memory_entries_merge_into_single_instructions() {
        let req = make_request(vec![
            ChatMessage::System {
                text: "[COGNITIVE] user likes rust".into(),
                kind: SystemKind::Memory(crate::logic::model::message::MemoryKind::Cognitive),
            },
            ChatMessage::User {
                text: "u".to_string(),
            },
        ]);
        let body = serde_json::to_value(build_responses_request(&req, false)).unwrap();
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.contains("## 认知记忆"));
        assert!(instructions.contains("[COGNITIVE] user likes rust"));
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_result_emits_function_call_output_item() {
        let req = make_request(vec![
            ChatMessage::Assistant {
                text: "I'll read it".to_string(),
                tool_calls: vec![crate::logic::model::provider::ToolCall {
                    id: "fc_1".into(),
                    name: "file.read".into(),
                    arguments: serde_json::json!({"path": "a.txt"}),
                }],
            },
            ChatMessage::ToolResult {
                id: "fc_1".into(),
                name: "file.read".into(),
                text: "file body".into(),
                is_error: false,
            },
        ]);
        let body = serde_json::to_value(build_responses_request(&req, false)).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "fc_1");
        assert_eq!(input[1]["output"], "file body");
        assert!(input[1].get("role").is_none());
    }

    #[test]
    fn build_request_wires_top_p() {
        let req = LlmRequest {
            top_p: Some(0.5),
            ..make_request(vec![])
        };
        let body = serde_json::to_value(build_responses_request(&req, false)).unwrap();
        assert_eq!(body["top_p"], 0.5);
    }

    #[test]
    fn extract_content_skips_reasoning_first_item() {
        let output = Some(vec![
            ResponsesOutputItem {
                kind: "reasoning".to_string(),
                role: Some("assistant".to_string()),
                content: Some(vec![ResponsesContentBlock {
                    kind: "summary_text".to_string(),
                    text: Some("thinking".to_string()),
                    annotations: vec![],
                }]),
            },
            ResponsesOutputItem {
                kind: "message".to_string(),
                role: Some("assistant".to_string()),
                content: Some(vec![
                    ResponsesContentBlock {
                        kind: "output_text".to_string(),
                        text: Some("answer part1 ".to_string()),
                        annotations: vec![],
                    },
                    ResponsesContentBlock {
                        kind: "output_text".to_string(),
                        text: Some("answer part2".to_string()),
                        annotations: vec![],
                    },
                ]),
            },
        ]);
        assert_eq!(
            extract_message_content(&output),
            "answer part1 answer part2"
        );
    }

    #[test]
    fn extract_content_falls_back_to_text_blocks() {
        let output = Some(vec![ResponsesOutputItem {
            kind: "message".to_string(),
            role: Some("assistant".to_string()),
            content: Some(vec![ResponsesContentBlock {
                kind: "text".to_string(),
                text: Some("legacy text".to_string()),
                annotations: vec![],
            }]),
        }]);
        assert_eq!(extract_message_content(&output), "legacy text");
    }

    #[test]
    fn extract_content_empty_when_no_message_item() {
        let output = Some(vec![ResponsesOutputItem {
            kind: "reasoning".to_string(),
            role: Some("assistant".to_string()),
            content: Some(vec![]),
        }]);
        assert_eq!(extract_message_content(&output), "");
        assert_eq!(extract_message_content(&None), "");
    }

    fn run_sse(
        data: &[&str],
        acc: &mut String,
        usage: &mut Option<Usage>,
    ) -> Result<SseEventOutcome> {
        let mut chunks: Vec<StreamChunk> = Vec::new();
        let mut on_chunk = |c: StreamChunk| chunks.push(c);
        let mut outcome = SseEventOutcome::Continue;
        for d in data {
            outcome = process_sse_data(d, acc, usage, &mut on_chunk)?;
            if matches!(outcome, SseEventOutcome::Stop) {
                break;
            }
        }
        Ok(outcome)
    }

    #[test]
    fn sse_delta_accumulates_text() {
        let mut acc = String::new();
        let mut usage = None;
        let out = run_sse(
            &[
                r#"{"type":"response.output_text.delta","delta":"hel"}"#,
                r#"{"type":"response.output_text.delta","delta":"lo"}"#,
            ],
            &mut acc,
            &mut usage,
        )
        .unwrap();
        assert!(matches!(out, SseEventOutcome::Continue));
        assert_eq!(acc, "hello");
    }

    #[test]
    fn sse_completed_parses_usage_and_stops() {
        let mut acc = String::new();
        let mut usage = None;
        let out = run_sse(
            &[r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":11,"output_tokens":22,"total_tokens":33}}}"#],
            &mut acc,
            &mut usage,
        )
        .unwrap();
        assert!(matches!(out, SseEventOutcome::Stop));
        let u = usage.unwrap();
        assert_eq!(u.prompt_tokens, 11);
        assert_eq!(u.completion_tokens, 22);
        assert_eq!(u.total_tokens, 33);
    }

    #[test]
    fn sse_failed_returns_error() {
        let mut acc = String::new();
        let mut usage = None;
        let err = run_sse(
            &[r#"{"type":"response.failed","response":{"error":{"code":"rate_limit","message":"slow down"}}}"#],
            &mut acc,
            &mut usage,
        )
        .unwrap_err();
        assert!(err.to_string().contains("slow down"), "err: {err}");
    }

    #[test]
    fn sse_invalid_json_returns_error() {
        let mut acc = String::new();
        let mut usage = None;
        let err = run_sse(&["{not json"], &mut acc, &mut usage).unwrap_err();
        assert!(err.to_string().contains("SSE parse"), "err: {err}");
    }

    #[test]
    fn sse_done_stops_stream() {
        let mut acc = String::new();
        let mut usage = None;
        let out = run_sse(&["[DONE]"], &mut acc, &mut usage).unwrap();
        assert!(matches!(out, SseEventOutcome::Stop));
    }

    #[test]
    fn sse_function_call_event_warns_and_continues() {
        let mut acc = String::new();
        let mut usage = None;
        let out = run_sse(
            &[r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"fc_1","name":"cap_x"}}"#],
            &mut acc,
            &mut usage,
        )
        .unwrap();
        assert!(matches!(out, SseEventOutcome::Continue));
    }

    #[test]
    fn responses_id_and_name() {
        let p = ResponsesProvider::new();
        assert_eq!(p.id(), "responses");
        assert_eq!(p.name(), "Responses API");
        assert_eq!(p.tool_call_format(), ToolCallFormat::OpenAI);
    }
}
