use crate::common::AgentError;
use crate::data::bootstrap::AppState;
use crate::data::duckdb::loader::{
    find_provider_sample, has_configured_model, insert_model, update_model_api_key_by_provider,
};
use crate::data::workspace_store::{WorkspaceRow, WorkspaceStore};
use crate::data::ModelRow;
use crate::logic::model::api_key::resolve_api_key;
use crate::logic::model::message::ChatMessage;
use crate::logic::model::openai::OpenAiProvider;
use crate::logic::model::provider::LlmRequest;
use crate::logic::model::registry::ProviderRegistry;
use crate::logic::model::responses::ResponsesProvider;
use secrecy::SecretString;
use std::path::Path;
use std::sync::Arc;

/// A3（v0.5.4）：provider 环提示语（用户拍板精确文案）。
pub const PROVIDER_PROMPT: &str =
    "您的供应商名称，如：Deepseek / GLM / Kimi / Minimax / Siliconflow，或其他自定义名称";

/// A4（v0.5.4）：api_type 枚举二选（显示名, 内部值）。恰好两项，方向键 + Enter。
pub const API_TYPE_SELECTIONS: &[(&str, &str)] = &[
    ("OpenAI-Chatcompletion", "OpenAI"),
    ("OpenAI-Responses", "Responses"),
];

/// A4 纯函数：Select 选中索引 → 内部 api_type 值；越界为 None。
pub fn api_type_selection_internal(index: usize) -> Option<&'static str> {
    API_TYPE_SELECTIONS
        .get(index)
        .map(|(_, internal)| *internal)
}

/// A6 纯函数：API key 掩码摘要。
/// 长度 ≥12：`已接收：{前3}****{后4}（{N} 字符）`；<12：`已接收：****（{N} 字符）`。
pub fn mask_api_key_summary(api_key: &str) -> String {
    let n = api_key.chars().count();
    if n >= 12 {
        let first: String = api_key.chars().take(3).collect();
        let last: String = api_key.chars().skip(n - 4).collect();
        format!("已接收：{first}****{last}（{n} 字符）")
    } else {
        format!("已接收：****（{n} 字符）")
    }
}

/// A2（v0.5.4）：首屏文案（重写；不含 TUI 快捷键块——快捷键只留在已配置屏与收尾 n 分支）。
/// 拍板文案逐字对齐：`⑤ API_key` 行尾分号后接"每步都有说明"为同段延续（；分隔，不换行），
/// ①~⑤ 行内对齐空格保留。
pub fn first_run_banner(version: &str) -> String {
    let steps_line = concat!(
        "接下来 5 步完成模型配置：\n",
        "  ① provider   ② API_url   ③ api_type   ④ model_id   ⑤ API_key",
        "；每步都有说明；配置失败会请你重填；随时 Ctrl+C 退出。",
    );
    format!("cipher v{version} — 终端原生 AI 代理\n首次配置\n\n{steps_line}")
}

/// A7（v0.5.4）：收尾 n 分支（或 exec 失败 fallback）打印的启动提示块。
pub const POST_CONFIG_HINT: &str = concat!(
    "配置完成。随时启动：\n",
    "  cipher         进入 TUI\n",
    "  cipher config  管理配置\n",
    "启动后：Tab/Shift+Tab 切换模式 · /config 配置管理 · /exit 退出",
);

/// A7 纯函数：从原 argv 中挑出显式出现的全局参数（`--config <path>` /
/// `--data-dir <path>`，含 `=` 形式），供 exec 自身重启时透传——
/// 否则 `cipher setup --data-dir X` 重启后数据目录错位。子命令与其余参数不透传
/// （重启目标是默认 run/TUI）。
pub fn restart_global_args(argv: &[String]) -> Vec<String> {
    const GLOBAL_FLAGS: &[&str] = &["config", "data-dir"];
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < argv.len() {
        let arg = &argv[i];
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline_value) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (rest, None),
            };
            if GLOBAL_FLAGS.contains(&name) {
                out.push(arg.clone());
                if inline_value.is_none() {
                    if let Some(value) = argv.get(i + 1) {
                        out.push(value.clone());
                        i += 1;
                    }
                }
            }
        }
        i += 1;
    }
    out
}

pub async fn init_flow(app: &AppState, data_dir: &Path) -> Result<(), AgentError> {
    if has_configured_model(&app.duckdb)? {
        tracing::info!("init_flow: 已有已配置 model, 跳过首启引导");
    } else {
        tracing::info!("init_flow: 首启, 进入交互引导");
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
        tracing::info!(path = workspace, "seed default workspace");
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
        tracing::info!(name = "Agent", "seed default agent");
    }
    Ok(())
}

/// A1（v0.5.4）：砍掉模板环（`PRESET_TEMPLATES` 与 provider Select 已删除）。
/// 向导固定五步顺序：provider → api_url → api_type → model_id → API key。
/// ping 失败重填循环与 temperature 启发式保留不动。
async fn prompt_and_configure_model(app: &AppState, _data_dir: &Path) -> Result<(), AgentError> {
    use dialoguer::{Input, Select};

    loop {
        // ① provider：手填、trim、大小写原样保留；重名检测提示同步 api_key。
        let input = Input::<String>::new()
            .with_prompt(PROVIDER_PROMPT)
            .interact_text()
            .map_err(|e| AgentError::Parse(format!("provider input: {}", e)))?;
        let provider = input.trim().to_string();
        if find_provider_sample(&app.duckdb, &provider)?.is_some() {
            println!("provider 已存在，将同步更新其 api_key");
        }

        // ② api_url
        let api_url = Input::<String>::new()
            .with_prompt("api_url (完整 base URL)")
            .interact_text()
            .map_err(|e| AgentError::Parse(format!("api_url input: {}", e)))?;

        // ③ api_type：枚举二选（方向键 + Enter），内部值映射。
        let api_type_items: Vec<&str> = API_TYPE_SELECTIONS
            .iter()
            .map(|(display, _)| *display)
            .collect();
        let sel = Select::new()
            .with_prompt("api_type")
            .items(&api_type_items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("api_type select: {}", e)))?;
        let api_type = api_type_selection_internal(sel)
            .expect("Select 项数与 API_TYPE_SELECTIONS 一致")
            .to_string();

        // ④ model_id（A5：显示名环删除，name = model_id）
        let model_id = Input::<String>::new()
            .with_prompt("model_id (如 ep-xxx / gpt-4o)")
            .interact_text()
            .map_err(|e| AgentError::Parse(format!("model_id input: {}", e)))?;

        // ⑤ API key：Password 不回显 + 掩码摘要确认（回车确认 / r 重填）。
        let api_key = prompt_api_key_confirmed()?;

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
            name: model_id.clone(),
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
            tracing::info!(provider = %provider, updated = n, "init_flow: provider api_key 同步完成");
        }

        match ping_model(&row).await {
            Ok(_) => {
                tracing::info!("init_flow: ping 成功, 模型配置完成");

                let config_path = crate::startup::Config::default_path();
                if let Ok(mut config) = crate::startup::init::init(&config_path) {
                    config.default_model = Some(row.id.clone());
                    if let Err(e) = config.save(&config_path) {
                        tracing::warn!("init_flow: 保存 default_model 到 config.toml 失败: {e}");
                    } else {
                        tracing::info!(id = %row.id, "init_flow: 已设为默认模型");
                    }
                }

                finish_after_ping_success()?;
                return Ok(());
            }
            Err(e) => {
                eprintln!("ping 失败: {}。请检查配置后重填。", e);
                continue;
            }
        }
    }
}

/// A6 确认判定纯函数（拍板字面语义）：空输入/回车 = 确认；`r`/`R` = 重填；
/// 其余输入无效（返回 None，调用方重新提示）。
pub fn api_key_confirm_decision(input: &str) -> Option<bool> {
    match input.trim() {
        "" => Some(true),
        "r" | "R" => Some(false),
        _ => None,
    }
}

/// A6：API key 环。Password 不回显；提交后打印掩码摘要并请求确认——
/// 字面语义"回车确认 / r 重填"：Input 接收，空输入/回车=确认，r/R=重填（回到
/// Password 重输），其余无效输入重新提示；空 key 校验保留。
fn prompt_api_key_confirmed() -> Result<String, AgentError> {
    use dialoguer::{Input, Password};
    loop {
        let api_key = Password::new()
            .with_prompt("API key")
            .interact()
            .map_err(|k| AgentError::Parse(format!("api_key input: {}", k)))?;
        if api_key.trim().is_empty() {
            eprintln!("API key 不能为空，请重填。");
            continue;
        }
        println!("{}", mask_api_key_summary(&api_key));
        let confirmed = loop {
            let input = Input::<String>::new()
                .with_prompt("回车确认 / r 重填")
                .allow_empty(true)
                .interact_text()
                .map_err(|e| AgentError::Parse(format!("api_key confirm: {e}")))?;
            match api_key_confirm_decision(&input) {
                Some(decision) => break decision,
                None => eprintln!("无效输入：回车确认，或输入 r 重填。"),
            }
        };
        if confirmed {
            return Ok(api_key);
        }
    }
}

/// A7：收尾 y/n。ping 成功、写入 default_model 后提问 `是否立即启动 cipher?`。
/// 是 → exec 自身重启进 TUI（透传原 argv 显式全局参数，见 restart_global_args）；
/// 否（或 exec 失败）→ 打印启动提示块后正常退出（setup 余下种子步骤照常收尾，
/// 重启后的 `cipher` run 路径会重新执行幂等种子导入）。
fn finish_after_ping_success() -> Result<(), AgentError> {
    use dialoguer::Confirm;
    let launch = Confirm::new()
        .with_prompt("是否立即启动 cipher?")
        .default(true)
        .interact()
        .map_err(|e| AgentError::Parse(format!("launch confirm: {e}")))?;
    if launch {
        match exec_into_tui() {
            Ok(()) => Ok(()), // exec 成功即替换进程，不会走到这里
            Err(e) => {
                tracing::warn!("init_flow: exec 自身重启失败: {e}");
                println!("{POST_CONFIG_HINT}");
                Ok(())
            }
        }
    } else {
        println!("{POST_CONFIG_HINT}");
        Ok(())
    }
}

/// A7：exec 自身重启进 TUI（默认 run 子命令），仅透传 `--config` / `--data-dir`。
/// `exec` 成功即替换进程不返回；失败时返回 io::Error 供调用方 fallback。
#[cfg(unix)]
fn exec_into_tui() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut cmd = std::process::Command::new(exe);
    cmd.args(restart_global_args(&argv));
    Err(cmd.exec())
}

#[cfg(not(unix))]
fn exec_into_tui() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exec 自身重启仅支持 Unix 平台",
    ))
}

pub fn build_provider_registry(model_row: &ModelRow) -> Result<ProviderRegistry, AgentError> {
    let mut registry = ProviderRegistry::new();
    match model_row.api_type.to_lowercase().as_str() {
        "openai" => registry.register(Arc::new(OpenAiProvider::new())),
        "responses" => registry.register(Arc::new(ResponsesProvider::new())),
        "anthropic" => {
            return Err(AgentError::Llm(
                "Anthropic API 支持已移除（只保留 Chat Completions + Responses 两种接入）；\
                 请重新配置模型（api_type=OpenAI/Responses）"
                    .to_string(),
            ))
        }
        other => {
            return Err(AgentError::Llm(format!(
                "build_provider_registry: 未知 api_type '{}' (仅支持 OpenAI / Responses)",
                other
            )))
        }
    }
    Ok(registry)
}

pub async fn ping_model(row: &ModelRow) -> Result<(), AgentError> {
    let api_key = resolve_api_key(row)?;
    let messages = vec![ChatMessage::User {
        text: "ping (cipher 首启验证)".to_string(),
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

    #[test]
    fn provider_prompt_is_exact_approved_text() {
        // A3：用户拍板的 provider 提示语精确文本。
        assert_eq!(
            PROVIDER_PROMPT,
            "您的供应商名称，如：Deepseek / GLM / Kimi / Minimax / Siliconflow，或其他自定义名称"
        );
    }

    #[test]
    fn api_type_selections_map_display_to_internal() {
        // A4：恰好两项；显示名 → 内部值映射。
        assert_eq!(API_TYPE_SELECTIONS.len(), 2);
        assert_eq!(API_TYPE_SELECTIONS[0].0, "OpenAI-Chatcompletion");
        assert_eq!(api_type_selection_internal(0), Some("OpenAI"));
        assert_eq!(API_TYPE_SELECTIONS[1].0, "OpenAI-Responses");
        assert_eq!(api_type_selection_internal(1), Some("Responses"));
        assert_eq!(api_type_selection_internal(2), None, "越界 → None");
    }

    #[test]
    fn mask_api_key_summary_long_key_shows_head_and_tail() {
        // A6：≥12 位 → 前3****后4（N 字符）。
        let key = "sk-abcdef1234567890"; // 19 字符
        assert_eq!(key.chars().count(), 19);
        assert_eq!(mask_api_key_summary(key), "已接收：sk-****7890（19 字符）");
    }

    #[test]
    fn mask_api_key_summary_short_key_is_fully_masked() {
        // A6：<12 位 → ****（N 字符）。
        assert_eq!(mask_api_key_summary("abc45678"), "已接收：****（8 字符）");
        let boundary12 = "abcdefghijkl"; // 恰 12 位 → 长档
        assert_eq!(
            mask_api_key_summary(boundary12),
            "已接收：abc****ijkl（12 字符）"
        );
    }

    #[test]
    fn first_run_banner_has_five_steps_and_no_tui_shortcuts() {
        // A2：首屏文案重写——五步引导；TUI 快捷键块必须从首屏删除。
        let banner = first_run_banner("0.5.4");
        assert!(banner.starts_with("cipher v0.5.4 — 终端原生 AI 代理\n首次配置\n"));
        // 拍板文案逐字对齐：`⑤ API_key` 行尾分号后接"每步都有说明"为同段延续（不换行），
        // ①~⑤ 行内对齐空格保留。
        assert!(banner.contains(
            "接下来 5 步完成模型配置：\n  ① provider   ② API_url   ③ api_type   ④ model_id   ⑤ API_key；每步都有说明；配置失败会请你重填；随时 Ctrl+C 退出。"
        ));
        assert!(
            !banner.contains("进入 TUI 后") && !banner.contains("/exit"),
            "首屏不得再含 TUI 快捷键块: {banner}"
        );
    }

    #[test]
    fn api_key_confirm_decision_follows_approved_literal_semantics() {
        // A6：拍板字面语义——空输入/回车=确认，r/R=重填，其余无效（重新提示）。
        assert_eq!(api_key_confirm_decision(""), Some(true), "空输入 = 确认");
        assert_eq!(api_key_confirm_decision("   "), Some(true), "纯空白 = 确认");
        assert_eq!(api_key_confirm_decision("r"), Some(false), "r = 重填");
        assert_eq!(api_key_confirm_decision("R"), Some(false), "R = 重填");
        assert_eq!(api_key_confirm_decision("y"), None, "其余输入无效");
        assert_eq!(api_key_confirm_decision("rr"), None, "其余输入无效");
    }

    #[test]
    fn post_config_hint_lists_start_commands_and_shortcuts() {
        // A7：n 分支提示块。
        assert!(POST_CONFIG_HINT.starts_with("配置完成。随时启动："));
        assert!(POST_CONFIG_HINT.contains("  cipher         进入 TUI"));
        assert!(POST_CONFIG_HINT.contains("  cipher config  管理配置"));
        assert!(POST_CONFIG_HINT
            .contains("启动后：Tab/Shift+Tab 切换模式 · /config 配置管理 · /exit 退出"));
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn restart_global_args_keeps_space_and_inline_forms() {
        // A7：`--config <path>` / `--data-dir <path>` / `=` 形式均透传。
        assert_eq!(
            restart_global_args(&argv(&["--data-dir", "/x/data", "setup"])),
            argv(&["--data-dir", "/x/data"])
        );
        assert_eq!(
            restart_global_args(&argv(&["setup", "--config=/x/c.toml"])),
            argv(&["--config=/x/c.toml"])
        );
        assert_eq!(
            restart_global_args(&argv(&["--config", "/a", "--data-dir", "/b"])),
            argv(&["--config", "/a", "--data-dir", "/b"])
        );
    }

    #[test]
    fn restart_global_args_drops_subcommand_and_unknown_flags() {
        assert_eq!(restart_global_args(&argv(&["setup"])), Vec::<String>::new());
        assert_eq!(restart_global_args(&[]), Vec::<String>::new());
        assert_eq!(
            restart_global_args(&argv(&["--verbose", "run", "--config", "/c"])),
            argv(&["--config", "/c"]),
            "未知全局参数与子命令不透传"
        );
    }

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
    fn build_provider_registry_rejects_anthropic() {
        // TE：Anthropic API 支持已移除（只保留 Chat Completions + Responses），api_type 校验拒绝。
        let row = crate::data::ModelRow {
            id: "t".into(),
            name: "T".into(),
            provider: "p".into(),
            api_url: "https://x".into(),
            api_type: "Anthropic".into(),
            api_protocol: "openai-v1".into(),
            model_id: "m".into(),
            api_key: Some("k".into()),
            config: None,
        };
        assert!(
            build_provider_registry(&row).is_err(),
            "Anthropic api_type → Err"
        );
    }

    #[test]
    fn build_provider_registry_responses() {
        let row = crate::data::ModelRow {
            id: "t".into(),
            name: "T".into(),
            provider: "p".into(),
            api_url: "https://x".into(),
            api_type: "Responses".into(),
            api_protocol: "openai-v1".into(),
            model_id: "m".into(),
            api_key: Some("k".into()),
            config: None,
        };
        let r = build_provider_registry(&row).expect("responses registry");
        assert!(r.pick_by_kind("responses").is_some());
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
