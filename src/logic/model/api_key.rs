use crate::common::AgentError;
use crate::data::duckdb::loader::ModelRow;
use secrecy::SecretString;

pub fn resolve_api_key(model_row: &ModelRow) -> Result<SecretString, AgentError> {
    if let Some(k) = &model_row.api_key {
        if !k.is_empty() {
            return Ok(SecretString::new(k.clone()));
        }
    }

    Err(AgentError::StartupFailed(
        "model.api_key is empty or None (per ADR-130 设计点 16 整体落盘; \
         iter64+ NOVA_AGENT_ARK_API_KEY env fallback 已作废 — 请通过 init_flow 首启引导或 /config slash 填入)"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn row_with_key(k: Option<&str>) -> ModelRow {
        ModelRow {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            provider: "test".to_string(),
            api_url: "https://example.com/v1".to_string(),
            api_type: "OpenAI".to_string(),
            api_protocol: "openai-v1".to_string(),
            model_id: "ep-test".to_string(),
            api_key: k.map(|s| s.to_string()),
            config: None,
        }
    }

    #[test]
    fn resolve_reads_model_row_api_key() {
        let row = row_with_key(Some("sk-row-key-456"));
        let key = resolve_api_key(&row).expect("should resolve from model row");
        assert_eq!(key.expose_secret(), "sk-row-key-456");
    }

    #[test]
    fn resolve_fails_on_empty_api_key() {
        let row = row_with_key(Some(""));
        assert!(resolve_api_key(&row).is_err(), "empty api_key → Err");
    }

    #[test]
    fn resolve_fails_on_none_api_key() {
        let row = row_with_key(None);
        assert!(resolve_api_key(&row).is_err(), "None api_key → Err");
    }
}
