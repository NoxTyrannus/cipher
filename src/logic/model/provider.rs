use super::message::ChatMessage;
use super::stream::StreamChunk;
use crate::common::{AgentError, Result};
use crate::data::ModelRow;
use async_trait::async_trait;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    pub model: String,

    pub system: Option<String>,

    pub messages: Vec<ChatMessage>,

    pub temperature: Option<f32>,

    pub top_p: Option<f32>,

    pub max_tokens: Option<u32>,

    pub response_format: Option<serde_json::Value>,

    pub stream: bool,

    pub api_url: String,

    pub api_key: Option<SecretString>,

    pub provider_kind: String,

    pub config: Option<serde_json::Value>,
}

/// D1（v0.5.4）：model config 缺 `max_output` 键或值非 u64 时的兜底上限。
/// 注意与 /config 新增路径写入的默认 1024（config_flow.rs）是**两回事**，不得统一。
pub const FALLBACK_MAX_OUTPUT_TOKENS: u64 = 8192;

/// D1 纯函数：从 model config 解析 max_tokens；无 config / 无键 / 非 u64 一律兜底
/// `FALLBACK_MAX_OUTPUT_TOKENS`（存量行 config 缺键 → 兜底，而非不发送该参数）。
pub fn resolve_max_tokens_from_config(config: Option<&serde_json::Value>) -> u32 {
    config
        .and_then(|c| c.get("max_output"))
        .and_then(|v| v.as_u64())
        // Bug 14（v0.5.11）：`n as u32` 对超 u32::MAX 的 u64 静默回绕（4294967296→0，
        // 恰为"不发上限"的坏值），改饱和 clamp 到 u32::MAX；真实超限由 D2 去
        // max_tokens 重试兜底。
        .map(|n| n.min(u32::MAX as u64) as u32)
        .unwrap_or(FALLBACK_MAX_OUTPUT_TOKENS as u32)
}

impl LlmRequest {
    pub fn from_model_row(
        model_row: &ModelRow,
        messages: Vec<ChatMessage>,
        api_key: SecretString,
    ) -> Result<Self> {
        let config = model_row.config.clone();

        let capability = super::capability::resolve_model_capability(model_row);
        // Bug 15（v0.5.11）：不再强制注入 temperature=1.0——拒收该参数的模型（o 系）
        // 会 400。None 时序列化层省略该字段（openai.rs / responses.rs 的请求结构均
        // `skip_serializing_if = "Option::is_none"`），保留 Option 原语义。
        let temperature = capability.temperature;
        let top_p = capability.top_p;
        // D1：无 config / 无键 / 非 u64 一律兜底 8192（恒发送 max_tokens 参数）。
        let max_tokens = Some(resolve_max_tokens_from_config(config.as_ref()));
        Ok(Self {
            model: model_row.model_id.clone(),
            system: None,
            messages,
            temperature,
            top_p,
            max_tokens,
            response_format: if model_row.api_type.eq_ignore_ascii_case("openai") {
                Some(serde_json::json!({"type": "json_object"}))
            } else {
                config
                    .as_ref()
                    .and_then(|c| c.get("response_format").cloned())
            },
            stream: false,
            api_url: model_row.api_url.clone(),
            api_key: Some(api_key),

            provider_kind: model_row.api_type.to_lowercase(),
            config,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,

    #[serde(default)]
    pub usage: Option<Usage>,

    /// D3（v0.5.4）：供应商返回的结束原因（openai `choices[0].finish_reason` /
    /// responses `status`+`incomplete_details.reason`）。缺失为 None。
    /// say 链路据此在 `Some("length")` 时追加截断标记（呈现层，thinking.rs）。
    #[serde(default)]
    pub finish_reason: Option<String>,
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

/// D2 分类纯函数：判断供应商错误串是否属于 max_tokens / max_output 上限语义
/// （如 "max_tokens is too large"、"reduce max_tokens to ..."）。
/// 命中时调用方应去掉 max_tokens 参数重试一次。
pub fn is_max_tokens_limit_error(message: &str) -> bool {
    let m = message.to_lowercase();
    let mentions_param = m.contains("max_tokens")
        || m.contains("max_output_tokens")
        || m.contains("max output tokens")
        || m.contains("max_output");
    let limit_word = m.contains("too large")
        || m.contains("too big")
        || m.contains("exceed")
        || m.contains("reduce")
        || m.contains("maximum")
        || m.contains("less than or equal");
    mentions_param && limit_word
}

/// D2 包装层：think/say 共用的 LLM 调用入口。首刷失败且错误命中
/// `is_max_tokens_limit_error`、且请求确实带了 max_tokens 时，**仅一次**去掉
/// max_tokens 参数重试；其余错误原样透传。
pub async fn call_with_max_tokens_fallback(
    provider: &dyn LlmProvider,
    req: &LlmRequest,
) -> Result<LlmResponse> {
    match provider.call(req).await {
        Err(e) if req.max_tokens.is_some() && is_max_tokens_limit_error(&e.to_string()) => {
            tracing::warn!(
                error = %e,
                "供应商拒绝 max_tokens（超模型上限），去掉该参数重试一次"
            );
            let mut retry = req.clone();
            retry.max_tokens = None;
            provider.call(&retry).await
        }
        other => other,
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
    async fn provider_default_call_returns_not_implemented() {
        let provider = StubProvider;
        let req = LlmRequest::default();
        let err = provider.call(&req).await.unwrap_err();
        assert!(err.to_string().to_lowercase().contains("not implemented"));
    }

    // ---- D1：max_output 兜底（四态：无 config / 无键 / 非 u64 / 显式值）----

    #[test]
    fn resolve_max_tokens_falls_back_when_config_missing() {
        assert_eq!(
            resolve_max_tokens_from_config(None),
            FALLBACK_MAX_OUTPUT_TOKENS as u32,
            "无 config → 8192 兜底"
        );
    }

    #[test]
    fn resolve_max_tokens_falls_back_when_key_missing() {
        let config = serde_json::json!({"temperature": 1.0});
        assert_eq!(
            resolve_max_tokens_from_config(Some(&config)),
            FALLBACK_MAX_OUTPUT_TOKENS as u32,
            "config 无 max_output 键 → 8192 兜底"
        );
    }

    #[test]
    fn resolve_max_tokens_falls_back_when_value_not_u64() {
        let config = serde_json::json!({"max_output": "4096"});
        assert_eq!(
            resolve_max_tokens_from_config(Some(&config)),
            FALLBACK_MAX_OUTPUT_TOKENS as u32,
            "max_output 非 u64（字符串）→ 8192 兜底"
        );
        let config = serde_json::json!({"max_output": 1.5});
        assert_eq!(
            resolve_max_tokens_from_config(Some(&config)),
            FALLBACK_MAX_OUTPUT_TOKENS as u32,
            "max_output 非 u64（浮点）→ 8192 兜底"
        );
    }

    #[test]
    fn resolve_max_tokens_uses_explicit_value() {
        let config = serde_json::json!({"max_output": 1024});
        assert_eq!(
            resolve_max_tokens_from_config(Some(&config)),
            1024,
            "显式 max_output（如 /config 新增行写入的 1024）原样生效"
        );
    }

    #[test]
    fn resolve_max_tokens_saturates_above_u32_range() {
        // Bug 14：4294967296（2^32）经 `as u32` 回绕成 0，须饱和到 u32::MAX。
        let config = serde_json::json!({"max_output": 4_294_967_296_u64});
        assert_eq!(
            resolve_max_tokens_from_config(Some(&config)),
            u32::MAX,
            "超 u32::MAX 的 max_output 饱和而非回绕为 0"
        );
        let config = serde_json::json!({"max_output": u64::MAX});
        assert_eq!(
            resolve_max_tokens_from_config(Some(&config)),
            u32::MAX,
            "u64::MAX 同样饱和到 u32::MAX"
        );
        let config = serde_json::json!({"max_output": u32::MAX as u64});
        assert_eq!(
            resolve_max_tokens_from_config(Some(&config)),
            u32::MAX,
            "恰好 u32::MAX 的边界值原样生效"
        );
    }

    // ---- Bug 15：不强制注入 temperature（None → 请求体省略该字段）----

    fn make_model_row(model_id: &str, config: Option<serde_json::Value>) -> ModelRow {
        ModelRow {
            id: "test".to_string(),
            name: "Test".to_string(),
            provider: "test".to_string(),
            api_url: "https://example.com".to_string(),
            api_protocol: crate::data::duckdb::loader::default_api_protocol("openai"),
            api_type: "openai".to_string(),
            model_id: model_id.to_string(),
            api_key: None,
            config,
        }
    }

    /// 按 openai.rs `build_openai_request` 的透传方式（temperature: req.temperature）
    /// 组请求体并序列化——None 经 `skip_serializing_if` 省略。
    fn serialize_openai_body(req: &LlmRequest) -> String {
        let body = crate::logic::model::openai::OpenAiRequest {
            model: &req.model,
            messages: Vec::new(),
            temperature: req.temperature,
            top_p: req.top_p,
            max_tokens: req.max_tokens,
            response_format: req.response_format.clone(),
            stream: false,
        };
        serde_json::to_string(&body).expect("序列化请求体")
    }

    #[test]
    fn from_model_row_without_temperature_omits_field_in_body() {
        // gpt-4o 的内置 capability 无 temperature（capability.rs 已有对应测试）。
        let row = make_model_row("gpt-4o", None);
        let req = LlmRequest::from_model_row(&row, Vec::new(), SecretString::from("x".to_string()))
            .expect("构建请求");
        assert!(
            req.temperature.is_none(),
            "capability 无 temperature 时不得强制注入 1.0"
        );
        let body = serialize_openai_body(&req);
        assert!(
            !body.contains("temperature"),
            "请求体应省略 temperature 键：{body}"
        );
        assert!(
            body.contains("max_tokens"),
            "max_tokens 仍恒发送（D1 语义不变）：{body}"
        );
    }

    #[test]
    fn from_model_row_keeps_declared_temperature() {
        // 内置声明（kimi）与 config 显式值两条路径均照常透传。
        let row = make_model_row("kimi-k3", None);
        let req = LlmRequest::from_model_row(&row, Vec::new(), SecretString::from("x".to_string()))
            .expect("构建请求");
        assert_eq!(req.temperature, Some(1.0));
        assert!(serialize_openai_body(&req).contains("temperature"));

        let row = make_model_row("gpt-4o", Some(serde_json::json!({"temperature": 0.42})));
        let req = LlmRequest::from_model_row(&row, Vec::new(), SecretString::from("x".to_string()))
            .expect("构建请求");
        assert_eq!(req.temperature, Some(0.42));
    }

    // ---- D2：超限错误分类 + 仅一次去 max_tokens 重试 ----

    #[test]
    fn classifier_matches_max_tokens_limit_semantics() {
        assert!(is_max_tokens_limit_error(
            "openai HTTP 400: max_tokens is too large: expected <= 4096"
        ));
        assert!(is_max_tokens_limit_error(
            "Invalid parameter: max_output_tokens is too large"
        ));
        assert!(is_max_tokens_limit_error(
            "please reduce max_tokens to 4096 or less"
        ));
        assert!(is_max_tokens_limit_error(
            "max_tokens must be less than or equal to 8192"
        ));
        assert!(
            !is_max_tokens_limit_error("connection timed out"),
            "无关错误不命中"
        );
        assert!(
            !is_max_tokens_limit_error("HTTP 401 unauthorized: invalid api key"),
            "认证错误不命中"
        );
    }

    struct LimitThenOkProvider {
        calls: std::sync::atomic::AtomicUsize,
        captured_max_tokens: std::sync::Mutex<Vec<Option<u32>>>,
    }

    impl LimitThenOkProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                captured_max_tokens: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for LimitThenOkProvider {
        fn id(&self) -> &'static str {
            "limit-then-ok"
        }
        fn name(&self) -> &'static str {
            "LimitThenOk"
        }
        async fn call(&self, req: &LlmRequest) -> Result<LlmResponse> {
            self.captured_max_tokens
                .lock()
                .unwrap()
                .push(req.max_tokens);
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Err(AgentError::Llm(
                    "max_tokens is too large: expected <= 4096".to_string(),
                ))
            } else {
                Ok(LlmResponse {
                    content: "ok".to_string(),
                    usage: None,
                    finish_reason: None,
                })
            }
        }
    }

    #[tokio::test]
    async fn wrapper_retries_once_without_max_tokens_on_limit_error() {
        let provider = LimitThenOkProvider::new();
        let req = LlmRequest {
            model: "m".into(),
            max_tokens: Some(8192),
            ..Default::default()
        };
        let resp = call_with_max_tokens_fallback(&provider, &req)
            .await
            .expect("重试后应成功");
        assert_eq!(resp.content, "ok");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "首刷失败 + 去掉 max_tokens 重试一次，共 2 次调用"
        );
        let captured = provider.captured_max_tokens.lock().unwrap();
        assert_eq!(captured[0], Some(8192), "首刷带 max_tokens");
        assert_eq!(captured[1], None, "重试已去掉 max_tokens 参数");
    }

    struct AlwaysLimitProvider;

    #[async_trait]
    impl LlmProvider for AlwaysLimitProvider {
        fn id(&self) -> &'static str {
            "always-limit"
        }
        fn name(&self) -> &'static str {
            "AlwaysLimit"
        }
        async fn call(&self, _req: &LlmRequest) -> Result<LlmResponse> {
            Err(AgentError::Llm("max_tokens is too large".to_string()))
        }
    }

    #[tokio::test]
    async fn wrapper_only_retries_once_even_if_retry_fails() {
        let provider = AlwaysLimitProvider;
        let req = LlmRequest {
            max_tokens: Some(8192),
            ..Default::default()
        };
        let err = call_with_max_tokens_fallback(&provider, &req)
            .await
            .expect_err("重试仍失败应透传错误");
        assert!(is_max_tokens_limit_error(&err.to_string()));
    }

    #[tokio::test]
    async fn wrapper_does_not_retry_without_max_tokens_or_unrelated_error() {
        // 请求未带 max_tokens：即使错误像超限也不重试。
        let provider = LimitThenOkProvider::new();
        let req = LlmRequest::default();
        assert!(req.max_tokens.is_none());
        let _ = call_with_max_tokens_fallback(&provider, &req).await;
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "无 max_tokens 参数时不重试"
        );
    }
}
