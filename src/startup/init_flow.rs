use crate::common::AgentError;
use crate::data::bootstrap::AppState;
use crate::data::duckdb::loader::{
    has_configured_model, insert_model, update_model_api_key_by_provider,
};
use crate::data::workspace_store::{WorkspaceRow, WorkspaceStore};
use crate::data::ModelRow;
use crate::logic::model::anthropic::AnthropicProvider;
use crate::logic::model::api_key::resolve_api_key;
use crate::logic::model::message::ChatMessage;
use crate::logic::model::openai::OpenAiProvider;
use crate::logic::model::provider::LlmRequest;
use crate::logic::model::registry::ProviderRegistry;
use crate::logic::model::responses::ResponsesProvider;
use secrecy::SecretString;
use std::path::Path;
use std::sync::Arc;

const PRESET_TEMPLATES: &[(&str, &str, &str, &str)] = &[
    (
        "OpenAI 官方",
        "openai",
        "https://api.openai.com/v1",
        "OpenAI",
    ),
    (
        "Anthropic 官方",
        "anthropic",
        "https://api.anthropic.com",
        "Anthropic",
    ),
];

pub async fn init_flow(app: &AppState, data_dir: &Path) -> Result<(), AgentError> {
    if has_configured_model(&app.duckdb)? {
        tracing::info!(
            "init_flow: model 表已有已配置 model (api_key 非空), 跳过首启引导 (设计点 1)"
        );
    } else {
        tracing::info!("init_flow: 首启, 进入交互引导 (设计点 2 必配 1 模型 + ping 无逃生)");
        prompt_and_configure_model(app, data_dir).await?;
    }

    let workspace = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "$PWD".to_string());
    let workspace_store = WorkspaceStore::open(app.paths.storage_root())?;
    if workspace_store.seed_if_empty(WorkspaceRow {
        id: "default".to_string(),
        name: "default".to_string(),
        path: workspace.clone(),
        is_default: true,
    })? {
        tracing::info!(path = workspace, "seed default workspace (设计点 20)");
    }

    let inserted = app
        .duckdb
        .execute(
            "INSERT INTO agent (id, name, mode, is_default) \
             SELECT 'agent', 'Agent', 'unni', true \
             WHERE NOT EXISTS (SELECT 1 FROM agent)",
            [],
        )
        .map_err(|error| AgentError::Bootstrap(format!("seed default agent: {error}")))?;
    if inserted > 0 {
        tracing::info!(name = "Agent", "seed default agent (设计点 20)");
    }
    Ok(())
}

async fn prompt_and_configure_model(app: &AppState, _data_dir: &Path) -> Result<(), AgentError> {
    use dialoguer::{Input, Password, Select};

    loop {
        let mut items: Vec<String> = PRESET_TEMPLATES
            .iter()
            .map(|(n, _, _, _)| n.to_string())
            .collect();
        items.push("自定义".to_string());
        let sel = Select::new()
            .with_prompt("选择 LLM provider (设计点 8 预置模板)")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("provider select: {}", e)))?;

        let (provider, api_url, api_type) = if sel < PRESET_TEMPLATES.len() {
            let t = PRESET_TEMPLATES[sel];
            (t.1.to_string(), t.2.to_string(), t.3.to_string())
        } else {
            let p = Input::<String>::new()
                .with_prompt(
                    "provider 标识 (与 api_url 匹配的简短标识, e.g. openai / anthropic / minimax)",
                )
                .interact_text()
                .map_err(|e| AgentError::Parse(format!("provider input: {}", e)))?;
            let u = Input::<String>::new()
                .with_prompt("api_url (完整 base URL)")
                .interact_text()
                .map_err(|e| AgentError::Parse(format!("api_url input: {}", e)))?;
            let t = Input::<String>::new()
                .with_prompt("api_type (OpenAI / Anthropic)")
                .default("OpenAI".to_string())
                .interact_text()
                .map_err(|e| AgentError::Parse(format!("api_type input: {}", e)))?;
            (p, u, t)
        };

        let name = Input::<String>::new()
            .with_prompt("模型显示名 (e.g. Doubao Pro)")
            .interact_text()
            .map_err(|e| AgentError::Parse(format!("name input: {}", e)))?;
        let model_id = Input::<String>::new()
            .with_prompt("model_id (传服务商, e.g. ep-xxx / gpt-4o)")
            .interact_text()
            .map_err(|e| AgentError::Parse(format!("model_id input: {}", e)))?;
        let api_key = Password::new()
            .with_prompt("API key (整体落盘 model.api_key, 设计点 16)")
            .interact()
            .map_err(|e| AgentError::Parse(format!("api_key input: {}", e)))?;
        if api_key.trim().is_empty() {
            eprintln!("api_key 不能为空 (设计点 4 必选). 请重填 (设计点 22).");
            continue;
        }

        let lower_model_id = model_id.to_lowercase();

        let temperature_config = if lower_model_id.contains("kimi")
            || lower_model_id.contains("k3")
            || lower_model_id.contains("moonshot")
            || lower_model_id.contains("minimax")
        {
            Some(serde_json::json!(1.0_f32))
        } else {
            None
        };
        let row = ModelRow {
            id: format!("{}-{}", provider, model_id),
            name,
            provider: provider.clone(),
            api_url: api_url.clone(),
            api_protocol: crate::data::duckdb::loader::default_api_protocol(&api_type),
            api_type: api_type.clone(),
            model_id: model_id.clone(),
            api_key: Some(api_key.clone()),
            config: temperature_config.map(|t| serde_json::json!({"temperature": t})),
        };
        let secret = SecretString::new(api_key);

        let n = update_model_api_key_by_provider(&app.duckdb, &provider, &secret)?;
        if n == 0 {
            insert_model(&app.duckdb, &row)?;
            tracing::info!(id = %row.id, "init_flow: 新 provider, insert 用户配置行");
        } else {
            tracing::info!(provider = %provider, updated = n, "init_flow: provider api_key 一致性 update (设计点 23)");
        }

        match ping_model(&row).await {
            Ok(_) => {
                tracing::info!("init_flow: ping 成功, 模型配置完成 (设计点 19)");

                let config_path = crate::startup::Config::default_path();
                if let Ok(mut config) = crate::startup::init::init(&config_path) {
                    config.default_model = Some(row.id.clone());
                    if let Err(e) = config.save(&config_path) {
                        tracing::warn!("init_flow: 保存 default_model 到 config.toml 失败: {e}");
                    } else {
                        tracing::info!(id = %row.id, "init_flow: 已设为默认模型");
                    }
                }

                return Ok(());
            }
            Err(e) => {
                eprintln!("ping 失败: {}. 请重填 (设计点 22 无逃生, 失败必须重填).", e);
                continue;
            }
        }
    }
}

pub fn build_provider_registry(model_row: &ModelRow) -> Result<ProviderRegistry, AgentError> {
    let mut registry = ProviderRegistry::new();
    match model_row.api_type.to_lowercase().as_str() {
        "openai" => registry.register(Arc::new(OpenAiProvider::new())),
        "anthropic" => registry.register(Arc::new(AnthropicProvider::new())),
        "responses" => registry.register(Arc::new(ResponsesProvider::new())),
        other => {
            return Err(AgentError::Llm(format!(
            "build_provider_registry: 未知 api_type '{}' (仅支持 OpenAI / Anthropic / Responses)",
            other
        )))
        }
    }
    Ok(registry)
}

pub async fn ping_model(row: &ModelRow) -> Result<(), AgentError> {
    let api_key = resolve_api_key(row)?;
    let messages = vec![ChatMessage::User {
        text: "ping (cipher init_flow 首启验证, 设计点 19)".to_string(),
    }];
    let req = LlmRequest::from_model_row(row, messages, api_key)?;

    let registry = build_provider_registry(row)?;
    let provider = registry.pick_by_kind(&req.provider_kind).ok_or_else(|| {
        AgentError::Llm(format!(
            "ping_model: 无 provider impl for kind '{}'",
            req.provider_kind
        ))
    })?;
    let resp = provider.call(&req).await?;
    if resp.content.is_empty() {
        return Err(AgentError::Llm(
            "ping 返回空 content (可能 api_key/api_url/model_id 无效)".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires interactive TTY + real LLM network; manual smoke test"]
    async fn init_flow_interactive_smoke() {
        let data_dir =
            std::env::temp_dir().join(format!("cipher-init-flow-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let app = crate::data::bootstrap::bootstrap(&data_dir).expect("bootstrap");
        init_flow(&app, &data_dir).await.expect("init_flow ok");
        assert!(has_configured_model(&app.duckdb).expect("has_configured_model"));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn build_provider_registry_openai() {
        let row = crate::data::ModelRow {
            id: "t".into(),
            name: "T".into(),
            provider: "p".into(),
            api_url: "https://x".into(),
            api_type: "OpenAI".into(),
            api_protocol: "openai-v1".into(),
            model_id: "m".into(),
            api_key: Some("k".into()),
            config: None,
        };
        let r = build_provider_registry(&row).expect("openai registry");
        assert!(r.pick_by_kind("openai").is_some(), "应注册 openai impl");
        assert!(r.pick_by_kind("anthropic").is_none());
    }

    #[test]
    fn build_provider_registry_anthropic() {
        let row = crate::data::ModelRow {
            id: "t".into(),
            name: "T".into(),
            provider: "p".into(),
            api_url: "https://x".into(),
            api_type: "Anthropic".into(),
            api_protocol: "anthropic-messages".into(),
            model_id: "m".into(),
            api_key: Some("k".into()),
            config: None,
        };
        let r = build_provider_registry(&row).expect("anthropic registry");
        assert!(r.pick_by_kind("anthropic").is_some());
    }

    #[test]
    fn build_provider_registry_unknown_api_type_errs() {
        let row = crate::data::ModelRow {
            id: "t".into(),
            name: "T".into(),
            provider: "p".into(),
            api_url: "https://x".into(),
            api_type: "Weird".into(),
            api_protocol: "openai-v1".into(),
            model_id: "m".into(),
            api_key: Some("k".into()),
            config: None,
        };
        assert!(
            build_provider_registry(&row).is_err(),
            "未知 api_type → Err"
        );
    }
}
