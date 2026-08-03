use super::error::map_reqwest_error;
use super::provider::{LlmProvider, LlmRequest, LlmResponse, Message, ToolCallFormat, Usage};
use super::stream::{find_double_newline, StreamChunk};
use crate::common::Result;
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
    role: String,
    content: String,
}

fn build_responses_request(req: &LlmRequest, stream: bool) -> ResponsesRequest<'_> {
    let input: Vec<ResponsesInputItem> = req
        .messages
        .iter()
        .map(|msg: &Message| ResponsesInputItem {
            item_type: "message".to_string(),
            role: msg.role.to_string(),
            content: msg.content.clone(),
        })
        .collect();

    ResponsesRequest {
        model: &req.model,
        instructions: req.system.clone(),
        input,
        temperature: req.temperature,
        max_output_tokens: req.max_tokens,
        tools: req.tools.clone(),
        stream,
    }
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

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| map_reqwest_error(e, PROVIDER_NAME))?;

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

        let content = api_resp
            .output
            .as_ref()
            .and_then(|output| output.first())
            .and_then(|item| item.content.as_ref())
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.text.clone())
            .unwrap_or_default();

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

        let stream_resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| map_reqwest_error(e, PROVIDER_NAME))?;

        if !stream_resp.status().is_success() {
            let status = stream_resp.status();
            let text = stream_resp.text().await.unwrap_or_default();
            return Err(crate::common::AgentError::Llm(format!(
                "Responses API stream error (HTTP {status}): {text}"
            )));
        }

        let mut accumulated = String::new();
        let mut buf: Vec<u8> = Vec::new();

        let mut byte_stream = stream_resp.bytes_stream();
        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.map_err(|e| map_reqwest_error(e, PROVIDER_NAME))?;
            buf.extend_from_slice(&chunk);

            while let Some(pos) = find_double_newline(&buf) {
                let event_bytes = buf[..pos].to_vec();
                buf = buf[pos + 2..].to_vec();
                let event_str = String::from_utf8_lossy(&event_bytes);

                for line in event_str.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            on_chunk(StreamChunk::Done);
                            break;
                        }

                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                            let event_type =
                                json.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match event_type {
                                "response.output_text.delta" => {
                                    if let Some(delta) = json.get("delta").and_then(|v| v.as_str())
                                    {
                                        accumulated.push_str(delta);
                                        on_chunk(StreamChunk::Delta(delta.to_string()));
                                    }
                                }
                                "response.completed" => {
                                    on_chunk(StreamChunk::Done);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        let usage = None;
        Ok(LlmResponse {
            content: accumulated,
            tool_calls: vec![],
            usage,
        })
    }
}
