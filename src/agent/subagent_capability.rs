//! v0.3.1 执行中台 subagent 体系 —— TA 能力域：六个 `subagent.*` 分子能力 + `usage_method.observe`。
//!
//! # 职责
//! - 持久生命周期状态机（任务书 §4.3）：`created -> idle ⇄ running`；
//!   `running -> failed/idle`；`idle/failed -> sleeping`（仅 wake 回 idle）；
//!   `idle/sleeping/failed -> tombstoned`（终态，幂等）。
//! - 状态存 agent 表：`mode='subagent'` 实例行，`config.subagent.*` 承载生命周期/启动/预算。
//! - 全局 invocation 事实日志（§4.5）：`<storage_root>/invocations/<id>.json`（不可变首文件）
//!   + `<id>.result.json`（执行后闭合终态）。所有副作用前 `invocation.prewrite`，预写失败禁止副作用。
//! - `usage_method.observe`（§4.4）：只写 usage_method 行，不改 base/composite 稳定契约；
//!   校验 capability_id 必须出现在最近一次能力调用日志中。
//!
//! # 对外接口（TB runtime / TC 接线用）
//! - `execute_subagent_capability`：CapabilityExecutor::execute_builtin 的分发入口；
//! - `set_subagent_lifecycle`：runtime 完成/失败后更新持久生命周期（running -> idle/failed）；
//! - `subagent_definition`：按 id 重新读取冻结定义快照（崩溃恢复可复用）。
//!
//! 本模块不实现定时/条件调度器（v0.3.1 只做字段/模板/范例/状态链）。

use crate::agent::execution_types::{
    SubagentBudget, SubagentDefinition, SubagentLifecycle, SubagentLifecycleKind, SubagentStartup,
};
use crate::common::{AgentError, Result};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use crate::logic::capability::executor::{SubagentSpawnEvent, SubagentSpawnHook};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// 所有 subagent.* 分子 id 的判定前缀。
pub const SUBAGENT_MOLECULE_PREFIX: &str = "subagent.";
/// `usage_method.observe` 分子 id。
pub const USAGE_METHOD_OBSERVE_ID: &str = "usage_method.observe";
pub const METHOD_INVOKE_ID: &str = "method.invoke";

const DEFAULT_MEMORY_WINDOW_PCT: u8 = 80;
const DEFAULT_BRIEFING: bool = true;
const DEFAULT_MAX_RETRIES: u32 = 0;
const DEFAULT_ATTEMPT_TIMEOUT_SECONDS: u64 = 600;
const DEFAULT_TOTAL_TIMEOUT_SECONDS: u64 = 3600;
const MAX_ARGUMENTS_CHARS: usize = 2000;

/// 持久生命周期状态的稳定字符串（§4.3 六态）。
pub const LIFECYCLE_CREATED: &str = "created";
pub const LIFECYCLE_IDLE: &str = "idle";
pub const LIFECYCLE_RUNNING: &str = "running";
pub const LIFECYCLE_FAILED: &str = "failed";
pub const LIFECYCLE_SLEEPING: &str = "sleeping";
pub const LIFECYCLE_TOMBSTONED: &str = "tombstoned";

/// 模块级单次标记：进程内首次访问 invocations 目录时，把本进程之前遗留的未闭合
/// invocation 标记为 `process_unexpected_exit`（写入 `<id>.result.json`），保持首文件不可变。
static UNCLOSED_MARKED_ROOTS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn unclosed_marked_roots() -> &'static Mutex<HashSet<PathBuf>> {
    UNCLOSED_MARKED_ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// subagent 实例行的 `config.subagent` 配置（§5.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentConfig {
    /// 来源模板 id。
    pub template_id: String,
    /// 持久生命周期状态（created/idle/running/failed/sleeping/tombstoned）。
    ///
    /// 注意：T0 冻结的 `SubagentLifecycle` 枚举缺少 §7.4 状态集中的 `sleeping` 变体
    /// （冻结注释已描述 sleeping 语义但无对应变体），故此处用字符串持久化六态；
    /// 冻结枚举仅用于 TB runtime 收口接口（running -> idle/failed）。
    pub lifecycle: String,
    /// 生命周期种类（temporary/resident，来自模板或 create 参数）。
    pub lifecycle_kind: SubagentLifecycleKind,
    /// 启动方式。
    pub startup: SubagentStartup,
    /// 触发配置（scheduled/condition 范例，v0.3.1 不调度）。
    #[serde(default)]
    pub trigger: Option<serde_json::Value>,
    /// 服务层从模型注册表分配的 model id。
    pub model_id: String,
    /// 运行预算。
    pub budget: SubagentBudget,
    /// 记忆窗口百分比（模型上下文窗口 * pct / 100）。
    #[serde(default = "default_memory_window_pct")]
    pub memory_window_pct: u8,
    /// 是否生成简报（done.summary -> last_output）。
    #[serde(default = "default_briefing")]
    pub briefing: bool,
    /// 软删除时间（非 None 表示 tombstoned）。
    #[serde(default)]
    pub tombstoned_at: Option<String>,
    /// 创建时携带的初始 task_input；subagent.run 未提供时回退使用。
    #[serde(default)]
    pub task_input: Option<String>,
}

/// subagent 模板行的 `config.subagent` 配置（§5.1）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateConfig {
    /// 模板生命周期种类（temporary/resident）。
    #[serde(rename = "lifecycle")]
    pub lifecycle: SubagentLifecycleKind,
    /// 启动方式。
    pub startup: SubagentStartup,
    /// 触发配置。
    #[serde(default)]
    pub trigger: Option<serde_json::Value>,
    /// 记忆窗口百分比。
    #[serde(default = "default_memory_window_pct")]
    pub memory_window_pct: u8,
    /// 是否生成简报。
    #[serde(default = "default_briefing")]
    pub briefing: bool,
    /// 有限重试次数。
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// 单次 attempt 超时（秒）。
    #[serde(default = "default_attempt_timeout_seconds")]
    pub attempt_timeout_seconds: u64,
    /// 整个 run 总超时（秒）。
    #[serde(default = "default_total_timeout_seconds")]
    pub total_timeout_seconds: u64,
    /// 软删除时间（模板被归档时）。
    #[serde(default)]
    pub tombstoned_at: Option<String>,
}

fn default_memory_window_pct() -> u8 {
    DEFAULT_MEMORY_WINDOW_PCT
}
fn default_briefing() -> bool {
    DEFAULT_BRIEFING
}
fn default_max_retries() -> u32 {
    DEFAULT_MAX_RETRIES
}
fn default_attempt_timeout_seconds() -> u64 {
    DEFAULT_ATTEMPT_TIMEOUT_SECONDS
}
fn default_total_timeout_seconds() -> u64 {
    DEFAULT_TOTAL_TIMEOUT_SECONDS
}

/// agent 表行（子集视图，覆盖 subagent 分子所需字段）。
#[derive(Debug, Clone)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub prompt: String,
    pub capability_allowlist: Vec<String>,
    pub config: serde_json::Value,
}

impl AgentRow {
    /// 解析实例行 `config.subagent`。
    pub fn subagent_config(&self) -> Result<SubagentConfig> {
        let block = self.config.get("subagent").ok_or_else(|| {
            AgentError::Bootstrap(format!(
                "subagent '{}' config missing 'subagent' block",
                self.id
            ))
        })?;
        serde_json::from_value(block.clone()).map_err(|error| {
            AgentError::Bootstrap(format!(
                "subagent '{id}' config invalid: {error}",
                id = self.id
            ))
        })
    }

    /// 解析模板行 `config.subagent`。
    fn template_config(&self) -> Result<TemplateConfig> {
        let block = self.config.get("subagent").ok_or_else(|| {
            AgentError::Bootstrap(format!(
                "subagent template '{}' config missing 'subagent' block",
                self.id
            ))
        })?;
        serde_json::from_value(block.clone()).map_err(|error| {
            AgentError::Bootstrap(format!(
                "subagent template '{}' config invalid: {error}",
                self.id
            ))
        })
    }
}

/// 分子执行中的内部错误分类：
/// - `Rejected`：前置状态不满足 / 参数不合法 / 授权失败（终态 rejected）；
/// - `Failed`：能力内部错误（io / db / 完整性）（终态 failed）。
#[derive(Debug)]
pub enum SubagentError {
    Rejected {
        invocation_id: Option<String>,
        message: String,
    },
    Failed {
        invocation_id: Option<String>,
        message: String,
    },
}

impl SubagentError {
    pub fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected {
            invocation_id: None,
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            invocation_id: None,
            message: message.into(),
        }
    }

    /// 把 `AgentError` 包装为内部失败（用于 `.map_err`）。
    pub fn from_agent(error: AgentError) -> Self {
        Self::Failed {
            invocation_id: None,
            message: error.to_string(),
        }
    }

    fn with_invocation(self, invocation_id: String) -> Self {
        match self {
            Self::Rejected { message, .. } => Self::Rejected {
                invocation_id: Some(invocation_id),
                message,
            },
            Self::Failed { message, .. } => Self::Failed {
                invocation_id: Some(invocation_id),
                message,
            },
        }
    }

    fn invocation_id(&self) -> Option<&str> {
        match self {
            Self::Rejected { invocation_id, .. } | Self::Failed { invocation_id, .. } => {
                invocation_id.as_deref()
            }
        }
    }

    fn final_state(&self) -> &'static str {
        match self {
            Self::Rejected { .. } => "rejected",
            Self::Failed { .. } => "failed",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Rejected { message, .. } | Self::Failed { message, .. } => message,
        }
    }

    fn into_agent_error(self) -> AgentError {
        match self {
            Self::Rejected { message, .. } => AgentError::Parse(message),
            Self::Failed { message, .. } => AgentError::Script(message),
        }
    }
}

/// 分子执行的成功结果（dispatcher 用于闭合 invocation 并返回输出）。
struct MoleculeOutcome {
    invocation_id: String,
    final_state: &'static str,
    output: serde_json::Value,
}

/// 修法 2（任务书 §3.3）：分子内部短锁辅助 —— 分子自行获取 duckdb 锁，
/// 调用方（CapabilityExecutor）不再持锁作用域内调用分子，避免同锁重入死锁。
fn lock_molecule_duckdb<'a>(
    duckdb: &'a Arc<Mutex<duckdb::Connection>>,
    name: &str,
) -> std::result::Result<MutexGuard<'a, duckdb::Connection>, SubagentError> {
    duckdb
        .lock()
        .map_err(|e| SubagentError::failed(format!("{name}: duckdb lock poisoned: {e}")))
}

/// 分发入口：由 CapabilityExecutor::execute_builtin 调用。
///
/// 每个分子在副作用前先 `invocation.prewrite`；预写失败直接报错且不产生副作用。
/// 执行完成后（成功或失败）都会闭合 invocation 终态（`<id>.result.json`）。
pub fn execute_subagent_capability(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    capability_id: &str,
    args: &serde_json::Value,
    spawn_hook: Option<&dyn SubagentSpawnHook>,
) -> Result<serde_json::Value> {
    let outcome = match capability_id {
        "subagent.create" => subagent_create(duckdb, storage_root, args, spawn_hook),
        "subagent.update" => subagent_update(duckdb, storage_root, args),
        "subagent.run" => subagent_run(duckdb, storage_root, args, spawn_hook),
        "subagent.sleep" => subagent_sleep(duckdb, storage_root, args),
        "subagent.wake" => subagent_wake(duckdb, storage_root, args),
        "subagent.delete" => subagent_delete(duckdb, storage_root, args),
        USAGE_METHOD_OBSERVE_ID => usage_observe(duckdb, storage_root, args),
        METHOD_INVOKE_ID => method_invoke(duckdb, storage_root, args, spawn_hook),
        other => Err(SubagentError::rejected(format!(
            "unknown subagent molecule: {other}"
        ))),
    };

    match outcome {
        Ok(result) => {
            append_invocation_result(
                storage_root,
                &result.invocation_id,
                result.final_state,
                None,
            )?;
            Ok(result.output)
        }
        Err(error) => {
            if let Some(invocation_id) = error.invocation_id() {
                // 闭合已预写的 invocation（失败/拒绝终态）；闭合失败只告警，不掩盖原始错误。
                let _ = append_invocation_result(
                    storage_root,
                    invocation_id,
                    error.final_state(),
                    Some(error.message()),
                );
            }
            Err(error.into_agent_error())
        }
    }
}

// ---------------------------------------------------------------------------
// 六个 subagent.* 分子 + usage_method.observe
// ---------------------------------------------------------------------------

fn subagent_create(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    args: &serde_json::Value,
    spawn_hook: Option<&dyn SubagentSpawnHook>,
) -> std::result::Result<MoleculeOutcome, SubagentError> {
    // 修法 2（任务书 §3.3）：分子内部短锁，不在 executor 持锁作用域内调用分子。
    let guard = lock_molecule_duckdb(duckdb, "subagent.create")?;
    let conn: &duckdb::Connection = &guard;
    let template_id = required_str(args, "template_id")?;

    // 1) resolve 模板：mode='subagent_template'，未启用/tombstoned 拒绝。
    let template = resolve_template(conn, template_id)?;
    let template_config = template
        .template_config()
        .map_err(SubagentError::from_agent)?;

    // 2) 校验 allowlist ⊆ 模板 allowlist、model_id 存在于 model 表。
    let allowlist = chosen_allowlist(args, &template.capability_allowlist)?;
    let model_id = required_str(args, "model_id")?;
    ensure_model_exists(conn, model_id)?;
    let lifecycle_kind =
        parse_optional_lifecycle_kind(args.get("lifecycle"))?.unwrap_or(template_config.lifecycle);
    let startup = parse_optional_startup(args.get("startup"))?.unwrap_or(template_config.startup);
    let trigger = args.get("trigger").cloned();
    let budget = budget_from_args(args, &template_config)?;
    let name = args
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| template.name.clone());

    // 3) invocation.prewrite（任何副作用之前；失败禁止副作用）。
    let invocation_id =
        prewrite_invocation(conn, storage_root, "subagent.create", args).map_err(|error| {
            SubagentError::failed(format!("subagent.create prewrite failed: {error}"))
        })?;

    // 4) 生成 sg_<uuid> 实例行（prompt 缺省继承模板基线，可被任务专属 prompt 覆盖）。
    let subagent_id = format!("sg_{}", uuid_simple());
    let instance_prompt = parse_optional_prompt(args)?.unwrap_or_else(|| template.prompt.clone());
    let task_input = args
        .get("task_input")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let instance_config = SubagentConfig {
        template_id: template_id.to_string(),
        lifecycle: LIFECYCLE_IDLE.to_string(),
        lifecycle_kind,
        startup,
        trigger,
        model_id: model_id.to_string(),
        budget: budget.clone(),
        memory_window_pct: template_config.memory_window_pct,
        briefing: template_config.briefing,
        tombstoned_at: None,
        task_input: task_input.clone(),
    };
    let row = AgentRow {
        id: subagent_id.clone(),
        name,
        mode: "subagent".to_string(),
        prompt: instance_prompt,
        capability_allowlist: allowlist.clone(),
        config: serde_json::json!({ "subagent": instance_config }),
    };
    persist_new_agent_row(conn, &row).map_err(|error| {
        SubagentError::failed(format!("subagent.create persist instance: {error}"))
            .with_invocation(invocation_id.clone())
    })?;

    // 5) 初始化 <storage_root>/subagents/<id>/memory.json 与 last_output.json。
    init_subagent_files(storage_root, &subagent_id).map_err(|error| {
        SubagentError::failed(format!("subagent.create init files: {error}"))
            .with_invocation(invocation_id.clone())
    })?;

    // 6) 通知 AgentPool/TC 已创建（hook 未安装时分子仍完成持久化并正常返回）。
    if let Some(hook) = spawn_hook {
        hook.notify(SubagentSpawnEvent::Created {
            subagent_id: subagent_id.clone(),
        });
    }

    Ok(MoleculeOutcome {
        output: serde_json::json!({
            "subagent_id": subagent_id,
            "lifecycle": "idle",
            "capability_allowlist": allowlist,
            "model_id": model_id,
            "task_input": task_input.clone(),
            "memory_ref": format!("subagents/{subagent_id}/memory.json"),
            "last_output_ref": format!("subagents/{subagent_id}/last_output.json"),
        }),
        final_state: "completed",
        invocation_id,
    })
}

fn subagent_update(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    args: &serde_json::Value,
) -> std::result::Result<MoleculeOutcome, SubagentError> {
    let guard = lock_molecule_duckdb(duckdb, "subagent.update")?;
    let conn: &duckdb::Connection = &guard;
    let subagent_id = required_str(args, "subagent_id")?;

    // 1) resolve 实例：mode='subagent' 且未 tombstoned。
    let instance = resolve_subagent_instance(conn, subagent_id)?;
    let current = instance
        .subagent_config()
        .map_err(SubagentError::from_agent)?;
    if current.lifecycle == LIFECYCLE_TOMBSTONED {
        return Err(SubagentError::rejected(
            "subagent.update: subagent is tombstoned",
        ));
    }

    // 2) 重新 resolve 其来源模板；变更后 allowlist 仍必须 ⊆ 模板 allowlist；model_id 必须存在。
    let template = resolve_template(conn, &current.template_id)?;
    let new_allowlist = match args.get("capability_allowlist") {
        Some(list) if list.is_array() => {
            let list = list.as_array().expect("is_array checked");
            validate_subset(list, &template.capability_allowlist)?;
            to_string_vec(list)
        }
        Some(_) => {
            return Err(SubagentError::rejected(
                "subagent.update: capability_allowlist must be an array",
            ))
        }
        None => instance.capability_allowlist.clone(),
    };
    let new_model_id = match args.get("model_id").and_then(|value| value.as_str()) {
        Some(model_id) if !model_id.trim().is_empty() => {
            ensure_model_exists(conn, model_id)?;
            model_id.to_string()
        }
        Some(_) => {
            return Err(SubagentError::rejected(
                "subagent.update: model_id must not be empty",
            ))
        }
        None => current.model_id.clone(),
    };
    let new_prompt = parse_optional_prompt(args)?.unwrap_or_else(|| instance.prompt.clone());
    let new_startup = if args.get("startup").is_some() {
        parse_optional_startup(args.get("startup"))?.unwrap_or(current.startup)
    } else {
        current.startup
    };
    let new_trigger = if args.get("trigger").is_some() {
        args.get("trigger").cloned()
    } else {
        current.trigger.clone()
    };
    let new_budget = if args.get("budget").is_some() {
        budget_from_args(
            args,
            &template
                .template_config()
                .map_err(SubagentError::from_agent)?,
        )?
    } else {
        current.budget.clone()
    };

    // 3) invocation.prewrite。
    let invocation_id =
        prewrite_invocation(conn, storage_root, "subagent.update", args).map_err(|error| {
            SubagentError::failed(format!("subagent.update prewrite failed: {error}"))
        })?;

    // 4) persist：更新 prompt/config/updated_at。running 中只写持久行（冻结快照语义由 runtime 侧保证）。
    let mut updated_config = current.clone();
    updated_config.model_id = new_model_id;
    updated_config.startup = new_startup;
    updated_config.trigger = new_trigger;
    updated_config.budget = new_budget;
    let mut updated_fields = Vec::new();
    if args.get("prompt").is_some() {
        updated_fields.push("prompt".to_string());
    }
    if args.get("capability_allowlist").is_some() {
        updated_fields.push("capability_allowlist".to_string());
    }
    if args.get("startup").is_some() {
        updated_fields.push("startup".to_string());
    }
    if args.get("trigger").is_some() {
        updated_fields.push("trigger".to_string());
    }
    if args.get("model_id").is_some() {
        updated_fields.push("model_id".to_string());
    }
    if args.get("budget").is_some() {
        updated_fields.push("budget".to_string());
    }

    persist_agent_row(
        conn,
        &instance.id,
        &new_prompt,
        &new_allowlist,
        &serde_json::json!({ "subagent": updated_config }),
    )
    .map_err(|error| {
        SubagentError::failed(format!("subagent.update persist: {error}"))
            .with_invocation(invocation_id.clone())
    })?;

    Ok(MoleculeOutcome {
        output: serde_json::json!({
            "subagent_id": subagent_id,
            "lifecycle": updated_config.lifecycle,
            "updated_fields": updated_fields,
        }),
        final_state: "completed",
        invocation_id,
    })
}

fn subagent_run(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    args: &serde_json::Value,
    spawn_hook: Option<&dyn SubagentSpawnHook>,
) -> std::result::Result<MoleculeOutcome, SubagentError> {
    let guard = lock_molecule_duckdb(duckdb, "subagent.run")?;
    let conn: &duckdb::Connection = &guard;
    let subagent_id = required_str(args, "subagent_id")?;
    let instance = resolve_subagent_instance(conn, subagent_id)?;
    let config = instance
        .subagent_config()
        .map_err(SubagentError::from_agent)?;

    // 前置状态：idle 或 failed；sleeping/running/tombstoned 直接拒绝，重复 run 拒绝。
    match config.lifecycle.as_str() {
        LIFECYCLE_IDLE | LIFECYCLE_FAILED => {}
        LIFECYCLE_RUNNING => {
            return Err(SubagentError::rejected(
                "subagent.run: subagent is already running",
            ))
        }
        LIFECYCLE_SLEEPING => {
            return Err(SubagentError::rejected(
                "subagent.run: subagent is sleeping; wake it first",
            ))
        }
        LIFECYCLE_TOMBSTONED => {
            return Err(SubagentError::rejected(
                "subagent.run: subagent is tombstoned",
            ))
        }
        LIFECYCLE_CREATED => {
            return Err(SubagentError::rejected(
                "subagent.run: subagent is not ready (created)",
            ))
        }
        other => {
            return Err(SubagentError::failed(format!(
                "subagent.run: unknown lifecycle '{other}'"
            )))
        }
    }

    let task_input = args
        .get("task_input")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| config.task_input.clone())
        .unwrap_or_default();

    // 冻结本次 run 的定义快照。
    let definition = SubagentDefinition {
        subagent_id: subagent_id.to_string(),
        prompt: instance.prompt.clone(),
        capability_allowlist: instance.capability_allowlist.clone(),
        model_id: config.model_id.clone(),
        budget: config.budget.clone(),
        startup: config.startup,
        trigger: config.trigger.clone(),
    };

    let invocation_id = prewrite_invocation(conn, storage_root, "subagent.run", args)
        .map_err(|error| SubagentError::failed(format!("subagent.run prewrite failed: {error}")))?;

    // persist lifecycle=running。
    persist_subagent_lifecycle(conn, subagent_id, LIFECYCLE_RUNNING).map_err(|error| {
        SubagentError::failed(format!("subagent.run persist running: {error}"))
            .with_invocation(invocation_id.clone())
    })?;

    // 受理后先写 running 占位，便于区分“未启动”和“运行中”。
    if let Err(e) =
        crate::agent::subagent_memory::write_last_output(storage_root, subagent_id, "running", "")
    {
        tracing::warn!("subagent.run: write running placeholder failed: {e}");
    }

    // 通知 runtime spawn（hook 未安装时分子仍完成持久化并正常返回 accepted）。
    if let Some(hook) = spawn_hook {
        hook.notify(SubagentSpawnEvent::RunAccepted {
            definition,
            task_input,
            invocation_id: invocation_id.clone(),
        });
    }

    Ok(MoleculeOutcome {
        output: serde_json::json!({
            "subagent_id": subagent_id,
            "accepted": true,
            "lifecycle": "running",
        }),
        final_state: "accepted",
        invocation_id,
    })
}

fn subagent_sleep(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    args: &serde_json::Value,
) -> std::result::Result<MoleculeOutcome, SubagentError> {
    let guard = lock_molecule_duckdb(duckdb, "subagent.sleep")?;
    let conn: &duckdb::Connection = &guard;
    let subagent_id = required_str(args, "subagent_id")?;
    let instance = resolve_subagent_instance(conn, subagent_id)?;
    let config = instance
        .subagent_config()
        .map_err(SubagentError::from_agent)?;

    // 前置状态：idle 或 failed；running/sleeping/tombstoned 直接拒绝，不等待在途 run。
    match config.lifecycle.as_str() {
        LIFECYCLE_IDLE | LIFECYCLE_FAILED => {}
        LIFECYCLE_RUNNING => {
            return Err(SubagentError::rejected(
                "subagent.sleep: cannot sleep while running",
            ))
        }
        LIFECYCLE_SLEEPING => {
            return Err(SubagentError::rejected(
                "subagent.sleep: subagent is already sleeping",
            ))
        }
        LIFECYCLE_TOMBSTONED => {
            return Err(SubagentError::rejected(
                "subagent.sleep: subagent is tombstoned",
            ))
        }
        LIFECYCLE_CREATED => {
            return Err(SubagentError::rejected(
                "subagent.sleep: subagent is not ready (created)",
            ))
        }
        other => {
            return Err(SubagentError::failed(format!(
                "subagent.sleep: unknown lifecycle '{other}'"
            )))
        }
    }

    let invocation_id =
        prewrite_invocation(conn, storage_root, "subagent.sleep", args).map_err(|error| {
            SubagentError::failed(format!("subagent.sleep prewrite failed: {error}"))
        })?;
    persist_subagent_lifecycle(conn, subagent_id, LIFECYCLE_SLEEPING).map_err(|error| {
        SubagentError::failed(format!("subagent.sleep persist: {error}"))
            .with_invocation(invocation_id.clone())
    })?;

    Ok(MoleculeOutcome {
        output: serde_json::json!({
            "subagent_id": subagent_id,
            "lifecycle": "sleeping",
        }),
        final_state: "completed",
        invocation_id,
    })
}

fn subagent_wake(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    args: &serde_json::Value,
) -> std::result::Result<MoleculeOutcome, SubagentError> {
    let guard = lock_molecule_duckdb(duckdb, "subagent.wake")?;
    let conn: &duckdb::Connection = &guard;
    let subagent_id = required_str(args, "subagent_id")?;
    let instance = resolve_subagent_instance(conn, subagent_id)?;
    let config = instance
        .subagent_config()
        .map_err(SubagentError::from_agent)?;

    // 前置状态：sleeping。
    if config.lifecycle != LIFECYCLE_SLEEPING {
        return Err(SubagentError::rejected(
            "subagent.wake: wake requires lifecycle 'sleeping'",
        ));
    }

    let invocation_id =
        prewrite_invocation(conn, storage_root, "subagent.wake", args).map_err(|error| {
            SubagentError::failed(format!("subagent.wake prewrite failed: {error}"))
        })?;
    persist_subagent_lifecycle(conn, subagent_id, LIFECYCLE_IDLE).map_err(|error| {
        SubagentError::failed(format!("subagent.wake persist: {error}"))
            .with_invocation(invocation_id.clone())
    })?;

    Ok(MoleculeOutcome {
        output: serde_json::json!({
            "subagent_id": subagent_id,
            "lifecycle": "idle",
        }),
        final_state: "completed",
        invocation_id,
    })
}

fn subagent_delete(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    args: &serde_json::Value,
) -> std::result::Result<MoleculeOutcome, SubagentError> {
    let guard = lock_molecule_duckdb(duckdb, "subagent.delete")?;
    let conn: &duckdb::Connection = &guard;
    let subagent_id = required_str(args, "subagent_id")?;
    let instance = resolve_subagent_instance(conn, subagent_id)?;
    let config = instance
        .subagent_config()
        .map_err(SubagentError::from_agent)?;

    // 前置状态：idle/sleeping/failed；running 直接拒绝；tombstoned 幂等返回。
    match config.lifecycle.as_str() {
        LIFECYCLE_IDLE | LIFECYCLE_SLEEPING | LIFECYCLE_FAILED => {}
        LIFECYCLE_RUNNING => {
            return Err(SubagentError::rejected(
                "subagent.delete: cannot delete while running; wait for run terminal state",
            ))
        }
        LIFECYCLE_TOMBSTONED => {
            // 幂等：仍记录本次调用并返回已归档结果。
            let invocation_id = prewrite_invocation(conn, storage_root, "subagent.delete", args)
                .map_err(|error| {
                    SubagentError::failed(format!("subagent.delete prewrite failed: {error}"))
                })?;
            return Ok(MoleculeOutcome {
                output: serde_json::json!({
                    "subagent_id": subagent_id,
                    "lifecycle": "tombstoned",
                    "archived": true,
                }),
                final_state: "completed",
                invocation_id,
            });
        }
        LIFECYCLE_CREATED => {
            return Err(SubagentError::rejected(
                "subagent.delete: subagent is not ready (created)",
            ))
        }
        other => {
            return Err(SubagentError::failed(format!(
                "subagent.delete: unknown lifecycle '{other}'"
            )))
        }
    }

    let invocation_id =
        prewrite_invocation(conn, storage_root, "subagent.delete", args).map_err(|error| {
            SubagentError::failed(format!("subagent.delete prewrite failed: {error}"))
        })?;

    // persist 软删除：tombstoned_at + lifecycle=tombstoned；记忆/last_output 文件保留。
    let mut updated = config.clone();
    updated.lifecycle = LIFECYCLE_TOMBSTONED.to_string();
    updated.tombstoned_at = Some(now_iso());
    persist_agent_row(
        conn,
        &instance.id,
        &instance.prompt,
        &instance.capability_allowlist,
        &serde_json::json!({ "subagent": updated }),
    )
    .map_err(|error| {
        SubagentError::failed(format!("subagent.delete persist: {error}"))
            .with_invocation(invocation_id.clone())
    })?;

    Ok(MoleculeOutcome {
        output: serde_json::json!({
            "subagent_id": subagent_id,
            "lifecycle": "tombstoned",
            "archived": true,
        }),
        final_state: "completed",
        invocation_id,
    })
}

fn method_invoke(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    args: &serde_json::Value,
    spawn_hook: Option<&dyn SubagentSpawnHook>,
) -> std::result::Result<MoleculeOutcome, SubagentError> {
    let method_id = required_str(args, "method_id")?;
    let task_input = required_str(args, "task_input")?;
    let model_id = required_str(args, "model_id")?;
    let called_by = args
        .get("called_by")
        .and_then(|value| value.as_str())
        .unwrap_or("execution-platform");

    // 1. method.invoke 自身 invocation 预写。
    let invocation_id = {
        let guard = lock_molecule_duckdb(duckdb, "method.invoke")?;
        prewrite_invocation(&guard, storage_root, METHOD_INVOKE_ID, args)
            .map_err(|error| SubagentError::failed(format!("method.invoke prewrite failed: {error}")))?
    };

    // 2. 读取方法定义（注册表只读 brief，这里取完整文档）。
    let (brief, metadata) = {
        let guard = lock_molecule_duckdb(duckdb, "method.invoke")?;
        read_usage_method(&guard, method_id)
            .map_err(SubagentError::from_agent)?
    };
    let full_document = metadata
        .get("full_document")
        .and_then(|value| value.as_str())
        .unwrap_or(&brief)
        .to_string();
    let required_capabilities: Vec<String> = metadata
        .get("required_capabilities")
        .and_then(|value| value.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    let method_prompt = format!(
        "你是“{}”方法的执行单元。请先完整阅读下方方法文档，再组织可用的原子/分子能力完成任务。\n\n## 方法文档\n{}\n\n## 本次任务\n{}",
        method_id, full_document, task_input
    );

    // 3. 创建方法执行子代理；allowlist 先为空，所需能力随后自动授予。
    let create_args = serde_json::json!({
        "template_id": "subagent.template.normal",
        "model_id": model_id,
        "prompt": method_prompt,
        "task_input": task_input,
        "capability_allowlist": [],
        "name": format!("method-{}", method_id),
    });
    let create_out = subagent_create(duckdb, storage_root, &create_args, spawn_hook)?;
    let subagent_id = create_out
        .output
        .get("subagent_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| SubagentError::failed("method.invoke: create output missing subagent_id"))?
        .to_string();

    // 4. 自动授权方法内所需能力（统一由执行中台授权，one-shot 最小权限）。
    let mut granted = Vec::new();
    for capability_id in &required_capabilities {
        let grant_args = serde_json::json!({
            "target_agent_id": subagent_id,
            "capability_id": capability_id,
            "mode": "one_shot",
        });
        let guard = lock_molecule_duckdb(duckdb, "method.invoke")?;
        crate::logic::capability::permission::grant(&guard, called_by, &grant_args)
            .map_err(SubagentError::from_agent)?;
        granted.push(capability_id.clone());
    }

    // 5. 运行方法执行子代理。
    let run_args = serde_json::json!({
        "subagent_id": subagent_id,
        "task_input": task_input,
    });
    let _run_out = subagent_run(duckdb, storage_root, &run_args, spawn_hook)?;

    // 6. 写入方法调用审计。
    let method_call_id = {
        let guard = lock_molecule_duckdb(duckdb, "method.invoke")?;
        write_method_call_audit_row(
            &guard,
            method_id,
            called_by,
            &granted,
            &subagent_id,
        )
        .map_err(SubagentError::from_agent)?
    };

    Ok(MoleculeOutcome {
        output: serde_json::json!({
            "success": true,
            "method_id": method_id,
            "method_call_id": method_call_id,
            "subagent_id": subagent_id,
            "status": "running",
            "granted_capabilities": granted,
        }),
        final_state: "accepted",
        invocation_id,
    })
}

fn usage_observe(
    duckdb: &Arc<std::sync::Mutex<duckdb::Connection>>,
    storage_root: &Path,
    args: &serde_json::Value,
) -> std::result::Result<MoleculeOutcome, SubagentError> {
    let guard = lock_molecule_duckdb(duckdb, "usage_method.observe")?;
    let conn: &duckdb::Connection = &guard;
    let capability_id = required_str(args, "capability_id")?;
    let observation = required_str(args, "observation")?;
    let suggestion = required_str(args, "suggestion")?;

    // 校验 capability_id 必须出现在最近一次能力调用日志（invocations 目录中最新事实文件）中。
    let present = match latest_invocation_log(storage_root) {
        Ok(Some(latest)) => {
            latest.get("capability_id").and_then(|value| value.as_str()) == Some(capability_id)
        }
        Ok(None) => false,
        Err(error) => {
            return Err(SubagentError::failed(format!(
                "usage_method.observe: read latest invocation log: {error}"
            )))
        }
    };
    if !present {
        return Err(SubagentError::rejected(format!(
            "usage_method.observe: capability_id '{capability_id}' not found in the most recent invocation log"
        )));
    }

    let invocation_id = prewrite_invocation(conn, storage_root, USAGE_METHOD_OBSERVE_ID, args)
        .map_err(|error| {
            SubagentError::failed(format!("usage_method.observe prewrite failed: {error}"))
        })?;

    let (usage_method_id, created, updated) =
        write_usage_observation_row(conn, capability_id, observation, suggestion).map_err(
            |error| {
                SubagentError::failed(format!("usage_method.observe write: {error}"))
                    .with_invocation(invocation_id.clone())
            },
        )?;

    Ok(MoleculeOutcome {
        output: serde_json::json!({
            "success": true,
            "capability_id": capability_id,
            "usage_method_id": usage_method_id,
            "created": created,
            "updated": updated,
        }),
        final_state: "completed",
        invocation_id,
    })
}

// ---------------------------------------------------------------------------
// agent 表内部原子函数（Rust 函数，不建注册表行）
// ---------------------------------------------------------------------------

/// 读取 agent 表行（供本模块与 TB runtime 复用）。
pub fn resolve_agent_row(conn: &duckdb::Connection, id: &str) -> Result<Option<AgentRow>> {
    let mut statement = conn
        .prepare(
            "SELECT id, name, mode, prompt, CAST(capability_allowlist AS VARCHAR),              CAST(config AS VARCHAR) FROM agent WHERE id = ?",
        )
        .map_err(|error| {
            AgentError::Bootstrap(format!("resolve_agent_row prepare '{id}': {error}"))
        })?;
    let mut rows = statement
        .query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|error| {
            AgentError::Bootstrap(format!("resolve_agent_row query '{id}': {error}"))
        })?;
    let Some(row) = rows.next() else {
        return Ok(None);
    };
    let (id, name, mode, prompt, allowlist, config) = row.map_err(|error| {
        AgentError::Bootstrap(format!("resolve_agent_row read '{id}': {error}"))
    })?;
    let prompt = prompt.unwrap_or_default();
    let allowlist = match allowlist {
        Some(text) => parse_string_array(&text, &format!("agent '{id}'.capability_allowlist"))?,
        None => Vec::new(),
    };
    let config = match config {
        Some(text) => serde_json::from_str(&text).map_err(|error| {
            AgentError::Bootstrap(format!("agent '{id}' config invalid JSON: {error}"))
        })?,
        None => serde_json::Value::Null,
    };
    Ok(Some(AgentRow {
        id,
        name,
        mode,
        prompt,
        capability_allowlist: allowlist,
        config,
    }))
}

/// 写/更新 agent 表行（供本模块与 TB runtime 复用）。
pub fn persist_agent_row(
    conn: &duckdb::Connection,
    id: &str,
    prompt: &str,
    capability_allowlist: &[String],
    config: &serde_json::Value,
) -> Result<()> {
    let allowlist_json = serde_json::to_string(capability_allowlist).map_err(|error| {
        AgentError::Parse(format!(
            "serialize capability_allowlist for '{id}': {error}"
        ))
    })?;
    let config_json = serde_json::to_string(config)
        .map_err(|error| AgentError::Parse(format!("serialize config for '{id}': {error}")))?;
    conn.execute(
        "UPDATE agent SET prompt = ?, capability_allowlist = CAST(? AS JSON),          config = CAST(? AS JSON), updated_at = now() WHERE id = ?",
        duckdb::params![prompt, allowlist_json, config_json, id],
    )
    .map_err(|error| {
        AgentError::Bootstrap(format!("persist_agent_row update '{id}': {error}"))
    })?;
    Ok(())
}

/// 按稳定字符串更新 subagent 实例持久生命周期（内部路径，可表达 sleeping）。
fn set_subagent_lifecycle_str(
    conn: &duckdb::Connection,
    subagent_id: &str,
    lifecycle: &str,
) -> Result<()> {
    let instance = resolve_agent_row(conn, subagent_id)?
        .ok_or_else(|| AgentError::NotFound(format!("subagent instance: {subagent_id}")))?;
    if instance.mode != "subagent" {
        return Err(AgentError::NotFound(format!(
            "set_subagent_lifecycle: '{subagent_id}' is not a subagent instance"
        )));
    }
    let mut config = instance.subagent_config()?;
    config.lifecycle = lifecycle.to_string();
    persist_agent_row(
        conn,
        &instance.id,
        &instance.prompt,
        &instance.capability_allowlist,
        &serde_json::json!({ "subagent": config }),
    )
}

/// 更新 subagent 实例的持久生命周期（running -> idle/failed 由 TB runtime 收口时调用）。
///
/// 注：冻结 `SubagentLifecycle` 缺少 §7.4 状态集中的 `sleeping` 变体，
/// 本接口用于 TB runtime 收口（running -> idle/failed）；`sleeping` 仅由
/// `subagent.sleep` 分子内部写入。
pub fn set_subagent_lifecycle(
    conn: &duckdb::Connection,
    subagent_id: &str,
    lifecycle: SubagentLifecycle,
) -> Result<()> {
    set_subagent_lifecycle_str(conn, subagent_id, lifecycle_name(lifecycle))
}

/// 按 id 读取冻结定义快照（TB runtime 崩溃恢复可复用）。
pub fn subagent_definition(
    conn: &duckdb::Connection,
    subagent_id: &str,
) -> Result<SubagentDefinition> {
    let instance = resolve_agent_row(conn, subagent_id)?
        .ok_or_else(|| AgentError::NotFound(format!("subagent instance: {subagent_id}")))?;
    if instance.mode != "subagent" {
        return Err(AgentError::NotFound(format!(
            "subagent_definition: '{subagent_id}' is not a subagent instance"
        )));
    }
    let config = instance.subagent_config()?;
    Ok(SubagentDefinition {
        subagent_id: instance.id.clone(),
        prompt: instance.prompt,
        capability_allowlist: instance.capability_allowlist,
        model_id: config.model_id,
        budget: config.budget,
        startup: config.startup,
        trigger: config.trigger,
    })
}

/// 直接更新持久生命周期（内部快捷路径，供分子使用；字符串形态可表达 sleeping）。
fn persist_subagent_lifecycle(
    conn: &duckdb::Connection,
    subagent_id: &str,
    lifecycle: &str,
) -> Result<()> {
    set_subagent_lifecycle_str(conn, subagent_id, lifecycle)
}

fn persist_new_agent_row(conn: &duckdb::Connection, row: &AgentRow) -> Result<()> {
    let allowlist_json = serde_json::to_string(&row.capability_allowlist).map_err(|error| {
        AgentError::Parse(format!(
            "serialize capability_allowlist for '{id}': {error}",
            id = row.id
        ))
    })?;
    let config_json = serde_json::to_string(&row.config).map_err(|error| {
        AgentError::Parse(format!("serialize config for '{id}': {error}", id = row.id))
    })?;
    conn.execute(
        "INSERT INTO agent (id, name, mode, prompt, capability_allowlist, config, is_default)          VALUES (?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), false)",
        duckdb::params![
            row.id,
            row.name,
            row.mode,
            row.prompt,
            allowlist_json,
            config_json,
        ],
    )
    .map_err(|error| {
        AgentError::Bootstrap(format!("insert agent row '{id}': {error}", id = row.id))
    })?;
    Ok(())
}

fn resolve_subagent_instance(
    conn: &duckdb::Connection,
    subagent_id: &str,
) -> std::result::Result<AgentRow, SubagentError> {
    let row = resolve_agent_row(conn, subagent_id)
        .map_err(SubagentError::from_agent)?
        .ok_or_else(|| {
            SubagentError::rejected(format!("subagent instance not found: {subagent_id}"))
        })?;
    if row.mode != "subagent" {
        return Err(SubagentError::rejected(format!(
            "'{subagent_id}' is not a subagent instance"
        )));
    }
    Ok(row)
}

fn resolve_template(
    conn: &duckdb::Connection,
    template_id: &str,
) -> std::result::Result<AgentRow, SubagentError> {
    let row = resolve_agent_row(conn, template_id)
        .map_err(SubagentError::from_agent)?
        .ok_or_else(|| {
            SubagentError::rejected(format!("subagent template not found: {template_id}"))
        })?;
    if row.mode != "subagent_template" {
        return Err(SubagentError::rejected(format!(
            "'{template_id}' is not a subagent template"
        )));
    }
    let template_config = row.template_config().map_err(SubagentError::from_agent)?;
    if template_config.tombstoned_at.is_some() {
        return Err(SubagentError::rejected(format!(
            "subagent template is tombstoned: {template_id}"
        )));
    }
    Ok(row)
}

fn ensure_model_exists(
    conn: &duckdb::Connection,
    model_id: &str,
) -> std::result::Result<(), SubagentError> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM model WHERE id = ?",
            duckdb::params![model_id],
            |row| row.get(0),
        )
        .map_err(|error| {
            SubagentError::failed(format!("query model registry '{model_id}': {error}"))
        })?;
    if count == 0 {
        return Err(SubagentError::rejected(format!(
            "model_id not found in model registry: {model_id}"
        )));
    }
    Ok(())
}

fn chosen_allowlist(
    args: &serde_json::Value,
    template_allowlist: &[String],
) -> std::result::Result<Vec<String>, SubagentError> {
    match args.get("capability_allowlist") {
        Some(list) if list.is_array() => {
            let list = list.as_array().expect("is_array checked");
            validate_subset(list, template_allowlist)?;
            Ok(to_string_vec(list))
        }
        Some(_) => Err(SubagentError::rejected(
            "subagent.create: capability_allowlist must be an array",
        )),
        None => Ok(template_allowlist.to_vec()),
    }
}

/// 可选 prompt 校验（subagent.create / subagent.update 共享）：
/// 缺省 → `Ok(None)`（继承模板基线/保持现状）；非字符串 → 拒绝；字符串但
/// trim 后为空/纯空白 → 拒绝（明示）。
fn parse_optional_prompt(
    args: &serde_json::Value,
) -> std::result::Result<Option<String>, SubagentError> {
    match args.get("prompt") {
        None => Ok(None),
        Some(serde_json::Value::String(prompt)) if !prompt.trim().is_empty() => {
            Ok(Some(prompt.clone()))
        }
        Some(serde_json::Value::String(_)) => Err(SubagentError::rejected(
            "prompt must not be empty or whitespace",
        )),
        Some(_) => Err(SubagentError::rejected("prompt must be a string")),
    }
}

/// allowlist 只能缩小：参数必须是模板 allowlist 的子集，且无重复。
fn validate_subset(
    list: &[serde_json::Value],
    template_allowlist: &[String],
) -> std::result::Result<(), SubagentError> {
    let mut seen = HashSet::new();
    for item in list {
        let value = item
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                SubagentError::rejected("capability_allowlist must contain only non-empty strings")
            })?;
        if !seen.insert(value) {
            return Err(SubagentError::rejected(format!(
                "capability_allowlist contains duplicate capability_id '{value}'"
            )));
        }
        if !template_allowlist.iter().any(|allowed| allowed == value) {
            return Err(SubagentError::rejected(format!(
                "capability_allowlist '{value}' exceeds template allowlist"
            )));
        }
    }
    Ok(())
}

fn to_string_vec(list: &[serde_json::Value]) -> Vec<String> {
    list.iter()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect()
}

fn budget_from_args(
    args: &serde_json::Value,
    template: &TemplateConfig,
) -> std::result::Result<SubagentBudget, SubagentError> {
    match args.get("budget") {
        Some(budget) if budget.is_object() => {
            let max_retries = budget
                .get("max_retries")
                .and_then(|value| value.as_u64())
                .map(|v| v as u32)
                .unwrap_or(template.max_retries);
            let attempt_timeout_seconds = budget
                .get("attempt_timeout_seconds")
                .and_then(|value| value.as_u64())
                .unwrap_or(template.attempt_timeout_seconds);
            let total_timeout_seconds = budget
                .get("total_timeout_seconds")
                .and_then(|value| value.as_u64())
                .unwrap_or(template.total_timeout_seconds);
            if attempt_timeout_seconds == 0 || total_timeout_seconds == 0 {
                return Err(SubagentError::rejected("budget timeouts must be positive"));
            }
            Ok(SubagentBudget {
                max_retries,
                attempt_timeout_seconds,
                total_timeout_seconds,
            })
        }
        Some(_) => Err(SubagentError::rejected("budget must be an object")),
        None => Ok(SubagentBudget {
            max_retries: template.max_retries,
            attempt_timeout_seconds: template.attempt_timeout_seconds,
            total_timeout_seconds: template.total_timeout_seconds,
        }),
    }
}

fn parse_optional_lifecycle_kind(
    value: Option<&serde_json::Value>,
) -> std::result::Result<Option<SubagentLifecycleKind>, SubagentError> {
    let Some(value) = value else { return Ok(None) };
    let Some(text) = value.as_str() else {
        return Err(SubagentError::rejected("lifecycle must be a string"));
    };
    match text {
        "temporary" => Ok(Some(SubagentLifecycleKind::Temporary)),
        "resident" => Ok(Some(SubagentLifecycleKind::Resident)),
        other => Err(SubagentError::rejected(format!(
            "lifecycle must be 'temporary' or 'resident', got '{other}'"
        ))),
    }
}

fn parse_optional_startup(
    value: Option<&serde_json::Value>,
) -> std::result::Result<Option<SubagentStartup>, SubagentError> {
    let Some(value) = value else { return Ok(None) };
    let Some(text) = value.as_str() else {
        return Err(SubagentError::rejected("startup must be a string"));
    };
    match text {
        "normal" => Ok(Some(SubagentStartup::Normal)),
        "scheduled" => Ok(Some(SubagentStartup::Scheduled)),
        "condition" => Ok(Some(SubagentStartup::Condition)),
        other => Err(SubagentError::rejected(format!(
            "startup must be 'normal'|'scheduled'|'condition', got '{other}'"
        ))),
    }
}

fn required_str<'a>(
    args: &'a serde_json::Value,
    field: &str,
) -> std::result::Result<&'a str, SubagentError> {
    args.get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| SubagentError::rejected(format!("missing required string field '{field}'")))
}

fn lifecycle_name(lifecycle: SubagentLifecycle) -> &'static str {
    match lifecycle {
        SubagentLifecycle::Created => "created",
        SubagentLifecycle::Running => "running",
        SubagentLifecycle::Idle => "idle",
        SubagentLifecycle::Sleeping => "sleeping",
        SubagentLifecycle::Failed => "failed",
        SubagentLifecycle::Tombstoned => "tombstoned",
    }
}

fn parse_string_array(text: &str, context: &str) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| AgentError::Bootstrap(format!("{context} invalid JSON: {error}")))?;
    let values = value
        .as_array()
        .ok_or_else(|| AgentError::Bootstrap(format!("{context} must be an array")))?;
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        let id = value
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                AgentError::Bootstrap(format!("{context} must contain only non-empty strings"))
            })?;
        ids.push(id.to_string());
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// 全局 invocation 日志（§4.5）
// ---------------------------------------------------------------------------

/// 副作用前预写 invocation 事实（不可变首文件）。失败时调用方不得继续副作用。
fn prewrite_invocation(
    conn: &duckdb::Connection,
    storage_root: &Path,
    capability_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    mark_unclosed_invocations(storage_root)?;

    let invocation_id = format!("inv_{}", uuid_simple());
    let capability_name =
        capability_name(conn, capability_id).unwrap_or_else(|| capability_id.to_string());
    let definition_hash = definition_hash(conn, capability_id)?;
    let started_at = now_iso();
    let task_ref = args
        .get("task_ref")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let fact = serde_json::json!({
        "invocation_id": invocation_id,
        "capability_id": capability_id,
        "capability_name": capability_name,
        "definition_hash": definition_hash,
        "arguments_redacted": redact_arguments(args),
        "started_at": started_at,
        "task_ref": task_ref,
    });
    let text = serde_json::to_string_pretty(&fact)
        .map_err(|error| AgentError::Script(format!("prewrite_invocation serialize: {error}")))?;

    let invocations_dir = storage_root.join("invocations");
    atomic_write_private(
        &invocations_dir.join(format!("{invocation_id}.json")),
        &text,
    )?;
    Ok(invocation_id)
}

/// TB runtime 收口时闭合 subagent.run 的 invocation 终态。
///
/// 该路径由 TC 的 finish 回调调用；成功后写 `<invocation_id>.result.json`，
/// 保持首文件不可变。失败只告警，不改变 runtime 已经写好的 last_output/memory。
pub fn close_subagent_invocation(
    storage_root: &Path,
    invocation_id: &str,
    final_state: &str,
    error: Option<&str>,
) -> Result<()> {
    append_invocation_result(storage_root, invocation_id, final_state, error)
}

/// 执行后追加 invocation 终态（<id>.result.json），保持首文件不可变。
fn append_invocation_result(
    storage_root: &Path,
    invocation_id: &str,
    final_state: &str,
    error: Option<&str>,
) -> Result<()> {
    let fact = serde_json::json!({
        "invocation_id": invocation_id,
        "final_state": final_state,
        "error": error.map(|text| serde_json::Value::String(text.to_string()))
            .unwrap_or(serde_json::Value::Null),
    });
    let text = serde_json::to_string_pretty(&fact).map_err(|error| {
        AgentError::Script(format!("append_invocation_result serialize: {error}"))
    })?;
    let invocations_dir = storage_root.join("invocations");
    atomic_write_private(
        &invocations_dir.join(format!("{invocation_id}.result.json")),
        &text,
    )?;
    Ok(())
}

/// 进程内首次访问 invocations 目录时，把本进程之前遗留的未闭合 invocation 标记
/// process_unexpected_exit（写入 <id>.result.json）。
fn mark_unclosed_invocations(storage_root: &Path) -> Result<()> {
    let mut roots = unclosed_marked_roots().lock().map_err(|_| {
        AgentError::Bootstrap("unclosed invocation marker lock poisoned".to_string())
    })?;
    if !roots.insert(storage_root.to_path_buf()) {
        return Ok(());
    }
    let invocations_dir = storage_root.join("invocations");
    if !invocations_dir.is_dir() {
        return Ok(());
    }
    let entries = std::fs::read_dir(&invocations_dir).map_err(|error| {
        AgentError::Io(format!(
            "mark_unclosed_invocations read_dir {:?}: {error}",
            invocations_dir
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            AgentError::Io(format!(
                "mark_unclosed_invocations entry {:?}: {error}",
                invocations_dir
            ))
        })?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.ends_with(".json") || name.ends_with(".result.json") {
            continue;
        }
        let Some(invocation_id) = name.strip_suffix(".json") else {
            continue;
        };
        let result_path = invocations_dir.join(format!("{invocation_id}.result.json"));
        if !result_path.exists() {
            let fact = serde_json::json!({
                "invocation_id": invocation_id,
                "final_state": "process_unexpected_exit",
                "error": "process exited before invocation was closed",
            });
            let text = serde_json::to_string_pretty(&fact)
                .map_err(|error| AgentError::Script(format!("mark_unclosed serialize: {error}")))?;
            atomic_write_private(&result_path, &text)?;
        }
    }
    Ok(())
}

/// 读取 invocations 目录中最近一次能力调用事实（按修改时间，最新者优先）。
fn latest_invocation_log(storage_root: &Path) -> Result<Option<serde_json::Value>> {
    mark_unclosed_invocations(storage_root)?;
    let invocations_dir = storage_root.join("invocations");
    if !invocations_dir.is_dir() {
        return Ok(None);
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(&invocations_dir)
        .map_err(|error| AgentError::Io(format!("latest_invocation_log read_dir: {error}")))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| AgentError::Io(format!("latest_invocation_log entry: {error}")))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") || name.ends_with(".result.json") {
            continue;
        }
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let better = match &best {
            Some((best_modified, best_path)) => {
                modified > *best_modified || (modified == *best_modified && path > *best_path)
            }
            None => true,
        };
        if better {
            best = Some((modified, path));
        }
    }
    match best {
        Some((_, path)) => {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                AgentError::Io(format!("latest_invocation_log read {:?}: {error}", path))
            })?;
            let value = serde_json::from_str(&text).map_err(|error| {
                AgentError::Parse(format!("latest_invocation_log parse {:?}: {error}", path))
            })?;
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

/// 能力名称（来自 base_capability 权威行）。
fn capability_name(conn: &duckdb::Connection, capability_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT name FROM base_capability WHERE id = ?",
        duckdb::params![capability_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// 定义哈希：对 base_capability 权威定义的稳定串做 sha256。
fn definition_hash(conn: &duckdb::Connection, capability_id: &str) -> Result<String> {
    let canonical = match conn.query_row(
        "SELECT name, description, CAST(schema_in AS VARCHAR), CAST(schema_out AS VARCHAR),          executor, version FROM base_capability WHERE id = ?",
        duckdb::params![capability_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        },
    ) {
        Ok((name, description, schema_in, schema_out, executor, version)) => {
            format!(
                "capability_id={capability_id}|name={name}|description={description}|                 schema_in={schema_in}|schema_out={schema_out}|executor={executor}|version={version}"
            )
        }
        Err(_) => format!("capability_id={capability_id}"),
    };
    Ok(hex_sha256(canonical.as_bytes()))
}

/// 参数脱敏：键名含 key/token/secret/password 的值打码；其余字符串截断 2000 字符。
fn redact_arguments(args: &serde_json::Value) -> serde_json::Value {
    match args {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                let key_lower = key.to_ascii_lowercase();
                if contains_secret_marker(&key_lower) {
                    out.insert(key.clone(), serde_json::Value::String("***".to_string()));
                } else {
                    out.insert(key.clone(), redact_arguments(value));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_arguments).collect())
        }
        serde_json::Value::String(text) => {
            let count = text.chars().count();
            if count > MAX_ARGUMENTS_CHARS {
                let truncated: String = text.chars().take(MAX_ARGUMENTS_CHARS).collect();
                serde_json::Value::String(format!("{truncated}...<truncated>"))
            } else {
                serde_json::Value::String(text.clone())
            }
        }
        other => other.clone(),
    }
}

fn contains_secret_marker(key: &str) -> bool {
    ["key", "token", "secret", "password"]
        .iter()
        .any(|marker| key.contains(marker))
}

// ---------------------------------------------------------------------------
// usage_method.observe 写回（§4.4）
// ---------------------------------------------------------------------------

/// 只 UPDATE usage_method 行（observation/suggestion 写入 prompt 追加段或 metadata.observations）；
/// 无对应 usage_method 行时按 capability_id 新建一行。禁止改 base/composite 稳定契约。
fn write_method_call_audit_row(
    conn: &duckdb::Connection,
    method_id: &str,
    called_by: &str,
    granted: &[String],
    subagent_id: &str,
) -> std::result::Result<String, AgentError> {
    let id = format!("mca_{}", uuid_simple());
    let called_at = now_iso();
    let granted_json = serde_json::to_string(granted)
        .map_err(|e| AgentError::Bootstrap(format!("method_call_audit serialize grants: {e}")))?;
    let result_json = serde_json::json!({"subagent_id": subagent_id, "status": "running"});
    let result_text = serde_json::to_string(&result_json)
        .map_err(|e| AgentError::Bootstrap(format!("method_call_audit serialize result: {e}")))?;
    conn.execute(
        "INSERT INTO method_call_audit \
         (id, method_id, called_at, called_by, granted_capabilities, executed_atoms, \
          state_machine_state, result, status, error) \
         VALUES (?, ?, ?, ?, CAST(? AS JSON), NULL, NULL, CAST(? AS JSON), 'running', NULL)",
        duckdb::params![
            id,
            method_id,
            called_at,
            called_by,
            granted_json,
            result_text,
        ],
    )
    .map_err(|e| AgentError::Bootstrap(format!("method_call_audit insert: {e}")))?;
    Ok(id)
}

fn write_usage_observation_row(
    conn: &duckdb::Connection,
    capability_id: &str,
    observation: &str,
    suggestion: &str,
) -> Result<(String, bool, bool)> {
    let existing = existing_usage_method_id(conn, capability_id)?;
    let observed_at = now_iso();
    match existing {
        Some(id) => {
            // 追加 prompt 段 + metadata.observations。
            let (prompt, metadata) = read_usage_method(conn, &id)?;
            let mut observations = metadata
                .get("observations")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            observations.push(serde_json::json!({
                "observation": observation,
                "suggestion": suggestion,
                "observed_at": observed_at,
            }));
            let mut new_metadata = metadata;
            new_metadata["observations"] = serde_json::Value::Array(observations);
            let is_method = new_metadata.get("full_document").is_some();
            let new_prompt = if is_method {
                // 方法文档：注册表简述保持简短，经验只追加到 metadata.observations。
                prompt
            } else {
                format!("{prompt}\n[usage.observe {observed_at}] {suggestion}")
            };
            conn.execute(
                "UPDATE usage_method SET prompt = ?, metadata = CAST(? AS JSON),                  updated_at = now() WHERE id = ?",
                duckdb::params![new_prompt, new_metadata.to_string(), id],
            )
            .map_err(|error| {
                AgentError::Bootstrap(format!(
                    "write_usage_observation_row update '{id}': {error}"
                ))
            })?;
            Ok((id, false, true))
        }
        None => {
            let digest = Sha256::digest(capability_id.as_bytes());
            let suffix: String = digest[..6]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let id = format!("um_{suffix}");
            let metadata = serde_json::json!({
                "observations": [{
                    "observation": observation,
                    "suggestion": suggestion,
                    "observed_at": observed_at,
                }]
            });
            conn.execute(
                "INSERT INTO usage_method (id, capability_id, name, prompt, examples, metadata,                  updated_at) VALUES (?, ?, ?, ?, NULL, CAST(? AS JSON), now())                  ON CONFLICT (id) DO UPDATE SET capability_id = excluded.capability_id,                  prompt = excluded.prompt, metadata = excluded.metadata, updated_at = now()",
                duckdb::params![id, capability_id, capability_id, suggestion, metadata.to_string()],
            )
            .map_err(|error| {
                AgentError::Bootstrap(format!(
                    "write_usage_observation_row insert '{id}': {error}"
                ))
            })?;
            Ok((id, true, false))
        }
    }
}

fn existing_usage_method_id(
    conn: &duckdb::Connection,
    capability_id: &str,
) -> Result<Option<String>> {
    let mut statement = conn
        .prepare("SELECT id FROM usage_method WHERE capability_id = ? ORDER BY id LIMIT 1")
        .map_err(|error| {
            AgentError::Bootstrap(format!("existing_usage_method_id prepare: {error}"))
        })?;
    let mut rows = statement
        .query_map([capability_id], |row| row.get::<_, String>(0))
        .map_err(|error| {
            AgentError::Bootstrap(format!("existing_usage_method_id query: {error}"))
        })?;
    match rows.next() {
        Some(row) => row.map(Some).map_err(|error| {
            AgentError::Bootstrap(format!("existing_usage_method_id row: {error}"))
        }),
        None => Ok(None),
    }
}

fn read_usage_method(conn: &duckdb::Connection, id: &str) -> Result<(String, serde_json::Value)> {
    let (prompt, metadata): (String, Option<String>) = conn
        .query_row(
            "SELECT prompt, CAST(metadata AS VARCHAR) FROM usage_method WHERE id = ?",
            duckdb::params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| AgentError::Bootstrap(format!("read_usage_method '{id}': {error}")))?;
    let metadata = match metadata {
        Some(text) => serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };
    Ok((prompt, metadata))
}

// ---------------------------------------------------------------------------
// 文件与工具函数
// ---------------------------------------------------------------------------

/// 原子写私有文件：目录 0700 / 文件 0600（复用 src/data/permissions.rs），临时文件 + rename。
fn atomic_write_private(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AgentError::Io(format!("atomic write: no parent for {:?}", path)))?;
    ensure_private_directory(parent)?;
    let temporary = parent.join(format!(".tmp-{}", uuid_simple()));
    std::fs::write(&temporary, content)
        .map_err(|error| AgentError::Io(format!("atomic write tmp {:?}: {error}", temporary)))?;
    secure_existing_file(&temporary)?;
    std::fs::rename(&temporary, path)
        .map_err(|error| AgentError::Io(format!("atomic write rename {:?}: {error}", path)))?;
    secure_existing_file(path)?;
    Ok(())
}

/// 初始化 subagent 记忆 / last_output 文件（0600 原子写，目录 0700）。
fn init_subagent_files(storage_root: &Path, subagent_id: &str) -> Result<()> {
    let dir = storage_root.join("subagents").join(subagent_id);
    ensure_private_directory(&dir)?;
    let memory = serde_json::json!({ "entries": [], "truncation_records": [] });
    let last_output = serde_json::json!({
        "subagent_id": subagent_id,
        "output": null,
        "updated_at": null,
    });
    atomic_write_private(
        &dir.join("memory.json"),
        &serde_json::to_string_pretty(&memory)
            .map_err(|e| AgentError::Script(format!("init memory serialize: {e}")))?,
    )?;
    atomic_write_private(
        &dir.join("last_output.json"),
        &serde_json::to_string_pretty(&last_output)
            .map_err(|e| AgentError::Script(format!("init last_output serialize: {e}")))?,
    )?;
    Ok(())
}

fn uuid_simple() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::duckdb::schema::create_all_tables;
    use std::sync::Arc;

    const TEMPLATE_NORMAL: &str = "subagent.template.normal";
    const MODEL_MINI: &str = "mini";

    struct RecordingHook {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingHook {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: Arc::clone(&events),
                },
                events,
            )
        }
    }

    impl SubagentSpawnHook for RecordingHook {
        fn notify(&self, event: SubagentSpawnEvent) {
            let text = match event {
                SubagentSpawnEvent::Created { subagent_id } => {
                    format!("created:{subagent_id}")
                }
                SubagentSpawnEvent::RunAccepted {
                    definition,
                    task_input,
                    invocation_id,
                } => format!(
                    "run:{subagent_id}:{task_input}:{invocation_id}",
                    subagent_id = definition.subagent_id
                ),
            };
            if let Ok(mut events) = self.events.lock() {
                events.push(text);
            }
        }
    }

    fn test_conn_arc() -> Arc<Mutex<duckdb::Connection>> {
        Arc::new(Mutex::new(test_conn()))
    }

    fn test_conn() -> duckdb::Connection {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        create_all_tables(&conn).unwrap();
        seed_contracts(&conn);
        conn
    }

    fn seed_contracts(conn: &duckdb::Connection) {
        conn.execute_batch(
            "INSERT INTO model (id, name, provider, api_url, api_type, api_protocol, api_key, model_id)              VALUES ('mini', 'Mini', 'test', 'http://localhost', 'OpenAI', 'openai-v1', '', 'mini');",
        )
        .unwrap();
        for (id, name) in [
            ("subagent.create", "Create Subagent"),
            ("subagent.update", "Update Subagent"),
            ("subagent.run", "Run Subagent"),
            ("subagent.sleep", "Sleep Subagent"),
            ("subagent.wake", "Wake Subagent"),
            ("subagent.delete", "Delete Subagent"),
            ("usage_method.observe", "Observe Usage Method"),
            ("file.read", "Read File"),
        ] {
            conn.execute(
                "INSERT INTO base_capability (id, name, type, description, schema_in, schema_out,                  executor, version, enabled) VALUES (?, ?, 'molecule', 'test', '{}', '{}', ?, '1.0.0', true)",
                duckdb::params![id, name, format!("builtin:{id}")],
            )
            .unwrap();
        }
        insert_template(
            conn,
            TEMPLATE_NORMAL,
            "temporary",
            &["file.read", "file.list", "path.exists", "text.grep"],
        );
    }

    fn insert_template(conn: &duckdb::Connection, id: &str, lifecycle: &str, allowlist: &[&str]) {
        let allowlist_json = serde_json::to_string(&allowlist.to_vec()).unwrap();
        let config = serde_json::json!({
            "subagent": {
                "lifecycle": lifecycle,
                "startup": "normal",
                "trigger": null,
                "memory_window_pct": 80,
                "briefing": true,
                "max_retries": 0,
                "attempt_timeout_seconds": 600,
                "total_timeout_seconds": 3600,
            }
        });
        conn.execute(
            "INSERT INTO agent (id, name, mode, prompt, capability_allowlist, config, is_default)              VALUES (?, ?, 'subagent_template', ?, CAST(? AS JSON), CAST(? AS JSON), false)",
            duckdb::params![
                id,
                id,
                format!("你是 subagent。\n任务边界：只处理分配的任务。\n能力调用规范见系统提供的统一片段。\n按声明顺序调用固定能力组，结束输出 {{\"done\": true, \"summary\": \"...\"}}。"),
                allowlist_json,
                config.to_string(),
            ],
        )
        .unwrap();
    }

    fn call(
        db: &Arc<Mutex<duckdb::Connection>>,
        storage_root: &Path,
        capability_id: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        execute_subagent_capability(db, storage_root, capability_id, &args, None)
    }

    fn create_default(
        db: &Arc<Mutex<duckdb::Connection>>,
        storage_root: &Path,
    ) -> serde_json::Value {
        call(
            db,
            storage_root,
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": MODEL_MINI,
                "name": "demo",
            }),
        )
        .expect("create should succeed")
    }

    fn with_conn<T>(
        db: &Arc<Mutex<duckdb::Connection>>,
        f: impl FnOnce(&duckdb::Connection) -> T,
    ) -> T {
        let guard = db.lock().unwrap();
        f(&guard)
    }

    fn subagent_id(output: &serde_json::Value) -> String {
        output["subagent_id"].as_str().unwrap().to_string()
    }

    fn lifecycle_of(conn: &duckdb::Connection, id: &str) -> String {
        let row = resolve_agent_row(conn, id).unwrap().unwrap();
        row.subagent_config().unwrap().lifecycle.to_string()
    }

    #[test]
    fn create_makes_idle_instance_with_memory_files() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());

        let id = subagent_id(&output);
        assert!(id.starts_with("sg_"), "id should be sg_*: {id}");
        assert_eq!(output["lifecycle"], "idle");
        assert_eq!(output["model_id"], MODEL_MINI);
        assert_eq!(output["memory_ref"], format!("subagents/{id}/memory.json"));
        assert_eq!(
            output["last_output_ref"],
            format!("subagents/{id}/last_output.json")
        );

        let row = with_conn(&db, |conn| resolve_agent_row(conn, &id).unwrap().unwrap());
        assert_eq!(row.mode, "subagent");
        assert_eq!(
            row.capability_allowlist,
            vec!["file.read", "file.list", "path.exists", "text.grep"]
        );
        let config = row.subagent_config().unwrap();
        assert_eq!(config.lifecycle, "idle");
        assert_eq!(config.template_id, TEMPLATE_NORMAL);
        assert_eq!(config.startup, SubagentStartup::Normal);

        assert!(storage_root
            .path()
            .join("subagents")
            .join(&id)
            .join("memory.json")
            .exists());
        assert!(storage_root
            .path()
            .join("subagents")
            .join(&id)
            .join("last_output.json")
            .exists());

        // invocation 已闭合为 completed。
        let result = latest_invocation_log(storage_root.path()).unwrap().unwrap();
        assert_eq!(result["capability_id"], "subagent.create");
        let invocations = storage_root.path().join("invocations");
        let result_file = std::fs::read_dir(&invocations)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .find(|name| name.ends_with(".result.json"))
            .expect("result file should exist");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(invocations.join(result_file)).unwrap()
            )
            .unwrap()["final_state"],
            "completed"
        );
    }

    #[test]
    fn create_rejects_unknown_template() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let result = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": "subagent.template.nope",
                "model_id": MODEL_MINI,
            }),
        );
        assert!(matches!(result, Err(AgentError::Parse(_))));
        // 无副作用：agent 表无新 subagent 行。
        let agents = with_conn(&db, |conn| resolve_agent_row(conn, "sg_whatever").unwrap());
        assert!(agents.is_none());
    }

    #[test]
    fn create_rejects_allowlist_superset() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let result = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": MODEL_MINI,
                "capability_allowlist": ["file.read", "shell.exec"],
            }),
        );
        assert!(result.is_err(), "superset must be rejected");
    }

    #[test]
    fn create_rejects_unknown_model() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let result = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": "no-such-model",
            }),
        );
        assert!(result.is_err());
    }

    #[test]
    fn create_uses_shrunk_allowlist_and_overrides() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": MODEL_MINI,
                "capability_allowlist": ["file.read"],
                "lifecycle": "resident",
                "startup": "scheduled",
                "trigger": {"type": "schedule", "cron": "* * * * *"},
            }),
        )
        .unwrap();
        let id = subagent_id(&output);
        assert_eq!(
            output["capability_allowlist"],
            serde_json::json!(["file.read"])
        );
        let row = with_conn(&db, |conn| resolve_agent_row(conn, &id).unwrap().unwrap());
        let config = row.subagent_config().unwrap();
        assert_eq!(config.lifecycle_kind, SubagentLifecycleKind::Resident);
        assert_eq!(config.startup, SubagentStartup::Scheduled);
        assert_eq!(
            config.trigger,
            Some(serde_json::json!({"type": "schedule", "cron": "* * * * *"}))
        );
    }

    #[test]
    fn create_prompt_overrides_template_baseline() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": MODEL_MINI,
                "prompt": "任务专属方法论：先 grep 再 read，按声明顺序执行。",
            }),
        )
        .unwrap();
        let id = subagent_id(&output);
        let row = with_conn(&db, |conn| resolve_agent_row(conn, &id).unwrap().unwrap());
        assert_eq!(
            row.prompt,
            "任务专属方法论：先 grep 再 read，按声明顺序执行。"
        );
    }

    #[test]
    fn create_prompt_defaults_to_template_baseline() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);
        let row = with_conn(&db, |conn| resolve_agent_row(conn, &id).unwrap().unwrap());
        // 缺省 prompt = 模板 prompt（继承，向后兼容）。
        let template_prompt: Option<String> = with_conn(&db, |conn| {
            conn.query_row(
                "SELECT prompt FROM agent WHERE id = ?",
                duckdb::params![TEMPLATE_NORMAL],
                |row| row.get(0),
            )
            .unwrap()
        });
        assert_eq!(row.prompt, template_prompt.unwrap());
    }

    #[test]
    fn create_rejects_empty_or_whitespace_prompt() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        for bad_prompt in ["", "   ", "\n\t "] {
            let result = call(
                &db,
                storage_root.path(),
                "subagent.create",
                serde_json::json!({
                    "template_id": TEMPLATE_NORMAL,
                    "model_id": MODEL_MINI,
                    "prompt": bad_prompt,
                }),
            );
            assert!(result.is_err(), "空/空白 prompt 应被拒绝: {bad_prompt:?}");
        }
    }

    #[test]
    fn create_rejects_non_string_prompt() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let result = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": MODEL_MINI,
                "prompt": 42,
            }),
        );
        assert!(result.is_err(), "非字符串 prompt 应被拒绝");
    }

    #[test]
    fn create_allowlist_equal_to_template_wide_set_ok() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        // 等于模板宽集边界：OK（模板宽集上限允许任意任务按需取用）。
        let result = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": MODEL_MINI,
                "capability_allowlist": ["file.read", "file.list", "path.exists", "text.grep"],
            }),
        );
        assert!(result.is_ok(), "等于模板宽集应为 OK");
    }

    #[test]
    fn parse_optional_prompt_shared_behavior() {
        // 抽取的公共校验函数：缺省 / 正常 / 空白 / 非字符串。
        assert_eq!(parse_optional_prompt(&serde_json::json!({})).unwrap(), None);
        assert_eq!(
            parse_optional_prompt(&serde_json::json!({"prompt": "p"})).unwrap(),
            Some("p".to_string())
        );
        assert!(parse_optional_prompt(&serde_json::json!({"prompt": "  "})).is_err());
        assert!(parse_optional_prompt(&serde_json::json!({"prompt": 7})).is_err());
        assert!(parse_optional_prompt(&serde_json::json!({"prompt": null})).is_err());
    }

    #[test]
    fn run_from_idle_accepts_and_persists_running_and_fires_hook() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);

        let (hook, events) = RecordingHook::new();
        let run_output = execute_subagent_capability(
            &db,
            storage_root.path(),
            "subagent.run",
            &serde_json::json!({"subagent_id": id, "task_input": "do the thing"}),
            Some(&hook),
        )
        .unwrap();
        assert_eq!(run_output["accepted"], true);
        assert_eq!(run_output["lifecycle"], "running");
        assert_eq!(with_conn(&db, |conn| lifecycle_of(conn, &id)), "running");
        let last = crate::agent::subagent_memory::read_last_output(storage_root.path(), &id)
            .unwrap()
            .expect("last_output should exist after run accepted");
        assert_eq!(last.status, "running");

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].starts_with("run:"), "got: {}", events[0]);
        assert!(events[0].contains("do the thing"));
    }

    #[test]
    fn run_without_task_input_falls_back_to_create_task_input() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": MODEL_MINI,
                "task_input": "initial task",
            }),
        )
        .expect("create should succeed");
        let id = subagent_id(&output);

        let (hook, events) = RecordingHook::new();
        let run_output = execute_subagent_capability(
            &db,
            storage_root.path(),
            "subagent.run",
            &serde_json::json!({"subagent_id": id}),
            Some(&hook),
        )
        .unwrap();
        assert_eq!(run_output["accepted"], true);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            events[0].contains("initial task"),
            "run should fallback to create task_input, got: {}",
            events[0]
        );
    }

    #[test]
    fn run_rejects_duplicate_and_sleeping_and_tombstoned() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);

        call(
            &db,
            storage_root.path(),
            "subagent.run",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        // running -> run rejected
        let result = call(
            &db,
            storage_root.path(),
            "subagent.run",
            serde_json::json!({"subagent_id": id}),
        );
        assert!(matches!(result, Err(AgentError::Parse(_))));

        // 回到 idle（runtime 收口语义用 set_subagent_lifecycle）
        with_conn(&db, |conn| {
            set_subagent_lifecycle(conn, &id, SubagentLifecycle::Idle).unwrap()
        });
        call(
            &db,
            storage_root.path(),
            "subagent.sleep",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        // sleeping -> run rejected
        let result = call(
            &db,
            storage_root.path(),
            "subagent.run",
            serde_json::json!({"subagent_id": id}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn sleep_requires_idle_or_failed() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);

        call(
            &db,
            storage_root.path(),
            "subagent.run",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        // running -> sleep rejected
        let result = call(
            &db,
            storage_root.path(),
            "subagent.sleep",
            serde_json::json!({"subagent_id": id}),
        );
        assert!(result.is_err());

        with_conn(&db, |conn| {
            set_subagent_lifecycle(conn, &id, SubagentLifecycle::Idle).unwrap()
        });
        let output = call(
            &db,
            storage_root.path(),
            "subagent.sleep",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        assert_eq!(output["lifecycle"], "sleeping");
        assert_eq!(with_conn(&db, |conn| lifecycle_of(conn, &id)), "sleeping");
    }

    #[test]
    fn wake_requires_sleeping() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);

        // idle -> wake rejected
        let result = call(
            &db,
            storage_root.path(),
            "subagent.wake",
            serde_json::json!({"subagent_id": id}),
        );
        assert!(result.is_err());

        call(
            &db,
            storage_root.path(),
            "subagent.sleep",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        let output = call(
            &db,
            storage_root.path(),
            "subagent.wake",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        assert_eq!(output["lifecycle"], "idle");
        assert_eq!(with_conn(&db, |conn| lifecycle_of(conn, &id)), "idle");
    }

    #[test]
    fn delete_soft_archives_idempotent_and_rejects_running() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);
        let memory_path = storage_root
            .path()
            .join("subagents")
            .join(&id)
            .join("memory.json");
        assert!(memory_path.exists());

        call(
            &db,
            storage_root.path(),
            "subagent.run",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        // running -> delete rejected
        let result = call(
            &db,
            storage_root.path(),
            "subagent.delete",
            serde_json::json!({"subagent_id": id}),
        );
        assert!(result.is_err());

        with_conn(&db, |conn| {
            set_subagent_lifecycle(conn, &id, SubagentLifecycle::Failed).unwrap()
        });
        let output = call(
            &db,
            storage_root.path(),
            "subagent.delete",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        assert_eq!(output["lifecycle"], "tombstoned");
        assert_eq!(output["archived"], true);
        assert!(memory_path.exists(), "memory files must be retained");
        let row = with_conn(&db, |conn| resolve_agent_row(conn, &id).unwrap().unwrap());
        assert!(row.subagent_config().unwrap().tombstoned_at.is_some());

        // tombstoned -> delete idempotent
        let output = call(
            &db,
            storage_root.path(),
            "subagent.delete",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();
        assert_eq!(output["lifecycle"], "tombstoned");
        assert_eq!(output["archived"], true);

        // tombstoned -> run rejected
        let result = call(
            &db,
            storage_root.path(),
            "subagent.run",
            serde_json::json!({"subagent_id": id}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn update_shrinks_allowlist_and_persists() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);

        let updated = call(
            &db,
            storage_root.path(),
            "subagent.update",
            serde_json::json!({
                "subagent_id": id,
                "prompt": "new role",
                "capability_allowlist": ["file.read"],
                "budget": {"max_retries": 2, "attempt_timeout_seconds": 30, "total_timeout_seconds": 90},
            }),
        )
        .unwrap();
        assert_eq!(updated["lifecycle"], "idle");
        assert_eq!(
            updated["updated_fields"],
            serde_json::json!(["prompt", "capability_allowlist", "budget"])
        );

        let row = with_conn(&db, |conn| resolve_agent_row(conn, &id).unwrap().unwrap());
        assert_eq!(row.prompt, "new role");
        assert_eq!(row.capability_allowlist, vec!["file.read".to_string()]);
        let config = row.subagent_config().unwrap();
        assert_eq!(config.budget.max_retries, 2);
        assert_eq!(config.budget.attempt_timeout_seconds, 30);
        assert_eq!(config.budget.total_timeout_seconds, 90);
    }

    #[test]
    fn update_rejects_superset_and_unknown_model() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);

        let result = call(
            &db,
            storage_root.path(),
            "subagent.update",
            serde_json::json!({
                "subagent_id": id,
                "capability_allowlist": ["file.read", "shell.exec"],
            }),
        );
        assert!(result.is_err());

        let result = call(
            &db,
            storage_root.path(),
            "subagent.update",
            serde_json::json!({
                "subagent_id": id,
                "model_id": "no-such-model",
            }),
        );
        assert!(result.is_err());
    }

    #[test]
    fn update_while_running_only_writes_persistent_row() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);
        call(
            &db,
            storage_root.path(),
            "subagent.run",
            serde_json::json!({"subagent_id": id}),
        )
        .unwrap();

        let updated = call(
            &db,
            storage_root.path(),
            "subagent.update",
            serde_json::json!({"subagent_id": id, "prompt": "persisted while running"}),
        )
        .unwrap();
        assert_eq!(updated["lifecycle"], "running");
        let row = with_conn(&db, |conn| resolve_agent_row(conn, &id).unwrap().unwrap());
        assert_eq!(row.prompt, "persisted while running");
        assert_eq!(with_conn(&db, |conn| lifecycle_of(conn, &id)), "running");
    }

    #[test]
    fn invocation_prewrite_failure_has_no_side_effects() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        // 让 invocations 路径成为文件，prewrite 必然失败。
        std::fs::write(storage_root.path().join("invocations"), b"not a dir").unwrap();
        let agent_count_before: i64 = with_conn(&db, |conn| {
            conn.query_row("SELECT COUNT(*) FROM agent", [], |row| row.get(0))
                .unwrap()
        });
        let result = call(
            &db,
            storage_root.path(),
            "subagent.create",
            serde_json::json!({
                "template_id": TEMPLATE_NORMAL,
                "model_id": MODEL_MINI,
            }),
        );
        assert!(result.is_err(), "prewrite failure must propagate as error");
        let agent_count_after: i64 = with_conn(&db, |conn| {
            conn.query_row("SELECT COUNT(*) FROM agent", [], |row| row.get(0))
                .unwrap()
        });
        assert_eq!(
            agent_count_before, agent_count_after,
            "no side effects expected"
        );
    }

    #[test]
    fn usage_observe_rejects_when_capability_not_in_latest_log() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let result = call(
            &db,
            storage_root.path(),
            "usage_method.observe",
            serde_json::json!({
                "capability_id": "file.read",
                "observation": "read was fine",
                "suggestion": "keep as is",
            }),
        );
        assert!(result.is_err(), "no latest invocation log -> must reject");
    }

    #[test]
    fn usage_observe_writes_only_usage_method_row() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        // 手动写入最新能力调用事实（file.read）。
        let invocations = storage_root.path().join("invocations");
        std::fs::create_dir_all(&invocations).unwrap();
        std::fs::write(
            invocations.join("inv_filer.json"),
            serde_json::json!({
                "invocation_id": "inv_filer",
                "capability_id": "file.read",
                "capability_name": "Read File",
                "definition_hash": "h",
                "arguments_redacted": {},
                "started_at": "2026-01-01T00:00:00.000Z",
                "task_ref": null,
            })
            .to_string(),
        )
        .unwrap();

        let base_before = with_conn(&db, |conn| {
            conn.query_row(
                "SELECT name, description, executor FROM base_capability WHERE id = 'file.read'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap()
        });
        let agent_count_before: i64 = with_conn(&db, |conn| {
            conn.query_row("SELECT COUNT(*) FROM agent", [], |row| row.get(0))
                .unwrap()
        });

        let output = call(
            &db,
            storage_root.path(),
            "usage_method.observe",
            serde_json::json!({
                "capability_id": "file.read",
                "observation": "large files take long",
                "suggestion": "chunk read for large files",
            }),
        )
        .unwrap();
        assert_eq!(output["success"], true);
        assert_eq!(output["created"], true);

        let base_after = with_conn(&db, |conn| {
            conn.query_row(
                "SELECT name, description, executor FROM base_capability WHERE id = 'file.read'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap()
        });
        assert_eq!(base_before, base_after, "stable contract must not change");
        let agent_count_after: i64 = with_conn(&db, |conn| {
            conn.query_row("SELECT COUNT(*) FROM agent", [], |row| row.get(0))
                .unwrap()
        });
        assert_eq!(agent_count_before, agent_count_after);

        let (prompt, metadata): (String, Option<String>) = with_conn(&db, |conn| {
            conn.query_row(
                "SELECT prompt, CAST(metadata AS VARCHAR) FROM usage_method WHERE capability_id = 'file.read'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        });
        assert!(prompt.contains("chunk read for large files"));
        let metadata: serde_json::Value = serde_json::from_str(&metadata.unwrap()).unwrap();
        assert_eq!(
            metadata["observations"][0]["observation"],
            "large files take long"
        );
    }

    #[test]
    fn usage_observe_updates_existing_usage_method() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        with_conn(&db, |conn| {
            conn.execute_batch(
                "INSERT INTO usage_method (id, capability_id, name, prompt)                  VALUES ('um_existing', 'file.read', 'read_lessons', 'original prompt');",
            )
            .unwrap();
        });
        let invocations = storage_root.path().join("invocations");
        std::fs::create_dir_all(&invocations).unwrap();
        std::fs::write(
            invocations.join("inv_filer.json"),
            serde_json::json!({
                "invocation_id": "inv_filer",
                "capability_id": "file.read",
                "capability_name": "Read File",
                "definition_hash": "h",
                "arguments_redacted": {},
                "started_at": "2026-01-01T00:00:00.000Z",
                "task_ref": null,
            })
            .to_string(),
        )
        .unwrap();

        let output = call(
            &db,
            storage_root.path(),
            "usage_method.observe",
            serde_json::json!({
                "capability_id": "file.read",
                "observation": "edge case",
                "suggestion": "handle edge case",
            }),
        )
        .unwrap();
        assert_eq!(output["usage_method_id"], "um_existing");
        assert_eq!(output["updated"], true);
        assert_eq!(output["created"], false);
        let (prompt, metadata): (String, Option<String>) = with_conn(&db, |conn| {
            conn.query_row(
                "SELECT prompt, CAST(metadata AS VARCHAR) FROM usage_method WHERE id = 'um_existing'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        });
        assert!(
            prompt.starts_with("original prompt"),
            "existing prompt preserved: {prompt}"
        );
        assert!(prompt.contains("handle edge case"));
        let metadata: serde_json::Value = serde_json::from_str(&metadata.unwrap()).unwrap();
        assert_eq!(
            metadata["observations"][0]["suggestion"],
            "handle edge case"
        );
    }

    #[test]
    fn redact_arguments_masks_secrets_and_truncates_long_values() {
        let long = "x".repeat(5000);
        let redacted = redact_arguments(&serde_json::json!({
            "api_key": "super-secret",
            "token": "abc",
            "content": long,
            "nested": {"password": "pw", "ok": 1},
            "normal": "short",
        }));
        assert_eq!(redacted["api_key"], "***");
        assert_eq!(redacted["token"], "***");
        assert_eq!(redacted["nested"]["password"], "***");
        assert_eq!(redacted["nested"]["ok"], 1);
        assert_eq!(redacted["normal"], "short");
        let truncated = redacted["content"].as_str().unwrap();
        assert!(truncated.ends_with("<truncated>"));
        // 2000 字符截断 + "...<truncated>" 标记（3 + 11 字符）。
        assert!(
            truncated.chars().count() <= 2000 + 14,
            "got {}",
            truncated.chars().count()
        );
    }

    #[test]
    fn unexpected_exit_marks_unclosed_invocations_once() {
        let storage_root = tempfile::tempdir().unwrap();
        let invocations = storage_root.path().join("invocations");
        std::fs::create_dir_all(&invocations).unwrap();
        std::fs::write(invocations.join("inv_leftover.json"), "{}").unwrap();
        std::fs::write(invocations.join("inv_closed.json"), "{}").unwrap();
        std::fs::write(invocations.join("inv_closed.result.json"), "{}").unwrap();

        mark_unclosed_invocations(storage_root.path()).unwrap();

        let leftover_result = invocations.join("inv_leftover.result.json");
        assert!(
            leftover_result.exists(),
            "unclosed invocation must be marked"
        );
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&leftover_result).unwrap()).unwrap();
        assert_eq!(value["final_state"], "process_unexpected_exit");
        assert!(!invocations.join("inv_closed.extra.json").exists());
    }

    #[test]
    fn method_invoke_creates_subagent_and_audit() {
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();

        // 测试用方法定义：需要 web.fetch.public。
        with_conn(&db, |conn| {
            conn.execute(
                "INSERT INTO base_capability (id, name, type, description, schema_in, schema_out, executor, version, enabled) \
                 VALUES ('web.fetch.public', 'Fetch', 'function', 'fetch', '{}', '{}', 'builtin:web.fetch.public', '1.0.0', true)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO usage_method \
                 (id, capability_id, name, prompt, examples, metadata) \
                 VALUES (?, ?, ?, ?, NULL, CAST(? AS JSON))",
                duckdb::params![
                    "um_test_fetch",
                    "web.fetch.public",
                    "Test Fetch Method",
                    "brief",
                    r#"{"full_document":"full method doc","required_capabilities":["web.fetch.public"]}"#,
                ],
            )
            .unwrap();
        });

        let output = call(
            &db,
            storage_root.path(),
            "method.invoke",
            serde_json::json!({
                "method_id": "um_test_fetch",
                "task_input": "fetch a test page",
                "model_id": MODEL_MINI,
            }),
        )
        .expect("method.invoke should succeed");

        assert_eq!(output["method_id"], "um_test_fetch");
        assert_eq!(output["status"], "running");
        assert!(output["method_call_id"].as_str().is_some());
        let subagent_id = output["subagent_id"].as_str().unwrap();

        // 子代理应已获得方法内所需能力。
        with_conn(&db, |conn| {
            let allowlist: Option<String> = conn
                .query_row(
                    "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = ?",
                    duckdb::params![subagent_id],
                    |row| row.get(0),
                )
                .unwrap();
            let arr: serde_json::Value = serde_json::from_str(&allowlist.unwrap()).unwrap();
            assert!(arr.as_array().unwrap().iter().any(|v| v == "web.fetch.public"));
        });

        // 方法调用审计应已写入。
        with_conn(&db, |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM method_call_audit WHERE method_id = 'um_test_fetch'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn memory_files_have_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let db = test_conn_arc();
        let storage_root = tempfile::tempdir().unwrap();
        let output = create_default(&db, storage_root.path());
        let id = subagent_id(&output);
        let dir = storage_root.path().join("subagents").join(&id);
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["memory.json", "last_output.json"] {
            let mode = std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "wrong permissions for {name}: {mode:o}");
        }
    }
}
