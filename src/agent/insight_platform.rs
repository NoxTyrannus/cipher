use crate::agent::agent_pool::registry::{AgentEntry, AgentIdentity, AgentStatus};
use crate::agent::agent_pool::AgentPool;
use crate::agent::communication::{
    AgentMessage, CapabilityLifecycleRecord, CapabilityLifecycleState, ExecutionOutput,
    InsightOutput, InsightResult, ThinkingOutput, UsageObservation,
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

/// 宽松解析洞察中台输出：候选链（全文 → ```json 块 → strip+首对象 → 全文）+
/// 每候选先 serde 再 `repair_json` 重试 + Value 级字段提取（不整体判死）。
///
/// - `insight` 非字符串（数字/对象等）→ `to_string()` 保留（不判死）；
/// - `usage_observations` 数组内坏项独立跳过（字段级容错），其余项照常；
/// - 全部候选失败 → `insight=None, usage_observations=[]`（由调用方 fallback 处理）。
fn parse_insight_output(content: &str) -> Result<InsightRawOutput> {
    let mut candidates = Vec::new();
    let trimmed = content.trim().to_string();
    if !trimmed.is_empty() {
        candidates.push(trimmed.clone());
    }
    if let Some(block) = extract_json_block(content) {
        if !candidates.contains(&block) {
            candidates.push(block);
        }
    }
    let stripped = crate::common::json_util::strip_reasoning_preamble(content);
    if let Some(obj) = crate::common::json_util::extract_first_json_object(&stripped) {
        if !candidates.contains(&obj) {
            candidates.push(obj);
        }
    }
    if !candidates.contains(&trimmed) {
        candidates.push(trimmed);
    }

    for candidate in candidates {
        if let Some(raw) = parse_insight_raw(&candidate) {
            if raw.insight.is_some() {
                return Ok(raw);
            }
        }
        let repaired = crate::common::json_util::repair_json(&candidate);
        if repaired != candidate {
            if let Some(raw) = parse_insight_raw(&repaired) {
                if raw.insight.is_some() {
                    return Ok(raw);
                }
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

/// Value 级宽容提取：整体反序列化为 Value 后逐字段处理（不依赖 serde 结构整体判死）。
fn parse_insight_raw(text: &str) -> Option<InsightRawOutput> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = value.as_object()?;
    let insight = obj.get("insight").map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let mut usage_observations = Vec::new();
    if let Some(observations) = obj.get("usage_observations").and_then(|v| v.as_array()) {
        for item in observations {
            match serde_json::from_value::<UsageObservation>(item.clone()) {
                Ok(observation) => usage_observations.push(observation),
                Err(e) => tracing::warn!("insight_platform: 坏 usage_observation 项跳过: {e}"),
            }
        }
    }
    Some(InsightRawOutput {
        insight,
        usage_observations,
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
        usage_observation_tx: mpsc::Sender<Vec<UsageObservation>>,
        prompts_dir: Option<PathBuf>,
        storage_root: Option<PathBuf>,
    ) -> Self {
        Self {
            insight_rx,
            pool,
            provider,
            model_row,
            api_key,
            usage_observation_tx,
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
                publish_insight(&self.pool, turn_id, fallback_insight(""), &subagent_results).await;
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
            subagent_results.clone(),
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
    fact_segments: Vec<String>,
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
                insight_text = Some(fallback_insight(&snapshot.0).insight);
            }
        }

        if !snapshot.1 {
            if let Some(text) = insight_text {
                publish_insight(
                    &pool,
                    &turn_id,
                    InsightResult { insight: text },
                    &fact_segments,
                )
                .await;
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

/// 事实节单段最大字符数（系统提取摘要截断上限）。
const MAX_INSIGHT_FACT_CHARS: usize = 1200;

/// 输出修饰（事实节 ⊕ 判断节）：事实节 = 确定性系统提取（subagent 结果段，
/// 不依赖模型）；判断节 = 洞察模型解读原文。无 subagent 结果时省略事实节。
fn decorate_insight_text(insight: &str, fact_segments: &[String]) -> String {
    let mut parts = Vec::new();
    if !fact_segments.is_empty() {
        let facts = crate::common::json_util::truncate_head_tail(
            &fact_segments.join("\n\n"),
            MAX_INSIGHT_FACT_CHARS,
        );
        parts.push(format!("## 事实（系统提取）\n{facts}"));
    }
    parts.push(format!("## 洞察判断\n{insight}"));
    parts.join("\n\n")
}

async fn publish_insight(
    pool: &AgentPool,
    turn_id: &str,
    insight: InsightResult,
    fact_segments: &[String],
) {
    let insight = InsightResult {
        insight: decorate_insight_text(&insight.insight, fact_segments),
    };
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
        text: "判断本轮方向是否仍然正确，并输出 JSON。".to_string(),
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

/// 原文保留替代模板 fallback：不再生成模板话术——
/// `insight = 原始输出截断文本`（max 800 字符），下游可见模型原话；
/// 仅当原始输出完全为空/纯空白时返回标记文本「（洞察中台无输出）」。
fn fallback_insight(raw_output: &str) -> InsightResult {
    let text = if raw_output.trim().is_empty() {
        "（洞察中台无输出）".to_string()
    } else {
        crate::common::json_util::truncate_utf8_boundary(raw_output, 800).to_string()
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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pool: Arc<AgentPool>,
    rx: mpsc::Receiver<AgentMessage>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    usage_observation_tx: mpsc::Sender<Vec<UsageObservation>>,
    prompts_dir: Option<PathBuf>,
    storage_root: Option<PathBuf>,
) {
    let platform = InsightPlatform::new(
        rx,
        pool,
        provider,
        model_row,
        api_key,
        usage_observation_tx,
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
    fn fallback_insight_keeps_raw_output() {
        // 原文保留替代模板：insight = 原始输出截断文本。
        let result = fallback_insight("方向可能偏了，但这是模型原话");
        assert_eq!(result.insight, "方向可能偏了，但这是模型原话");
    }

    #[test]
    fn fallback_insight_truncates_to_800_chars() {
        let long = "x".repeat(2000);
        let result = fallback_insight(&long);
        assert!(result.insight.len() <= 800, "{}", result.insight.len());
        assert!(result.insight.starts_with("x"));
    }

    #[test]
    fn fallback_insight_empty_output_marker() {
        assert_eq!(fallback_insight("").insight, "（洞察中台无输出）");
        assert_eq!(fallback_insight("   \n\t ").insight, "（洞察中台无输出）");
    }

    #[test]
    fn parse_insight_output_partial_damage_extracts_leading_json() {
        // 部分损坏（散文 + 带头 JSON）：strip+首对象 提取。
        let raw = parse_insight_output(
            "先分析：\n{\"insight\":\"方向正确\",\"usage_observations\":[]}\n后记（未闭合",
        )
        .unwrap();
        assert_eq!(raw.insight.as_deref(), Some("方向正确"));
    }

    #[test]
    fn parse_insight_output_repair_path_recovers_bare_newline() {
        let raw = parse_insight_output(
            "{\"insight\":\"方向正确\",\"usage_observations\":[{\"capability_id\":\"file.read\",\"observation\":\"a\nb\",\"suggestion\":\"c\"}]}",
        )
        .unwrap();
        assert_eq!(raw.insight.as_deref(), Some("方向正确"));
        assert_eq!(raw.usage_observations.len(), 1);
        assert_eq!(raw.usage_observations[0].observation, "a\nb");
    }

    #[test]
    fn parse_insight_output_tolerates_non_string_insight() {
        // 类型容错：insight=数字 → to_string 保留（不判死）。
        let raw = parse_insight_output(r#"{"insight":42}"#).unwrap();
        assert_eq!(raw.insight.as_deref(), Some("42"));
        let raw = parse_insight_output(r#"{"insight":{"direction":"ok"}}"#).unwrap();
        assert_eq!(raw.insight.as_deref(), Some(r#"{"direction":"ok"}"#));
    }

    #[test]
    fn parse_insight_output_skips_bad_usage_observation_items() {
        // usage_observations 数组内坏项跳过（字段级容错），其余项照常。
        let raw = parse_insight_output(
            r#"{"insight":"x","usage_observations":[
                {"capability_id":"file.read","observation":"ok","suggestion":"s"},
                "not-an-object",
                {"capability_id":123}
            ]}"#,
        )
        .unwrap();
        assert_eq!(raw.insight.as_deref(), Some("x"));
        assert_eq!(raw.usage_observations.len(), 1);
        assert_eq!(raw.usage_observations[0].capability_id, "file.read");
    }

    #[test]
    fn parse_insight_output_candidate_chain_order() {
        // 候选链顺序：全文优先，其次 ```json 块。
        let raw = parse_insight_output(
            r#"{"insight":"from_plain"}\n```json\n{"insight":"from_block"}\n```"#,
        )
        .unwrap();
        assert_eq!(raw.insight.as_deref(), Some("from_plain"));
        // 无法整段解析时回退 ```json 块。
        let raw =
            parse_insight_output("```json\n{\"insight\":\"from_block\"}\n```\n尾部垃圾").unwrap();
        assert_eq!(raw.insight.as_deref(), Some("from_block"));
    }

    #[test]
    fn decorate_insight_text_without_facts_omits_fact_section() {
        let decorated = decorate_insight_text("判断：继续", &[]);
        assert_eq!(decorated, "## 洞察判断\n判断：继续");
        assert!(!decorated.contains("事实"));
    }

    #[test]
    fn decorate_insight_text_with_facts_prepends_fact_section() {
        let decorated = decorate_insight_text(
            "判断：继续",
            &["subagent_id: sg_1\nlifecycle: Idle\noutput: 完成".to_string()],
        );
        assert!(decorated.starts_with("## 事实（系统提取）\nsubagent_id: sg_1"));
        assert!(decorated.contains("\n\n## 洞察判断\n判断：继续"));
    }

    #[test]
    fn decorate_insight_text_truncates_facts_at_1200() {
        let long_segment = "y".repeat(1500);
        let decorated = decorate_insight_text("判断", &[long_segment]);
        assert!(
            decorated.contains("[truncated"),
            "事实节应被截断: {decorated}"
        );
        assert!(decorated.ends_with("## 洞察判断\n判断"));
    }
}
