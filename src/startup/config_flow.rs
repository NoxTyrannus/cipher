use crate::common::AgentError;
use crate::data::bootstrap::AppState;
use crate::data::duckdb::loader::{
    find_provider_sample, insert_model, load_all_into_memory, rename_agent, set_default_agent,
    update_model_api_key_by_provider, ModelRow,
};
use crate::data::workspace_store::WorkspaceStore;
use crate::startup::cli::WorkspaceCommand;
use dialoguer::{Input, Password, Select};
use secrecy::SecretString;

use super::config::{Config, UnniStyle};

const PRESET_TEMPLATES: &[(&str, &str, &str, &str)] = &[(
    "OpenAI 官方",
    "openai",
    "https://api.openai.com/v1",
    "OpenAI",
)];

pub fn run_workspace_command(
    app: &AppState,
    command: &WorkspaceCommand,
) -> Result<(), AgentError> {
    let store = WorkspaceStore::open(app.paths.storage_root())?;
    match command {
        WorkspaceCommand::List => {
            let ws = store.list()?;
            if ws.is_empty() {
                println!("(无工作区)");
                return Ok(());
            }
            for w in &ws {
                let mark = if w.is_default { "★" } else { " " };
                println!("[{}] {} | {} | {}", mark, w.id, w.name, w.path);
            }
            Ok(())
        }
        WorkspaceCommand::Add { path } => {
            let path = path.to_string_lossy().to_string();
            let p = std::path::Path::new(&path);
            if !p.is_absolute() {
                println!("错误: 工作区路径必须是绝对路径。");
                return Ok(());
            }
            match store.add_from_path(&path) {
                Ok(row) => {
                    // v0.5.0 §6.1：路径不存在允许保存，但需给出提示（非交互 CLI 不做确认流）。
                    if !p.exists() {
                        println!(
                            "提示: 该目录当前不存在，将按确认继续保存（新任务写入时会按需创建目录）。"
                        );
                    }
                    println!("已新增工作区: {} -> {} ({})", row.id, row.name, row.path);
                    Ok(())
                }
                Err(e) => {
                    println!("新增失败: {e}");
                    Ok(())
                }
            }
        }
        WorkspaceCommand::Delete { id } => match store.delete(id) {
            Ok(Some(removed)) => {
                println!("已删除工作区: {}", removed.id);
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => {
                println!("删除失败: {e}");
                Ok(())
            }
        },
        WorkspaceCommand::Use { id } | WorkspaceCommand::SetDefault { id } => {
            match store.set_default(id) {
                Ok(()) => {
                    println!("已设置默认工作区 -> {}", id);
                    Ok(())
                }
                Err(e) => {
                    println!("设置默认失败: {e}");
                    Ok(())
                }
            }
        }
    }
}

pub fn run(app: &AppState) -> Result<(), AgentError> {
    loop {
        let items = vec![
            "新增/管理 model+provider",
            "工作区管理",
            "agent 改名",
            "切默认 workspace/agent",
            "模式设置",
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
            _ => return Ok(()),
        }
    }
}

/// 模式设置（与 TUI 面板「模式设置」分组一致）：
/// UNNI 模式设置（思考输出 开/关）/ KEEP 模式设置（Token/时间预算）。
/// 放弃项 1/2/10：协同节点固定洞察 + mix 机制删除，只沉淀不触发；LOOP 暂无模式项。
fn manage_mode_styles() -> Result<(), AgentError> {
    let config_path = Config::default_path();
    let Some(mut config) = Config::load(&config_path)? else {
        println!("未找到 config.toml, 请先运行 `cipher setup` 初始化。");
        return Ok(());
    };
    loop {
        let items = vec![
            "UNNI 模式设置".to_string(),
            "KEEP 模式设置".to_string(),
            "返回 /config 主菜单".to_string(),
        ];
        let sel = Select::new()
            .with_prompt("模式设置 — 选择模式")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("mode styles select: {e}")))?;
        match sel {
            0 => manage_unni_mode(&mut config, &config_path)?,
            1 => manage_keep_mode(&mut config, &config_path)?,
            _ => return Ok(()),
        }
    }
}

/// UNNI 模式设置分组：思考输出 开/关 二选（只写 `[mode_styles.unni] show_think`，
/// 不暴露全局 `[ui] show_think`；只控制 TUI 渲染，不改 thinking 执行链）。
fn manage_unni_mode(config: &mut Config, config_path: &std::path::Path) -> Result<(), AgentError> {
    loop {
        let current = config.mode_styles.unni.as_ref().and_then(|u| u.show_think);
        let current_display = match current {
            None => "跟随全局".to_string(),
            Some(true) => "开".to_string(),
            Some(false) => "关".to_string(),
        };
        let items = vec![
            format!("思考输出 (当前: {current_display})"),
            "返回模式设置菜单".to_string(),
        ];
        let sel = Select::new()
            .with_prompt("UNNI 模式设置")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("unni mode select: {e}")))?;
        match sel {
            0 => set_unni_show_think(config, config_path)?,
            _ => return Ok(()),
        }
    }
}

/// 思考输出 开/关 二选 → `[mode_styles.unni] show_think = Some(bool)`。
fn set_unni_show_think(
    config: &mut Config,
    config_path: &std::path::Path,
) -> Result<(), AgentError> {
    let current = config.mode_styles.unni.as_ref().and_then(|u| u.show_think);
    let items = ["开（显示思考输出）", "关（隐藏思考输出）"];
    let default = if current == Some(false) { 1 } else { 0 };
    let sel = Select::new()
        .with_prompt("思考输出")
        .items(&items)
        .default(default)
        .interact()
        .map_err(|e| AgentError::Parse(format!("unni show_think select: {e}")))?;
    let show = sel == 0;
    config
        .mode_styles
        .unni
        .get_or_insert_with(UnniStyle::default)
        .show_think = Some(show);
    config.save(config_path)?;
    println!(
        "UNNI 思考输出已设为 {} (config.toml [mode_styles.unni] show_think={show})",
        if show { "开" } else { "关" }
    );
    Ok(())
}

/// KEEP 模式设置分组：Token 预算 / 时间预算（交互逻辑原样搬入分组）。
fn manage_keep_mode(config: &mut Config, config_path: &std::path::Path) -> Result<(), AgentError> {
    loop {
        let keep = config.mode_styles.keep;
        let token_display = if keep.token_budget == 0 {
            "无限".to_string()
        } else {
            format!("{}K", keep.token_budget / 1000)
        };
        let time_display = if keep.time_budget_secs == 0 {
            "无限".to_string()
        } else {
            format!("{}min", keep.time_budget_secs / 60)
        };
        let items = vec![
            format!("Token 预算 (当前: {token_display})"),
            format!("时间预算 (当前: {time_display})"),
            "返回模式设置菜单".to_string(),
        ];
        let sel = Select::new()
            .with_prompt("KEEP 模式设置")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("keep mode select: {e}")))?;
        match sel {
            0 => manage_keep_token(config, config_path)?,
            1 => manage_keep_time(config, config_path)?,
            _ => return Ok(()),
        }
    }
}

fn manage_keep_token(config: &mut Config, config_path: &std::path::Path) -> Result<(), AgentError> {
    let current_display = if config.mode_styles.keep.token_budget == 0 {
        "无限".to_string()
    } else {
        format!("{}K", config.mode_styles.keep.token_budget / 1000)
    };
    let input = Input::<u64>::new()
        .with_prompt(format!(
            "Token 预算 (当前: {current_display}, 0=无限, 单位 千 token, 最小 100)"
        ))
        .default(if config.mode_styles.keep.token_budget == 0 {
            0
        } else {
            config.mode_styles.keep.token_budget / 1000
        })
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("keep token budget: {e}")))?;
    let budget = if input == 0 {
        0
    } else {
        input.saturating_mul(1000).max(100_000)
    };
    if budget == config.mode_styles.keep.token_budget {
        println!("Token 预算未变");
        return Ok(());
    }
    config.mode_styles.keep.token_budget = budget;
    config.save(config_path)?;
    println!(
        "Token 预算已设为 {} (config.toml)",
        if budget == 0 {
            "无限".to_string()
        } else {
            format!("{}K", budget / 1000)
        }
    );
    Ok(())
}

/// KEEP 时间预算（KEEP 模式设置分组内）。
fn manage_keep_time(config: &mut Config, config_path: &std::path::Path) -> Result<(), AgentError> {
    let current_display = if config.mode_styles.keep.time_budget_secs == 0 {
        "无限".to_string()
    } else {
        format!("{}min", config.mode_styles.keep.time_budget_secs / 60)
    };
    let input = Input::<u64>::new()
        .with_prompt(format!(
            "时间预算 (当前: {current_display}, 0=无限, 单位 min, 最小 5)"
        ))
        .default(if config.mode_styles.keep.time_budget_secs == 0 {
            0
        } else {
            config.mode_styles.keep.time_budget_secs / 60
        })
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("keep time budget: {e}")))?;
    let secs = if input == 0 {
        0
    } else {
        input.saturating_mul(60).max(300)
    };
    if secs == config.mode_styles.keep.time_budget_secs {
        println!("时间预算未变");
        return Ok(());
    }
    config.mode_styles.keep.time_budget_secs = secs;
    config.save(config_path)?;
    println!(
        "时间预算已设为 {} (config.toml)",
        if secs == 0 {
            "无限".to_string()
        } else {
            format!("{}min", secs / 60)
        }
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
            .with_prompt("api_type (OpenAI/Responses)")
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
        let items = vec!["新增工作区", "删除工作区", "设置默认工作区", "返回 /config 主菜单"];
        let sel = Select::new()
            .with_prompt("工作区管理")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("ws mgmt select: {}", e)))?;
        match sel {
            0 => add_workspace(app)?,
            1 => delete_workspace(app)?,
            2 => set_default_workspace(app)?,
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
    let path = Input::<String>::new()
        .with_prompt("请输入工作区绝对路径")
        .interact_text()
        .map_err(|e| AgentError::Parse(format!("ws path: {}", e)))?;
    let path = path.trim().to_string();
    let p = std::path::Path::new(&path);
    if !p.is_absolute() {
        println!("路径必须是绝对路径，请重新输入。");
        return Ok(());
    }
    if !p.exists() {
        let items = vec!["继续（允许保存不存在路径）", "重新输入"];
        let sel = Select::new()
            .with_prompt("该目录当前不存在，是否继续？")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| AgentError::Parse(format!("ws not exists confirm: {}", e)))?;
        if sel == 1 {
            return Ok(());
        }
    }
    let store = WorkspaceStore::open(app.paths.storage_root())?;
    match store.add_from_path(&path) {
        Ok(row) => println!("已新增工作区: {} -> {} ({})", row.id, row.name, row.path),
        Err(e) => println!("新增失败: {e}"),
    }
    Ok(())
}

fn delete_workspace(app: &AppState) -> Result<(), AgentError> {
    let store = WorkspaceStore::open(app.paths.storage_root())?;
    let ws = store.list()?;
    if ws.len() <= 1 {
        println!("至少需要保留一个工作区，无法删除。");
        return Ok(());
    }
    let items: Vec<String> = ws
        .iter()
        .map(|w| {
            format!(
                "{} {} ({})",
                if w.is_default { "★" } else { " " },
                w.name,
                w.path
            )
        })
        .collect();
    let sel = Select::new()
        .with_prompt("选择要删除的工作区")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("ws delete select: {}", e)))?;
    let id = ws[sel].id.clone();
    match store.delete(&id) {
        Ok(Some(removed)) => println!("已删除工作区: {}", removed.id),
        Ok(None) => println!("未删除任何工作区。"),
        Err(e) => println!("删除失败: {e}"),
    }
    Ok(())
}

fn set_default_workspace(app: &AppState) -> Result<(), AgentError> {
    let store = WorkspaceStore::open(app.paths.storage_root())?;
    let ws = store.list()?;
    if ws.is_empty() {
        println!("无工作区，请先新增。");
        return Ok(());
    }
    let items: Vec<String> = ws
        .iter()
        .map(|w| format!("{} ({})", w.name, w.path))
        .collect();
    let sel = Select::new()
        .with_prompt("选择要设为默认的工作区")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| AgentError::Parse(format!("ws set default select: {}", e)))?;
    let id = ws[sel].id.clone();
    store.set_default(&id)?;
    println!("已设置默认工作区 -> {}", id);
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
