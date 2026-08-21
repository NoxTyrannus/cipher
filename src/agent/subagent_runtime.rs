//! 异步 subagent runtime（TB 运行域）。
//!
//! 流程（任务书 §12）：
//! 1. 读取 memory.json 窗口；
//! 2. 组装 prompt：模板 prompt + compose_agent_capability_prompt(可用能力表) + 当前输入 + 记忆窗口；
//! 3. 有界能力循环（max_turns 默认 8）：每轮一次 LLM，0/1/多个 capability_call 按声明顺序执行
//!    并把结果回填；Done{summary} 收口；Invalid 回填纠错提示继续；
//! 4. attempt_timeout_seconds 限制单次 provider 调用；total_timeout_seconds 限制整个 run；
//!    失败最多 max_retries 次有限重试（默认 0），超时/重试耗尽写失败事实，不无限重试；
//! 5. 运行中每 1 秒向 AgentPool touch_subagent_heartbeat；
//! 6. 结束：追加 memory 条目 + 写 last_output.json + AgentPool 状态回 idle + 生命周期 idle/failed。

use crate::agent::agent_pool::registry::AgentStatus;
use crate::agent::agent_pool::AgentPool;
use crate::agent::capability_protocol::{
    parse_capability_output, CapabilityAction, CapabilityInvocation,
};
use crate::agent::execution_types::{SubagentBudget, SubagentDefinition, SubagentLifecycle};
use crate::agent::memory::capability_agent::CapabilityCallRecord;
use crate::agent::subagent_memory::{
    append_entry, init_subagent_memory, memory_window_tokens, read_last_output_truncated,
    read_memory, write_last_output, MemoryEntry, SubagentMemory,
};
use crate::common::{AgentError, Result, UtcTimestamp};
use crate::data::duckdb::Registry;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::capability::service::{CapabilityCall, CapabilityService};
use crate::logic::model::capability::resolve_model_capability;
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::prompts::{compose_agent_capability_prompt, CapabilityPromptEntry};
use crate::logic::model::provider::{LlmProvider, LlmRequest};
use secrecy::SecretString;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// 单次 provider 调用的默认超时（秒），budget.attempt_timeout_seconds=0 时使用。
pub const DEFAULT_ATTEMPT_TIMEOUT_SECONDS: u64 = 600;
/// 整个 run 的默认总超时（秒），budget.total_timeout_seconds=0 时使用。
pub const DEFAULT_TOTAL_TIMEOUT_SECONDS: u64 = 3600;
/// 有界能力循环默认最大轮数（config.subagent 无 max_turns 时）。
pub const DEFAULT_MAX_TURNS: u32 = 8;
/// 心跳上报周期（秒），任务书 §7：subagent 实例 1 秒一次。
pub const HEARTBEAT_INTERVAL_SECONDS: u64 = 1;
/// 心跳来源标识（AgentEntry.heartbeat_source）。
pub const HEARTBEAT_SOURCE: &str = "subagent-runtime";

/// 一次异步 run 的全部输入（TC 接线用）。
pub struct SubagentRunParams {
    /// subagent.run 受理时冻结的不可变定义快照。
    pub definition: SubagentDefinition,
    /// 本轮运行输入。
    pub task_input: String,
    /// 全局 invocation 引用（仅日志/事实）。
    pub invocation_id: String,
    /// storage_root（DataPaths::storage_root），其下 subagents/<id>/ 存放记忆文件。
    pub storage_root: PathBuf,
    /// 服务层从模型注册表解析的模型行（含 API key 字段，不进入日志）。
    pub model_row: ModelRow,
    /// 服务层解析的 API key（不进入 prompt/参数/日志）。
    pub api_key: SecretString,
    /// 已选 provider。
    pub provider: Arc<dyn LlmProvider>,
    /// AgentPool 句柄（心跳/状态/生命周期上报）。
    pub pool: Arc<AgentPool>,
    /// agent/capability 注册表快照（含 subagent actor 行与可用能力）。
    pub registry: Registry,
    /// 能力执行器。
    pub executor: Arc<CapabilityExecutor>,
    /// 有界能力循环最大轮数（TC 从 config.subagent 读取；None 用默认 8）。
    pub max_turns: Option<u32>,
    /// 完成回调：run 收口（成功/失败）时调用。
    ///
    /// 本 runtime 不直接写 agent 表 SQL 收口生命周期；TC 把回调接到 TA 的
    /// set_subagent_lifecycle + invocation result 闭合（SubagentFinish 含
    /// subagent_id / outcome / invocation_id / turns / logs）。
    pub finish: Arc<dyn Fn(&SubagentFinish) + Send + Sync>,
}

/// 一次 run 的完成结果（完成回调入参）。
pub struct SubagentFinish {
    pub subagent_id: String,
    pub outcome: SubagentOutcome,
    pub invocation_id: String,
    pub turns: u32,
    pub logs: Vec<String>,
}

/// run 终态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentOutcome {
    /// 以 done.summary 收口。
    Done { summary: String },
    /// 超时/重试耗尽/轮数超限等失败（写失败事实）。
    Failed { reason: String },
}

/// 独立异步 run：立即返回 JoinHandle，不等待完成（执行中台路径不 await subagent）。
pub fn spawn_subagent(params: SubagentRunParams) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_subagent(params).await;
    })
}

/// 与 TA SubagentSpawnEvent 对齐的 runtime 事件（桥接侧）。
///
/// TA 的 spawn hook 契约（集成分支 src/logic/capability/executor.rs）：
/// pub trait SubagentSpawnHook { fn notify(&self, event: SubagentSpawnEvent); }
/// 其中 SubagentSpawnEvent 含 Created / RunAccepted 两个变体。本 worktree 无 TA 代码，
/// 此处定义等价事件类型与 RuntimeSpawnHook 桥接实现；TC 接线时以 TA 合并版为准做适配，
/// 不改 TA 文件。
#[derive(Debug, Clone)]
pub enum RuntimeSpawnEvent {
    /// subagent.create 完成（池内注册 idle 由服务层完成，runtime 不重复注册）。
    Created { subagent_id: String },
    /// subagent.run 受理：解析模型/构造 provider 并 spawn 独立 runtime。
    RunAccepted {
        definition: SubagentDefinition,
        task_input: String,
        invocation_id: String,
    },
}

/// RuntimeSpawnHook：按 RunAccepted 查 model 表 -> 解析 API key -> 选 provider -> spawn。
#[derive(Clone)]
pub struct RuntimeSpawnHook {
    pool: Arc<AgentPool>,
    registry: Arc<std::sync::Mutex<Registry>>,
    duckdb: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
    executor: Arc<CapabilityExecutor>,
    storage_root: PathBuf,
    providers: crate::logic::model::registry::ProviderRegistry,
    max_turns: Option<u32>,
    finish: Arc<dyn Fn(&SubagentFinish) + Send + Sync>,
}

impl RuntimeSpawnHook {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Arc<AgentPool>,
        registry: Registry,
        duckdb: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
        executor: Arc<CapabilityExecutor>,
        storage_root: PathBuf,
        providers: crate::logic::model::registry::ProviderRegistry,
        max_turns: Option<u32>,
        finish: Arc<dyn Fn(&SubagentFinish) + Send + Sync>,
    ) -> Self {
        Self {
            pool,
            registry: Arc::new(std::sync::Mutex::new(registry)),
            duckdb,
            executor,
            storage_root,
            providers,
            max_turns,
            finish,
        }
    }

    /// 处理一个 runtime 事件（与 TA SubagentSpawnHook::notify 语义对齐）。
    pub fn notify(&self, event: RuntimeSpawnEvent) {
        match event {
            RuntimeSpawnEvent::Created { subagent_id } => {
                tracing::debug!(
                    subagent_id = %subagent_id,
                    "subagent created（池内注册 idle 由服务层完成）"
                );
            }
            RuntimeSpawnEvent::RunAccepted {
                definition,
                task_input,
                invocation_id,
            } => {
                // 修法 1（任务书 §3.3）：异步化 spawn —— 执行中台持 duckdb 锁时若同步
                // spawn_for 会再次 lock 同一把 std Mutex 造成同线程重入死锁。
                // spawn_for 含同步阻塞的 load_all_into_memory 与锁等待，放入 spawn_blocking；
                // spawn 失败时回滚 lifecycle（running → failed）+ last_output + invocation。
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let hook = self.clone();
                    let subagent_id = definition.subagent_id.clone();
                    let invocation_id = invocation_id.clone();
                    handle.spawn_blocking(move || {
                        if let Err(e) =
                            hook.spawn_for(definition, task_input, invocation_id.clone())
                        {
                            tracing::error!("subagent spawn failed for {subagent_id}: {e}");
                            hook.rollback_spawn_failure(&subagent_id, &invocation_id, &e);
                        }
                    });
                } else {
                    // 非 tokio 上下文（单元测试）：保持同步语义
                    let subagent_id = definition.subagent_id.clone();
                    if let Err(e) = self.spawn_for(definition, task_input, invocation_id.clone()) {
                        tracing::error!("subagent spawn failed for {subagent_id}: {e}");
                        self.rollback_spawn_failure(&subagent_id, &invocation_id, &e);
                    }
                }
            }
        }
    }

    fn spawn_for(
        &self,
        definition: SubagentDefinition,
        task_input: String,
        invocation_id: String,
    ) -> Result<tokio::task::JoinHandle<()>> {
        // 每次 run 前刷新注册表，确保运行时创建的 subagent actor 行对能力服务可见。
        if let Some(duckdb) = &self.duckdb {
            if let Ok(conn) = duckdb.lock() {
                if let Ok(registry) = crate::data::duckdb::loader::load_all_into_memory(&conn) {
                    *self.registry.lock().unwrap() = registry;
                }
            }
        }
        let registry = self.registry.lock().unwrap().clone();
        let model_row = registry
            .models
            .get(&definition.model_id)
            .cloned()
            .ok_or_else(|| {
                AgentError::NotFound(format!("subagent model row: {}", definition.model_id))
            })?;
        let api_key = crate::logic::model::api_key::resolve_api_key(&model_row)?;
        let provider = self
            .providers
            .pick_by_kind(&model_row.api_type.to_lowercase())
            .cloned()
            .ok_or_else(|| {
                AgentError::NotFound(format!(
                    "subagent provider for api_type: {}",
                    model_row.api_type
                ))
            })?;
        let subagent_id = definition.subagent_id.clone();
        let handle = spawn_subagent(SubagentRunParams {
            definition,
            task_input,
            invocation_id,
            storage_root: self.storage_root.clone(),
            model_row,
            api_key,
            provider,
            pool: self.pool.clone(),
            registry,
            executor: self.executor.clone(),
            max_turns: self.max_turns,
            finish: self.finish.clone(),
        });
        tracing::info!("subagent runtime spawned for {subagent_id}");
        Ok(handle)
    }

    /// 修法 1（任务书 §3.3）：spawn 失败回滚 —— lifecycle running → failed、
    /// last_output 写失败、close_invocation(failed)，避免 lifecycle 悬挂 running
    /// （现状：悬挂后后续 run 被 "already running" 拒绝，只能手动 sleep/delete）。
    fn rollback_spawn_failure(&self, subagent_id: &str, invocation_id: &str, error: &AgentError) {
        if let Some(duckdb) = &self.duckdb {
            if let Ok(conn) = duckdb.lock() {
                if let Err(e) = crate::agent::subagent_capability::set_subagent_lifecycle(
                    &conn,
                    subagent_id,
                    SubagentLifecycle::Failed,
                ) {
                    tracing::warn!("subagent spawn rollback: set_subagent_lifecycle failed: {e}");
                }
            }
        }
        if let Err(e) = crate::agent::subagent_capability::close_subagent_invocation(
            &self.storage_root,
            invocation_id,
            "failed",
            Some(&error.to_string()),
        ) {
            tracing::warn!("subagent spawn rollback: close_subagent_invocation failed: {e}");
        }
        if let Err(e) = write_last_output(
            &self.storage_root,
            subagent_id,
            "failed",
            &format!("spawn failed: {error}"),
        ) {
            tracing::warn!("subagent spawn rollback: write_last_output failed: {e}");
        }
        let pool = Arc::clone(&self.pool);
        let subagent_id = subagent_id.to_string();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                pool.update_subagent_lifecycle(&subagent_id, SubagentLifecycle::Failed)
                    .await;
            });
        }
    }
}

/// 单次 run 完成的输出。
struct CompletedOutcome {
    summary: String,
    turns: u32,
    calls: Vec<CapabilityCallRecord>,
    logs: Vec<String>,
}

/// run 失败原因（有界重试耗尽/超时/轮数超限）。
#[derive(Debug, Clone)]
enum RunError {
    /// 单次 provider 调用超时（attempt_timeout）。
    AttemptTimeout { turn: u32 },
    /// provider 调用返回错误。
    ProviderError(String),
    /// 整个 run 超时（total_timeout）。
    TotalTimeout,
    /// 有界循环达到 max_turns 仍未 done。
    MaxTurnsExceeded,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::AttemptTimeout { turn } => {
                write!(f, "attempt timeout at turn {turn}")
            }
            RunError::ProviderError(reason) => write!(f, "provider error: {reason}"),
            RunError::TotalTimeout => write!(f, "total run timeout"),
            RunError::MaxTurnsExceeded => write!(f, "max_turns exceeded without done"),
        }
    }
}

/// run 失败收口载体：错误原因 + 失败时已累积的逐轮调用记录与日志（供 L3 取证落盘）。
#[derive(Debug)]
struct RunFailure {
    error: RunError,
    calls: Vec<CapabilityCallRecord>,
    logs: Vec<String>,
}

impl fmt::Display for RunFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

async fn run_subagent(params: SubagentRunParams) {
    let subagent_id = params.definition.subagent_id.clone();
    let _ = init_subagent_memory(&params.storage_root, &subagent_id);
    let total_timeout = total_timeout_duration(&params.definition.budget);

    // 独立心跳任务：每 1 秒向 AgentPool 主动上报，不轮询业务文件。
    let heartbeat = spawn_heartbeat(&params.pool, &subagent_id);

    let result = tokio::time::timeout(total_timeout, execute_with_retries(&params)).await;
    match result {
        Ok(Ok(outcome)) => finalize_completed(&params, &outcome).await,
        Ok(Err(failure)) => finalize_failed(&params, &failure).await,
        Err(_) => {
            finalize_failed(
                &params,
                &RunFailure {
                    error: RunError::TotalTimeout,
                    calls: Vec::new(),
                    logs: Vec::new(),
                },
            )
            .await;
        }
    }

    heartbeat.abort();
}

fn spawn_heartbeat(pool: &Arc<AgentPool>, subagent_id: &str) -> tokio::task::JoinHandle<()> {
    let pool = pool.clone();
    let subagent_id = subagent_id.to_string();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECONDS));
        loop {
            tick.tick().await;
            pool.touch_subagent_heartbeat(&subagent_id, HEARTBEAT_SOURCE)
                .await;
        }
    })
}

/// 有界重试：失败最多 max_retries 次重试（默认 0），耗尽返回最后一次失败（含当时已累积的证据）。
async fn execute_with_retries(
    params: &SubagentRunParams,
) -> std::result::Result<CompletedOutcome, RunFailure> {
    let max_retries = params.definition.budget.max_retries;
    let mut last_failure = RunFailure {
        error: RunError::ProviderError("no attempt started".to_string()),
        calls: Vec::new(),
        logs: Vec::new(),
    };
    for attempt in 0..=max_retries {
        match run_once(params).await {
            Ok(outcome) => return Ok(outcome),
            Err(failure) => {
                tracing::warn!(
                    subagent_id = %params.definition.subagent_id,
                    invocation = %params.invocation_id,
                    attempt,
                    "subagent run attempt failed: {}",
                    failure.error
                );
                last_failure = failure;
                if attempt < max_retries {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }
    Err(last_failure)
}

/// 有界能力循环的一次执行（每次重试全新开始：重读记忆、重拼 prompt）。
async fn run_once(params: &SubagentRunParams) -> std::result::Result<CompletedOutcome, RunFailure> {
    let memory =
        read_memory(&params.storage_root, &params.definition.subagent_id).unwrap_or_default();
    let system_prompt = compose_system_prompt(params, &memory);

    if params.task_input.trim().is_empty() {
        return Err(RunFailure {
            error: RunError::ProviderError(
                "subagent run messages would be empty: task_input is empty".to_string(),
            ),
            calls: Vec::new(),
            logs: Vec::new(),
        });
    }

    let mut messages = vec![
        ChatMessage::System {
            text: system_prompt,
            kind: SystemKind::Primary,
        },
        ChatMessage::User {
            text: params.task_input.clone(),
        },
    ];

    let max_turns = params.max_turns.unwrap_or(DEFAULT_MAX_TURNS).max(1);
    let attempt_timeout = attempt_timeout_duration(&params.definition.budget);
    let mut calls: Vec<CapabilityCallRecord> = Vec::new();
    let mut logs: Vec<String> = Vec::new();

    for turn in 0..max_turns {
        let request = match LlmRequest::from_model_row(
            &params.model_row,
            messages.clone(),
            params.api_key.clone(),
        ) {
            Ok(request) => request,
            Err(e) => {
                return Err(RunFailure {
                    error: RunError::ProviderError(format!("build llm request: {e}")),
                    calls,
                    logs,
                });
            }
        };

        let response =
            match tokio::time::timeout(attempt_timeout, params.provider.call(&request)).await {
                Err(_) => {
                    return Err(RunFailure {
                        error: RunError::AttemptTimeout { turn },
                        calls,
                        logs,
                    });
                }
                Ok(Err(e)) => {
                    return Err(RunFailure {
                        error: RunError::ProviderError(e.to_string()),
                        calls,
                        logs,
                    });
                }
                Ok(Ok(resp)) => resp,
            };

        match parse_capability_output(&response.content) {
            CapabilityAction::Done { summary } => {
                // T0 契约（任务书_subagent失败重试闭环_v0.3.1 §2.0）：累计调用存在失败且
                // 全程零成功时拒绝 done —— 不以 summary 收口，回填失败摘要强制下一轮重试；
                // 至少一次成功或 calls 为空则维持现状收口。
                let has_success = calls.iter().any(|c| c.ok);
                let has_failure = calls.iter().any(|c| !c.ok);
                if has_failure && !has_success {
                    let failed: Vec<String> = calls
                        .iter()
                        .filter(|c| !c.ok)
                        .map(|c| {
                            format!(
                                "{}: {}",
                                c.capability_id,
                                c.error.as_deref().unwrap_or("未知错误")
                            )
                        })
                        .collect();
                    let summary_short: String = summary.chars().take(200).collect();
                    logs.push(format!("DONE rejected (no successful call yet): {summary}"));
                    tracing::warn!(
                        subagent_id = %params.definition.subagent_id,
                        invocation = %params.invocation_id,
                        "subagent DONE rejected (no successful call yet): {summary_short}"
                    );
                    messages.push(ChatMessage::User {
                        text: format!(
                            "上一轮调用全部失败（{}）。分析错误、调整参数后重新调用，不要直接结束；除非能力永久不可用，输出明确失败说明。",
                            failed.join("；")
                        ),
                    });
                    continue;
                }
                logs.push(format!("DONE: {summary}"));
                return Ok(CompletedOutcome {
                    summary,
                    turns: turn + 1,
                    calls,
                    logs,
                });
            }
            CapabilityAction::LegacyArguments { .. } => {
                let reason = "本 subagent 拥有多个能力，调用时必须输出 capability_call/capability_calls 结构（含 capability_id 与 arguments）";
                logs.push(format!("INVALID output (turn {turn}): {reason}"));
                messages.push(ChatMessage::User {
                    text: reason.to_string(),
                });
            }
            CapabilityAction::Invalid(reason) => {
                logs.push(format!("INVALID output (turn {turn}): {reason}"));
                messages.push(ChatMessage::User {
                    text: format!(
                        "你的输出无法解析: {reason}\n只输出 JSON：capability_calls 数组（每项含 capability_id/arguments）或 done+summary。"
                    ),
                });
            }
            other => {
                let invocations = other
                    .into_calls()
                    .expect("CapabilityCall/CapabilityCalls 才能展开为调用列表");
                for invocation in invocations {
                    let record = execute_one(params, &invocation, &mut logs, &mut messages).await;
                    calls.push(record);
                }
            }
        }
    }

    logs.push(format!("EXCEEDED max_turns={max_turns}"));
    Err(RunFailure {
        error: RunError::MaxTurnsExceeded,
        calls,
        logs,
    })
}

/// 执行单个能力调用并把结果回填对话（错误回填纠错提示，不视为 run 失败）。
async fn execute_one(
    params: &SubagentRunParams,
    invocation: &CapabilityInvocation,
    logs: &mut Vec<String>,
    messages: &mut Vec<ChatMessage>,
) -> CapabilityCallRecord {
    let actor_id = &params.definition.subagent_id;
    let (capability_id, capability_name) =
        match resolve_capability_identity(&params.registry, invocation) {
            Ok(identity) => identity,
            Err(e) => {
                logs.push(format!("INVALID capability: {e}"));
                messages.push(ChatMessage::User {
                    text: format!("能力调用被拒绝: {e}\n请使用可用能力并重试，或输出 done 结束。"),
                });
                return CapabilityCallRecord {
                    capability_id: invocation.capability_id.clone(),
                    capability_name: invocation.capability_name.clone().unwrap_or_default(),
                    arguments: invocation.arguments.clone(),
                    output: serde_json::Value::Null,
                    ok: false,
                    error: Some(e),
                };
            }
        };

    let call = CapabilityCall {
        capability_id: capability_id.clone(),
        capability_name: invocation
            .capability_name
            .clone()
            .unwrap_or_else(|| capability_name.clone()),
        arguments: invocation.arguments.clone(),
    };

    let outcome = CapabilityService::new(&params.registry, &params.executor)
        .and_then(|service| service.execute_for_agent(actor_id, &call))
        .map(|result| result.output);

    match outcome {
        Ok(output) => {
            let truncated = crate::common::json_util::truncate_head_tail(&output.to_string(), 4000);
            logs.push(format!("OK {capability_id}: {truncated}"));
            messages.push(ChatMessage::Assistant {
                text: serde_json::json!({
                    "capability_call": {
                        "capability_id": capability_id,
                        "capability_name": capability_name,
                        "arguments": call.arguments,
                    }
                })
                .to_string(),
            });
            messages.push(ChatMessage::User {
                text: format!("能力 {capability_id} 执行结果: {truncated}"),
            });
            CapabilityCallRecord {
                capability_id,
                capability_name,
                arguments: call.arguments,
                output,
                ok: true,
                error: None,
            }
        }
        Err(e) => {
            logs.push(format!("FAIL {capability_id}: {e}"));
            messages.push(ChatMessage::User {
                text: format!(
                    "能力 {capability_id} 执行失败: {e}\n分析错误并调整参数重试，或输出 done 结束（说明失败原因）"
                ),
            });
            CapabilityCallRecord {
                capability_id,
                capability_name,
                arguments: call.arguments,
                output: serde_json::Value::Null,
                ok: false,
                error: Some(e.to_string()),
            }
        }
    }
}

/// 按 capability_id 解析注册表中的权威 (id, name)。
fn resolve_capability_identity(
    registry: &Registry,
    invocation: &CapabilityInvocation,
) -> std::result::Result<(String, String), String> {
    if let Some(row) = registry.base_capabilities.get(&invocation.capability_id) {
        return Ok((row.id.clone(), row.name.clone()));
    }
    if let Some(row) = registry
        .composite_capabilities
        .get(&invocation.capability_id)
    {
        return Ok((row.id.clone(), row.name.clone()));
    }
    Err(format!("未知能力 id: {}", invocation.capability_id))
}

/// 可用能力表（从冻结的 definition.capability_allowlist 解析为 LLM 可见条目）。
fn available_capabilities(
    registry: &Registry,
    definition: &SubagentDefinition,
) -> Vec<CapabilityPromptEntry> {
    let mut entries = Vec::new();
    for capability_id in &definition.capability_allowlist {
        if let Some(row) = registry.base_capabilities.get(capability_id) {
            entries.push(CapabilityPromptEntry {
                capability_id: row.id.clone(),
                capability_name: row.name.clone(),
                description: row.description.clone(),
            });
        } else if let Some(row) = registry.composite_capabilities.get(capability_id) {
            entries.push(CapabilityPromptEntry {
                capability_id: row.id.clone(),
                capability_name: row.name.clone(),
                description: row.description.clone(),
            });
        }
    }
    entries
}

/// 组装 system prompt：模板 prompt + 能力调用规范（available 非空才拼）+ 记忆窗口。
fn compose_system_prompt(params: &SubagentRunParams, memory: &SubagentMemory) -> String {
    let available = available_capabilities(&params.registry, &params.definition);
    let base = compose_agent_capability_prompt(&params.definition.prompt, &available);
    let mut prompt = base;
    prompt.push_str("\n\n## 记忆窗口\n");
    prompt.push_str(&render_memory_window(memory));
    prompt
}

fn render_memory_window(memory: &SubagentMemory) -> String {
    if memory.entries.is_empty() {
        return "(空)".to_string();
    }
    let mut lines = Vec::new();
    for (index, entry) in memory.entries.iter().enumerate() {
        let input = truncate_chars(&entry.input, 200);
        let output = truncate_chars(&entry.output, 400);
        lines.push(format!(
            "[{index}] t={} input={input} output={output} actions={} evidence={}",
            entry.t,
            entry.actions.join("; "),
            entry.evidence.join("; ")
        ));
    }
    lines.join("\n")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…")
}

fn attempt_timeout_duration(budget: &SubagentBudget) -> Duration {
    let seconds = if budget.attempt_timeout_seconds == 0 {
        DEFAULT_ATTEMPT_TIMEOUT_SECONDS
    } else {
        budget.attempt_timeout_seconds
    };
    Duration::from_secs(seconds)
}

fn total_timeout_duration(budget: &SubagentBudget) -> Duration {
    let seconds = if budget.total_timeout_seconds == 0 {
        DEFAULT_TOTAL_TIMEOUT_SECONDS
    } else {
        budget.total_timeout_seconds
    };
    Duration::from_secs(seconds)
}

/// 记忆窗口（token）= 模型 context_window * 80%。
fn memory_window(params: &SubagentRunParams) -> usize {
    let context_window = resolve_model_capability(&params.model_row).context_window;
    memory_window_tokens(context_window)
}

/// 完成收口：追加记忆 + 写 last_output + AgentPool 状态 idle + 生命周期 idle。
async fn finalize_completed(params: &SubagentRunParams, outcome: &CompletedOutcome) {
    let subagent_id = &params.definition.subagent_id;
    let entry = MemoryEntry {
        t: UtcTimestamp::now().to_string(),
        input: params.task_input.clone(),
        actions: outcome
            .calls
            .iter()
            .map(|call| {
                format!(
                    "capability_id={} status={}",
                    call.capability_id,
                    if call.ok { "OK" } else { "FAIL" }
                )
            })
            .collect(),
        evidence: outcome.logs.clone(),
        output: outcome.summary.clone(),
    };
    let window = memory_window(params);
    if let Err(e) = append_entry(&params.storage_root, subagent_id, entry, window) {
        tracing::warn!(subagent_id = %subagent_id, "subagent append memory failed: {e}");
    }
    if let Err(e) = write_last_output(
        &params.storage_root,
        subagent_id,
        "completed",
        &outcome.summary,
    ) {
        tracing::warn!(subagent_id = %subagent_id, "subagent write last_output failed: {e}");
    }

    params
        .pool
        .update_subagent_lifecycle(subagent_id, SubagentLifecycle::Idle)
        .await;
    params
        .pool
        .update_subagent_status(subagent_id, AgentStatus::Idle)
        .await;
    let truncated =
        read_last_output_truncated(&params.storage_root, subagent_id, window).unwrap_or(None);
    params
        .pool
        .set_subagent_last_output(subagent_id, truncated)
        .await;

    // 异步结果回传：通知主循环 subagent 已完成，触发新一轮思考/echo。
    if let Err(e) = params
        .pool
        .send_trigger(subagent_id, "subagent_complete")
        .await
    {
        tracing::warn!(subagent_id = %subagent_id, "subagent complete trigger failed: {e}");
    }

    // 完成回调：TC 接到 TA 的 set_subagent_lifecycle + invocation result 闭合（持久生命周期）。
    (params.finish)(&SubagentFinish {
        subagent_id: subagent_id.clone(),
        outcome: SubagentOutcome::Done {
            summary: outcome.summary.clone(),
        },
        invocation_id: params.invocation_id.clone(),
        turns: outcome.turns,
        logs: outcome.logs.clone(),
    });

    tracing::info!(
        subagent_id = %subagent_id,
        invocation = %params.invocation_id,
        turns = outcome.turns,
        "subagent run completed"
    );
}

/// 失败收口：失败事实进 memory（含逐轮 calls/logs 证据）+ last_output，生命周期 failed（AgentPool 以 idle 身份保留）。
async fn finalize_failed(params: &SubagentRunParams, failure: &RunFailure) {
    let subagent_id = &params.definition.subagent_id;
    let message = failure.error.to_string();
    let entry = MemoryEntry {
        t: UtcTimestamp::now().to_string(),
        input: params.task_input.clone(),
        actions: failure
            .calls
            .iter()
            .map(|call| {
                format!(
                    "capability_id={} status={}",
                    call.capability_id,
                    if call.ok { "OK" } else { "FAIL" }
                )
            })
            .collect(),
        evidence: if failure.logs.is_empty() {
            vec![format!("FAIL subagent.run: {message}")]
        } else {
            failure.logs.clone()
        },
        output: format!("failed: {message}"),
    };
    let window = memory_window(params);
    if let Err(e) = append_entry(&params.storage_root, subagent_id, entry, window) {
        tracing::warn!(subagent_id = %subagent_id, "subagent append failure memory failed: {e}");
    }
    let failure_summary = format!("failed: {message}");
    if let Err(e) = write_last_output(
        &params.storage_root,
        subagent_id,
        "failed",
        &failure_summary,
    ) {
        tracing::warn!(subagent_id = %subagent_id, "subagent write failure last_output failed: {e}");
    }

    params
        .pool
        .update_subagent_lifecycle(subagent_id, SubagentLifecycle::Failed)
        .await;
    // failed 实例以 idle 身份保留在池（供快照展示与后续 run/update/delete）。
    params
        .pool
        .update_subagent_status(subagent_id, AgentStatus::Idle)
        .await;
    let truncated =
        read_last_output_truncated(&params.storage_root, subagent_id, window).unwrap_or(None);
    params
        .pool
        .set_subagent_last_output(subagent_id, truncated)
        .await;

    // 异步结果回传：失败也通知主循环，便于把失败原因带给用户。
    if let Err(e) = params
        .pool
        .send_trigger(subagent_id, "subagent_complete")
        .await
    {
        tracing::warn!(subagent_id = %subagent_id, "subagent complete trigger failed: {e}");
    }

    // 完成回调：TC 接到 TA 的 set_subagent_lifecycle(failed) + invocation result 失败闭合。
    (params.finish)(&SubagentFinish {
        subagent_id: subagent_id.clone(),
        outcome: SubagentOutcome::Failed {
            reason: message.clone(),
        },
        invocation_id: params.invocation_id.clone(),
        turns: 0,
        logs: Vec::new(),
    });

    tracing::warn!(
        subagent_id = %subagent_id,
        invocation = %params.invocation_id,
        "subagent run failed: {message}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::execution_types::{SubagentLifecycleKind, SubagentStartup};
    use crate::agent::subagent_memory::{read_last_output, read_memory};
    use crate::data::duckdb::loader::{AgentRow, BaseCapabilityRow};
    use crate::logic::capability::base::BaseCapability;
    use crate::logic::model::provider::LlmResponse;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ---- mock provider ----

    struct ScriptedProvider {
        responses: Arc<Mutex<VecDeque<String>>>,
        sleep: Option<Duration>,
        calls: Arc<AtomicUsize>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<String>, sleep: Option<Duration>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from(responses))),
                sleep,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        fn id(&self) -> &'static str {
            "scripted"
        }
        fn name(&self) -> &'static str {
            "Scripted"
        }
        async fn call(&self, _req: &LlmRequest) -> crate::common::Result<LlmResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(duration) = self.sleep {
                tokio::time::sleep(duration).await;
            }
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"done": true, "summary": "ok"}"#.to_string());
            Ok(LlmResponse {
                content,
                usage: None,
            })
        }
    }

    // ---- recording capability（在 executor test registry 中按 executor 名注册）----

    struct FailingProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LlmProvider for FailingProvider {
        fn id(&self) -> &'static str {
            "failing"
        }
        fn name(&self) -> &'static str {
            "Failing"
        }
        async fn call(&self, _req: &LlmRequest) -> crate::common::Result<LlmResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(crate::common::AgentError::Llm(
                "mock provider failure".to_string(),
            ))
        }
    }

    struct RecordingCap {
        id: &'static str,
        order: Arc<Mutex<Vec<String>>>,
    }

    impl BaseCapability for RecordingCap {
        fn id(&self) -> &'static str {
            self.id
        }
        fn name(&self) -> &'static str {
            "Recording"
        }
        fn execute(
            &self,
            input: &crate::logic::capability::base::Schema,
        ) -> crate::common::Result<crate::logic::capability::base::Schema> {
            self.order.lock().unwrap().push(self.id.to_string());
            Ok(serde_json::json!({"value": self.id, "echo": input.clone()}))
        }
    }

    /// 能力存在但执行必失败（走 execute_for_agent 失败 → logs "FAIL {capability_id}: ..." 路径）。
    struct FailingCap {
        id: &'static str,
    }

    impl BaseCapability for FailingCap {
        fn id(&self) -> &'static str {
            self.id
        }
        fn name(&self) -> &'static str {
            "Failing"
        }
        fn execute(
            &self,
            _input: &crate::logic::capability::base::Schema,
        ) -> crate::common::Result<crate::logic::capability::base::Schema> {
            Err(crate::common::AgentError::Script(
                "probe failure".to_string(),
            ))
        }
    }

    // ---- fixtures ----

    fn test_registry() -> Registry {
        let mut reg = Registry::new();
        reg.agents.insert(
            "sg_test".to_string(),
            AgentRow {
                id: "sg_test".to_string(),
                name: "SG".to_string(),
                mode: "subagent".to_string(),
                prompt: Some("你是测试 subagent。".to_string()),
                capability_allowlist: vec!["probe.a".to_string(), "probe.b".to_string()],
                config: None,
                display_name: None,
                is_default: false,
            },
        );
        for (id, name, executor) in [
            ("probe.a", "Probe A", "test.probe.a"),
            ("probe.b", "Probe B", "test.probe.b"),
            ("probe.c", "Probe C", "test.probe.c"),
        ] {
            reg.base_capabilities.insert(
                id.to_string(),
                BaseCapabilityRow {
                    id: id.to_string(),
                    name: name.to_string(),
                    cap_type: "function".to_string(),
                    description: format!("{name} 探针"),
                    schema_in: serde_json::json!({}),
                    schema_out: serde_json::json!({}),
                    executor: executor.to_string(),
                    version: "1.0.0".to_string(),
                    enabled: true,
                    tombstoned_at: None,
                    metadata: None,
                },
            );
        }
        reg
    }

    fn test_executor(order: Arc<Mutex<Vec<String>>>) -> CapabilityExecutor {
        let mut executor = CapabilityExecutor::new();
        executor.register(Arc::new(RecordingCap {
            id: "test.probe.a",
            order: order.clone(),
        }));
        executor.register(Arc::new(RecordingCap {
            id: "test.probe.b",
            order,
        }));
        executor.register(Arc::new(FailingCap { id: "test.probe.c" }));
        executor
    }

    fn make_model_row() -> ModelRow {
        ModelRow {
            id: "model-test".to_string(),
            name: "Test".to_string(),
            provider: "test".to_string(),
            api_url: "https://example.com/v1".to_string(),
            api_protocol: "openai-v1".to_string(),
            api_type: "openai".to_string(),
            model_id: "mini-mock".to_string(),
            api_key: Some("sk-test".to_string()),
            config: Some(serde_json::json!({"context_window": 4096})),
        }
    }

    fn test_definition() -> SubagentDefinition {
        SubagentDefinition {
            subagent_id: "sg_test".to_string(),
            prompt: "你是测试 subagent，按能力协议执行。".to_string(),
            capability_allowlist: vec!["probe.a".to_string(), "probe.b".to_string()],
            model_id: "model-test".to_string(),
            budget: SubagentBudget {
                max_retries: 0,
                attempt_timeout_seconds: 600,
                total_timeout_seconds: 3600,
            },
            startup: SubagentStartup::Normal,
            trigger: None,
        }
    }

    fn runtime_state(
        lifecycle: SubagentLifecycle,
    ) -> crate::agent::execution_types::SubagentRuntimeState {
        crate::agent::execution_types::SubagentRuntimeState {
            subagent_id: "sg_test".to_string(),
            lifecycle,
            last_output_truncated: None,
            trigger: None,
            startup: SubagentStartup::Normal,
            lifecycle_kind: SubagentLifecycleKind::Temporary,
        }
    }

    fn noop_finish() -> Arc<dyn Fn(&SubagentFinish) + Send + Sync> {
        Arc::new(|_f| {})
    }

    /// 记录 finish 回调（断言 Done/Failed 结果与 invocation 关联）。
    #[allow(clippy::type_complexity)]
    fn recorded_finish() -> (
        Arc<dyn Fn(&SubagentFinish) + Send + Sync>,
        Arc<Mutex<Vec<SubagentFinish>>>,
    ) {
        let recorded: Arc<Mutex<Vec<SubagentFinish>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        (
            Arc::new(move |f: &SubagentFinish| {
                rec.lock().unwrap().push(SubagentFinish {
                    subagent_id: f.subagent_id.clone(),
                    outcome: f.outcome.clone(),
                    invocation_id: f.invocation_id.clone(),
                    turns: f.turns,
                    logs: f.logs.clone(),
                });
            }),
            recorded,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn make_params(
        definition: SubagentDefinition,
        storage_root: std::path::PathBuf,
        provider: Arc<dyn LlmProvider>,
        pool: Arc<AgentPool>,
        registry: Registry,
        executor: Arc<CapabilityExecutor>,
        max_turns: Option<u32>,
        finish: Arc<dyn Fn(&SubagentFinish) + Send + Sync>,
    ) -> SubagentRunParams {
        SubagentRunParams {
            definition,
            task_input: "任务输入".to_string(),
            invocation_id: "inv-test".to_string(),
            storage_root,
            model_row: make_model_row(),
            api_key: SecretString::new("sk-test".to_string()),
            provider,
            pool,
            registry,
            executor,
            max_turns,
            finish,
        }
    }

    async fn register_running(pool: &Arc<AgentPool>) {
        pool.register_subagent(
            runtime_state(SubagentLifecycle::Running),
            AgentStatus::Running,
        )
        .await;
    }

    // ---- tests ----

    #[tokio::test]
    async fn runtime_executes_multiple_capabilities_in_declaration_order() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let registry = test_registry();
        let executor = Arc::new(test_executor(order.clone()));
        let definition = test_definition();

        let responses = vec![
            r#"{"capability_calls": [{"capability_id": "probe.a"}, {"capability_id": "probe.b"}]}"#
                .to_string(),
            r#"{"done": true, "summary": "finished both"}"#.to_string(),
        ];
        let provider = Arc::new(ScriptedProvider::new(responses, None));
        let params = make_params(
            definition,
            storage.clone(),
            provider,
            pool.clone(),
            registry,
            executor,
            Some(8),
            noop_finish(),
        );
        spawn_subagent(params).await.unwrap();

        // 多调用按声明顺序执行。
        assert_eq!(*order.lock().unwrap(), vec!["test.probe.a", "test.probe.b"]);

        // 池状态回 idle + 生命周期 idle + last_output 截断。
        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Idle);
        assert_eq!(
            states[0].last_output_truncated.as_deref(),
            Some("finished both")
        );
        assert_eq!(
            pool.snapshot()
                .await
                .into_iter()
                .find(|e| e.id == "sg_test")
                .unwrap()
                .status,
            AgentStatus::Idle
        );

        // last_output.json + memory.json。
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.status, "completed");
        assert_eq!(last.summary, "finished both");
        let memory = read_memory(&storage, "sg_test").unwrap();
        assert_eq!(memory.entries.len(), 1);
        assert_eq!(memory.entries[0].actions.len(), 2);
        assert!(memory.entries[0]
            .actions
            .iter()
            .any(|a| a.contains("capability_id=probe.a status=OK")));
        assert!(memory.entries[0]
            .evidence
            .iter()
            .any(|line| line.contains("DONE: finished both")));
    }

    #[tokio::test]
    async fn runtime_attempt_timeout_leads_to_failed_terminal() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let mut definition = test_definition();
        definition.budget = SubagentBudget {
            max_retries: 0,
            attempt_timeout_seconds: 1,
            total_timeout_seconds: 10,
        };
        let provider = Arc::new(ScriptedProvider::new(
            vec![r#"{"done": true, "summary": "too late"}"#.to_string()],
            Some(Duration::from_secs(5)),
        ));
        let params = make_params(
            definition,
            storage.clone(),
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(8),
            noop_finish(),
        );
        spawn_subagent(params).await.unwrap();

        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Failed);
        // failed 实例以 idle 身份保留在池。
        assert_eq!(
            pool.snapshot()
                .await
                .into_iter()
                .find(|e| e.id == "sg_test")
                .unwrap()
                .status,
            AgentStatus::Idle
        );
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.status, "failed");
        assert!(
            last.summary.contains("attempt timeout"),
            "got: {}",
            last.summary
        );
        let memory = read_memory(&storage, "sg_test").unwrap();
        assert!(memory
            .entries
            .last()
            .unwrap()
            .evidence
            .iter()
            .any(|line| line.contains("FAIL subagent.run")));
    }

    #[tokio::test]
    async fn runtime_total_timeout_leads_to_failed_terminal() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let mut definition = test_definition();
        definition.budget = SubagentBudget {
            max_retries: 0,
            attempt_timeout_seconds: 10,
            total_timeout_seconds: 1,
        };
        let provider = Arc::new(ScriptedProvider::new(
            vec![r#"{"done": true, "summary": "too late"}"#.to_string()],
            Some(Duration::from_secs(5)),
        ));
        let params = make_params(
            definition,
            storage.clone(),
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(8),
            noop_finish(),
        );
        spawn_subagent(params).await.unwrap();

        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Failed);
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.status, "failed");
        assert!(
            last.summary.contains("total run timeout"),
            "got: {}",
            last.summary
        );
    }

    #[tokio::test]
    async fn runtime_max_retries_exhausted_failed_terminal() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let mut definition = test_definition();
        definition.budget = SubagentBudget {
            max_retries: 2,
            attempt_timeout_seconds: 10,
            total_timeout_seconds: 30,
        };
        // 每次 provider 调用都失败：初始 1 次 + 2 次重试 = 3 次调用，全部失败 -> failed 终态。
        let provider = Arc::new(FailingProvider {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let provider_calls = provider.calls.clone();
        let params = make_params(
            definition,
            storage.clone(),
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(1),
            noop_finish(),
        );
        spawn_subagent(params).await.unwrap();

        // 初始 1 次 + 2 次重试 = 3 次 provider 调用。
        assert_eq!(provider_calls.load(Ordering::SeqCst), 3);

        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Failed);
        assert_eq!(
            pool.snapshot()
                .await
                .into_iter()
                .find(|e| e.id == "sg_test")
                .unwrap()
                .status,
            AgentStatus::Idle
        );
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.status, "failed");
    }

    #[tokio::test]
    async fn runtime_heartbeat_observable_in_pool() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        // 3 轮 × 500ms ≈ 1.5s，跨过 1 秒心跳周期。
        let responses = vec![
            r#"{"capability_calls": [{"capability_id": "probe.a"}]}"#.to_string(),
            r#"{"capability_calls": [{"capability_id": "probe.b"}]}"#.to_string(),
            r#"{"done": true, "summary": "heartbeat ok"}"#.to_string(),
        ];
        let provider = Arc::new(ScriptedProvider::new(
            responses,
            Some(Duration::from_millis(500)),
        ));
        let params = make_params(
            test_definition(),
            storage,
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(8),
            noop_finish(),
        );
        spawn_subagent(params).await.unwrap();

        let entry = pool
            .snapshot()
            .await
            .into_iter()
            .find(|e| e.id == "sg_test")
            .expect("entry present");
        assert_eq!(
            entry.heartbeat_source.as_deref(),
            Some("subagent-runtime"),
            "心跳必须可观测（运行时主动上报）"
        );
        assert_eq!(entry.status, AgentStatus::Idle);
    }

    #[tokio::test]
    async fn runtime_invalid_output_fed_back_then_done() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let responses = vec![
            r#"{"foo": 1}"#.to_string(),
            r#"{"done": true, "summary": "recovered"}"#.to_string(),
        ];
        let provider = Arc::new(ScriptedProvider::new(responses, None));
        let params = make_params(
            test_definition(),
            storage.clone(),
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(8),
            noop_finish(),
        );
        spawn_subagent(params).await.unwrap();

        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Idle);
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.summary, "recovered");
    }

    #[tokio::test]
    async fn runtime_max_turns_exceeded_is_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        // 每轮都 Invalid；max_turns=2 -> 有界循环耗尽，无 done -> 失败。
        let responses = vec![r#"{"foo": 1}"#.to_string(), r#"{"foo": 1}"#.to_string()];
        let provider = Arc::new(ScriptedProvider::new(responses, None));
        let provider_calls = provider.calls.clone();
        let params = make_params(
            test_definition(),
            storage.clone(),
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(2),
            noop_finish(),
        );
        spawn_subagent(params).await.unwrap();

        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Failed);
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.status, "failed");
        assert!(last.summary.contains("max_turns"), "got: {}", last.summary);
    }

    #[tokio::test]
    async fn runtime_finish_callback_fired_with_done_outcome() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let (finish, recorded) = recorded_finish();
        let responses = vec![r#"{"done": true, "summary": "persisted"}"#.to_string()];
        let provider = Arc::new(ScriptedProvider::new(responses, None));
        let params = make_params(
            test_definition(),
            storage,
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(8),
            finish,
        );
        spawn_subagent(params).await.unwrap();

        let calls = recorded.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].subagent_id, "sg_test");
        assert_eq!(calls[0].invocation_id, "inv-test");
        assert_eq!(
            calls[0].outcome,
            SubagentOutcome::Done {
                summary: "persisted".to_string()
            }
        );
        assert_eq!(calls[0].turns, 1);
    }

    #[tokio::test]
    async fn runtime_finish_callback_fired_with_failed_outcome() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let mut definition = test_definition();
        definition.budget = SubagentBudget {
            max_retries: 0,
            attempt_timeout_seconds: 10,
            total_timeout_seconds: 30,
        };
        let (finish, recorded) = recorded_finish();
        // 每轮都 Invalid；max_turns=2 -> 有界循环耗尽 -> 失败终态。
        let responses = vec![r#"{"foo": 1}"#.to_string(), r#"{"foo": 1}"#.to_string()];
        let provider = Arc::new(ScriptedProvider::new(responses, None));
        let params = make_params(
            definition,
            storage,
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(2),
            finish,
        );
        spawn_subagent(params).await.unwrap();

        let calls = recorded.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].subagent_id, "sg_test");
        match &calls[0].outcome {
            SubagentOutcome::Failed { reason } => {
                assert!(reason.contains("max_turns"), "got: {reason}");
            }
            other => panic!("expected Failed outcome, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn done_after_all_failed_is_rejected_then_retry_succeeds() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let (finish, recorded) = recorded_finish();
        // turn0: 未知能力 -> 调用失败；turn1: 模型输出 done 被拒绝（回填强制重试）；
        // turn2: 重试成功；turn3: done 正常收口。
        let responses = vec![
            r#"{"capability_calls": [{"capability_id": "unknown.cap"}]}"#.to_string(),
            r#"{"done": true, "summary": "give up"}"#.to_string(),
            r#"{"capability_calls": [{"capability_id": "probe.a"}]}"#.to_string(),
            r#"{"done": true, "summary": "retry succeeded"}"#.to_string(),
        ];
        let provider = Arc::new(ScriptedProvider::new(responses, None));
        let params = make_params(
            test_definition(),
            storage.clone(),
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(8),
            finish,
        );
        spawn_subagent(params).await.unwrap();

        // 拒绝 done 后强制重试成功：turns>=3、summary 为最终成功收口。
        {
            let calls = recorded.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert!(
                calls[0].turns >= 3,
                "turns must be >= 3, got: {}",
                calls[0].turns
            );
            assert_eq!(
                calls[0].outcome,
                SubagentOutcome::Done {
                    summary: "retry succeeded".to_string()
                }
            );
        }

        // 最终 calls 含成功记录（memory actions），失败记录与拒绝痕迹都进 evidence。
        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Idle);
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.summary, "retry succeeded");
        let memory = read_memory(&storage, "sg_test").unwrap();
        let actions = &memory.entries[0].actions;
        assert!(
            actions
                .iter()
                .any(|a| a.contains("capability_id=probe.a status=OK")),
            "calls must contain a success record, got: {actions:?}"
        );
        assert!(
            actions
                .iter()
                .any(|a| a.contains("capability_id=unknown.cap status=FAIL")),
            "calls must contain the failure record, got: {actions:?}"
        );
        assert!(memory.entries[0]
            .evidence
            .iter()
            .any(|line| line.contains("DONE rejected")));
    }

    #[tokio::test]
    async fn done_after_successful_call_still_accepted() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        let (finish, recorded) = recorded_finish();
        // turn0: probe.a 成功；turn1: done -> 正常收口（回归保护：有成功调用后 done 不被拒绝）。
        let responses = vec![
            r#"{"capability_calls": [{"capability_id": "probe.a"}]}"#.to_string(),
            r#"{"done": true, "summary": "finished"}"#.to_string(),
        ];
        let provider = Arc::new(ScriptedProvider::new(responses, None));
        let params = make_params(
            test_definition(),
            storage.clone(),
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(8),
            finish,
        );
        spawn_subagent(params).await.unwrap();

        {
            let calls = recorded.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].turns, 2);
            assert_eq!(
                calls[0].outcome,
                SubagentOutcome::Done {
                    summary: "finished".to_string()
                }
            );
        }
        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Idle);
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.summary, "finished");
    }

    #[tokio::test]
    async fn all_failed_done_exhausts_max_turns_fails() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = temporary.path().to_path_buf();
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        register_running(&pool).await;

        // 每轮「失败调用 -> done」；max_turns=2 -> turn1 的 done 被拒绝后轮数耗尽 -> 失败收口。
        let responses = vec![
            r#"{"capability_calls": [{"capability_id": "probe.c"}]}"#.to_string(),
            r#"{"done": true, "summary": "give up"}"#.to_string(),
        ];
        let provider = Arc::new(ScriptedProvider::new(responses, None));
        let provider_calls = provider.calls.clone();
        let params = make_params(
            test_definition(),
            storage.clone(),
            provider,
            pool.clone(),
            test_registry(),
            Arc::new(test_executor(Arc::new(Mutex::new(Vec::new())))),
            Some(2),
            noop_finish(),
        );
        spawn_subagent(params).await.unwrap();

        // 2 次 provider 调用（失败调用 + 被拒绝的 done），无第三次。
        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        let states = pool.subagent_states().await;
        assert_eq!(states[0].lifecycle, SubagentLifecycle::Failed);
        let last = read_last_output(&storage, "sg_test").unwrap().unwrap();
        assert_eq!(last.status, "failed");
        assert!(last.summary.contains("max_turns"), "got: {}", last.summary);

        // 失败收口证据落盘（Q5）：evidence 含逐轮 logs（FAIL 调用 + DONE rejected + 轮数耗尽），
        // actions 含逐轮调用摘要 —— 失败轮证据可在 L3 取证。
        let memory = read_memory(&storage, "sg_test").unwrap();
        let entry = memory.entries.last().unwrap();
        let evidence = entry.evidence.join("\n");
        assert!(
            evidence.contains("FAIL probe.c"),
            "evidence must keep per-turn FAIL log, got: {evidence}"
        );
        assert!(
            evidence.contains("DONE rejected"),
            "evidence must keep DONE rejected log, got: {evidence}"
        );
        assert!(
            evidence.contains("EXCEEDED max_turns=2"),
            "evidence must keep max_turns exhaustion log, got: {evidence}"
        );
        assert!(
            entry
                .actions
                .iter()
                .any(|a| a.contains("capability_id=probe.c status=FAIL")),
            "actions must keep per-turn call summary, got: {:?}",
            entry.actions
        );
    }
}
