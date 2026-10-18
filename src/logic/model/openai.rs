use super::error::map_reqwest_error;
use super::message::{normalize_with_system, ChatMessage};
use super::provider::{LlmProvider, LlmRequest, LlmResponse, Usage};
use super::stream::{SseDecoder, StreamChunk};
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
        // 不设客户端级总超时（旧 90s 会掐断 >90s 的流式生成）：流式 body 改由
        // read_timeout 做「单次读空闲」保护（容忍慢推理模型的首 token 延迟）；
        // 非流式 call 在 RequestBuilder 上加请求级 90s 总超时，维持原行为。
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(300))
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
    /// D3（v0.5.4）：`choices[0].finish_reason`（如 "stop" / "length"），缺失为 None。
    #[serde(default)]
    pub finish_reason: Option<String>,
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

#[derive(Debug)]
enum OpenAiSseOutcome {
    Continue,
    /// 收到 [DONE]，调用方应直接结束读流循环（Done 通知由收尾统一发出）。
    Done,
}

/// 处理一条 SSE 事件的 data 载荷。
///
/// 空载荷直接跳过（部分服务器发空 data: 保活行，旧实现会在此 JSON 解析
/// 失败并终止整流）；非空但 JSON 解析失败仍按旧语义报 Error 并终止整流。
fn process_openai_sse_event(
    data: &str,
    accumulated: &mut String,
    finish_reason: &mut Option<String>,
    on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
) -> Result<OpenAiSseOutcome> {
    if data == "[DONE]" {
        return Ok(OpenAiSseOutcome::Done);
    }
    if data.is_empty() {
        return Ok(OpenAiSseOutcome::Continue);
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
    // D3：SSE 末块 choices[0].finish_reason（如 "length"）。
    if let Some(fr) = parsed
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
    {
        *finish_reason = Some(fr.to_string());
    }
    Ok(OpenAiSseOutcome::Continue)
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
            .timeout(Duration::from_secs(90))
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
                    .timeout(Duration::from_secs(90))
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
        let finish_reason = parsed.choices.first().and_then(|c| c.finish_reason.clone());

        Ok(LlmResponse {
            content,
            usage: parsed.usage.map(|u| Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            }),
            finish_reason,
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
        let mut decoder = SseDecoder::new();
        let mut accumulated = String::new();
        let mut stream_finish_reason: Option<String> = None;
        // 服务器忽略 stream=true 直接回完整 JSON 时的非流式补救原料：仅在尚无
        // 任何 delta 时保留原始 body；某批次已完成事件分发（说明确在走 SSE）
        // 则丢弃此前残留，近似旧 buf「最后一个完整事件之后的字节」语义。
        let mut raw_tail: Vec<u8> = Vec::new();
        let mut done = false;

        while !done {
            let events = match stream.next().await {
                Some(Ok(chunk)) => {
                    let events = decoder.feed(&chunk);
                    if events.is_empty() {
                        if accumulated.is_empty() {
                            raw_tail.extend_from_slice(&chunk);
                        }
                    } else if accumulated.is_empty() {
                        raw_tail.clear();
                    }
                    events
                }
                Some(Err(e)) => return Err(map_reqwest_error(e, "openai")),
                None => {
                    // EOF：解码器收尾（无空行结尾的残留事件仍按事件分发）。
                    done = true;
                    decoder.finish()
                }
            };
            for data in events {
                match process_openai_sse_event(
                    &data,
                    &mut accumulated,
                    &mut stream_finish_reason,
                    on_chunk,
                )? {
                    OpenAiSseOutcome::Continue => {}
                    OpenAiSseOutcome::Done => {
                        done = true;
                        break;
                    }
                }
            }
        }

        if accumulated.is_empty() && !raw_tail.is_empty() {
            if let Ok(parsed) = serde_json::from_slice::<OpenAiResponse>(&raw_tail) {
                accumulated = parsed
                    .choices
                    .first()
                    .and_then(|c| c.message.content.clone())
                    .unwrap_or_default();
                if let Some(fr) = parsed.choices.first().and_then(|c| c.finish_reason.clone()) {
                    stream_finish_reason = Some(fr);
                }
                if !accumulated.is_empty() {
                    on_chunk(StreamChunk::Delta(accumulated.clone()));
                }
            }
        }

        on_chunk(StreamChunk::Done);
        Ok(LlmResponse {
            content: accumulated,
            usage: None,
            finish_reason: stream_finish_reason,
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
    fn openai_response_parses_finish_reason() {
        // D3：choices[0].finish_reason 解析填充。
        let body = r#"{"choices":[{"message":{"content":"hi"},"finish_reason":"length"}]}"#;
        let parsed: OpenAiResponse = serde_json::from_str(body).unwrap();
        assert_eq!(
            parsed.choices[0].finish_reason.as_deref(),
            Some("length"),
            "finish_reason=length 应被解析"
        );
    }

    #[test]
    fn openai_response_missing_finish_reason_is_none() {
        // D3：缺 finish_reason 字段时为 None（不臆造）。
        let body = r#"{"choices":[{"message":{"content":"hi"}}]}"#;
        let parsed: OpenAiResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.choices[0].finish_reason.is_none());
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

    // httptest 0.15 仅支持整体 body 响应（无 body_from_fn），无法逐 chunk 慢发；
    // 跨 feed 边界切割由 stream.rs 的 SseDecoder 单测覆盖，此处验证
    // CRLF 分帧 / 无空格 data: / 注释行 / [DONE] 经真实 HTTP 栈端到端可达。
    #[tokio::test]
    async fn openai_call_stream_parses_crlf_events_over_http() {
        use httptest::{matchers::*, responders::*, Expectation, Server};
        let mut server = Server::run();
        let url_trimmed = server.url_str("").trim_end_matches('/').to_string();
        let sse = concat!(
            ": keep-alive\r\n\r\n",
            "data:{\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\r\n\r\n",
            "data: [DONE]\r\n\r\n",
        );
        server.expect(
            Expectation::matching(request::path("/v1/chat/completions")).respond_with(
                status_code(200)
                    .insert_header("Content-Type", "text/event-stream")
                    .body(sse),
            ),
        );
        let p = OpenAiProvider::new();
        let req = LlmRequest {
            model: "gpt-4o".to_string(),
            messages: vec![user_msg("hi")],
            api_url: format!("{url_trimmed}/v1"),
            api_key: Some(secrecy::SecretString::new("sk-test".to_string())),
            ..Default::default()
        };
        let mut chunks: Vec<StreamChunk> = Vec::new();
        let mut on_chunk = |chunk: StreamChunk| chunks.push(chunk);
        let resp = p.call_stream(&req, &mut on_chunk).await.unwrap();
        assert_eq!(resp.content, "hello");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(chunks.len(), 3, "Delta x2 + Done: {chunks:?}");
        assert!(matches!(&chunks[0], StreamChunk::Delta(s) if s == "he"));
        assert!(matches!(&chunks[1], StreamChunk::Delta(s) if s == "llo"));
        assert!(matches!(chunks.last(), Some(StreamChunk::Done)));
        server.verify_and_clear();
    }
}
