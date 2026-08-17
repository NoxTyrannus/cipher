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
}

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
        }
    }

    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("execution_platform: started, polling rx");
            while let Some(msg) = self.execution_rx.recv().await {
                let pending = self.execution_rx.len();
                self.pool
                    .update_platform_status(move |s| s.execution_pending = pending)
                    .await;
                match msg {
                    AgentMessage::Execute { turn_id } => {
                        self.pool
                            .update_platform_status(|s| s.execution_active = Some(turn_id.clone()))
                            .await;
                        self.handle_execute(&turn_id).await;
                        self.pool
                            .update_platform_status(|s| s.execution_active = None)
                            .await;
                    }
                    AgentMessage::Cancel { .. } => {}
                    other => {
                        tracing::warn!("execution_platform: unexpected message: {other:?}");
                    }
                }
                self.pool.snapshot_detailed().await;
            }
            tracing::info!("execution_platform: rx closed, shutting down");
        })
    }

    async fn handle_execute(&self, turn_id: &str) {
        let Some(ctx) = self.pool.get_turn_context(turn_id).await else {
            tracing::warn!("execution_platform: TurnContext not found for turn_id={turn_id}");
            return;
        };

        let pool_entries = self.pool.snapshot().await;
        let subagent_states = self.pool.subagent_states().await;
        let prompt = self.build_execution_prompt(&ctx, &pool_entries, &subagent_states);

        let messages = vec![
            ChatMessage::System {
                text: prompt,
                kind: SystemKind::Primary,
            },
            ChatMessage::User {
                text: "现在做一轮 subagent 生命周期管理。输出 JSON。".to_string(),
            },
        ];

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

    fn build_execution_prompt(
        &self,
        ctx: &crate::agent::communication::TurnContext,
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
        sections.push(format!(
            "## Thinking Input\n\n**goal:** {}\n\n**constraints:**\n{}\n\n**message:** {}\n\n**user_message:** {}",
            ctx.thinking.goal,
            if ctx.thinking.constraints.is_empty() {
                "none".to_string()
            } else {
                ctx.thinking.constraints.join("\n")
            },
            ctx.thinking.message,
            ctx.user_message,
        ));
        sections.push(self.pool_section(pool_entries, subagent_states));
        sections.push(self.templates_section());
        sections.push(self.models_section());
        sections.join("\n\n")
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
        for row in registry.models.values() {
            lines.push(format!(
                "- id={} name={} provider={} model_id={} api_type={} config={}",
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
        let mut actions = Vec::new();
        for call in raw.capability_calls {
            actions.push(self.execute_one_call(call).await);
        }
        let mut subagent_states = self.pool.subagent_states().await;
        if let Err(e) = self.refresh_subagent_states().await {
            tracing::warn!("execution_platform: refresh subagent states failed: {e}");
        } else {
            subagent_states = self.pool.subagent_states().await;
        }
        ExecutionOutput {
            task_design: raw.task_design.unwrap_or_default(),
            task_status: raw.task_status.unwrap_or_default(),
            lifecycle_actions: actions,
            subagent_states,
        }
    }

    async fn execute_one_call(&self, call: RawCapabilityCall) -> CapabilityLifecycleRecord {
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
            return record(
                capability_id,
                capability_name(&call, None),
                arguments_summary,
                CapabilityLifecycleState::Rejected,
                Some("capability runtime unavailable".to_string()),
                logs,
            );
        };

        let service = match CapabilityService::new(registry, executor) {
            Ok(service) => service,
            Err(e) => {
                logs.push(format!("FAIL {capability_id}: {e}"));
                return record(
                    capability_id,
                    capability_name(&call, None),
                    arguments_summary,
                    CapabilityLifecycleState::Rejected,
                    Some(e.to_string()),
                    logs,
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
                record(
                    capability_id,
                    Some(result.capability_name),
                    arguments_summary,
                    state,
                    None,
                    logs,
                )
            }
            Err(e) => {
                logs.push(format!("FAIL {capability_id}: {e}"));
                let state = classify_error(&e);
                record(
                    capability_id,
                    capability_name(&call, None),
                    arguments_summary,
                    state,
                    Some(e.to_string()),
                    logs,
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
        self.pool
            .publish_event("execution_complete", turn_id.to_string());
    }
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
        if let Ok(raw) = serde_json::from_str::<ExecutionPlatformRawOutput>(&candidate) {
            return raw;
        }
        let repaired = crate::common::json_util::repair_json(&candidate);
        if repaired != candidate {
            if let Ok(raw) = serde_json::from_str::<ExecutionPlatformRawOutput>(&repaired) {
                return raw;
            }
        }
    }

    ExecutionPlatformRawOutput {
        task_design: Some("执行中台输出解析失败，本轮无能力调用。".to_string()),
        task_status: Some(format!(
            "invalid execution platform output: {}",
            crate::common::json_util::truncate_utf8_boundary(content, 160)
        )),
        capability_calls: vec![],
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
                tool_calls: vec![],
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
        let raw = parse_execution_output("not json");
        assert!(raw.capability_calls.is_empty());
        assert!(raw.task_status.as_deref().unwrap().contains("invalid"));
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
        )
        .models_section();
        assert!(!platform_section_text.contains("super-secret"));
    }
}
