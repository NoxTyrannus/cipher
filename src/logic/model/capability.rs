use crate::data::ModelRow;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenCountingStrategy {
    Tiktoken,
    Conservative,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapability {
    pub context_window: usize,
    pub max_output_tokens: usize,
    pub token_counting_strategy: TokenCountingStrategy,
    #[serde(default)]
    pub temperature: Option<f32>,

    #[serde(default)]
    pub top_p: Option<f32>,
}

fn builtin_model_capability(model_id: &str) -> Option<ModelCapability> {
    let id = model_id.to_lowercase();
    let entry = match id.as_str() {
        "gpt-4o" | "gpt-4o-2024-08-06" | "gpt-4o-2024-05-13" => {
            Some((128000, 16384, TokenCountingStrategy::Tiktoken))
        }
        "gpt-4o-mini" | "gpt-4o-mini-2024-07-18" => {
            Some((128000, 16384, TokenCountingStrategy::Tiktoken))
        }
        "gpt-4-turbo" | "gpt-4-turbo-2024-04-09" => {
            Some((128000, 4096, TokenCountingStrategy::Tiktoken))
        }
        "gpt-4" | "gpt-4-0613" => Some((8192, 4096, TokenCountingStrategy::Tiktoken)),
        "gpt-3.5-turbo" | "gpt-3.5-turbo-0125" => {
            Some((16385, 4096, TokenCountingStrategy::Tiktoken))
        }

        id if id.contains("doubao-pro") || id.contains("doubao-pro") => {
            Some((200000, 8192, TokenCountingStrategy::Conservative))
        }
        id if id.contains("doubao-lite") => {
            Some((200000, 8192, TokenCountingStrategy::Conservative))
        }
        id if id.contains("doubao-mini") => {
            Some((32000, 4096, TokenCountingStrategy::Conservative))
        }

        id if id.contains("deepseek") => Some((128000, 8192, TokenCountingStrategy::Conservative)),

        id if id.contains("glm") => Some((128000, 8192, TokenCountingStrategy::Conservative)),

        id if id.contains("minimax") => {
            Some((1_000_000, 8192, TokenCountingStrategy::Conservative))
        }

        id if id.contains("kimi") || id.contains("moonshot") => {
            Some((200000, 8192, TokenCountingStrategy::Conservative))
        }
        _ => None,
    };
    entry.map(|(ctx, out, strat)| {
        let temperature = if id.contains("kimi") || id.contains("moonshot") || id.contains("k3") {
            Some(1.0_f32)
        } else if id.contains("sensenova") || id.contains("sense") {
            Some(0.75)
        } else if id.contains("minimax") {
            Some(1.0)
        } else {
            None
        };

        let top_p = if id.contains("minimax") {
            Some(0.95)
        } else {
            None
        };
        ModelCapability {
            context_window: ctx,
            max_output_tokens: out,
            token_counting_strategy: strat,
            temperature,
            top_p,
        }
    })
}

fn provider_default_capability(api_type: &str) -> Option<ModelCapability> {
    match api_type.to_lowercase().as_str() {
        "openai" => Some(ModelCapability {
            context_window: 128000,
            max_output_tokens: 4096,
            token_counting_strategy: TokenCountingStrategy::Tiktoken,
            temperature: None,
            top_p: None,
        }),
        _ => None,
    }
}

pub fn resolve_model_capability(row: &ModelRow) -> ModelCapability {
    let config_temperature = row.config.as_ref().and_then(|c| {
        c.get("temperature")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
            .or_else(|| {
                c.get("default_temperature")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
            })
    });

    if let Some(ref config) = row.config {
        let context_window = config
            .get("context_window")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let max_output_tokens = config
            .get("max_output")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        if context_window.is_some() || max_output_tokens.is_some() {
            let tiktoken_support = match row.api_type.to_lowercase().as_str() {
                "openai" => TokenCountingStrategy::Tiktoken,
                _ => TokenCountingStrategy::Conservative,
            };
            let config_top_p = config
                .get("top_p")
                .and_then(|v| v.as_f64())
                .map(|v| v as f32);
            return ModelCapability {
                context_window: context_window.unwrap_or(128000),
                max_output_tokens: max_output_tokens.unwrap_or(4096),
                token_counting_strategy: tiktoken_support,
                temperature: config_temperature,
                top_p: config_top_p,
            };
        }
    }

    let config_top_p = row
        .config
        .as_ref()
        .and_then(|c| c.get("top_p").and_then(|v| v.as_f64()).map(|v| v as f32));

    if let Some(mut cap) = builtin_model_capability(&row.model_id) {
        cap.temperature = config_temperature.or(cap.temperature);
        cap.top_p = config_top_p.or(cap.top_p);
        return cap;
    }

    if let Some(mut cap) = provider_default_capability(&row.api_type) {
        cap.temperature = config_temperature.or(cap.temperature);
        cap.top_p = config_top_p.or(cap.top_p);
        return cap;
    }

    ModelCapability {
        context_window: 4096,
        max_output_tokens: 512,
        token_counting_strategy: TokenCountingStrategy::Conservative,
        temperature: config_temperature,
        top_p: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(model_id: &str, api_type: &str, config: Option<serde_json::Value>) -> ModelRow {
        ModelRow {
            id: "test".to_string(),
            name: "Test".to_string(),
            provider: "test".to_string(),
            api_url: "https://example.com".to_string(),
            api_protocol: crate::data::duckdb::loader::default_api_protocol(api_type),
            api_type: api_type.to_string(),
            model_id: model_id.to_string(),
            api_key: None,
            config,
        }
    }

    #[test]
    fn test_builtin_gpt4o() {
        let cap = resolve_model_capability(&make_row("gpt-4o", "openai", None));
        assert_eq!(cap.context_window, 128000);
        assert_eq!(cap.token_counting_strategy, TokenCountingStrategy::Tiktoken);
    }

    #[test]
    fn test_config_overrides_builtin() {
        let config = serde_json::json!({"context_window": 64000});
        let cap = resolve_model_capability(&make_row("gpt-4o", "openai", Some(config)));
        assert_eq!(cap.context_window, 64000);
        assert_eq!(cap.max_output_tokens, 4096);
    }

    #[test]
    fn test_unknown_model_conservative_defaults() {
        let cap = resolve_model_capability(&make_row("unknown-model-v42", "generic", None));
        assert_eq!(cap.context_window, 4096);
        assert_eq!(cap.max_output_tokens, 512);
        assert_eq!(
            cap.token_counting_strategy,
            TokenCountingStrategy::Conservative
        );
    }

    #[test]
    fn test_config_overrides_partial() {
        let config = serde_json::json!({"max_output": 2048});
        let cap = resolve_model_capability(&make_row("gpt-4o", "openai", Some(config)));
        assert_eq!(cap.context_window, 128000);
        assert_eq!(cap.max_output_tokens, 2048);
    }

    #[test]
    fn test_doubao_mini_no_tools() {
        let cap = resolve_model_capability(&make_row("doubao-mini-1.5", "openai", None));
        assert_eq!(cap.context_window, 32000);
        assert_eq!(cap.max_output_tokens, 4096);
    }

    #[test]
    fn test_serialization_roundtrip() {
        let cap = ModelCapability {
            context_window: 128000,
            max_output_tokens: 4096,
            token_counting_strategy: TokenCountingStrategy::Tiktoken,
            top_p: None,
            temperature: Some(0.7),
        };
        let json = serde_json::to_string(&cap).unwrap();
        let deserialized: ModelCapability = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.context_window, 128000);
        assert_eq!(deserialized.max_output_tokens, 4096);
        assert_eq!(
            deserialized.token_counting_strategy,
            TokenCountingStrategy::Tiktoken
        );
    }

    #[test]
    fn test_builtin_kimi_temperature() {
        let cap = resolve_model_capability(&make_row("kimi-k3", "openai", None));
        assert_eq!(cap.temperature, Some(1.0));
    }

    #[test]
    fn test_builtin_minimax_temperature() {
        let cap = resolve_model_capability(&make_row("MiniMax-M3", "openai", None));

        assert_eq!(cap.temperature, Some(1.0));
        assert_eq!(cap.top_p, Some(0.95));
    }

    #[test]
    fn test_config_temperature_overrides_builtin() {
        let config = serde_json::json!({"temperature": 0.42});
        let cap = resolve_model_capability(&make_row("MiniMax-M3", "openai", Some(config)));
        assert_eq!(cap.temperature, Some(0.42));
    }

    #[test]
    fn test_config_default_temperature_backward_compat() {
        let config = serde_json::json!({"default_temperature": 0.3});
        let cap = resolve_model_capability(&make_row("MiniMax-M3", "openai", Some(config)));
        assert_eq!(cap.temperature, Some(0.3));
    }

    #[test]
    fn test_config_temperature_priority_over_default() {
        let config = serde_json::json!({"temperature": 0.5, "default_temperature": 0.9});
        let cap = resolve_model_capability(&make_row("MiniMax-M3", "openai", Some(config)));
        assert_eq!(cap.temperature, Some(0.5));
    }

    #[test]
    fn test_builtin_temperature_none_for_gpt() {
        let cap = resolve_model_capability(&make_row("gpt-4o", "openai", None));
        assert_eq!(cap.temperature, None);
    }
}
