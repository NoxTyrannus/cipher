use crate::agent::agent_pool::registry::{AgentEntry, AgentIdentity, AgentStatus};
use crate::agent::agent_pool::AgentPool;
use crate::agent::communication::{
    AgentMessage, CapabilityLifecycleRecord, CapabilityLifecycleState, ExecutionOutput,
    InsightOutput, InsightResult, UsageObservation,
};
use crate::common::Result;
use crate::data::ModelRow;
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest};
use crate::logic::model::stream::StreamChunk;
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct InsightRawOutput {
    #[serde(default)]
    insight: Option<String>,
    #[serde(default)]
    usage_observations: Vec<UsageObservation>,
}

fn parse_insight_output(content: &str) -> Result<InsightRawOutput> {
    if let Ok(raw) = serde_json::from_str::<InsightRawOutput>(content) {
        if raw.insight.is_some() {
            return Ok(raw);
        }
    }

    if let Some(json_block) = extract_json_block(content) {
        if let Ok(raw) = serde_json::from_str::<InsightRawOutput>(&json_block) {
            if raw.insight.is_some() {
                return Ok(raw);
            }
        }
    }

    tracing::warn!(
        "insight_platform: failed to parse insight output: {}",
        crate::common::json_util::truncate_utf8_boundary(content, 200)
    );
    Ok(InsightRawOutput {
        insight: None,
        usage_observations: vec![],
    })
}

/// 从尚未闭合的 JSON 流中提取 `"insight"` 字符串字段，使 insight 闭合即可先驱动下游。
fn extract_json_string_field_prefix(text: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let key_start = text.find(&key)?;
    let rest = &text[key_start + key.len()..];
    let colon = rest.find(':')?;
    let after_colon = &rest[colon + 1..];
    let value_start = after_colon.find('"')?;
    let bytes = after_colon.as_bytes();
    let mut index = value_start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => {
                let candidate = &after_colon[value_start..=index];
                if let Ok(value) = serde_json::from_str::<String>(candidate) {
                    return Some(value);
                }
                return None;
            }
            _ => index += 1,
        }
    }
    None
}

fn extract_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let after_start = &text[start + 7..];
    let end = after_start.find("```")?;
    Some(after_start[..end].trim().to_string())
}

pub struct InsightPlatform {
    insight_rx: mpsc::Receiver<AgentMessage>,
    pool: Arc<AgentPool>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    usage_observation_tx: mpsc::Sender<Vec<UsageObservation>>,
    prompts_dir: Option<PathBuf>,
}

impl InsightPlatform {
    pub fn new(
        insight_rx: mpsc::Receiver<AgentMessage>,
        pool: Arc<AgentPool>,
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: SecretString,
        usage_observation_tx: mpsc::Sender<Vec<UsageObservation>>,
        prompts_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            insight_rx,
            pool,
            provider,
            model_row,
            api_key,
            usage_observation_tx,
            prompts_dir,
        }
    }

    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("insight_platform: started, polling rx");
            let heartbeat =
                AgentPool::spawn_core_heartbeat(&self.pool, "insight-platform", "insight-platform");

            while let Some(msg) = self.insight_rx.recv().await {
                let pending = self.insight_rx.len();
                self.pool
                    .update_platform_status(move |s| s.insight_pending = pending)
                    .await;
                match msg {
                    AgentMessage::ExecutionDone { turn_id } => {
                        self.pool
                            .update_platform_status(|s| s.insight_active = Some(turn_id.clone()))
                            .await;
                        self.pool
                            .set_core_agent_status("insight-platform", AgentStatus::Running)
                            .await;
                        self.handle_insight(&turn_id).await;
                        self.pool
                            .set_core_agent_status("insight-platform", AgentStatus::Idle)
                            .await;
                        self.pool
                            .update_platform_status(|s| s.insight_active = None)
                            .await;
                    }
                    AgentMessage::Cancel { .. } => {}
                    AgentMessage::MessageDeliver { .. } => {}
                    other => {
                        tracing::warn!("insight_platform: unexpected message: {other:?}");
                    }
                }
                self.pool.snapshot_detailed().await;
            }

            heartbeat.abort();
            tracing::info!("insight_platform: rx closed, shutting down");
        })
    }

    async fn handle_insight(&self, turn_id: &str) {
        let Some(ctx) = self.pool.get_turn_context(turn_id).await else {
            tracing::warn!("insight_platform: TurnContext not found for turn_id={turn_id}");
            return;
        };
        let execution = ctx.execution.as_ref();
        let pool_snapshot = self.pool.snapshot().await;

        let prompt = build_insight_prompt(
            turn_id,
            &ctx.thinking.goal,
            &ctx.thinking.constraints,
            execution,
            &pool_snapshot,
            self.prompts_dir.as_deref(),
        );

        // §8.4.2：洞察中台序列 [System(平台提示词+分析), User(原始用户输入), System(判断指令)]。
        let messages = build_insight_messages(prompt, &ctx.user_message);
        let req = match LlmRequest::from_model_row(&self.model_row, messages, self.api_key.clone())
        {
            Ok(req) => req,
            Err(e) => {
                tracing::error!(
                    "insight_platform: request build failed for turn_id={turn_id}: {e}"
                );
                publish_insight(&self.pool, turn_id, fallback_insight(execution)).await;
                return;
            }
        };

        let used_capabilities = execution_capability_ids(execution);
        let state = Arc::new(Mutex::new(InsightStreamState::default()));
        let notify = Arc::new(Notify::new());
        let parser = tokio::spawn(run_insight_stream_parser(
            Arc::clone(&self.pool),
            self.usage_observation_tx.clone(),
            turn_id.to_string(),
            used_capabilities,
            execution.cloned(),
            Arc::clone(&state),
            Arc::clone(&notify),
        ));

        let mut on_chunk = {
            let state = Arc::clone(&state);
            let notify = Arc::clone(&notify);
            move |chunk: StreamChunk| {
                if let StreamChunk::Delta(delta) = chunk {
                    if let Ok(mut guard) = state.lock() {
                        guard.text.push_str(&delta);
                    }
                    notify.notify_one();
                }
            }
        };

        let call_result = self.provider.call_stream(&req, &mut on_chunk).await;
        {
            if let Ok(mut guard) = state.lock() {
                if let Ok(resp) = &call_result {
                    if !resp.content.is_empty()
                        && (guard.text.is_empty() || resp.content.len() > guard.text.len())
                    {
                        guard.text = resp.content.clone();
                    }
                }
                guard.finished = true;
            }
        }
        notify.notify_one();

        if let Err(join_error) = parser.await {
            tracing::error!(
                "insight_platform: stream parser task failed for turn_id={turn_id}: {join_error}"
            );
        }
    }
}

#[derive(Default)]
struct InsightStreamState {
    text: String,
    insight_sent: bool,
    usage_done: bool,
    finished: bool,
}

async fn run_insight_stream_parser(
    pool: Arc<AgentPool>,
    usage_observation_tx: mpsc::Sender<Vec<UsageObservation>>,
    turn_id: String,
    used_capabilities: Vec<String>,
    execution: Option<ExecutionOutput>,
    state: Arc<Mutex<InsightStreamState>>,
    notify: Arc<Notify>,
) {
    loop {
        notify.notified().await;

        let snapshot = {
            let guard = match state.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            (
                guard.text.clone(),
                guard.insight_sent,
                guard.usage_done,
                guard.finished,
            )
        };

        let mut insight_text = extract_json_string_field_prefix(&snapshot.0, "insight");
        let mut observations = Vec::new();

        if snapshot.3 {
            match parse_insight_output(&snapshot.0) {
                Ok(raw) => {
                    if insight_text.is_none() {
                        insight_text = raw.insight;
                    }
                    observations = raw.usage_observations;
                }
                Err(e) => {
                    tracing::warn!(
                        "insight_platform: full insight output parse failed for turn_id={turn_id}: {e}"
                    );
                }
            }
            if insight_text.is_none() {
                tracing::warn!(
                    "insight_platform: no insight text received for turn_id={turn_id}, using fallback"
                );
                insight_text = Some(fallback_insight(execution.as_ref()).insight);
            }
        }

        if !snapshot.1 {
            if let Some(text) = insight_text {
                publish_insight(&pool, &turn_id, InsightResult { insight: text }).await;
                if let Ok(mut guard) = state.lock() {
                    guard.insight_sent = true;
                }
            }
        }

        if snapshot.3 && !snapshot.2 {
            let dropped = filter_usage_observations(&mut observations, &used_capabilities);
            if dropped > 0 {
                tracing::warn!(
                    dropped,
                    allowed = ?used_capabilities,
                    "insight_platform: dropped usage_observations for capability_id(s) not used this turn"
                );
            }
            if !observations.is_empty() {
                if let Err(e) = usage_observation_tx.try_send(observations) {
                    tracing::warn!(
                        "insight_platform: usage_observation_tx send error turn_id={turn_id}, error={e}"
                    );
                }
            }
            if let Ok(mut guard) = state.lock() {
                guard.usage_done = true;
            }
        }

        let complete = {
            let guard = match state.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            guard.insight_sent && guard.usage_done
        };
        if complete {
            return;
        }
    }
}

fn filter_usage_observations(
    observations: &mut Vec<UsageObservation>,
    used_capabilities: &[String],
) -> usize {
    let before = observations.len();
    observations.retain(|observation| {
        used_capabilities
            .iter()
            .any(|id| id == &observation.capability_id)
    });
    before - observations.len()
}

async fn publish_insight(pool: &AgentPool, turn_id: &str, insight: InsightResult) {
    let output = InsightOutput {
        insight,
        usage_observations: vec![],
    };
    pool.set_insight(turn_id, output).await;
    if let Err(e) = pool.send_insight_done(turn_id).await {
        tracing::warn!("insight_platform: send_insight_done failed: {e}");
    }
    if let Err(e) = pool.send_trigger(turn_id, "insight_complete").await {
        tracing::warn!("insight_platform: send_trigger insight_complete failed: {e}");
    }
    pool.publish_event("insight_complete", turn_id.to_string());
}

fn build_insight_messages(prompt: String, user_input: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::System {
            text: prompt,
            kind: SystemKind::Primary,
        },
        ChatMessage::User {
            text: user_input.to_string(),
        },
        ChatMessage::System {
            text: "判断本轮方向是否仍然正确，并输出 JSON。".to_string(),
            kind: SystemKind::Primary,
        },
    ]
}

fn build_insight_prompt(
    turn_id: &str,
    goal: &str,
    constraints: &[String],
    execution: Option<&ExecutionOutput>,
    pool_snapshot: &[AgentEntry],
    prompts_dir: Option<&std::path::Path>,
) -> String {
    let base = if let Some(dir) = prompts_dir {
        read_platform_prompt(dir, "insight_platform.md")
    } else {
        String::from(
            "You are the Insight Platform. Judge whether the current direction is still correct.",
        )
    };
    let constraints_str = if constraints.is_empty() {
        "none".to_string()
    } else {
        constraints
            .iter()
            .enumerate()
            .map(|(i, c)| format!("  {}. {}", i + 1, c))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let execution_section = match execution {
        Some(exec) => {
            let actions = exec
                .lifecycle_actions
                .iter()
                .map(format_lifecycle_record)
                .collect::<Vec<_>>()
                .join("\n---\n");
            let failed: Vec<&CapabilityLifecycleRecord> = exec
                .lifecycle_actions
                .iter()
                .filter(|record| {
                    matches!(
                        record.lifecycle_state,
                        CapabilityLifecycleState::Failed | CapabilityLifecycleState::Rejected
                    )
                })
                .collect();
            let failure_summary = if failed.is_empty() {
                "No failed or rejected lifecycle actions.".to_string()
            } else {
                let mut lines = vec![format!(
                    "{} lifecycle action(s) failed/rejected:",
                    failed.len()
                )];
                for record in &failed {
                    lines.push(format!(
                        "  - capability_id={}, error={}",
                        record.capability_id,
                        record.error.as_deref().unwrap_or("unknown")
                    ));
                }
                lines.join("\n")
            };
            format!(
                "## Execution Direction\n\n**task_design:** {}\n\n**task_status:** {}\n\n## Lifecycle Actions\n\n{}\n\n## Failure Summary\n\n{}",
                exec.task_design,
                exec.task_status,
                actions,
                failure_summary,
            )
        }
        None => "## Execution\n\n(本轮无执行——say-only 轮。请基于 Goal 与对话内容判断方向。)"
            .to_string(),
    };

    let used_capabilities = execution_capability_ids(execution);
    let capability_section = if used_capabilities.is_empty() {
        "## Actual Capability IDs Used This Turn\n\n(none — 没有能力调用证据；usage_observations 必须为空)"
            .to_string()
    } else {
        let lines = used_capabilities
            .iter()
            .map(|id| format!("- {id}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "## Actual Capability IDs Used This Turn\n\n{lines}\n\n规则：usage_observations 中的 capability_id 只能是上述 id 之一。禁止使用上述列表之外的能力名。"
        )
    };

    let trace_hint = format!(
        "## Trace Access\n\n本轮能力调用日志已落盘。判断方向时以 lifecycle_actions 中的 START/OK/FAIL 证据为准，不把计划或总结当作完成事实。turn_id={turn_id}\n"
    );

    let pool_summary = build_pool_snapshot_summary(pool_snapshot);

    format!(
        "{}\n\n## Task Input\n\n**Goal:** {}\n\n**Constraints:**\n{}\n\n{}\n\n{}\n\n{}\n\n## Agent Pool Status\n\n{}",
        base, goal, constraints_str, execution_section, capability_section, trace_hint, pool_summary,
    )
}

fn execution_capability_ids(execution: Option<&ExecutionOutput>) -> Vec<String> {
    let Some(execution) = execution else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for record in &execution.lifecycle_actions {
        if !record.capability_id.is_empty() {
            ids.push(record.capability_id.clone());
        }
        for log in &record.capability_call_logs {
            if let Some(id) = capability_id_from_call_log(log) {
                ids.push(id);
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

fn capability_id_from_call_log(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["START ", "OK ", "FAIL "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let id = rest.split(':').next().unwrap_or("").trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn format_lifecycle_record(record: &CapabilityLifecycleRecord) -> String {
    let mut lines = vec![
        format!("  capability_id: {}", record.capability_id),
        format!("  lifecycle_state: {:?}", record.lifecycle_state),
        format!("  arguments_summary: {}", record.arguments_summary),
    ];
    if let Some(error) = &record.error {
        lines.push(format!("  error: {error}"));
    }
    if let Some(invocation_ref) = &record.invocation_ref {
        lines.push(format!("  invocation_ref: {invocation_ref}"));
    }
    if !record.capability_call_logs.is_empty() {
        lines.push("  capability_call_logs:".to_string());
        for log in &record.capability_call_logs {
            lines.push(format!("    - {log}"));
        }
    }
    lines.join("\n")
}

fn fallback_insight(execution: Option<&ExecutionOutput>) -> InsightResult {
    let text = match execution {
        None => {
            "本轮无执行，属于纯对话轮；没有能力调用证据需要复核，也不产生能力使用观察。".to_string()
        }
        Some(execution) => {
            let failed: Vec<&CapabilityLifecycleRecord> = execution
                .lifecycle_actions
                .iter()
                .filter(|record| {
                    matches!(
                        record.lifecycle_state,
                        CapabilityLifecycleState::Failed | CapabilityLifecycleState::Rejected
                    )
                })
                .collect();
            if failed.is_empty() {
                "本轮生命周期动作已受理或完成；方向是否真正正确仍需以实际产物和下一轮 subagent 结果为最终证据。".to_string()
            } else {
                let ids = failed
                    .iter()
                    .map(|record| record.capability_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                if failed.len() == execution.lifecycle_actions.len() {
                    format!(
                        "本轮全部生命周期动作失败/被拒绝（{ids}），应先修正原因再继续，不把计划或总结当作完成。"
                    )
                } else {
                    format!(
                        "本轮部分生命周期动作失败/被拒绝（{ids}）。已受理/完成的事实可保留，失败项需在下一轮修正。"
                    )
                }
            }
        }
    };
    InsightResult { insight: text }
}

fn build_pool_snapshot_summary(snapshot: &[AgentEntry]) -> String {
    if snapshot.is_empty() {
        return "Agent pool is empty (no agents registered).".to_string();
    }

    let mut lines = vec![format!("Total agents in pool: {}", snapshot.len())];
    let mut thinking_count = 0;
    let mut execution_count = 0;
    let mut insight_count = 0;
    let mut memory_count = 0;
    let mut subagent_running = 0;
    let mut subagent_pending = 0;
    let mut subagent_resident = 0;

    for entry in snapshot {
        match &entry.identity {
            AgentIdentity::ThinkingEngine { .. } => thinking_count += 1,
            AgentIdentity::ExecutionPlatform => execution_count += 1,
            AgentIdentity::InsightPlatform => insight_count += 1,
            AgentIdentity::MemoryPlatform => memory_count += 1,
            AgentIdentity::SubagentRunning { .. } => subagent_running += 1,
            AgentIdentity::SubagentPending { .. } => subagent_pending += 1,
            AgentIdentity::SubagentResident { .. } => subagent_resident += 1,
        }
    }

    lines.push(format!(
        "  Platforms: execution={execution_count}, insight={insight_count}, memory={memory_count}"
    ));
    lines.push(format!("  Thinking engines (active): {thinking_count}"));
    lines.push(format!(
        "  Subagents: running={subagent_running}, pending={subagent_pending}, resident={subagent_resident}"
    ));

    let idle = snapshot
        .iter()
        .filter(|e| e.status == AgentStatus::Idle)
        .count();
    let running = snapshot
        .iter()
        .filter(|e| e.status == AgentStatus::Running)
        .count();
    let pending = snapshot
        .iter()
        .filter(|e| e.status == AgentStatus::Pending)
        .count();
    lines.push(format!(
        "  By status: idle={idle}, running={running}, pending={pending}"
    ));

    for entry in snapshot {
        let identity_str = match &entry.identity {
            AgentIdentity::ThinkingEngine { instance_id } => {
                format!("ThinkingEngine({instance_id})")
            }
            AgentIdentity::ExecutionPlatform => "ExecutionPlatform".into(),
            AgentIdentity::InsightPlatform => "InsightPlatform".into(),
            AgentIdentity::MemoryPlatform => "MemoryPlatform".into(),
            AgentIdentity::SubagentRunning { agent_id } => {
                format!("SubagentRunning({agent_id})")
            }
            AgentIdentity::SubagentPending { agent_id } => {
                format!("SubagentPending({agent_id})")
            }
            AgentIdentity::SubagentResident { agent_id } => {
                format!("SubagentResident({agent_id})")
            }
        };
        lines.push(format!(
            "  - {} | {} | {:?}",
            entry.id, identity_str, entry.status
        ));
    }

    lines.join("\n")
}

pub async fn run(
    pool: Arc<AgentPool>,
    rx: mpsc::Receiver<AgentMessage>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    usage_observation_tx: mpsc::Sender<Vec<UsageObservation>>,
    prompts_dir: Option<PathBuf>,
) {
    let platform = InsightPlatform::new(
        rx,
        pool,
        provider,
        model_row,
        api_key,
        usage_observation_tx,
        prompts_dir,
    );
    let handle = platform.spawn();
    match handle.await {
        Ok(()) => tracing::info!("insight_platform::run: platform spawn completed"),
        Err(e) => tracing::error!(
            "insight_platform::run: platform task panicked/aborted: {e} (thread death = channel closed)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::communication::CapabilityLifecycleRecord;

    fn record(
        capability_id: &str,
        state: CapabilityLifecycleState,
        logs: Vec<String>,
        error: Option<String>,
    ) -> CapabilityLifecycleRecord {
        CapabilityLifecycleRecord {
            capability_id: capability_id.into(),
            capability_name: String::new(),
            arguments_summary: "{}".into(),
            lifecycle_state: state,
            invocation_ref: Some("inv_1".into()),
            error,
            capability_call_logs: logs,
        }
    }

    fn execution_with(actions: Vec<CapabilityLifecycleRecord>) -> ExecutionOutput {
        ExecutionOutput {
            task_design: "设计".into(),
            task_status: "等待".into(),
            lifecycle_actions: actions,
            subagent_states: vec![],
        }
    }

    #[test]
    fn parse_valid_insight_raw_output() {
        let raw =
            parse_insight_output(r#"{"insight":"方向正确","usage_observations":[]}"#).unwrap();
        assert_eq!(raw.insight.as_deref(), Some("方向正确"));
        assert!(raw.usage_observations.is_empty());
    }

    #[test]
    fn parse_insight_raw_with_usage_observation() {
        let raw = parse_insight_output(
            r#"{"insight":"file.read 大文件超时","usage_observations":[{"capability_id":"file.read","observation":"大文件超时","suggestion":"分块读取"}]}"#,
        )
        .unwrap();
        assert_eq!(raw.usage_observations.len(), 1);
        assert_eq!(raw.usage_observations[0].capability_id, "file.read");
        assert_eq!(raw.usage_observations[0].suggestion, "分块读取");
    }

    #[test]
    fn parse_insight_output_invalid_returns_none_insight() {
        let raw = parse_insight_output("garbage").unwrap();
        assert!(raw.insight.is_none());
        assert!(raw.usage_observations.is_empty());
    }

    #[test]
    fn extract_insight_from_partial_stream() {
        assert_eq!(
            extract_json_string_field_prefix(r#"{"insight":"判断完成","#, "insight").as_deref(),
            Some("判断完成")
        );
        assert_eq!(
            extract_json_string_field_prefix(r#"{"insight":"判"#, "insight"),
            None
        );
    }

    #[test]
    fn filter_usage_observations_keeps_only_used_capabilities() {
        let mut observations = vec![
            UsageObservation {
                capability_id: "file.read".into(),
                observation: "keep".into(),
                suggestion: "keep".into(),
            },
            UsageObservation {
                capability_id: "ghost.cap".into(),
                observation: "drop".into(),
                suggestion: "drop".into(),
            },
        ];
        let dropped = filter_usage_observations(&mut observations, &["file.read".to_string()]);
        assert_eq!(dropped, 1);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].capability_id, "file.read");
    }

    #[test]
    fn execution_capability_ids_reads_lifecycle_evidence() {
        let execution = execution_with(vec![record(
            "subagent.run",
            CapabilityLifecycleState::Accepted,
            vec!["START subagent.run: accepted".into()],
            None,
        )]);
        assert_eq!(
            execution_capability_ids(Some(&execution)),
            vec!["subagent.run"]
        );
    }

    #[test]
    fn build_insight_prompt_contains_lifecycle_failure() {
        let execution = execution_with(vec![record(
            "subagent.create",
            CapabilityLifecycleState::Rejected,
            vec!["FAIL subagent.create: bad template".into()],
            Some("bad template".into()),
        )]);
        let prompt = build_insight_prompt("turn-1", "goal", &[], Some(&execution), &[], None);
        assert!(prompt.contains("goal"));
        assert!(prompt.contains("task_design"));
        assert!(prompt.contains("task_status"));
        assert!(prompt.contains("subagent.create"));
        assert!(prompt.contains("bad template"));
        assert!(prompt.contains("Actual Capability IDs Used This Turn"));
    }

    #[test]
    fn build_insight_prompt_none_execution_marks_say_only() {
        let prompt = build_insight_prompt("turn-1", "用户说你好", &[], None, &[], None);
        assert!(prompt.contains("say-only"));
        assert!(prompt.contains("用户说你好"));
    }

    #[test]
    fn insight_messages_include_original_user_input_and_system_instruction() {
        let messages = build_insight_messages("PLATFORM_PROMPT".to_string(), "用户原始输入");
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            &messages[0],
            ChatMessage::System {
                kind: SystemKind::Primary,
                ..
            }
        ));
        assert_eq!(
            messages[1],
            ChatMessage::User {
                text: "用户原始输入".to_string()
            }
        );
        assert!(matches!(
            &messages[2],
            ChatMessage::System {
                kind: SystemKind::Primary,
                ..
            }
        ));
    }

    #[test]
    fn fallback_insight_reports_rejected() {
        let result = fallback_insight(Some(&execution_with(vec![record(
            "subagent.run",
            CapabilityLifecycleState::Rejected,
            vec![],
            Some("sleeping".into()),
        )])));
        assert!(result.insight.contains("失败/被拒绝"));
    }

    #[test]
    fn fallback_insight_accepted_is_not_success_claim() {
        let result = fallback_insight(Some(&execution_with(vec![record(
            "subagent.run",
            CapabilityLifecycleState::Accepted,
            vec![],
            None,
        )])));
        assert!(result.insight.contains("受理或完成"));
    }

    #[test]
    fn fallback_insight_none_execution_say_only() {
        let result = fallback_insight(None);
        assert!(result.insight.contains("纯对话轮"));
    }
}
