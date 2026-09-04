//! v0.3.1 执行中台：无记忆的 subagent 生命周期管理器。
//!
//! 每轮处理：
//! 1. 读 TurnContext；
//! 2. 组装执行中台上下文（角色 prompt + capability_call 片段 + 本轮输入 +
//!    AgentPool/subagent 状态 + subagent 模板 + model 表元信息）；
//! 3. 恰好一次 LLM 调用，解析 task_design/task_status + capability_calls 数组；
//! 4. 通过 CapabilityService 按声明顺序执行能力；解析失败/LLM 失败 fail-closed，不重试；
//! 5. 同步 AgentPool 的 subagent 展示状态，生成新 ExecutionOutput，发 ExecutionDone。

use crate::agent::agent_pool::AgentPool;
use crate::agent::communication::{
    AgentMessage, CapabilityLifecycleRecord, CapabilityLifecycleState, ExecutionOutput,
    SubagentLifecycle, SubagentRuntimeState,
};
use crate::common::{AgentError, Result};
use crate::data::duckdb::Registry;
use crate::data::ModelRow;
use crate::logic::capability::service::{CapabilityCall, CapabilityService};
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::prompts::{
    compose_agent_capability_prompt, read_platform_prompt, CapabilityPromptEntry,
};
use crate::logic::model::provider::{LlmProvider, LlmRequest};
use secrecy::SecretString;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ExecutionPlatformRawOutput {
    task_design: Option<String>,
    task_status: Option<String>,
    capability_calls: Vec<RawCapabilityCall>,
    /// 坏调用证据：capability_id 非字符串/空的项原文、capability_calls 非数组的字段
    /// 原文（截断 200 字符），逐项提取时跳过并记入；组装 ExecutionOutput 时并入
    /// task_status 供洞察可读。
    bad_calls: Vec<String>,
}

const EXECUTION_INSTRUCTION: &str = "现在做一轮 subagent 生命周期管理。输出 JSON。";

#[derive(Debug, Clone, Deserialize)]
struct RawCapabilityCall {
    capability_id: String,
    #[serde(default)]
    capability_name: Option<String>,
    #[serde(default)]
    arguments: serde_json::Value,
}

/// 把 TA 的 `SubagentSpawnHook` 事件直接转接到 TB 的 `RuntimeSpawnHook`。
/// AgentPool 状态同步由执行中台在每轮 capability_calls 执行完后统一 refresh。
pub struct SubagentRuntimeBridge {
    inner: crate::agent::subagent_runtime::RuntimeSpawnHook,
}

impl SubagentRuntimeBridge {
    pub fn new(inner: crate::agent::subagent_runtime::RuntimeSpawnHook) -> Self {
        Self { inner }
    }
}

impl crate::logic::capability::executor::SubagentSpawnHook for SubagentRuntimeBridge {
    fn notify(&self, event: crate::logic::capability::executor::SubagentSpawnEvent) {
        match event {
            crate::logic::capability::executor::SubagentSpawnEvent::Created { subagent_id } => {
                self.inner
                    .notify(crate::agent::subagent_runtime::RuntimeSpawnEvent::Created {
                        subagent_id,
                    });
            }
            crate::logic::capability::executor::SubagentSpawnEvent::RunAccepted {
                definition,
                task_input,
                invocation_id,
            } => {
                self.inner.notify(
                    crate::agent::subagent_runtime::RuntimeSpawnEvent::RunAccepted {
                        definition,
                        task_input,
                        invocation_id,
                    },
                );
            }
        }
    }
}

pub struct ExecutionPlatform {
    execution_rx: mpsc::Receiver<AgentMessage>,
    pool: Arc<AgentPool>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    prompts_dir: Option<PathBuf>,
    registry: Option<Registry>,
    executor: Option<Arc<crate::logic::capability::executor::CapabilityExecutor>>,
    duckdb: Option<Arc<Mutex<duckdb::Connection>>>,
    storage_root: Option<PathBuf>,
    /// 机制式排队合并开关（v0.4.7）：true=批=连续处理组（逐轮产物逐轮执行/触发，
    /// 飞行缓冲消除队列空隙）；false=完全回退逐条现状。
    merge_enabled: bool,
}

impl ExecutionPlatform {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_rx: mpsc::Receiver<AgentMessage>,
        pool: Arc<AgentPool>,
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: SecretString,
        prompts_dir: Option<PathBuf>,
        registry: Option<Registry>,
        executor: Option<Arc<crate::logic::capability::executor::CapabilityExecutor>>,
        duckdb: Option<Arc<Mutex<duckdb::Connection>>>,
        storage_root: Option<PathBuf>,
        merge_enabled: bool,
    ) -> Self {
        Self {
            execution_rx,
            pool,
            provider,
            model_row,
            api_key,
            prompts_dir,
            registry,
            executor,
            duckdb,
            storage_root,
            merge_enabled,
        }
    }

    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!(
                "execution_platform: started, polling rx (merge_enabled={})",
                self.merge_enabled
            );
            let heartbeat = AgentPool::spawn_core_heartbeat(
                &self.pool,
                "execution-platform",
                "execution-platform",
            );
            if self.merge_enabled {
                // v0.4.7 机制式合并：批 = 连续处理组；批内逐轮 handle_execute（每轮独立
                // LLM 调用、独立 ExecutionOutput、逐轮触发 ExecutionDone/execution_complete）；
                // 飞行缓冲消除队列空隙（处理中到达的 Execute 立即进下一批连续处理）。
                let mut queue = crate::agent::batch_queue::PendingBatchQueue::<AgentMessage>::new();
                // v0.4.9 P2：退出关断——平台收到 Shutdown 后 break 'platform 自然退出。
                'platform: loop {
                    let Some(batch) = queue.next_batch(&mut self.execution_rx).await else {
                        break 'platform;
                    };
                    queue.absorb_channel(&mut self.execution_rx);
                    let batch_len = batch.len();
                    for (i, msg) in batch.into_iter().enumerate() {
                        let turn_id = match msg {
                            AgentMessage::Execute { turn_id } => turn_id,
                            AgentMessage::Cancel { .. } => continue,
                            AgentMessage::Shutdown => break 'platform,
                            other => {
                                tracing::warn!("execution_platform: unexpected message: {other:?}");
                                continue;
                            }
                        };
                        self.pool
                            .update_platform_status(|s| s.execution_active = Some(turn_id.clone()))
                            .await;
                        self.pool
                            .set_core_agent_status(
                                "execution-platform",
                                crate::agent::agent_pool::registry::AgentStatus::Running,
                            )
                            .await;
                        self.handle_execute(&turn_id).await;
                        self.pool
                            .set_core_agent_status(
                                "execution-platform",
                                crate::agent::agent_pool::registry::AgentStatus::Idle,
                            )
                            .await;
                        self.pool
                            .update_platform_status(|s| s.execution_active = None)
                            .await;

                        // 待处理消息数口径 = 飞行缓冲 + 本批剩余。
                        queue.absorb_channel(&mut self.execution_rx);
                        let pending = queue.len() + (batch_len - 1 - i);
                        self.pool
                            .update_platform_status(move |s| s.execution_pending = pending)
                            .await;
                    }
                    queue.on_batch_finished();
                    self.pool.snapshot_detailed().await;
                }
            } else {
                // 完全回退逐条现状（v0.4.6 及以前行为）。
                while let Some(msg) = self.execution_rx.recv().await {
                    let pending = self.execution_rx.len();
                    self.pool
                        .update_platform_status(move |s| s.execution_pending = pending)
                        .await;
                    match msg {
                        AgentMessage::Execute { turn_id } => {
                            self.pool
                                .update_platform_status(|s| {
                                    s.execution_active = Some(turn_id.clone())
                                })
                                .await;
                            self.pool
                                .set_core_agent_status(
                                    "execution-platform",
                                    crate::agent::agent_pool::registry::AgentStatus::Running,
                                )
                                .await;
                            self.handle_execute(&turn_id).await;
                            self.pool
                                .set_core_agent_status(
                                    "execution-platform",
                                    crate::agent::agent_pool::registry::AgentStatus::Idle,
                                )
                                .await;
                            self.pool
                                .update_platform_status(|s| s.execution_active = None)
                                .await;
                        }
                        AgentMessage::Cancel { .. } => {}
                        AgentMessage::Shutdown => break,
                        other => {
                            tracing::warn!("execution_platform: unexpected message: {other:?}");
                        }
                    }
                    self.pool.snapshot_detailed().await;
                }
            }
            heartbeat.abort();
            tracing::info!("execution_platform: rx closed, shutting down");
        })
    }

    /// 单轮执行：读 TurnContext → 组装执行中台上下文 → 恰好一次 LLM 调用 → 解析
    /// task_design/task_status + capability_calls → 逐项执行 → 发布 ExecutionOutput +
    /// ExecutionDone/execution_complete（v0.4.7 逐轮产物逐轮触发语义）。
    async fn handle_execute(&self, turn_id: &str) {
        let Some(ctx) = self.pool.get_turn_context(turn_id).await else {
            tracing::warn!("execution_platform: TurnContext not found for turn_id={turn_id}");
            return;
        };

        if let Err(e) = self.refresh_subagent_states().await {
            tracing::warn!(
                "execution_platform: refresh_subagent_states failed for turn_id={turn_id}: {e}"
            );
        }

        let pool_entries = self.pool.snapshot().await;
        let subagent_states = self.pool.subagent_states().await;
        let prompt = self.build_execution_base_prompt(&pool_entries, &subagent_states);
        let messages = self.build_execution_messages(std::slice::from_ref(&ctx), &prompt);

        let req = match LlmRequest::from_model_row(&self.model_row, messages, self.api_key.clone())
        {
            Ok(req) => req,
            Err(e) => {
                let output = failure_output(format!("build LLM request failed: {e}"));
                self.publish_execution(turn_id, output).await;
                return;
            }
        };

        let raw = match self.provider.call(&req).await {
            Ok(response) => parse_execution_output(&response.content),
            Err(e) => {
                tracing::error!("execution_platform: LLM call failed for turn_id={turn_id}: {e}");
                let output = failure_output(format!("LLM call failed: {e}"));
                self.publish_execution(turn_id, output).await;
                return;
            }
        };

        let output = self.execute_capability_calls(raw).await;
        self.publish_execution(turn_id, output).await;
    }

    fn build_execution_base_prompt(
        &self,
        pool_entries: &[crate::agent::agent_pool::registry::AgentEntry],
        subagent_states: &[SubagentRuntimeState],
    ) -> String {
        let base = self
            .prompts_dir
            .as_deref()
            .map(|dir| read_platform_prompt(dir, "execution_platform.md"))
            .unwrap_or_else(|| {
                "You are the Execution Platform. Manage subagent lifecycle with one LLM call per round."
                    .to_string()
            });

        let available = self.available_capabilities();
        let base_with_capabilities = compose_agent_capability_prompt(&base, &available);

        let mut sections = vec![base_with_capabilities];
        sections.push(self.full_capability_registry_section());
        sections.push(self.pool_section(pool_entries, subagent_states));
        sections.push(self.templates_section());
        sections.push(self.models_section());
        sections.join("\n\n")
    }

    fn build_execution_messages(
        &self,
        contexts: &[crate::agent::communication::TurnContext],
        base_prompt: &str,
    ) -> Vec<ChatMessage> {
        let mut system_text = base_prompt.to_string();
        // 单条时保留 Thinking Input 说明段；N 条合并时按确认序列
        // [System(平台+池状态), User_i, Assistant_i, ..., System(平台指令)] 组装。
        if let [ctx] = contexts {
            system_text.push_str("\n\n");
            system_text.push_str(&Self::thinking_input_section(ctx));
        }

        let mut messages = vec![ChatMessage::System {
            text: system_text,
            kind: SystemKind::Primary,
        }];
        for ctx in contexts {
            messages.push(ChatMessage::User {
                text: ctx.user_message.clone(),
            });
            messages.push(ChatMessage::Assistant {
                text: ctx.thinking.think_message.clone(),
            });
        }
        messages.push(ChatMessage::System {
            text: EXECUTION_INSTRUCTION.to_string(),
            kind: SystemKind::Primary,
        });
        messages
    }

    fn thinking_input_section(ctx: &crate::agent::communication::TurnContext) -> String {
        // 2.0.4 think_message 合并：think 全文只保留一处 System 段 + Assistant 段（双份消除）。
        format!(
            "## Thinking Input\n\n**think_message:** {}\n\n**constraints:**\n{}",
            ctx.thinking.think_message,
            if ctx.thinking.constraints.is_empty() {
                "none".to_string()
            } else {
                ctx.thinking.constraints.join("\n")
            },
        )
    }

    fn available_capabilities(&self) -> Vec<CapabilityPromptEntry> {
        let (Some(registry), Some(executor)) = (&self.registry, &self.executor) else {
            return Vec::new();
        };
        let Ok(service) = CapabilityService::new(registry, executor) else {
            return Vec::new();
        };
        let Ok(definitions) = service.definitions_for_agent("execution-platform") else {
            return Vec::new();
        };
        definitions
            .into_iter()
            .map(|definition| CapabilityPromptEntry {
                capability_id: definition.capability_id,
                capability_name: definition.capability_name,
                description: definition.description,
            })
            .collect()
    }

    /// 完整能力注册表（供 subagent.capability_allowlist 设计参考）。
    ///
    /// 执行中台自己不能直接调用这些能力；它们只能通过 subagent 实例的 allowlist
    /// 委派给 subagent 执行。
    fn full_capability_registry_section(&self) -> String {
        let Some(registry) = &self.registry else {
            return "## Full Capability Registry\n\n(registry unavailable)".to_string();
        };

        let mut lines = vec!["## Full Capability Registry".to_string()];
        lines.push("(Reference only: these are NOT directly callable by you unless listed in your authorized capability group. Choose base/composite capabilities for subagent.capability_allowlist; usage methods are invoked via method.invoke, not added to subagent allowlists.)".to_string());
        lines.push("".to_string());

        let mut base: Vec<_> = registry.base_capabilities.values().collect();
        base.sort_by_key(|row| row.id.as_str());
        let mut composite: Vec<_> = registry.composite_capabilities.values().collect();
        composite.sort_by_key(|row| row.id.as_str());
        let mut methods: Vec<_> = registry.usage_methods.values().collect();
        methods.sort_by_key(|row| row.id.as_str());

        lines.push("### Base capabilities".to_string());
        for row in base {
            lines.push(format!(
                "- `{}` / {}: {}",
                row.id, row.name, row.description
            ));
        }
        lines.push("".to_string());
        lines.push("### Composite capabilities".to_string());
        if composite.is_empty() {
            lines.push("- (none)".to_string());
        }
        for row in composite {
            lines.push(format!(
                "- `{}` / {}: {}",
                row.id, row.name, row.description
            ));
        }
        lines.push("".to_string());
        lines.push("### Usage Methods / 方法库".to_string());
        lines.push("(Run a method with method.invoke(method_id=...). Usage methods are not subagent capabilities.)".to_string());
        if methods.is_empty() {
            lines.push("- (none)".to_string());
        }
        for row in methods {
            lines.push(format!(
                "- `{}` / {}: {}",
                row.id, row.name, row.prompt
            ));
        }
        lines.join("\n")
    }

    fn pool_section(
        &self,
        entries: &[crate::agent::agent_pool::registry::AgentEntry],
        subagent_states: &[SubagentRuntimeState],
    ) -> String {
        let mut lines = vec!["## AgentPool Status".to_string()];
        for entry in entries {
            lines.push(format!("- {} {:?}", entry.id, entry.status));
        }
        if entries.is_empty() {
            lines.push("- (empty)".to_string());
        }
        lines.push("## Subagent States".to_string());
        for state in subagent_states {
            lines.push(format!(
                "- {} lifecycle={:?} startup={:?} kind={:?} last_output={}",
                state.subagent_id,
                state.lifecycle,
                state.startup,
                state.lifecycle_kind,
                state.last_output_truncated.as_deref().unwrap_or("(none)"),
            ));
        }
        if subagent_states.is_empty() {
            lines.push("- (none)".to_string());
        }
        lines.join("\n")
    }

    fn templates_section(&self) -> String {
        let Some(registry) = &self.registry else {
            return "## Subagent Templates\n\n(registry unavailable)".to_string();
        };
        let mut lines = vec!["## Subagent Templates".to_string()];
        let mut templates: Vec<_> = registry
            .agents
            .iter()
            .filter(|(_, row)| row.mode == "subagent_template")
            .collect();
        templates.sort_by_key(|(id, _)| id.as_str());
        for (id, row) in &templates {
            lines.push(format!(
                "- id: {id}\n  name: {}\n  prompt: {}\n  capability_allowlist: {:?}\n  config: {}",
                row.name,
                row.prompt.as_deref().unwrap_or(""),
                row.capability_allowlist,
                row.config
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            ));
        }
        if templates.is_empty() {
            lines.push("- (none)".to_string());
        }
        lines.join("\n")
    }

    fn models_section(&self) -> String {
        let Some(registry) = &self.registry else {
            return "## Model Registry\n\n(registry unavailable)".to_string();
        };
        let mut lines = vec!["## Model Registry".to_string()];
        lines.push("For subagent.create/update, use the registry row `id` as the `model_id` argument, NOT the upstream api model_id.".to_string());
        for row in registry.models.values() {
            lines.push(format!(
                "- id={} (USE THIS AS model_id) name={} provider={} api_model_id={} api_type={} config={}",
                row.id,
                row.name,
                row.provider,
                row.model_id,
                row.api_type,
                row.config
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            ));
        }
        if registry.models.is_empty() {
            lines.push("- (none)".to_string());
        }
        lines.join("\n")
    }

    async fn execute_capability_calls(&self, raw: ExecutionPlatformRawOutput) -> ExecutionOutput {
        // v0.5.0 轮级工作区快照：本批（一次执行中台轮）开始时固化一次默认工作区，
        // 批内全部能力调用复用同一快照——切换默认工作区只影响之后开始的新批，
        // 本批（运行中任务）保持旧工作区快照（任务书 §7）。
        let frozen_host = self.executor.as_ref().map(|e| e.current_host_context());
        self.execute_capability_calls_with_host(raw, frozen_host.as_ref())
            .await
    }

    async fn execute_capability_calls_with_host(
        &self,
        raw: ExecutionPlatformRawOutput,
        frozen_host: Option<&crate::logic::builtin::host_context::HostContext>,
    ) -> ExecutionOutput {
        let mut actions = Vec::new();
        let mut created_real_id: Option<String> = None;
        let mut created_task_input: Option<String> = None;
        let known_ids: std::collections::HashSet<String> = self
            .pool
            .subagent_states()
            .await
            .iter()
            .map(|s| s.subagent_id.clone())
            .collect();
        for mut call in raw.capability_calls {
            // subagent.run 使用 create 返回的真实 ID，禁止使用计划/语义 ID。
            if call.capability_id == "subagent.run" {
                if let Some(args) = call.arguments.as_object_mut() {
                    // 空 task_input 回退 create 的 task_input。
                    let task_input_empty = args
                        .get("task_input")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true);
                    if task_input_empty {
                        if let Some(created_task_input) = &created_task_input {
                            args.insert(
                                "task_input".to_string(),
                                serde_json::Value::String(created_task_input.clone()),
                            );
                        }
                    }
                    // 只有计划/语义 ID（不在池中）才映射到真实 ID；真实 ID 不覆盖。
                    if let Some(real_id) = created_real_id.as_ref() {
                        if let Some(subagent_id) =
                            args.get("subagent_id").and_then(serde_json::Value::as_str)
                        {
                            let is_real = subagent_id.starts_with("sg_")
                                && (known_ids.contains(subagent_id)
                                    || Some(subagent_id) == created_real_id.as_deref());
                            if !is_real {
                                tracing::warn!(
                                    "execution_platform: rewriting subagent.run semantic id {:?} -> real id {:?}",
                                    subagent_id, real_id
                                );
                                args.insert(
                                    "subagent_id".to_string(),
                                    serde_json::Value::String(real_id.clone()),
                                );
                            }
                        }
                    }
                }
            }
            let (record, output) = self.execute_one_call(call, frozen_host).await;
            if record.capability_id == "subagent.create" {
                if let Some(output) = output {
                    if let Some(id) = output
                        .get("subagent_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        created_real_id = Some(id.to_string());
                    }
                    created_task_input = output
                        .get("task_input")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                }
            }
            actions.push(record);
        }
        let mut subagent_states = self.pool.subagent_states().await;
        if let Err(e) = self.refresh_subagent_states().await {
            tracing::warn!("execution_platform: refresh subagent states failed: {e}");
        } else {
            subagent_states = self.pool.subagent_states().await;
        }
        // 坏调用证据并入 task_status：洞察中台可读，不吞坏项。
        let task_status = append_bad_call_evidence(raw.task_status.as_deref(), &raw.bad_calls);
        ExecutionOutput {
            task_design: raw.task_design.unwrap_or_default(),
            task_status,
            lifecycle_actions: actions,
            subagent_states,
        }
    }

    async fn execute_one_call(
        &self,
        call: RawCapabilityCall,
        frozen_host: Option<&crate::logic::builtin::host_context::HostContext>,
    ) -> (CapabilityLifecycleRecord, Option<serde_json::Value>) {
        let capability_id = call.capability_id.clone();
        let arguments_summary = crate::common::json_util::truncate_head_tail(
            &serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string()),
            500,
        );
        let mut logs = vec![format!("START {capability_id}: accepted")];

        let (Some(registry), Some(executor)) = (&self.registry, &self.executor) else {
            logs.push(format!(
                "FAIL {capability_id}: capability runtime unavailable"
            ));
            return (
                record(
                    capability_id,
                    capability_name(&call, None),
                    arguments_summary,
                    CapabilityLifecycleState::Rejected,
                    Some("capability runtime unavailable".to_string()),
                    logs,
                ),
                None,
            );
        };

        // v0.5.0：轮级快照 host 存在时以其构造（同一批所有调用共享同一固化快照）；
        // 无快照（测试直连路径）退化为构造时刻快照（等效现状）。
        let service = match frozen_host {
            Some(host) => CapabilityService::new_with_host(registry, executor, host),
            None => CapabilityService::new(registry, executor),
        };
        let service = match service {
            Ok(service) => service,
            Err(e) => {
                logs.push(format!("FAIL {capability_id}: {e}"));
                return (
                    record(
                        capability_id,
                        capability_name(&call, None),
                        arguments_summary,
                        CapabilityLifecycleState::Rejected,
                        Some(e.to_string()),
                        logs,
                    ),
                    None,
                );
            }
        };

        let authority_name = call
            .capability_name
            .clone()
            .or_else(|| resolve_authority_name(registry, &capability_id));

        let capability_call = CapabilityCall {
            capability_id: capability_id.clone(),
            capability_name: authority_name.unwrap_or_default(),
            arguments: call.arguments.clone(),
        };

        match service.execute_for_agent("execution-platform", &capability_call) {
            Ok(result) => {
                let truncated =
                    crate::common::json_util::truncate_head_tail(&result.output.to_string(), 1200);
                logs.push(format!("OK {capability_id}: {truncated}"));
                let state = accepted_state(&capability_id);
                (
                    record(
                        capability_id,
                        Some(result.capability_name),
                        arguments_summary,
                        state,
                        None,
                        logs,
                    ),
                    Some(result.output),
                )
            }
            Err(e) => {
                logs.push(format!("FAIL {capability_id}: {e}"));
                let state = classify_error(&e);
                (
                    record(
                        capability_id,
                        capability_name(&call, None),
                        arguments_summary,
                        state,
                        Some(e.to_string()),
                        logs,
                    ),
                    None,
                )
            }
        }
    }

    async fn refresh_subagent_states(&self) -> Result<()> {
        let (Some(duckdb), Some(_storage_root)) = (&self.duckdb, &self.storage_root) else {
            return Ok(());
        };
        let rows = {
            let conn = duckdb
                .lock()
                .map_err(|e| AgentError::Io(format!("duckdb lock poisoned: {e}")))?;
            let mut statement = conn
                .prepare(
                    "SELECT id, prompt, CAST(capability_allowlist AS VARCHAR), CAST(config AS VARCHAR) \
                     FROM agent WHERE mode = 'subagent'",
                )
                .map_err(|e| AgentError::Bootstrap(format!("prepare subagent rows: {e}")))?;
            let mut rows = Vec::new();
            let mapped = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .map_err(|e| AgentError::Bootstrap(format!("query subagent rows: {e}")))?;
            for row in mapped {
                rows.push(row.map_err(|e| AgentError::Bootstrap(format!("subagent row: {e}")))?);
            }
            rows
        };

        for (id, _prompt, allowlist_json, config_json) in rows {
            let allowlist: Vec<String> = serde_json::from_str(&allowlist_json)
                .map_err(|e| AgentError::Parse(format!("allowlist {id}: {e}")))?;
            let config_value: serde_json::Value =
                serde_json::from_str(config_json.as_deref().unwrap_or("{}"))
                    .map_err(|e| AgentError::Parse(format!("subagent config {id}: {e}")))?;
            let config_block = config_value
                .get("subagent")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let config: crate::agent::subagent_capability::SubagentConfig =
                serde_json::from_value(config_block)
                    .map_err(|e| AgentError::Parse(format!("subagent config {id}: {e}")))?;
            if config.tombstoned_at.is_some() {
                self.pool.remove_subagent(&id).await;
                continue;
            }
            let lifecycle = parse_lifecycle(&config.lifecycle);
            let status = match lifecycle {
                SubagentLifecycle::Running => {
                    crate::agent::agent_pool::registry::AgentStatus::Running
                }
                _ => crate::agent::agent_pool::registry::AgentStatus::Idle,
            };
            let context_window = model_context_window(&self.registry, &config.model_id);
            let last_output_budget =
                crate::agent::subagent_memory::memory_window_tokens(context_window);
            let last_output_truncated = self
                .storage_root
                .as_ref()
                .map(|root| {
                    crate::agent::subagent_memory::read_last_output_truncated(
                        root,
                        &id,
                        last_output_budget,
                    )
                    .ok()
                    .flatten()
                })
                .unwrap_or(None);
            let state = SubagentRuntimeState {
                subagent_id: id.clone(),
                lifecycle,
                last_output_truncated,
                trigger: config.trigger.clone(),
                startup: config.startup,
                lifecycle_kind: config.lifecycle_kind,
            };
            self.pool.register_subagent(state, status.clone()).await;
            self.pool.update_subagent_lifecycle(&id, lifecycle).await;
            self.pool.update_subagent_status(&id, status).await;
            let _ = allowlist;
        }
        Ok(())
    }

    async fn publish_execution(&self, turn_id: &str, output: ExecutionOutput) {
        self.pool.set_execution(turn_id, output).await;
        if let Err(e) = self.pool.send_execution_done(turn_id).await {
            tracing::warn!("execution_platform: send_execution_done failed: {e}");
        }
        if let Err(e) = self.pool.send_trigger(turn_id, "execution_complete").await {
            tracing::warn!("execution_platform: send_trigger execution_complete failed: {e}");
        }
        self.pool
            .publish_event("execution_complete", turn_id.to_string());
    }
}

/// 坏调用证据并入 task_status 文本（洞察可读）；无坏项时原样返回。
/// 证据总量封顶：坏项正文合计不超过 `MAX_BAD_EVIDENCE_CHARS`（400）字符，
/// 只完整保留上限内的条目，其余以 `…(+N items)` 标记省略——防止极端坏输出
/// 膨胀 task_status 流入洞察请求上下文（insight_platform 组装该字段时不截断）。
const MAX_BAD_EVIDENCE_CHARS: usize = 400;

fn append_bad_call_evidence(task_status: Option<&str>, bad_calls: &[String]) -> String {
    let mut status = task_status.unwrap_or_default().to_string();
    if !bad_calls.is_empty() {
        if !status.is_empty() {
            status.push('\n');
        }
        status.push_str(&format!(
            "[坏调用证据] 本轮 {} 个能力调用项损坏被跳过: ",
            bad_calls.len()
        ));
        // 逐条完整装入上限（单条提取时已截断 200，首条必然可装入）；
        // 装不下的条目整条省略并计数。
        let mut kept: Vec<&str> = Vec::new();
        let mut used = 0usize;
        for item in bad_calls {
            let sep = if kept.is_empty() { 0 } else { 2 }; // "; "
            if used + sep + item.len() > MAX_BAD_EVIDENCE_CHARS {
                break;
            }
            used += sep + item.len();
            kept.push(item);
        }
        let omitted = bad_calls.len() - kept.len();
        status.push_str(&kept.join("; "));
        if omitted > 0 {
            status.push_str(&format!("…(+{omitted} items)"));
        }
    }
    status
}

fn model_context_window(registry: &Option<Registry>, model_id: &str) -> usize {
    registry
        .as_ref()
        .and_then(|registry| registry.models.get(model_id))
        .map(|row| crate::logic::model::capability::resolve_model_capability(row).context_window)
        .unwrap_or(4096)
}

fn parse_lifecycle(value: &str) -> SubagentLifecycle {
    serde_json::from_value(serde_json::json!(value)).unwrap_or(SubagentLifecycle::Idle)
}

fn record(
    capability_id: String,
    capability_name: Option<String>,
    arguments_summary: String,
    lifecycle_state: CapabilityLifecycleState,
    error: Option<String>,
    capability_call_logs: Vec<String>,
) -> CapabilityLifecycleRecord {
    CapabilityLifecycleRecord {
        capability_id,
        capability_name: capability_name.unwrap_or_default(),
        arguments_summary,
        lifecycle_state,
        invocation_ref: None,
        error,
        capability_call_logs,
    }
}

fn capability_name(call: &RawCapabilityCall, resolved: Option<String>) -> Option<String> {
    call.capability_name.clone().or(resolved)
}

fn resolve_authority_name(registry: &Registry, capability_id: &str) -> Option<String> {
    registry
        .base_capabilities
        .get(capability_id)
        .map(|row| row.name.clone())
        .or_else(|| {
            registry
                .composite_capabilities
                .get(capability_id)
                .map(|row| row.name.clone())
        })
}

fn accepted_state(capability_id: &str) -> CapabilityLifecycleState {
    if capability_id == "subagent.run" {
        CapabilityLifecycleState::Accepted
    } else {
        CapabilityLifecycleState::Completed
    }
}

fn classify_error(error: &AgentError) -> CapabilityLifecycleState {
    match error {
        AgentError::NotFound(_) | AgentError::Parse(_) => CapabilityLifecycleState::Rejected,
        _ => CapabilityLifecycleState::Failed,
    }
}

fn failure_output(message: String) -> ExecutionOutput {
    ExecutionOutput {
        task_design: String::new(),
        task_status: message.clone(),
        lifecycle_actions: vec![record(
            "execution-platform.llm".to_string(),
            Some("LLM Request".to_string()),
            "{}".to_string(),
            CapabilityLifecycleState::Failed,
            Some(message.clone()),
            vec![format!("FAIL execution-platform.llm: {message}")],
        )],
        subagent_states: vec![],
    }
}

/// 宽容解析执行中台输出：候选链（```json 块 / strip+首对象 / 全文）+
/// Value 级逐项提取（动作/语意分离），全程不整体判死。
///
/// - `capability_calls`：逐项独立处理，坏项（capability_id 非字符串/空）记入
///   `bad_calls` 证据并 warn 日志，其余项照常进入候选；字段为非数组
///   （对象/字符串等）时原文记入 `bad_calls` 证据并 warn，仍零动作；
/// - `task_design` / `task_status`：缺失 → None；类型非字符串 → `to_string()` 截断保留；
/// - 完全提取不到 → 诚实零动作：`task_design=None`，`task_status=Some("raw:" + 原文截断
///   200)`，`capability_calls=[]`——下游可见模型原话，不伪装「解析失败」占位。
fn parse_execution_output(content: &str) -> ExecutionPlatformRawOutput {
    let mut candidates = Vec::new();
    if let Some(block) = crate::common::json_util::extract_json_block(content) {
        candidates.push(block);
    }
    let stripped = crate::common::json_util::strip_reasoning_preamble(content);
    if let Some(obj) = crate::common::json_util::extract_first_json_object(&stripped) {
        candidates.push(obj);
    }
    let trimmed = content.trim().to_string();
    if !candidates.contains(&trimmed) {
        candidates.push(trimmed);
    }

    for candidate in candidates {
        if let Some(raw) = parse_execution_raw(&candidate) {
            return raw;
        }
        let repaired = crate::common::json_util::repair_json(&candidate);
        if repaired != candidate {
            if let Some(raw) = parse_execution_raw(&repaired) {
                return raw;
            }
        }
    }

    ExecutionPlatformRawOutput {
        task_design: None,
        task_status: Some(format!(
            "raw:{}",
            crate::common::json_util::truncate_utf8_boundary(content, 200)
        )),
        capability_calls: vec![],
        bad_calls: vec![],
    }
}

/// Value 级宽容提取：整体反序列化为 Value 后逐字段处理（不依赖 serde 结构整体判死）。
fn parse_execution_raw(text: &str) -> Option<ExecutionPlatformRawOutput> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = value.as_object()?;
    let mut raw = ExecutionPlatformRawOutput {
        task_design: optional_text_field(obj.get("task_design")),
        task_status: optional_text_field(obj.get("task_status")),
        ..Default::default()
    };
    match obj.get("capability_calls") {
        // 缺失：零动作（与诚实零动作语义一致）。
        None => {}
        Some(serde_json::Value::Array(calls)) => {
            for item in calls {
                match parse_raw_capability_call(item) {
                    Some(call) => raw.capability_calls.push(call),
                    None => {
                        let item_text = serde_json::to_string(item).unwrap_or_default();
                        let excerpt =
                            crate::common::json_util::truncate_utf8_boundary(&item_text, 200);
                        tracing::warn!("execution_platform: 坏 capability_call 项跳过: {excerpt}");
                        raw.bad_calls.push(excerpt.to_string());
                    }
                }
            }
        }
        // 非数组（对象/字符串等）：复述原文记坏证据 + warn，仍零动作。
        Some(other) => {
            let raw_text = serde_json::to_string(other).unwrap_or_default();
            let excerpt = crate::common::json_util::truncate_utf8_boundary(&raw_text, 200);
            tracing::warn!("execution_platform: capability_calls 非数组，跳过: {excerpt}");
            raw.bad_calls.push(excerpt.to_string());
        }
    }
    Some(raw)
}

/// 单个能力调用宽容解析：`capability_id` 非字符串/空 → None（记坏证据）；
/// `capability_name` 非字符串 → `to_string()` 保留；`arguments` 原样。
fn parse_raw_capability_call(item: &serde_json::Value) -> Option<RawCapabilityCall> {
    let obj = item.as_object()?;
    let capability_id = match obj.get("capability_id") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => return None,
    };
    let capability_name = obj.get("capability_name").map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
    Some(RawCapabilityCall {
        capability_id,
        capability_name,
        arguments,
    })
}

/// 语意字段容错：缺失 → None；非字符串 → `to_string()` 截断 200 字符保留（不判死）。
fn optional_text_field(v: Option<&serde_json::Value>) -> Option<String> {
    match v {
        None => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(other) => Some(
            crate::common::json_util::truncate_utf8_boundary(&other.to_string(), 200).to_string(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pool: Arc<AgentPool>,
    rx: mpsc::Receiver<AgentMessage>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    prompts_dir: Option<PathBuf>,
    registry: Option<Registry>,
    executor: Option<Arc<crate::logic::capability::executor::CapabilityExecutor>>,
    duckdb: Option<Arc<Mutex<duckdb::Connection>>>,
    storage_root: Option<PathBuf>,
    merge_enabled: bool,
) {
    let platform = ExecutionPlatform::new(
        rx,
        pool,
        provider,
        model_row,
        api_key,
        prompts_dir,
        registry,
        executor,
        duckdb,
        storage_root,
        merge_enabled,
    );
    let handle = platform.spawn();
    match handle.await {
        Ok(()) => tracing::info!("execution_platform::run: platform spawn completed"),
        Err(e) => tracing::error!(
            "execution_platform::run: platform task panicked/aborted: {e} (thread death = channel closed)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::model::provider::{LlmProvider, LlmRequest, LlmResponse};
    use crate::logic::model::stream::StreamChunk;

    struct NoopProvider;
    #[async_trait::async_trait]
    impl LlmProvider for NoopProvider {
        fn id(&self) -> &'static str {
            "noop"
        }
        fn name(&self) -> &'static str {
            "Noop"
        }
        async fn call(&self, _req: &LlmRequest) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: String::new(),
                usage: None,
            })
        }
        async fn call_stream(
            &self,
            _req: &LlmRequest,
            _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> Result<LlmResponse> {
            self.call(_req).await
        }
    }

    #[test]
    fn parse_output_accepts_plain_json() {
        let raw = parse_execution_output(
            r#"{"task_design":"d","task_status":"s","capability_calls":[{"capability_id":"subagent.run","arguments":{"subagent_id":"sg_1"}}]}"#,
        );
        assert_eq!(raw.task_design.as_deref(), Some("d"));
        assert_eq!(raw.capability_calls.len(), 1);
        assert_eq!(raw.capability_calls[0].capability_id, "subagent.run");
    }

    #[test]
    fn parse_output_accepts_fenced_json_with_reasoning_prefix() {
        let text = "<think>reasoning</think>\n```json\n{\"task_design\":\"d\",\"task_status\":\"s\",\"capability_calls\":[]}\n```";
        let raw = parse_execution_output(text);
        assert_eq!(raw.task_design.as_deref(), Some("d"));
        assert!(raw.capability_calls.is_empty());
    }

    #[test]
    fn parse_output_invalid_fails_closed_with_empty_calls() {
        // 诚实零动作：原话进 task_status（带 raw: 前缀），不伪装正常 0 动作。
        let raw = parse_execution_output("not json");
        assert!(raw.capability_calls.is_empty());
        assert!(raw.bad_calls.is_empty());
        let status = raw.task_status.as_deref().unwrap();
        assert!(status.starts_with("raw:"), "{status}");
        assert!(status.contains("not json"));
        assert!(raw.task_design.is_none());
    }

    #[test]
    fn parse_output_extracts_from_prose_mixed_json() {
        // 散文 + 裸 JSON 混合：strip+首对象 提取。
        let raw = parse_execution_output(
            "分析完成。\n{\"task_design\":\"继续\",\"task_status\":\"等待\",\"capability_calls\":[{\"capability_id\":\"file.read\",\"arguments\":{\"path\":\"a.txt\"}}]}",
        );
        assert_eq!(raw.task_design.as_deref(), Some("继续"));
        assert_eq!(raw.capability_calls.len(), 1);
        assert_eq!(raw.capability_calls[0].capability_id, "file.read");
        assert_eq!(raw.capability_calls[0].arguments["path"], "a.txt");
        assert!(raw.bad_calls.is_empty());
    }

    #[test]
    fn parse_output_skips_bad_calls_keeps_good_ones() {
        let raw = parse_execution_output(
            r#"{"task_design":"d","capability_calls":[
                {"capability_id":123,"arguments":{}},
                {"capability_id":"file.read","arguments":{"path":"a"}},
                {"capability_id":"","arguments":{}},
                {"capability_id":"text.grep","arguments":{"pattern":"x"}}
            ]}"#,
        );
        assert_eq!(raw.capability_calls.len(), 2, "2 好 2 坏 → 仅 2 好进入候选");
        assert_eq!(raw.capability_calls[0].capability_id, "file.read");
        assert_eq!(raw.capability_calls[1].capability_id, "text.grep");
        assert_eq!(raw.bad_calls.len(), 2, "坏证据 2 条");
        assert!(
            raw.bad_calls[0].contains("capability_id"),
            "证据含坏项原文: {}",
            raw.bad_calls[0]
        );
        assert_eq!(
            raw.task_design.as_deref(),
            Some("d"),
            "语意字段不受坏项影响"
        );
    }

    #[test]
    fn parse_output_tolerates_non_string_semantic_fields() {
        // task_design 数字 → to_string 保留；task_status 缺失 → None；capability_name 数字宽容。
        let raw = parse_execution_output(
            r#"{"task_design":42,"capability_calls":[{"capability_id":"file.read","capability_name":7}]}"#,
        );
        assert_eq!(raw.task_design.as_deref(), Some("42"));
        assert!(raw.task_status.is_none());
        assert_eq!(raw.capability_calls.len(), 1);
        assert_eq!(raw.capability_calls[0].capability_id, "file.read");
        assert_eq!(
            raw.capability_calls[0].capability_name.as_deref(),
            Some("7")
        );
        assert!(raw.bad_calls.is_empty());
    }

    #[test]
    fn parse_output_repair_path_recovers_bare_newline() {
        // repair_json 路径：字符串内裸换行。
        let raw = parse_execution_output(
            "{\"task_design\":\"d\",\"task_status\":\"s\",\"capability_calls\":[{\"capability_id\":\"file.read\",\"arguments\":{\"path\":\"a\nb\"}}]}",
        );
        assert_eq!(raw.capability_calls.len(), 1);
        assert_eq!(raw.capability_calls[0].capability_id, "file.read");
        assert_eq!(raw.capability_calls[0].arguments["path"], "a\nb");
    }

    #[test]
    fn parse_output_candidate_chain_prefers_fenced_block() {
        // 候选链顺序：```json 块优先于首对象/全文。
        let text = "```json\n{\"task_design\":\"from_block\",\"capability_calls\":[{\"capability_id\":\"file.read\"}]}\n```\n{\"task_design\":\"from_plain\",\"capability_calls\":[]}";
        let raw = parse_execution_output(text);
        assert_eq!(raw.task_design.as_deref(), Some("from_block"));
        assert_eq!(raw.capability_calls.len(), 1);
    }

    #[test]
    fn parse_output_candidate_chain_falls_back_to_plain_json() {
        let text = "先思考\n{\"task_design\":\"from_plain\",\"capability_calls\":[{\"capability_id\":\"text.grep\"}]}";
        let raw = parse_execution_output(text);
        assert_eq!(raw.task_design.as_deref(), Some("from_plain"));
        assert_eq!(raw.capability_calls[0].capability_id, "text.grep");
    }

    #[test]
    fn append_bad_call_evidence_merges_into_task_status() {
        assert_eq!(append_bad_call_evidence(None, &[]), "");
        assert_eq!(
            append_bad_call_evidence(Some("继续"), &[]),
            "继续",
            "无坏项原样返回"
        );
        let merged =
            append_bad_call_evidence(Some("继续"), &["{\"capability_id\":123}".to_string()]);
        assert!(merged.starts_with("继续\n[坏调用证据] 本轮 1 个能力调用项损坏被跳过: "));
        assert!(merged.contains("{\"capability_id\":123}"));
    }

    #[test]
    fn append_bad_call_evidence_caps_total_and_marks_omitted() {
        // 50 条 × 28 字节 = 1400 字节，远超上限 → 只完整保留能装入 400 上限的整条，
        // 尾部 …(+N items) 标记省略条数，坏项正文有界。
        let items: Vec<String> = (0..50)
            .map(|i| format!("item-{i:02}:{}", "y".repeat(20)))
            .collect();
        let merged = append_bad_call_evidence(None, &items);
        assert!(merged.starts_with("[坏调用证据] 本轮 50 个能力调用项损坏被跳过: "));
        let body_start = merged.find(": ").unwrap() + 2;
        let body = &merged[body_start..];
        assert!(body.contains("…(+"), "应含省略标记: {merged}");
        let kept_part = body.split("…(+").next().unwrap();
        assert!(
            kept_part.len() <= 400,
            "坏项正文应≤400: {}",
            kept_part.len()
        );
        assert!(kept_part.contains("item-00"), "首条完整保留");
        assert!(kept_part.contains("item-12"), "完整条目逐条装入");
        assert!(!kept_part.contains("item-13"), "超出上限的条目省略");
        let omitted: usize = body
            .split("…(+")
            .nth(1)
            .and_then(|s| s.split(" items)").next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        assert!((1..50).contains(&omitted), "omitted={omitted}");
        assert_eq!(omitted, 50 - kept_part.matches("item-").count());
        // 单条 200 内小样本：原样并入，无省略标记。
        let small = append_bad_call_evidence(
            Some("继续"),
            &[
                "{\"capability_id\":123}".to_string(),
                "{\"capability_id\":\"\"}".to_string(),
            ],
        );
        assert!(!small.contains("…(+"), "小样本不省略: {small}");
        assert!(small.contains("{\"capability_id\":123}; {\"capability_id\":\"\"}"));
    }

    #[test]
    fn parse_output_non_array_capability_calls_records_bad_evidence() {
        // capability_calls 为对象：复述原文记坏证据，仍零动作，语意字段不受影响。
        let raw = parse_execution_output(
            r#"{"task_design":"d","capability_calls":{"capability_id":"file.read","arguments":{"path":"a"}}}"#,
        );
        assert!(raw.capability_calls.is_empty(), "对象形态零动作");
        assert_eq!(raw.bad_calls.len(), 1, "记 1 条坏证据");
        assert!(
            raw.bad_calls[0].contains("capability_id"),
            "{}",
            raw.bad_calls[0]
        );
        assert_eq!(raw.task_design.as_deref(), Some("d"));

        // 字符串形态同样记证据，坏证据截断 200。
        let raw2 =
            parse_execution_output(r#"{"task_status":"s","capability_calls":"oops not an array"}"#);
        assert!(raw2.capability_calls.is_empty());
        assert_eq!(raw2.bad_calls.len(), 1);
        assert!(
            raw2.bad_calls[0].contains("oops not an array"),
            "{}",
            raw2.bad_calls[0]
        );
        assert_eq!(raw2.task_status.as_deref(), Some("s"));
    }

    #[test]
    fn accepted_state_only_for_run() {
        assert_eq!(
            accepted_state("subagent.run"),
            CapabilityLifecycleState::Accepted
        );
        assert_eq!(
            accepted_state("subagent.create"),
            CapabilityLifecycleState::Completed
        );
    }

    #[test]
    fn classify_rejected_and_failed() {
        assert_eq!(
            classify_error(&AgentError::NotFound("x".into())),
            CapabilityLifecycleState::Rejected
        );
        assert_eq!(
            classify_error(&AgentError::Parse("x".into())),
            CapabilityLifecycleState::Rejected
        );
        assert_eq!(
            classify_error(&AgentError::Script("x".into())),
            CapabilityLifecycleState::Failed
        );
    }

    #[tokio::test]
    async fn execute_create_then_run_reaches_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        crate::data::cognitive_seed::ensure_default_capabilities(temp.path()).unwrap();
        crate::data::cognitive_seed::import_factory_defaults(&conn, temp.path()).unwrap();
        conn.execute(
            "INSERT INTO model (id, name, provider, api_url, api_type, api_protocol, api_key, model_id) \
             VALUES ('m1', 'Mock', 'mock', 'http://mock', 'openai', 'openai-v1', '', 'mock-model')",
            [],
        )
        .unwrap();
        let registry = crate::data::duckdb::loader::load_all_into_memory(&conn).unwrap();
        let duckdb = Arc::new(Mutex::new(conn));
        let mut executor = crate::logic::capability::executor::CapabilityExecutor::new();
        executor.set_duckdb(duckdb.clone());
        executor.set_storage_root(temp.path());
        let executor = Arc::new(executor);
        let (pool, _receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        let platform = ExecutionPlatform::new(
            mpsc::channel(1).1,
            pool,
            Arc::new(NoopProvider),
            registry.models.get("m1").unwrap().clone(),
            SecretString::from(String::new()),
            None,
            Some(registry),
            Some(executor),
            Some(duckdb),
            Some(temp.path().to_path_buf()),
            true,
        );

        let created = platform
            .execute_capability_calls(ExecutionPlatformRawOutput {
                task_design: Some("create".into()),
                task_status: Some("next run".into()),
                capability_calls: vec![RawCapabilityCall {
                    capability_id: "subagent.create".into(),
                    capability_name: None,
                    arguments: serde_json::json!({
                        "template_id": "subagent.template.normal",
                        "model_id": "m1",
                        "capability_allowlist": ["file.read"],
                    }),
                }],
                bad_calls: vec![],
            })
            .await;
        assert_eq!(created.lifecycle_actions.len(), 1);
        assert_eq!(
            created.lifecycle_actions[0].lifecycle_state,
            CapabilityLifecycleState::Completed
        );
        let subagent_id = created.subagent_states[0].subagent_id.clone();

        let run = platform
            .execute_capability_calls(ExecutionPlatformRawOutput {
                task_design: Some("run".into()),
                task_status: Some("async accepted".into()),
                capability_calls: vec![RawCapabilityCall {
                    capability_id: "subagent.run".into(),
                    capability_name: None,
                    arguments: serde_json::json!({
                        "subagent_id": subagent_id,
                        "task_input": "probe",
                    }),
                }],
                bad_calls: vec![],
            })
            .await;
        assert_eq!(run.lifecycle_actions.len(), 1);
        assert_eq!(
            run.lifecycle_actions[0].lifecycle_state,
            CapabilityLifecycleState::Accepted
        );
        assert!(run.subagent_states.iter().any(|state| {
            state.subagent_id == subagent_id && state.lifecycle == SubagentLifecycle::Running
        }));
    }

    #[test]
    fn parse_lifecycle_maps_snake_case() {
        assert_eq!(parse_lifecycle("running"), SubagentLifecycle::Running);
        assert_eq!(parse_lifecycle("sleeping"), SubagentLifecycle::Sleeping);
        assert_eq!(parse_lifecycle("tombstoned"), SubagentLifecycle::Tombstoned);
        assert_eq!(parse_lifecycle("unknown"), SubagentLifecycle::Idle);
    }

    #[tokio::test]
    async fn merged_loop_processes_each_turn_individually_in_order() {
        // v0.4.7 机制式合并（执行中台）：批 = 连续处理组，批内逐轮独立产出/触发下游。
        // 四个 turn 依次到达 → 每轮都有独立 ExecutionOutput（NoopProvider 空响应 →
        // failure_output，task_status 以 raw: 开头），且保序处理、不漏轮。
        let (pool, mut receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        for t in ["t1", "t2", "t3", "t4"] {
            pool.create_turn_context(execution_turn_context(t, "user", "think"))
                .await;
        }
        let (tx, rx) = mpsc::channel(8);
        let platform = ExecutionPlatform::new(
            rx,
            pool.clone(),
            Arc::new(NoopProvider),
            crate::data::ModelRow {
                id: "m1".into(),
                name: "test".into(),
                provider: "p".into(),
                api_url: "u".into(),
                api_type: "openai".into(),
                api_protocol: "openai-v1".into(),
                api_key: None,
                model_id: "model-x".into(),
                config: None,
            },
            SecretString::from("x".to_string()),
            None,
            None,
            None,
            None,
            None,
            true,
        );
        let handle = platform.spawn();

        for t in ["t1", "t2", "t3"] {
            tx.send(AgentMessage::Execute {
                turn_id: t.to_string(),
            })
            .await
            .unwrap();
        }
        // 处理中再补发 t4（飞行缓冲路径）——与 t3 同批或下一批，均逐轮处理。
        tx.send(AgentMessage::Execute {
            turn_id: "t4".into(),
        })
        .await
        .unwrap();
        drop(tx);

        tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("platform task 应在 rx 关闭后退出")
            .expect("platform task 不得 panic");

        // 逐轮产物：每个 turn 都有独立 ExecutionOutput。
        for t in ["t1", "t2", "t3", "t4"] {
            let ctx = pool.get_turn_context(t).await.expect("turn context 存在");
            let exec = ctx.execution.as_ref().expect("每轮都有执行产物");
            assert!(
                exec.task_status.starts_with("raw:"),
                "NoopProvider 空响应 → failure_output（task_status 以 raw: 开头），got {:?}",
                exec.task_status
            );
        }
        // 每个 turn 都触发了 ExecutionDone → 洞察 channel 收到 4 条（合并不得吞消息）。
        let mut done_count = 0;
        while let Ok(msg) = receivers.insight_rx.try_recv() {
            if matches!(msg, AgentMessage::ExecutionDone { .. }) {
                done_count += 1;
            }
        }
        assert_eq!(done_count, 4, "逐轮触发 ExecutionDone，不得合并吞消息");
    }

    #[tokio::test]
    async fn merged_loop_single_turn_degradation_identical() {
        // 单条到达：批=[1 条]=单条处理，无额外延迟；行为与逐条现状一致。
        let (pool, mut receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        pool.create_turn_context(execution_turn_context("t1", "user", "think"))
            .await;
        let (tx, rx) = mpsc::channel(8);
        let platform = ExecutionPlatform::new(
            rx,
            pool.clone(),
            Arc::new(NoopProvider),
            crate::data::ModelRow {
                id: "m1".into(),
                name: "test".into(),
                provider: "p".into(),
                api_url: "u".into(),
                api_type: "openai".into(),
                api_protocol: "openai-v1".into(),
                api_key: None,
                model_id: "model-x".into(),
                config: None,
            },
            SecretString::from("x".to_string()),
            None,
            None,
            None,
            None,
            None,
            true,
        );
        let handle = platform.spawn();
        tx.send(AgentMessage::Execute {
            turn_id: "t1".into(),
        })
        .await
        .unwrap();
        drop(tx);
        tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("platform task 应退出")
            .expect("platform task 不得 panic");

        let ctx = pool.get_turn_context("t1").await.unwrap();
        assert!(ctx.execution.is_some(), "单条退化行为与逐条现状一致");
        let mut done_count = 0;
        while let Ok(msg) = receivers.insight_rx.try_recv() {
            if matches!(msg, AgentMessage::ExecutionDone { .. }) {
                done_count += 1;
            }
        }
        assert_eq!(done_count, 1);
    }

    fn execution_turn_context(
        id: &str,
        user: &str,
        think: &str,
    ) -> crate::agent::communication::TurnContext {
        crate::agent::communication::TurnContext {
            turn_id: id.into(),
            thinking: crate::agent::communication::ThinkingOutput {
                decision: crate::agent::communication::ThinkDecision::Execute,
                think_message: think.into(),
                constraints: vec![],
            },
            execution: None,
            insight: None,
            memory: None,
            status: crate::agent::communication::TurnStatus::Executing,
            user_message: user.into(),
            input_kind: "user".into(),
            has_subagent_result: false,
        }
    }

    fn execution_message_platform() -> ExecutionPlatform {
        ExecutionPlatform::new(
            mpsc::channel(1).1,
            {
                let (pool, _) = AgentPool::new();
                Arc::new(pool)
            },
            Arc::new(NoopProvider),
            crate::data::ModelRow {
                id: "m1".into(),
                name: "test".into(),
                provider: "p".into(),
                api_url: "u".into(),
                api_type: "openai".into(),
                api_protocol: "openai-v1".into(),
                api_key: None,
                model_id: "model-x".into(),
                config: None,
            },
            SecretString::from("x".to_string()),
            None,
            None,
            None,
            None,
            None,
            true,
        )
    }

    #[tokio::test]
    async fn execution_messages_single_uses_user_assistant_and_system_instruction() {
        let platform = execution_message_platform();
        let ctx = execution_turn_context("t1", "原始用户输入ABC", "执行意图");
        let messages = platform.build_execution_messages(&[ctx], "BASE_PROMPT");

        assert_eq!(messages.len(), 4);
        assert!(matches!(
            &messages[0],
            ChatMessage::System {
                kind: SystemKind::Primary,
                ..
            }
        ));
        assert!(matches!(&messages[1], ChatMessage::User { .. }));
        assert!(matches!(&messages[2], ChatMessage::Assistant { .. }));
        assert!(matches!(
            &messages[3],
            ChatMessage::System {
                kind: SystemKind::Primary,
                ..
            }
        ));
        match &messages[0] {
            ChatMessage::System { text, .. } => {
                assert!(text.contains("BASE_PROMPT"));
                assert!(text.contains("## Thinking Input"));
                assert!(
                    !text.contains("原始用户输入ABC"),
                    "user input moved to User role"
                );
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn execution_messages_batch_has_n_contiguous_pairs() {
        let platform = execution_message_platform();
        let contexts = vec![
            execution_turn_context("t1", "第一个任务", "执行意图 1"),
            execution_turn_context("t2", "第二个任务", "执行意图 2"),
            execution_turn_context("t3", "第三个任务", "执行意图 3"),
        ];
        let messages = platform.build_execution_messages(&contexts, "BASE_PROMPT");

        // System(base) + N*(User+Assistant) + System(instruction)
        assert_eq!(messages.len(), 1 + contexts.len() * 2 + 1);
        for (i, ctx) in contexts.iter().enumerate() {
            assert!(matches!(&messages[1 + i * 2], ChatMessage::User { .. }));
            assert!(matches!(
                &messages[2 + i * 2],
                ChatMessage::Assistant { .. }
            ));
            match &messages[1 + i * 2] {
                ChatMessage::User { text } => assert_eq!(text, &ctx.user_message),
                _ => panic!(),
            }
        }
        match &messages[0] {
            ChatMessage::System { text, .. } => {
                assert!(!text.contains("## Thinking Input"));
            }
            _ => panic!(),
        }
        assert!(matches!(
            messages.last(),
            Some(ChatMessage::System {
                kind: SystemKind::Primary,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn full_capability_registry_lists_base_and_composite_for_subagent_design() {
        let mut registry = Registry::new();
        registry.base_capabilities.insert(
            "shell.exec".to_string(),
            crate::data::duckdb::loader::BaseCapabilityRow {
                id: "shell.exec".to_string(),
                name: "Execute Shell".to_string(),
                cap_type: "function".to_string(),
                description: "Run a shell command in the workspace".to_string(),
                schema_in: serde_json::json!({}),
                schema_out: serde_json::json!({}),
                executor: "builtin:shell.exec".to_string(),
                version: "1.0.0".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        registry.composite_capabilities.insert(
            "composite.probe".to_string(),
            crate::data::duckdb::loader::CompositeCapabilityRow {
                id: "composite.probe".to_string(),
                name: "Probe Composite".to_string(),
                description: "A composite example".to_string(),
                schema_in: Some(serde_json::json!({})),
                schema_out: Some(serde_json::json!({})),
                executor: None,
                dag: serde_json::json!([]),
                version: "1.0.0".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        let platform = ExecutionPlatform::new(
            mpsc::channel(1).1,
            {
                let (pool, _) = AgentPool::new();
                Arc::new(pool)
            },
            Arc::new(NoopProvider),
            crate::data::ModelRow {
                id: "m1".into(),
                name: "test".into(),
                provider: "p".into(),
                api_url: "u".into(),
                api_type: "openai".into(),
                api_protocol: "openai-v1".into(),
                api_key: None,
                model_id: "model-x".into(),
                config: None,
            },
            SecretString::from("x".to_string()),
            None,
            Some(registry),
            None,
            None,
            None,
            true,
        );

        let section = platform.full_capability_registry_section();
        assert!(section.contains("Full Capability Registry"));
        assert!(section.contains("Reference only"));
        assert!(section.contains("`shell.exec`"));
        assert!(section.contains("Run a shell command in the workspace"));
        assert!(section.contains("`composite.probe`"));
    }

    #[tokio::test]
    async fn models_section_never_prints_api_key() {
        let mut registry = Registry::new();
        registry.models.insert(
            "m1".to_string(),
            crate::data::duckdb::loader::ModelRow {
                id: "m1".into(),
                name: "test".into(),
                provider: "p".into(),
                api_url: "u".into(),
                api_type: "openai".into(),
                api_protocol: "openai-v1".into(),
                api_key: Some("super-secret".into()),
                model_id: "model-x".into(),
                config: None,
            },
        );
        let platform_section_text = ExecutionPlatform::new(
            mpsc::channel(1).1,
            {
                let (pool, _) = AgentPool::new();
                Arc::new(pool)
            },
            Arc::new(NoopProvider),
            crate::data::ModelRow {
                id: "m1".into(),
                name: "test".into(),
                provider: "p".into(),
                api_url: "u".into(),
                api_type: "openai".into(),
                api_protocol: "openai-v1".into(),
                api_key: Some("super-secret".into()),
                model_id: "model-x".into(),
                config: None,
            },
            SecretString::from("x".to_string()),
            None,
            Some(registry),
            None,
            None,
            None,
            true,
        )
        .models_section();
        assert!(!platform_section_text.contains("super-secret"));
    }
}
