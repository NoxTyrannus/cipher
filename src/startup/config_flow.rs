use crate::common::AgentError;
use crate::data::bootstrap::AppState;
use crate::data::duckdb::loader::{
    find_provider_sample, insert_model, load_all_into_memory, rename_agent, set_default_agent,
    update_model_api_key_by_provider, ModelRow,
};
use crate::data::workspace_store::{WorkspaceRow, WorkspaceStore};
use dialoguer::{Input, Password, Select};
use secrecy::SecretString;

use super::config::{CollaborationStyle, Config, TriggerNode};

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

pub fn run(app: &AppState) -> Result<(), AgentError> {
    loop {
        let items = vec![
            "新增/管理 model+provider",
            "工作区管理",
            "agent 改名",
            "切默认 workspace/agent",
            "协同模式风格 (Mode Style: 协同方式/协同节点/预算/融合思考)",
            "手动改上下文 (待后续)",
            "退出 /config",
        ];
        let sel = Select::new()
            .with_prompt("/config — 选择管理项")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("/config select: {}", e)))?;
        match sel {
            0 => manage_models_and_providers(app)?,
            1 => manage_workspaces(app)?,
            2 => manage_agents(app)?,
            3 => switch_defaults(app)?,
            4 => manage_mode_styles()?,
            5 => println!("手动改上下文: 运行时上下文编辑, 待后续落地 (非 workspace/agent schema)"),
            _ => return Ok(()),
        }
    }
}

/// 三模式协同风格管理：UNNI 可配置协同方式/协同节点，KEEP 配置预算，LOOP 配置融合思考。
fn manage_mode_styles() -> Result<(), AgentError> {
    let config_path = Config::default_path();
    let Some(mut config) = Config::load(&config_path)? else {
        println!("未找到 config.toml, 请先运行 `cipher setup` 初始化。");
        return Ok(());
    };
    loop {
        let unni = config.mode_styles.unni;
        let keep = config.mode_styles.keep;
        let r#loop = config.mode_styles.r#loop;
        let items = vec![
            format!("UNNI 协同方式 (当前: {})", unni.style.as_str()),
            format!("UNNI 协同节点 (当前: {})", unni.node.as_str()),
            format!("KEEP Token 预算 (当前: {}K)", keep.token_budget / 1000),
            format!("KEEP 时间预算 (当前: {}min)", keep.time_budget_secs / 60),
            format!(
                "LOOP 融合思考 (当前: {})",
                if r#loop.mix_thinking { "开" } else { "关" }
            ),
            "返回 /config 主菜单".to_string(),
        ];
        let sel = Select::new()
            .with_prompt("协同模式风格 — 选择管理项 (KEEP/LOOP 协同方式/节点固定不可设置)")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("mode style select: {e}")))?;
        match sel {
            0 => manage_unni_style(&mut config, &config_path, true)?,
            1 => manage_unni_style(&mut config, &config_path, false)?,
            2 => manage_keep_token(&mut config, &config_path)?,
            3 => manage_keep_time(&mut config, &config_path)?,
            4 => manage_loop_mix(&mut config, &config_path)?,
            _ => return Ok(()),
        }
    }
}

fn manage_unni_style(
    config: &mut Config,
    config_path: &std::path::Path,
    pick_style: bool,
) -> Result<(), AgentError> {
    if pick_style {
        let current = config.mode_styles.unni.style;
        let items = vec![
            CollaborationStyle::Autonomous.label(),
            CollaborationStyle::Follow.label(),
        ];
        let default_index = match current {
            CollaborationStyle::Autonomous => 0,
            CollaborationStyle::Follow => 1,
        };
        let sel = Select::new()
            .with_prompt(format!("UNNI 协同方式 (当前: {})", current.as_str()))
            .items(&items)
            .default(default_index)
            .interact()
            .map_err(|e| AgentError::Parse(format!("unni style select: {e}")))?;
        let next = match sel {
            0 => CollaborationStyle::Autonomous,
            _ => CollaborationStyle::Follow,
        };
        if next == current {
            println!("协同方式未变, 保持 {}", current.as_str());
            return Ok(());
        }
        config.mode_styles.unni.style = next;
        config.save(config_path)?;
        println!("UNNI 协同方式已切换为 {} (config.toml)", next.as_str());
    } else {
        let current = config.mode_styles.unni.node;
        let items = vec![
            TriggerNode::Execution.label(),
            TriggerNode::Insight.label(),
            TriggerNode::Memory.label(),
        ];
        let default_index = match current {
            TriggerNode::Execution => 0,
            TriggerNode::Insight => 1,
            TriggerNode::Memory => 2,
        };
        let sel = Select::new()
            .with_prompt(format!("UNNI 协同节点 (当前: {})", current.as_str()))
            .items(&items)
            .default(default_index)
            .interact()
            .map_err(|e| AgentError::Parse(format!("unni node select: {e}")))?;
        let next = match sel {
            0 => TriggerNode::Execution,
            1 => TriggerNode::Insight,
            _ => TriggerNode::Memory,
        };
        if next == current {
            println!("协同节点未变, 保持 {}", current.as_str());
            return Ok(());
        }
        config.mode_styles.unni.node = next;
        config.save(config_path)?;
        println!("UNNI 协同节点已切换为 {} (config.toml)", next.as_str());
    }
    Ok(())
}

fn manage_keep_token(config: &mut Config, config_path: &std::path::Path) -> Result<(), AgentError> {
    let input = Input::<u64>::new()
        .with_prompt(format!(
            "KEEP Token 预算 (当前: {}, 单位 千 token, 最小 100)",
            config.mode_styles.keep.token_budget / 1000
        ))
        .default(config.mode_styles.keep.token_budget / 1000)
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("keep token budget: {e}")))?;
    let budget = input.saturating_mul(1000).max(100_000);
    if budget == config.mode_styles.keep.token_budget {
        println!("Token 预算未变");
        return Ok(());
    }
    config.mode_styles.keep.token_budget = budget;
    config.save(config_path)?;
    println!("KEEP Token 预算已设为 {}K (config.toml)", budget / 1000);
    Ok(())
}

fn manage_keep_time(config: &mut Config, config_path: &std::path::Path) -> Result<(), AgentError> {
    let input = Input::<u64>::new()
        .with_prompt(format!(
            "KEEP 时间预算 (当前: {}min, 最小 5)",
            config.mode_styles.keep.time_budget_secs / 60
        ))
        .default(config.mode_styles.keep.time_budget_secs / 60)
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("keep time budget: {e}")))?;
    let secs = input.saturating_mul(60).max(300);
    if secs == config.mode_styles.keep.time_budget_secs {
        println!("时间预算未变");
        return Ok(());
    }
    config.mode_styles.keep.time_budget_secs = secs;
    config.save(config_path)?;
    println!("KEEP 时间预算已设为 {}min (config.toml)", secs / 60);
    Ok(())
}

fn manage_loop_mix(config: &mut Config, config_path: &std::path::Path) -> Result<(), AgentError> {
    let current = config.mode_styles.r#loop.mix_thinking;
    let items = vec!["关 (默认, 顺序触发)", "开 (流水线并行 + 拼接合并)"];
    let default_index = if current { 1 } else { 0 };
    let sel = Select::new()
        .with_prompt(format!(
            "LOOP 融合思考 (当前: {})",
            if current { "开" } else { "关" }
        ))
        .items(&items)
        .default(default_index)
        .interact()
        .map_err(|e| AgentError::Parse(format!("loop mix select: {e}")))?;
    let next = sel == 1;
    if next == current {
        println!("融合思考未变");
        return Ok(());
    }
    config.mode_styles.r#loop.mix_thinking = next;
    config.save(config_path)?;
    println!(
        "LOOP 融合思考已切换为 {} (config.toml)",
        if next { "开" } else { "关" }
    );
    Ok(())
}

fn manage_models_and_providers(app: &AppState) -> Result<(), AgentError> {
    loop {
        list_models(app)?;
        let items = vec![
            "新增 model",
            "快速新增 (选已有 provider 带出)",
            "改 provider 的 api_key",
            "切默认模型",
            "返回 /config 主菜单",
        ];
        let sel = Select::new()
            .with_prompt("model+provider 管理")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("model mgmt select: {}", e)))?;
        match sel {
            0 => add_model(app)?,
            1 => quick_add_model(app)?,
            2 => change_provider_key(app)?,
            3 => set_default_model(app)?,
            _ => return Ok(()),
        }
    }
}

fn list_models(app: &AppState) -> Result<(), AgentError> {
    let reg = load_all_into_memory(&app.duckdb)?;
    println!("\n当前 model 表 ({} 行):", reg.models.len());
    for (id, m) in &reg.models {
        let has_key = m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false);
        let mark = if has_key { "✓" } else { "✗" };
        println!(
            "  [{}] {} | provider={} api_type={} model_id={} (api_key {})",
            mark,
            id,
            m.provider,
            m.api_type,
            m.model_id,
            if has_key { "已设" } else { "空" }
        );
    }
    Ok(())
}

fn add_model(app: &AppState) -> Result<(), AgentError> {
    let mut items: Vec<String> = PRESET_TEMPLATES
        .iter()
        .map(|(n, _, _, _)| n.to_string())
        .collect();
    items.push("自定义".to_string());
    let sel = Select::new()
        .with_prompt("选择 provider 模板")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("add select: {}", e)))?;
    let (provider, default_api_url, default_api_type) = if sel < PRESET_TEMPLATES.len() {
        let t = PRESET_TEMPLATES[sel];
        (t.1.to_string(), t.2.to_string(), t.3.to_string())
    } else {
        let p = Input::<String>::new()
            .with_prompt("provider")
            .interact_text()
            .map_err(|e| AgentError::Parse(format!("provider: {}", e)))?;
        let u = Input::<String>::new()
            .with_prompt("api_url")
            .interact_text()
            .map_err(|e| AgentError::Parse(format!("api_url: {}", e)))?;
        let t = Input::<String>::new()
            .with_prompt("api_type (OpenAI/Anthropic)")
            .default("OpenAI".to_string())
            .interact_text()
            .map_err(|e| AgentError::Parse(format!("api_type: {}", e)))?;
        (p, u, t)
    };

    let existing = find_provider_sample(&app.duckdb, &provider)?;
    let (api_url, api_type, api_key) = match existing.as_ref() {
        Some(em) if em.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false) => {
            println!(
                "provider={} 已存在，已自动带出其 api_url/api_key/api_type，只填 name + model_id",
                provider
            );
            (
                em.api_url.clone(),
                em.api_type.clone(),
                em.api_key.clone().unwrap(),
            )
        }
        _ => {
            let api_key = Password::new()
                .with_prompt("API key")
                .interact()
                .map_err(|e| AgentError::Parse(format!("api_key: {}", e)))?;
            (default_api_url, default_api_type, api_key)
        }
    };

    let name = Input::<String>::new()
        .with_prompt("模型显示名")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("name: {}", e)))?;
    let model_id = Input::<String>::new()
        .with_prompt("model_id")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("model_id: {}", e)))?;

    let row = ModelRow {
        id: format!("{}-{}", provider, model_id),
        name,
        provider: provider.clone(),
        api_protocol: crate::data::duckdb::loader::default_api_protocol(&api_type),
        api_url,
        api_type,
        model_id,
        api_key: Some(api_key.clone()),
        config: None,
    };
    insert_model(&app.duckdb, &row)?;
    let secret = SecretString::new(api_key);
    let n = update_model_api_key_by_provider(&app.duckdb, &provider, &secret)?;
    println!(
        "已新增 model 行 {} (provider={} 共 {} 行 key 已同步)",
        row.id, provider, n
    );
    Ok(())
}

fn quick_add_model(app: &AppState) -> Result<(), AgentError> {
    let reg = load_all_into_memory(&app.duckdb)?;

    let mut providers: Vec<String> = reg
        .models
        .values()
        .filter(|m| m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false))
        .map(|m| m.provider.clone())
        .collect();
    providers.sort();
    providers.dedup();
    if providers.is_empty() {
        println!("无已配置 key 的 provider, 先 '新增 model' 配一个 (带出需已有 key)");
        return Ok(());
    }
    let sel = Select::new()
        .with_prompt("选 provider (带出其 api_url/api_key/api_type)")
        .items(&providers)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("quick provider select: {}", e)))?;
    let provider = providers[sel].clone();

    let em = reg
        .models
        .values()
        .find(|m| {
            m.provider == provider && m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false)
        })
        .expect("picked provider should have a keyed sample row")
        .clone();
    let name = Input::<String>::new()
        .with_prompt("模型显示名")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("name: {}", e)))?;
    let model_id = Input::<String>::new()
        .with_prompt("model_id")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("model_id: {}", e)))?;
    let key = em.api_key.clone().unwrap();
    let row = ModelRow {
        id: format!("{}-{}", provider, model_id),
        name,
        provider: provider.clone(),
        api_url: em.api_url.clone(),
        api_protocol: crate::data::duckdb::loader::default_api_protocol(&em.api_type),
        api_type: em.api_type.clone(),
        model_id,
        api_key: Some(key.clone()),
        config: None,
    };
    insert_model(&app.duckdb, &row)?;
    let n = update_model_api_key_by_provider(&app.duckdb, &provider, &SecretString::new(key))?;
    println!(
        "已快速新增 model 行 {} (provider={} 共 {} 行 key 已同步)",
        row.id, provider, n
    );
    Ok(())
}

fn change_provider_key(app: &AppState) -> Result<(), AgentError> {
    let provider = Input::<String>::new()
        .with_prompt("provider (要改 key 的)")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("provider: {}", e)))?;
    let api_key = Password::new()
        .with_prompt("新 API key")
        .interact()
        .map_err(|e| AgentError::Parse(format!("api_key: {}", e)))?;
    let secret = SecretString::new(api_key);
    let n = update_model_api_key_by_provider(&app.duckdb, &provider, &secret)?;
    if n == 0 {
        println!(
            "provider={} 无 model 行 (未更新). 先 '新增 model' 创建.",
            provider
        );
    } else {
        println!("已 update provider={} 的 {} 行 api_key", provider, n);
    }
    Ok(())
}

fn manage_workspaces(app: &AppState) -> Result<(), AgentError> {
    loop {
        list_workspaces(app)?;
        let items = vec!["新增 workspace", "返回 /config 主菜单"];
        let sel = Select::new()
            .with_prompt("workspace 管理")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("ws mgmt select: {}", e)))?;
        match sel {
            0 => add_workspace(app)?,
            _ => return Ok(()),
        }
    }
}

fn list_workspaces(app: &AppState) -> Result<(), AgentError> {
    let ws = WorkspaceStore::open(app.paths.storage_root())?.list()?;
    println!("\nworkspace 表 ({} 行):", ws.len());
    if ws.is_empty() {
        println!("  (空)");
        return Ok(());
    }
    for w in &ws {
        let mark = if w.is_default { "★" } else { " " };
        println!("  [{}] {} | name={} path={}", mark, w.id, w.name, w.path);
    }
    Ok(())
}

fn add_workspace(app: &AppState) -> Result<(), AgentError> {
    let name = Input::<String>::new()
        .with_prompt("workspace 名称")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("ws name: {}", e)))?;
    let path = Input::<String>::new()
        .with_prompt("workspace path (绝对路径)")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("ws path: {}", e)))?;
    let id = name.to_lowercase().replace(' ', "-");
    WorkspaceStore::open(app.paths.storage_root())?.upsert(WorkspaceRow {
        id,
        name,
        path,
        is_default: false,
    })?;
    println!("已新增 workspace (切默认见主菜单族 4)");
    Ok(())
}

fn manage_agents(app: &AppState) -> Result<(), AgentError> {
    let reg = load_all_into_memory(&app.duckdb)?;
    let list: Vec<_> = reg.agents.values().collect();
    if list.is_empty() {
        println!("无 agent 行 (init_flow 首启应已 seed 'Agent')");
        return Ok(());
    }
    let items: Vec<String> = list
        .iter()
        .map(|a| {
            format!(
                "{} (display_name={})",
                a.id,
                a.display_name.as_deref().unwrap_or("(未设)")
            )
        })
        .collect();
    let sel = Select::new()
        .with_prompt("选要改名的 agent")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("agent select: {}", e)))?;
    let target = list[sel];
    let new_name = Input::<String>::new()
        .with_prompt("新 display_name")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("new name: {}", e)))?;
    rename_agent(&app.duckdb, &target.id, &new_name)?;
    println!("已改 {} 的 display_name → {}", target.id, new_name);
    Ok(())
}

fn switch_defaults(app: &AppState) -> Result<(), AgentError> {
    let items = vec!["切默认 workspace", "切默认 agent", "返回 /config 主菜单"];
    let sel = Select::new()
        .with_prompt("切默认")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("switch select: {}", e)))?;
    match sel {
        0 => switch_default_workspace(app)?,
        1 => switch_default_agent(app)?,
        _ => {}
    }
    Ok(())
}

fn switch_default_workspace(app: &AppState) -> Result<(), AgentError> {
    let store = WorkspaceStore::open(app.paths.storage_root())?;
    let ws = store.list()?;
    if ws.is_empty() {
        println!("无 workspace, 先 '工作区管理' 新增");
        return Ok(());
    }
    let items: Vec<String> = ws
        .iter()
        .map(|w| format!("{} ({})", w.name, w.path))
        .collect();
    let sel = Select::new()
        .with_prompt("选默认 workspace")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("ws default select: {}", e)))?;
    let id = ws[sel].id.clone();
    store.set_default(&id)?;
    println!("已切默认 workspace → {}", id);
    Ok(())
}

fn switch_default_agent(app: &AppState) -> Result<(), AgentError> {
    let reg = load_all_into_memory(&app.duckdb)?;
    let list: Vec<_> = reg.agents.values().collect();
    if list.is_empty() {
        println!("无 agent");
        return Ok(());
    }
    let items: Vec<String> = list
        .iter()
        .map(|a| {
            let dn = a.display_name.as_deref().unwrap_or(a.name.as_str());
            format!("{} ({}){}", a.id, dn, if a.is_default { " ★" } else { "" })
        })
        .collect();
    let sel = Select::new()
        .with_prompt("选默认 agent")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("agent default select: {}", e)))?;
    let id = list[sel].id.clone();
    set_default_agent(&app.duckdb, &id)?;
    println!("已切默认 agent → {}", id);
    Ok(())
}

fn set_default_model(app: &AppState) -> Result<(), AgentError> {
    let reg = load_all_into_memory(&app.duckdb)?;
    let models: Vec<_> = reg
        .models
        .values()
        .filter(|m| m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false))
        .collect();
    if models.is_empty() {
        println!("无已配置 api_key 的模型, 先 '新增 model' 配一个");
        return Ok(());
    }
    let items: Vec<String> = models
        .iter()
        .map(|m| {
            format!(
                "{} (provider={}, model_id={})",
                m.id, m.provider, m.model_id
            )
        })
        .collect();
    let sel = Select::new()
        .with_prompt("选默认模型")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("default model select: {}", e)))?;
    let model_id = models[sel].id.clone();

    let config_path = crate::startup::Config::default_path();
    let mut config = crate::startup::init::init(&config_path)?;
    config.default_model = Some(model_id.clone());
    config.save(&config_path)?;
    println!("已切默认模型 → {} (已写入 config.toml)", model_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires interactive TTY; manual smoke test"]
    fn config_flow_interactive_smoke() {
        let data_dir =
            std::env::temp_dir().join(format!("cipher-config-flow-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let app = crate::data::bootstrap::bootstrap(&data_dir).expect("bootstrap");
        run(&app).expect("config_flow ok");
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
