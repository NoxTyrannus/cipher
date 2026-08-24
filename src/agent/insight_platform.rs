use crate::agent::agent_pool::registry::{AgentEntry, AgentIdentity, AgentStatus};
use crate::agent::agent_pool::AgentPool;
use crate::agent::communication::{
    AgentMessage, CapabilityLifecycleRecord, CapabilityLifecycleState, ExecutionOutput,
    InsightOutput, InsightResult, ThinkingOutput,
};
use crate::data::ModelRow;
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest};
use crate::logic::model::stream::StreamChunk;
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// 洞察正文发布前的确定性处理：剥离思考块（MiniMax 默认输出 <think> 块混入 content）、
/// trim、空输出标记、截断到发布预算。散文正文即最终产物——无 JSON 契约、无解析器。
const MAX_INSIGHT_BODY_CHARS: usize = 2000;

fn finalize_insight_text(raw_output: &str) -> String {
    let stripped = crate::common::json_util::strip_reasoning_preamble(raw_output);
    let text = stripped.trim();
    if text.is_empty() {
        "（洞察中台无输出）".to_string()
    } else {
        crate::common::json_util::truncate_utf8_boundary(text, MAX_INSIGHT_BODY_CHARS).to_string()
    }
}

pub struct InsightPlatform {
    insight_rx: mpsc::Receiver<AgentMessage>,
    pool: Arc<AgentPool>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    /// 洞察正文流投递通道：驱动洞察域的 capability-memory-agent（常驻滑动窗口）。
    capability_memory_tx: mpsc::Sender<String>,
    prompts_dir: Option<PathBuf>,
    /// subagent 记忆证据目录（<storage_root>/subagents/<id>/memory.json + last_output.json）。
    storage_root: Option<PathBuf>,
}

impl InsightPlatform {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        insight_rx: mpsc::Receiver<AgentMessage>,
        pool: Arc<AgentPool>,
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: SecretString,
        capability_memory_tx: mpsc::Sender<String>,
        prompts_dir: Option<PathBuf>,
        storage_root: Option<PathBuf>,
    ) -> Self {
        Self {
            insight_rx,
            pool,
            provider,
            model_row,
            api_key,
            capability_memory_tx,
            prompts_dir,
            storage_root,
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

        // 2.0.2 洞察中台输入组装：多段 assistant 原样连续、一次 LLM 调用。
        // 0~N 个已完成 subagent 结果段（memory.json evidence，代码侧组装）；
        // 「有结果」判定 = AgentPool subagent 状态变化（结果段数量 > 0，中间/最终结果均计）。
        let subagent_results = self.build_subagent_result_segments().await;
        let has_subagent_result = !subagent_results.is_empty();
        self.pool
            .set_turn_has_subagent_result(turn_id, has_subagent_result)
            .await;

        let used_capabilities = execution_capability_ids(execution);
        let prompt = build_insight_prompt(
            turn_id,
            &ctx.thinking.constraints,
            &used_capabilities,
            &pool_snapshot,
            self.prompts_dir.as_deref(),
        );

        // User 段语义：用户输入轮 = 原始用户输入；内部轮（回环轮触发洞察）省略 User 段。
        let user_segment = if ctx.input_kind == "user" {
            Some(ctx.user_message.as_str())
        } else {
            None
        };
        let messages = build_insight_messages(
            prompt,
            user_segment,
            &ctx.thinking,
            execution,
            &subagent_results,
        );
        let req = match LlmRequest::from_model_row(&self.model_row, messages, self.api_key.clone())
        {
            Ok(req) => req,
            Err(e) => {
                tracing::error!(
                    "insight_platform: request build failed for turn_id={turn_id}: {e}"
                );
                publish_insight(
                    &self.pool,
                    turn_id,
                    InsightResult {
                        insight: "（洞察中台无输出）".to_string(),
                    },
                    &self.capability_memory_tx,
                )
                .await;
                return;
            }
        };

        // 散文正文收集：流式增量拼接；流完成后 finalize（剥思考块→截断）并发布。
        let mut text = String::new();
        let mut on_chunk = |chunk: StreamChunk| {
            if let StreamChunk::Delta(delta) = chunk {
                text.push_str(&delta);
            }
        };
        let call_result = self.provider.call_stream(&req, &mut on_chunk).await;
        if let Ok(resp) = &call_result {
            if !resp.content.is_empty() && (text.is_empty() || resp.content.len() > text.len()) {
                text = resp.content.clone();
            }
        }

        let insight = finalize_insight_text(&text);
        publish_insight(
            &self.pool,
            turn_id,
            InsightResult {
                insight: insight.clone(),
            },
            &self.capability_memory_tx,
        )
        .await;
    }
}

async fn publish_insight(
    pool: &AgentPool,
    turn_id: &str,
    insight: InsightResult,
    capability_memory_tx: &mpsc::Sender<String>,
) {
    let output = InsightOutput {
        insight: insight.clone(),
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
    // 异步投递洞察正文给能力记忆 agent（常驻滑动窗口节点）；队列满则丢弃本轮（观察非关键路径）。
    if let Err(e) = capability_memory_tx.try_send(insight.insight) {
        tracing::warn!(
            "insight_platform: capability_memory_tx full, dropping current insight: {e}"
        );
    }
}

/// 2.0.2 洞察中台输入组装（每轮标准；多段 assistant **原样连续、一次 LLM 调用**）：
/// ```text
/// [System(平台提示词), (User(原始输入) 仅用户轮),
///  Assistant(思考引擎输出),              ← 必有
///  Assistant(执行中台输出),              ← 每轮都有（0 动作也算）
///  Assistant(subagent1 结果段),          ← 0~N 个（当前已完成的 subagent）
///  …,
///  System(指令)]
/// ```
fn build_insight_messages(
    prompt: String,
    user_segment: Option<&str>,
    thinking: &ThinkingOutput,
    execution: Option<&ExecutionOutput>,
    subagent_results: &[String],
) -> Vec<ChatMessage> {
    let mut messages = vec![ChatMessage::System {
        text: prompt,
        kind: SystemKind::Primary,
    }];
    if let Some(user_text) = user_segment {
        messages.push(ChatMessage::User {
            text: user_text.to_string(),
        });
    }
    // 思考引擎输出段（think_message 全文，必有）。
    messages.push(ChatMessage::Assistant {
        text: thinking.think_message.clone(),
    });
    // 执行中台输出段（每轮都有；无执行输出时以「无执行轮」占位，不做段内包装）。
    messages.push(ChatMessage::Assistant {
        text: build_execution_segment(execution),
    });
    // 0~N 个 subagent 结果段。
    for segment in subagent_results {
        messages.push(ChatMessage::Assistant {
            text: segment.clone(),
        });
    }
    messages.push(ChatMessage::System {
        text: "判断本轮方向是否仍然正确，并用一段自然语言输出你的判断。".to_string(),
        kind: SystemKind::Primary,
    });
    messages
}

fn build_insight_prompt(
    turn_id: &str,
    constraints: &[String],
    used_capabilities: &[String],
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

    let capability_section = if used_capabilities.is_empty() {
        "## Actual Capability IDs Used This Turn\n\n(none — 没有能力调用证据)".to_string()
    } else {
        let lines = used_capabilities
            .iter()
            .map(|id| format!("- {id}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("## Actual Capability IDs Used This Turn\n\n{lines}")
    };

    let trace_hint = format!(
        "## Trace Access\n\n本轮能力调用日志已落盘。判断方向时以 lifecycle_actions 中的 START/OK/FAIL 证据为准，不把计划或总结当作完成事实。turn_id={turn_id}\n"
    );

    let pool_summary = build_pool_snapshot_summary(pool_snapshot);

    format!(
        "{}\n\n## Task Input\n\n**Constraints:**\n{}\n\n{}\n\n{}\n\n## Agent Pool Status\n\n{}",
        base, constraints_str, capability_section, trace_hint, pool_summary,
    )
}

/// 执行中台输出段：task_design / task_status / lifecycle actions（含 START/OK/FAIL 证据）。
/// 无执行输出时输出「无执行轮」占位（2.0.5：不再是 say-only 语义）。
fn build_execution_segment(execution: Option<&ExecutionOutput>) -> String {
    let Some(exec) = execution else {
        return "（本轮无执行——无执行轮。请基于思考引擎输出与对话内容判断方向。）".to_string();
    };
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

/// subagent 结果段单段最大字符数（整段截断兜底）。
const MAX_SUBAGENT_RESULT_SEGMENT_CHARS: usize = 3000;

impl InsightPlatform {
    /// 组装 0~N 个已完成 subagent 的结果段（代码侧组装，不依赖模型自觉）：
    /// 读取 <storage_root>/subagents/<id>/memory.json（真实 input/actions/evidence/output）
    /// 与 last_output.json（status + summary）；「有结果」判定 = 该 subagent 至少有一条记忆
    /// 条目（AgentPool 状态变化已落盘：中间/最终结果均计）。
    async fn build_subagent_result_segments(&self) -> Vec<String> {
        let Some(root) = &self.storage_root else {
            return Vec::new();
        };
        let states = self.pool.subagent_states().await;
        let mut segments = Vec::new();
        for state in states {
            let memory = crate::agent::subagent_memory::read_memory(root, &state.subagent_id)
                .unwrap_or_default();
            if memory.entries.is_empty() {
                continue;
            }
            let last_output =
                crate::agent::subagent_memory::read_last_output(root, &state.subagent_id)
                    .ok()
                    .flatten();
            let text = format_subagent_result_segment(&state, &memory, last_output.as_ref());
            segments.push(crate::common::json_util::truncate_head_tail(
                &text,
                MAX_SUBAGENT_RESULT_SEGMENT_CHARS,
            ));
        }
        segments
    }
}

fn format_subagent_result_segment(
    state: &crate::agent::execution_types::SubagentRuntimeState,
    memory: &crate::agent::subagent_memory::SubagentMemory,
    last_output: Option<&crate::agent::subagent_memory::LastOutput>,
) -> String {
    let mut parts = vec![
        format!("subagent_id: {}", state.subagent_id),
        format!("lifecycle: {:?}", state.lifecycle),
    ];
    if let Some(lo) = last_output {
        let summary = crate::common::json_util::truncate_head_tail(
            &lo.summary,
            MAX_SUBAGENT_RESULT_SEGMENT_CHARS,
        );
        parts.push(format!("last_output: [{}] {}", lo.status, summary));
    }
    if !memory.entries.is_empty() {
        parts.push(format!("memory entries ({}):", memory.entries.len()));
        for (i, entry) in memory.entries.iter().enumerate() {
            let mut lines = vec![format!("  [entry {}]", i + 1)];
            if !entry.input.is_empty() {
                lines.push(format!(
                    "    input: {}",
                    crate::common::json_util::truncate_head_tail(&entry.input, 400)
                ));
            }
            if !entry.actions.is_empty() {
                lines.push(format!("    actions: {}", entry.actions.join("; ")));
            }
            if !entry.evidence.is_empty() {
                lines.push(format!("    evidence: {}", entry.evidence.join("; ")));
            }
            if !entry.output.is_empty() {
                lines.push(format!(
                    "    output: {}",
                    crate::common::json_util::truncate_head_tail(&entry.output, 400)
                ));
            }
            parts.push(lines.join("\n"));
        }
    }
    parts.join("\n")
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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pool: Arc<AgentPool>,
    rx: mpsc::Receiver<AgentMessage>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    capability_memory_tx: mpsc::Sender<String>,
    prompts_dir: Option<PathBuf>,
    storage_root: Option<PathBuf>,
) {
    let platform = InsightPlatform::new(
        rx,
        pool,
        provider,
        model_row,
        api_key,
        capability_memory_tx,
        prompts_dir,
        storage_root,
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
    fn build_insight_prompt_contains_constraints_and_capabilities() {
        let prompt = build_insight_prompt(
            "turn-1",
            &["约束1".to_string()],
            &["subagent.create".to_string()],
            &[],
            None,
        );
        assert!(prompt.contains("约束1"));
        assert!(prompt.contains("subagent.create"));
        assert!(prompt.contains("Actual Capability IDs Used This Turn"));
    }

    #[test]
    fn build_execution_segment_carries_lifecycle_failure_evidence() {
        let execution = execution_with(vec![record(
            "subagent.create",
            CapabilityLifecycleState::Rejected,
            vec!["FAIL subagent.create: bad template".into()],
            Some("bad template".into()),
        )]);
        let segment = build_execution_segment(Some(&execution));
        assert!(segment.contains("task_design"));
        assert!(segment.contains("task_status"));
        assert!(segment.contains("subagent.create"));
        assert!(segment.contains("bad template"));
    }

    #[test]
    fn build_execution_segment_none_marks_no_execution_round() {
        // 2.0.5：say-only 文案 → 「无执行轮」语义（机制保留：0 动作轮的洞察 fallback）。
        let segment = build_execution_segment(None);
        assert!(segment.contains("无执行轮"));
        assert!(!segment.contains("say-only"));
    }

    fn thinking_output(text: &str) -> ThinkingOutput {
        ThinkingOutput {
            decision: crate::agent::communication::ThinkDecision::Execute,
            think_message: text.to_string(),
            constraints: vec![],
        }
    }

    #[test]
    fn insight_messages_user_round_includes_three_assistant_segments() {
        // 用户轮：User 段存在；思考引擎输出 + 执行输出两段 assistant + 指令。
        let execution = execution_with(vec![record(
            "subagent.run",
            CapabilityLifecycleState::Accepted,
            vec!["START subagent.run: accepted".into()],
            None,
        )]);
        let messages = build_insight_messages(
            "PLATFORM_PROMPT".to_string(),
            Some("用户原始输入"),
            &thinking_output("think 全文"),
            Some(&execution),
            &[],
        );
        assert_eq!(messages.len(), 5);
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
        assert_eq!(
            messages[2],
            ChatMessage::Assistant {
                text: "think 全文".to_string()
            }
        );
        assert!(matches!(messages[3], ChatMessage::Assistant { .. }));
        assert!(matches!(
            &messages[4],
            ChatMessage::System {
                kind: SystemKind::Primary,
                ..
            }
        ));
    }

    #[test]
    fn insight_messages_internal_round_omits_user_segment() {
        // 内部轮（回环轮触发洞察）：省略 User 段。
        let messages = build_insight_messages(
            "PLATFORM_PROMPT".to_string(),
            None,
            &thinking_output("think 全文"),
            None,
            &[],
        );
        assert_eq!(messages.len(), 4);
        assert!(messages
            .iter()
            .all(|m| !matches!(m, ChatMessage::User { .. })));
        assert_eq!(
            messages[1],
            ChatMessage::Assistant {
                text: "think 全文".to_string()
            }
        );
        assert!(matches!(messages[2], ChatMessage::Assistant { .. }));
    }

    #[test]
    fn insight_messages_append_subagent_result_segments_verbatim() {
        // 0~N subagent 结果段：多段 assistant 原样连续、一次 LLM 调用。
        let messages = build_insight_messages(
            "PLATFORM_PROMPT".to_string(),
            None,
            &thinking_output("think 全文"),
            None,
            &["SEGMENT_A".to_string(), "SEGMENT_B".to_string()],
        );
        assert_eq!(messages.len(), 6);
        assert_eq!(
            messages[3],
            ChatMessage::Assistant {
                text: "SEGMENT_A".to_string()
            }
        );
        assert_eq!(
            messages[4],
            ChatMessage::Assistant {
                text: "SEGMENT_B".to_string()
            }
        );
    }

    #[test]
    fn format_subagent_result_segment_includes_evidence_and_last_output() {
        use crate::agent::execution_types::{
            SubagentLifecycle, SubagentRuntimeState, SubagentStartup,
        };
        use crate::agent::subagent_memory::{LastOutput, MemoryEntry, SubagentMemory};
        let state = SubagentRuntimeState {
            subagent_id: "sg_abc".into(),
            lifecycle: SubagentLifecycle::Idle,
            last_output_truncated: None,
            trigger: None,
            startup: SubagentStartup::Normal,
            lifecycle_kind: crate::agent::execution_types::SubagentLifecycleKind::Temporary,
        };
        let memory = SubagentMemory {
            entries: vec![MemoryEntry {
                t: "t1".into(),
                input: "统计日志".into(),
                actions: vec!["capability_id=shell.exec status=OK".into()],
                evidence: vec!["OK shell.exec: done".into()],
                output: "共 42 行".into(),
            }],
            truncation_records: vec![],
        };
        let last_output = LastOutput {
            subagent_id: "sg_abc".into(),
            t: "t2".into(),
            status: "completed".into(),
            summary: "任务完成".into(),
        };
        let text = format_subagent_result_segment(&state, &memory, Some(&last_output));
        assert!(text.contains("sg_abc"));
        assert!(text.contains("completed"));
        assert!(text.contains("任务完成"));
        assert!(text.contains("统计日志"));
        assert!(text.contains("shell.exec"));
        assert!(text.contains("共 42 行"));
    }

    #[test]
    fn format_subagent_result_segment_truncates_overlong_segment() {
        use crate::agent::execution_types::{
            SubagentLifecycle, SubagentRuntimeState, SubagentStartup,
        };
        use crate::agent::subagent_memory::{MemoryEntry, SubagentMemory};
        let state = SubagentRuntimeState {
            subagent_id: "sg_big".into(),
            lifecycle: SubagentLifecycle::Idle,
            last_output_truncated: None,
            trigger: None,
            startup: SubagentStartup::Normal,
            lifecycle_kind: crate::agent::execution_types::SubagentLifecycleKind::Temporary,
        };
        let memory = SubagentMemory {
            entries: vec![MemoryEntry {
                t: "t1".into(),
                input: "x".repeat(10_000),
                actions: vec![],
                evidence: vec![],
                output: String::new(),
            }],
            truncation_records: vec![],
        };
        let text = format_subagent_result_segment(&state, &memory, None);
        let truncated =
            crate::common::json_util::truncate_head_tail(&text, MAX_SUBAGENT_RESULT_SEGMENT_CHARS);
        assert!(truncated.contains("truncated"));
        assert!(
            truncated.chars().count() <= MAX_SUBAGENT_RESULT_SEGMENT_CHARS + 64,
            "segment must stay near budget, got {}",
            truncated.chars().count()
        );
    }

    #[test]
    fn finalize_insight_text_strips_think_and_trims() {
        let out = finalize_insight_text("<think>Let me analyze...</think>\n\n方向仍然正确。\n");
        assert_eq!(out, "方向仍然正确。");
    }

    #[test]
    fn finalize_insight_text_empty_marker() {
        assert_eq!(finalize_insight_text(""), "（洞察中台无输出）");
        assert_eq!(finalize_insight_text("   \n\t "), "（洞察中台无输出）");
    }

    #[test]
    fn finalize_insight_text_truncates_to_budget() {
        let long = "方".repeat(3000);
        let out = finalize_insight_text(&long);
        assert!(out.chars().count() <= MAX_INSIGHT_BODY_CHARS);
        assert!(out.ends_with('方'));
    }
}
