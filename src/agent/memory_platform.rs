use crate::data::duckdb::Registry;
use crate::data::triviumdb::TriviumDb;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::model::prompts::{
    compose_agent_capability_prompt, read_platform_prompt, CapabilityPromptEntry,
};
use crate::logic::model::provider::LlmProvider;
use secrecy::SecretString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::agent_pool::AgentPool;
use super::communication::{
    AgentMessage, AttentionFragment, AttentionRetireBatch, CapabilityLifecycleState,
    ExecutionOutput, ExperienceFragment, InsightOutput, MemoryOutput,
};
use super::memory::capability_agent::{run_capability_loop, CapabilityLoopRequest};

#[cfg(test)]
fn extract_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let after_start = &text[start + 7..];
    let end = after_start.find("```")?;
    Some(after_start[..end].trim().to_string())
}

pub struct MemoryPlatform {
    memory_rx: mpsc::Receiver<AgentMessage>,

    pool: Arc<AgentPool>,

    provider: Arc<dyn LlmProvider>,

    model_row: ModelRow,

    api_key: SecretString,

    triviumdb_path: Option<PathBuf>,

    shared_trivium: Option<Arc<std::sync::Mutex<TriviumDb>>>,

    memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,

    prompts_dir: Option<PathBuf>,

    registry: Option<Registry>,

    executor: Option<Arc<CapabilityExecutor>>,

    experience_tx: Option<mpsc::Sender<AttentionRetireBatch>>,

    preference_tx: Option<mpsc::Sender<AttentionRetireBatch>>,

    cognitive_tx: Option<mpsc::Sender<()>>,
}

impl MemoryPlatform {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory_rx: mpsc::Receiver<AgentMessage>,
        pool: Arc<AgentPool>,
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: SecretString,
        triviumdb_path: Option<PathBuf>,
        shared_trivium: Option<Arc<std::sync::Mutex<TriviumDb>>>,
        memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
        prompts_dir: Option<PathBuf>,
        registry: Option<Registry>,
        executor: Option<Arc<CapabilityExecutor>>,
        experience_tx: Option<mpsc::Sender<AttentionRetireBatch>>,
        preference_tx: Option<mpsc::Sender<AttentionRetireBatch>>,
        cognitive_tx: Option<mpsc::Sender<()>>,
    ) -> Self {
        Self {
            memory_rx,
            pool,
            provider,
            model_row,
            api_key,
            triviumdb_path,
            shared_trivium,
            memory_db,
            prompts_dir,
            registry,
            executor,
            experience_tx,
            preference_tx,
            cognitive_tx,
        }
    }

    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("memory_platform: started, polling rx");
            let heartbeat =
                AgentPool::spawn_core_heartbeat(&self.pool, "memory-platform", "memory-platform");

            let mut cognitive_count: u64 = 0;

            while let Some(msg) = self.memory_rx.recv().await {
                let pending = self.memory_rx.len();
                self.pool
                    .update_platform_status(move |s| s.memory_pending = pending)
                    .await;
                match msg {
                    AgentMessage::InsightDone { turn_id } => {
                        tracing::debug!("memory_platform: received InsightDone({turn_id})");
                        self.pool
                            .update_platform_status(|s| s.memory_active = Some(turn_id.clone()))
                            .await;
                        self.pool
                            .set_core_agent_status(
                                "memory-platform",
                                crate::agent::agent_pool::registry::AgentStatus::Running,
                            )
                            .await;

                        cognitive_count += 1;
                        if cognitive_count >= 29 {
                            cognitive_count = 0;
                        }
                        let remaining = 29 - cognitive_count;
                        self.pool
                            .update_platform_status(move |s| {
                                s.cognitive_remaining = remaining as u32
                            })
                            .await;
                        self.handle_memory(&turn_id).await;
                        self.pool
                            .set_core_agent_status(
                                "memory-platform",
                                crate::agent::agent_pool::registry::AgentStatus::Idle,
                            )
                            .await;
                        self.pool
                            .update_platform_status(|s| s.memory_active = None)
                            .await;
                    }
                    other => {
                        tracing::warn!("memory_platform: unexpected message: {:?}", other);
                    }
                }

                self.pool.snapshot_detailed().await;
            }

            heartbeat.abort();
            tracing::info!("memory_platform: rx closed, shutting down");
        })
    }

    async fn handle_memory(&self, turn_id: &str) {
        let ctx = match self.pool.get_turn_context(turn_id).await {
            Some(ctx) => ctx,
            None => {
                tracing::warn!("memory_platform: TurnContext not found for turn_id={turn_id}");
                return;
            }
        };

        let insight = match &ctx.insight {
            Some(i) => i,
            None => {
                tracing::warn!(
                    "memory_platform: no insight output for turn_id={turn_id}, fallback closing turn"
                );
                let output = fallback_memory(ctx.execution.as_ref());
                self.pool.set_memory(turn_id, output).await;
                self.pool.mark_done(turn_id).await;
                if let Err(e) = self.pool.send_trigger(turn_id, "memory_complete").await {
                    tracing::warn!("memory_platform: send_trigger failed: {e}");
                }
                self.pool
                    .publish_event("memory_complete", turn_id.to_string());
                return;
            }
        };

        let execution = ctx.execution.as_ref();

        tracing::debug!(
            "memory_platform: extracting memories for turn_id={turn_id}, has_execution={}",
            execution.is_some()
        );

        let base_prompt = match &self.prompts_dir {
            Some(dir) => read_platform_prompt(dir, "memory_attention.md"),
            None => String::new(),
        };

        let existing_attention = rag_retrieve(
            self.shared_trivium.as_ref(),
            self.triviumdb_path.as_deref(),
            "attention",
            100,
        )
        .await;

        // 2.0.3 记忆中台输入：一个完整轮次的三输出（思考引擎/执行中台/洞察中台），
        // 多段 assistant 原样连续、一次 LLM 调用（无 User 段、无段内包装）；
        // 异常路径缺段时不补空段。单段拼接 prompt（build_attention_prompt 段落合并）已废弃。
        let attention_prompt = build_attention_prompt(&base_prompt, &existing_attention);
        let assistant_segments = build_memory_assistant_segments(&ctx.thinking, execution, insight);

        let (Some(registry), Some(executor)) = (&self.registry, &self.executor) else {
            tracing::warn!(
                "memory_platform: capability runtime not configured, attention fallback"
            );
            let output = fallback_memory(execution);
            self.finish_turn(turn_id, output).await;
            return;
        };

        let available = memory_capability_entries(registry, executor, "attention-agent");
        let system_prompt = compose_agent_capability_prompt(&attention_prompt, &available);

        let outcome = run_capability_loop(
            &self.provider,
            &self.model_row,
            &self.api_key,
            registry,
            executor,
            CapabilityLoopRequest {
                actor_id: "attention-agent".to_string(),
                system_prompt,
                assistant_segments,
                user_prompt: format!(
                    "提取并维护注意力记忆。当前轮 thought_id = {turn_id}，作为 source_refs 证据索引。开始执行。"
                ),
            },
        )
        .await;

        let (output, retired) = match &outcome {
            Ok(trace) => {
                for line in &trace.logs {
                    tracing::info!("memory_platform attention-agent: {line}");
                }
                if !trace.completed {
                    tracing::warn!(
                        "memory_platform: attention-agent did not finish within max_turns ({} calls)",
                        trace.calls.len()
                    );
                }
                (
                    attention_output_from_trace(trace),
                    retired_focus_from_trace(trace),
                )
            }
            Err(e) => {
                tracing::warn!("memory_platform: attention-agent loop failed, using fallback: {e}");
                (fallback_memory(execution), (Vec::new(), Vec::new()))
            }
        };

        if !retired.0.is_empty() {
            let batch = AttentionRetireBatch {
                retired_focus: retired.0,
                source_refs: retired.1,
            };
            if let Some(ref tx) = self.experience_tx {
                let _ = tx.try_send(batch.clone());
            }
            if let Some(ref tx) = self.preference_tx {
                let _ = tx.try_send(batch);
            }
        }

        self.publish_attention_version(turn_id);

        if let Some(ref tx) = self.cognitive_tx {
            let _ = tx.try_send(());
        }

        self.pool.set_memory(turn_id, output).await;

        self.pool.mark_done(turn_id).await;

        if let Err(e) = self.pool.send_trigger(turn_id, "memory_complete").await {
            tracing::warn!("memory_platform: send_trigger failed: {e}");
        }
        self.pool
            .publish_event("memory_complete", turn_id.to_string());
        tracing::debug!("memory_platform: turn_id={turn_id} done, memory written, trigger sent");
    }
}

impl MemoryPlatform {
    async fn finish_turn(&self, turn_id: &str, output: MemoryOutput) {
        self.pool.set_memory(turn_id, output).await;
        self.pool.mark_done(turn_id).await;
        if let Err(e) = self.pool.send_trigger(turn_id, "memory_complete").await {
            tracing::warn!("memory_platform: send_trigger failed: {e}");
        }
        self.pool
            .publish_event("memory_complete", turn_id.to_string());
    }

    fn publish_attention_version(&self, turn_id: &str) {
        use crate::agent::memory::memory_version as mv;
        let Some(db) = &self.memory_db else {
            return;
        };
        let conn = match db.lock() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("memory_platform: memory_db lock poisoned: {e}");
                return;
            }
        };
        let snapshot_ref = format!("trivium://attention/{turn_id}");
        let sources = vec![turn_id.to_string()];
        match mv::stage(
            &conn,
            mv::MemoryVersionKind::Attention,
            &snapshot_ref,
            &sources,
        ) {
            Ok(vid) => {
                if let Err(e) = mv::publish(&conn, vid) {
                    tracing::warn!("memory_platform: publish attention version {vid} failed: {e}");
                } else {
                    tracing::debug!(
                        "memory_platform: attention version {vid} published (turn {turn_id})"
                    );
                }
            }
            Err(e) => {
                tracing::warn!("memory_platform: stage attention version failed: {e}");
            }
        }
    }
}

fn memory_capability_entries(
    registry: &Registry,
    executor: &Arc<CapabilityExecutor>,
    actor_id: &str,
) -> Vec<CapabilityPromptEntry> {
    let Ok(service) = crate::logic::capability::service::CapabilityService::new(registry, executor)
    else {
        return Vec::new();
    };
    let Ok(defs) = service.definitions_for_agent(actor_id) else {
        return Vec::new();
    };
    defs.into_iter()
        .map(|d| CapabilityPromptEntry {
            capability_id: d.capability_id,
            capability_name: d.capability_name,
            description: d.description,
        })
        .collect()
}

fn attention_output_from_trace(
    trace: &crate::agent::memory::capability_agent::CapabilityLoopOutcome,
) -> MemoryOutput {
    let mut attention = Vec::new();
    for call in &trace.calls {
        if call.capability_id != "memory.attention.write" || !call.ok {
            continue;
        }
        if let Some(entries) = call.arguments.get("entries").and_then(|v| v.as_array()) {
            for entry in entries {
                let focus = entry.get("focus").and_then(|v| v.as_str()).unwrap_or("");
                let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if focus.is_empty() || content.is_empty() {
                    continue;
                }
                let source_refs = entry
                    .get("source_refs")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                attention.push(AttentionFragment {
                    focus: focus.to_string(),
                    content: content.to_string(),
                    source_refs,
                });
            }
        }
    }
    MemoryOutput {
        attention,
        experience: vec![],
        preference: vec![],
        cognitive: vec![],
    }
}

fn retired_focus_from_trace(
    trace: &crate::agent::memory::capability_agent::CapabilityLoopOutcome,
) -> (Vec<String>, Vec<Vec<String>>) {
    let mut focus = Vec::new();
    let mut source_refs = Vec::new();
    for call in &trace.calls {
        if call.capability_id != "memory.attention.retire" || !call.ok {
            continue;
        }
        if let Some(items) = call.output.get("retired").and_then(|v| v.as_array()) {
            for item in items {
                let Some(f) = item.get("focus").and_then(|v| v.as_str()) else {
                    continue;
                };
                if focus.iter().any(|existing: &String| existing == f) {
                    continue;
                }
                let refs = item
                    .get("source_refs")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                focus.push(f.to_string());
                source_refs.push(refs);
            }
        }
    }
    (focus, source_refs)
}

async fn rag_retrieve(
    shared_trivium: Option<&Arc<std::sync::Mutex<TriviumDb>>>,
    triviumdb_path: Option<&Path>,
    memory_type: &str,
    limit: usize,
) -> String {
    if let Some(shared) = shared_trivium {
        let db = match shared.lock() {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("memory_platform: triviumdb lock poisoned: {e}");
                return String::new();
            }
        };
        return rag_retrieve_with_db(&db, memory_type, limit);
    }
    let db_path = match triviumdb_path {
        Some(p) => p,
        None => return String::new(),
    };

    if !db_path.exists() {
        return String::new();
    }

    let db = match TriviumDb::open(db_path, crate::data::triviumdb::DEFAULT_DIM) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("memory_platform: TriviumDB open failed for RAG: {e}");
            return String::new();
        }
    };

    rag_retrieve_with_db(&db, memory_type, limit)
}

fn rag_retrieve_with_db(db: &TriviumDb, memory_type: &str, limit: usize) -> String {
    let ids = db.db().get_all_ids();

    let mut lines = Vec::new();
    for id in ids {
        if lines.len() >= limit {
            break;
        }
        let payload = match db.db().get_payload(id) {
            Some(p) => p,
            None => continue,
        };

        if payload.get("_memory_type").and_then(|v| v.as_str()) != Some(memory_type) {
            continue;
        }

        let formatted = format_memory_entry(memory_type, &payload);
        lines.push(formatted);
    }

    if lines.is_empty() {
        return "No existing memories found.".to_string();
    }

    lines.join("\n---\n")
}

fn format_memory_entry(memory_type: &str, payload: &serde_json::Value) -> String {
    match memory_type {
        "attention" => {
            let focus = payload.get("focus").and_then(|v| v.as_str()).unwrap_or("");
            let content = payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("- focus: {}\n  content: {}", focus, content)
        }
        "experience" => {
            let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let summary = payload
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("- title: {}\n  summary: {}", title, summary)
        }
        "preference" => {
            let key = payload.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let value = payload.get("value").and_then(|v| v.as_str()).unwrap_or("");
            format!("- key: {}\n  value: {}", key, value)
        }
        "cognitive" => {
            let entity = payload.get("entity").and_then(|v| v.as_str()).unwrap_or("");
            let relation = payload
                .get("relation")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let target = payload.get("target").and_then(|v| v.as_str()).unwrap_or("");
            format!(
                "- entity: {}\n  relation: {}\n  target: {}",
                entity, relation, target
            )
        }
        _ => format!("{:?}", payload),
    }
}

#[cfg(test)]
async fn write_to_triviumdb<T: serde::Serialize>(
    shared_trivium: Option<&Arc<std::sync::Mutex<TriviumDb>>>,
    triviumdb_path: Option<&Path>,
    memory_type: &str,
    fragments: &[T],
) {
    if let Some(shared) = shared_trivium {
        let mut db = match shared.lock() {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("memory_platform: triviumdb lock poisoned: {e}");
                return;
            }
        };
        write_to_triviumdb_with_db(&mut db, memory_type, fragments);
        return;
    }
    let db_path = match triviumdb_path {
        Some(p) => p,
        None => return,
    };

    let mut db = match TriviumDb::open(db_path, crate::data::triviumdb::DEFAULT_DIM) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("memory_platform: TriviumDB open failed for write: {e}");
            return;
        }
    };

    write_to_triviumdb_with_db(&mut db, memory_type, fragments);
}

#[cfg(test)]
fn write_to_triviumdb_with_db<T: serde::Serialize>(
    db: &mut TriviumDb,
    memory_type: &str,
    fragments: &[T],
) {
    for fragment in fragments {
        let mut payload = match serde_json::to_value(fragment) {
            Ok(serde_json::Value::Object(payload)) => payload,
            Ok(_) => {
                tracing::warn!(
                    "memory_platform: structured {memory_type} fragment is not an object"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    "memory_platform: cannot serialize structured {memory_type} fragment: {error}"
                );
                continue;
            }
        };
        payload.insert(
            "_memory_type".to_string(),
            serde_json::Value::String(memory_type.to_string()),
        );
        let zero_vec = vec![0.0_f32; db.db().dim()];
        if let Err(e) = db
            .db_mut()
            .insert(&zero_vec, serde_json::Value::Object(payload))
        {
            tracing::warn!("memory_platform: TriviumDB insert failed for {memory_type}: {e}");
        }
    }

    if let Err(error) = db.flush() {
        tracing::warn!("memory_platform: TriviumDB flush failed for {memory_type}: {error}");
    }

    tracing::debug!(
        "memory_platform: wrote {} {memory_type} entries to TriviumDB",
        fragments.len()
    );
}

fn fallback_memory(execution: Option<&ExecutionOutput>) -> MemoryOutput {
    let mut experience = Vec::new();

    if let Some(exec) = execution {
        for record in &exec.lifecycle_actions {
            if matches!(
                record.lifecycle_state,
                CapabilityLifecycleState::Failed | CapabilityLifecycleState::Rejected
            ) {
                let title = format!("Lifecycle action {} failed", record.capability_id);
                let error_reason = record.error.as_deref().unwrap_or("unknown error");
                let summary = format!(
                    "Lifecycle action '{}' ended {:?} with error: {}. Capability calls: {}. This failure should be analyzed for root cause and the system should learn to handle similar situations.",
                    record.capability_id,
                    record.lifecycle_state,
                    error_reason,
                    record.capability_call_logs.len()
                );
                experience.push(ExperienceFragment {
                    title,
                    summary,
                    source_refs: vec![],
                });
            }
        }
    }

    MemoryOutput {
        attention: vec![],
        experience,
        preference: vec![],
        cognitive: vec![],
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg(test)]
struct SettleAction {
    new_attention: Vec<AttentionFragment>,
    retired_focus: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg(test)]
struct MemoryAgentOutput {
    settle: SettleAction,
}

#[cfg(test)]
fn parse_memory_agent_output(content: &str) -> MemoryAgentOutput {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return MemoryAgentOutput {
            settle: SettleAction {
                new_attention: vec![],
                retired_focus: vec![],
            },
        };
    }

    if let Ok(output) = serde_json::from_str::<MemoryAgentOutput>(trimmed) {
        return output;
    }

    if let Some(json_str) = extract_json_block(trimmed) {
        if let Ok(output) = serde_json::from_str::<MemoryAgentOutput>(&json_str) {
            return output;
        }
    }

    tracing::warn!("memory_platform: parse_memory_agent_output failed, raw={trimmed}");
    MemoryAgentOutput {
        settle: SettleAction {
            new_attention: vec![],
            retired_focus: vec![],
        },
    }
}

/// 记忆中台 System(平台提示词)：基础提示词 + 已有注意力快照（RAG 检索结果）。
/// 2.0.3：单段拼接（## Goal/## Think/## Insight/## Execution 段落合并）已废弃。
fn build_attention_prompt(base_prompt: &str, existing_attention: &str) -> String {
    format!(
        "{}\n\n## Existing Attention\n{}",
        base_prompt, existing_attention
    )
}

/// 三输出段截断预算（每段）。
const MEMORY_SEGMENT_TRUNCATE_CHARS: usize = 2000;

/// 2.0.3 记忆中台输入组装：一个完整轮次的三个输出 —— 思考引擎输出（think_message）、
/// 执行中台输出（ExecutionOutput）、洞察中台输出（InsightOutput）；
/// 多段 assistant 原样连续、一次 LLM 调用，不做段内包装；缺段时不补空段。
fn build_memory_assistant_segments(
    thinking: &crate::agent::communication::ThinkingOutput,
    execution: Option<&ExecutionOutput>,
    insight: &InsightOutput,
) -> Vec<String> {
    let mut segments = Vec::with_capacity(3);
    if !thinking.think_message.trim().is_empty() {
        segments.push(crate::common::json_util::truncate_head_tail(
            &thinking.think_message,
            MEMORY_SEGMENT_TRUNCATE_CHARS,
        ));
    }
    if let Some(exec) = execution {
        let mut lines = vec![
            format!("task_design: {}", exec.task_design),
            format!("task_status: {}", exec.task_status),
        ];
        for record in &exec.lifecycle_actions {
            lines.push(format!(
                "  - capability_id={} lifecycle={:?} (calls={})",
                record.capability_id,
                record.lifecycle_state,
                record.capability_call_logs.len()
            ));
            if let Some(ref err) = record.error {
                lines.push(format!("    error: {}", err));
            }
        }
        segments.push(crate::common::json_util::truncate_head_tail(
            &lines.join("\n"),
            MEMORY_SEGMENT_TRUNCATE_CHARS,
        ));
    }
    if !insight.insight.insight.trim().is_empty() {
        segments.push(crate::common::json_util::truncate_head_tail(
            &insight.insight.insight,
            MEMORY_SEGMENT_TRUNCATE_CHARS,
        ));
    }
    segments
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pool: Arc<AgentPool>,
    rx: mpsc::Receiver<AgentMessage>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    triviumdb_path: Option<PathBuf>,
    shared_trivium: Option<Arc<std::sync::Mutex<TriviumDb>>>,
    memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
    prompts_dir: Option<PathBuf>,
    registry: Option<Registry>,
    executor: Option<Arc<CapabilityExecutor>>,
    experience_tx: Option<mpsc::Sender<AttentionRetireBatch>>,
    preference_tx: Option<mpsc::Sender<AttentionRetireBatch>>,
    cognitive_tx: Option<mpsc::Sender<()>>,
) {
    let platform = MemoryPlatform::new(
        rx,
        pool,
        provider,
        model_row,
        api_key,
        triviumdb_path,
        shared_trivium,
        memory_db,
        prompts_dir,
        registry,
        executor,
        experience_tx,
        preference_tx,
        cognitive_tx,
    );
    let handle = platform.spawn();
    match handle.await {
        Ok(()) => tracing::info!("memory_platform::run: platform spawn completed"),
        Err(e) => tracing::error!(
            "memory_platform::run: platform task panicked/aborted: {e} (thread death = channel closed)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::communication::{
        CapabilityLifecycleRecord, CapabilityLifecycleState, ExecutionOutput, InsightOutput,
        InsightResult,
    };
    use super::*;

    #[allow(dead_code)]
    fn make_insight_output() -> InsightOutput {
        InsightOutput {
            insight: InsightResult {
                insight: "all within bounds".into(),
            },
            usage_observations: vec![],
        }
    }

    #[test]
    fn attention_prompt_keeps_base_and_existing_attention() {
        let prompt = build_attention_prompt("BASE", "existing attention");
        assert!(prompt.contains("BASE"));
        assert!(prompt.contains("Existing Attention"));
        assert!(prompt.contains("existing attention"));
    }

    #[test]
    fn memory_assistant_segments_three_outputs_in_order() {
        // 2.0.3：三输出多段 assistant 组装（思考引擎 / 执行中台 / 洞察中台）。
        let thinking = crate::agent::communication::ThinkingOutput {
            decision: crate::agent::communication::ThinkDecision::Execute,
            think_message: "think 中发现用户偏好暗色主题".into(),
            constraints: vec![],
        };
        let execution = ExecutionOutput {
            task_design: "设计".into(),
            task_status: "等待".into(),
            lifecycle_actions: vec![CapabilityLifecycleRecord {
                capability_id: "subagent.run".into(),
                capability_name: "Run Subagent".into(),
                arguments_summary: "{}".into(),
                lifecycle_state: CapabilityLifecycleState::Accepted,
                invocation_ref: None,
                error: None,
                capability_call_logs: vec!["START subagent.run".into()],
            }],
            subagent_states: vec![],
        };
        let segments =
            build_memory_assistant_segments(&thinking, Some(&execution), &make_insight_output());
        assert_eq!(segments.len(), 3);
        assert!(segments[0].contains("暗色主题"));
        assert!(segments[1].contains("task_design"));
        assert!(segments[1].contains("subagent.run"));
        assert!(segments[2].contains("all within bounds"));
    }

    #[test]
    fn memory_assistant_segments_missing_outputs_not_padded() {
        // 2.0.3：异常路径缺段时不补空段。
        let thinking = crate::agent::communication::ThinkingOutput {
            decision: crate::agent::communication::ThinkDecision::Execute,
            think_message: "only think".into(),
            constraints: vec![],
        };
        let segments = build_memory_assistant_segments(&thinking, None, &make_insight_output());
        assert_eq!(segments.len(), 2, "缺执行输出 → 不补空段");
        assert_eq!(segments[0], "only think");

        let empty_insight = InsightOutput {
            insight: crate::agent::communication::InsightResult {
                insight: String::new(),
            },
            usage_observations: vec![],
        };
        let segments = build_memory_assistant_segments(&thinking, None, &empty_insight);
        assert_eq!(segments.len(), 1, "洞察为空 + 无执行 → 只剩 think");
    }

    #[test]
    fn memory_assistant_segments_truncate_overlong() {
        let thinking = crate::agent::communication::ThinkingOutput {
            decision: crate::agent::communication::ThinkDecision::Execute,
            think_message: "x".repeat(10_000),
            constraints: vec![],
        };
        let segments = build_memory_assistant_segments(
            &thinking,
            None,
            &InsightOutput {
                insight: crate::agent::communication::InsightResult {
                    insight: String::new(),
                },
                usage_observations: vec![],
            },
        );
        assert_eq!(segments.len(), 1);
        assert!(segments[0].contains("truncated"));
        assert!(
            segments[0].chars().count() <= MEMORY_SEGMENT_TRUNCATE_CHARS + 64,
            "segment must stay near budget, got {}",
            segments[0].chars().count()
        );
    }

    #[test]
    fn fallback_memory_extracts_failure_experience() {
        let execution = ExecutionOutput {
            task_design: "test".into(),
            task_status: "waiting".into(),
            lifecycle_actions: vec![
                CapabilityLifecycleRecord {
                    capability_id: "subagent.create".into(),
                    capability_name: "Create Subagent".into(),
                    arguments_summary: "{}".into(),
                    lifecycle_state: CapabilityLifecycleState::Completed,
                    invocation_ref: None,
                    error: None,
                    capability_call_logs: vec![],
                },
                CapabilityLifecycleRecord {
                    capability_id: "subagent.run".into(),
                    capability_name: "Run Subagent".into(),
                    arguments_summary: "{}".into(),
                    lifecycle_state: CapabilityLifecycleState::Failed,
                    invocation_ref: None,
                    error: Some("timeout".into()),
                    capability_call_logs: vec!["retry 1 failed".into(), "retry 2 failed".into()],
                },
            ],
            subagent_states: vec![],
        };

        let result = fallback_memory(Some(&execution));
        assert_eq!(
            result.experience.len(),
            1,
            "should have 1 failure experience"
        );
        assert!(result.experience[0].title.contains("subagent.run"));
        assert!(result.experience[0].summary.contains("timeout"));
    }

    #[test]
    fn fallback_memory_no_execution_returns_empty() {
        let result = fallback_memory(None);
        assert!(result.experience.is_empty());
        assert!(result.attention.is_empty());
    }

    #[test]
    fn fallback_memory_all_success_no_experience() {
        let execution = ExecutionOutput {
            task_design: "test".into(),
            task_status: "done".into(),
            lifecycle_actions: vec![CapabilityLifecycleRecord {
                capability_id: "subagent.create".into(),
                capability_name: "Create Subagent".into(),
                arguments_summary: "{}".into(),
                lifecycle_state: CapabilityLifecycleState::Completed,
                invocation_ref: None,
                error: None,
                capability_call_logs: vec![],
            }],
            subagent_states: vec![],
        };

        let result = fallback_memory(Some(&execution));
        assert!(
            result.experience.is_empty(),
            "no failures = no experience entries"
        );
    }

    #[tokio::test]
    async fn rag_retrieve_no_path_returns_empty() {
        let result = rag_retrieve(None, None, "attention", 10).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn rag_retrieve_nonexistent_path_returns_empty() {
        let path = PathBuf::from("/nonexistent/path.trivium");
        let result = rag_retrieve(None, Some(&path), "attention", 10).await;
        assert!(result.is_empty());
    }

    #[test]
    fn format_memory_entry_attention() {
        let payload = serde_json::json!({
            "_memory_type": "attention",
            "focus": "test topic",
            "content": "description"
        });
        let formatted = format_memory_entry("attention", &payload);
        assert!(formatted.contains("test topic"));
        assert!(formatted.contains("description"));
    }

    #[test]
    fn format_memory_entry_experience() {
        let payload = serde_json::json!({
            "_memory_type": "experience",
            "title": "learned",
            "summary": "something"
        });
        let formatted = format_memory_entry("experience", &payload);
        assert!(formatted.contains("learned"));
        assert!(formatted.contains("something"));
    }

    #[test]
    fn format_memory_entry_preference() {
        let payload = serde_json::json!({
            "_memory_type": "preference",
            "key": "theme",
            "value": "dark"
        });
        let formatted = format_memory_entry("preference", &payload);
        assert!(formatted.contains("theme"));
        assert!(formatted.contains("dark"));
    }

    #[test]
    fn format_memory_entry_cognitive() {
        let payload = serde_json::json!({
            "_memory_type": "cognitive",
            "entity": "A",
            "relation": "uses",
            "target": "B"
        });
        let formatted = format_memory_entry("cognitive", &payload);
        assert!(formatted.contains("A"));
        assert!(formatted.contains("uses"));
        assert!(formatted.contains("B"));
    }

    #[tokio::test]
    async fn trivium_write_persists_structured_fragment_without_debug_envelope() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("memory.trivium");
        let fragments = vec![AttentionFragment {
            focus: "migration".to_string(),
            content: "keep structured fields".to_string(),
            source_refs: vec![],
        }];

        write_to_triviumdb(None, Some(&path), "attention", &fragments).await;

        let database = TriviumDb::open(&path, crate::data::triviumdb::DEFAULT_DIM).unwrap();
        let ids = database.db().all_node_ids();
        assert_eq!(ids.len(), 1);
        let payload = database.db().get_payload(ids[0]).unwrap();
        assert_eq!(payload["_memory_type"], "attention");
        assert_eq!(payload["focus"], "migration");
        assert_eq!(payload["content"], "keep structured fields");
        assert!(payload.get("data").is_none());
    }

    #[test]
    fn extract_json_block_finds_json() {
        let text = "prefix\n```json\n{\"key\": \"value\"}\n```\nsuffix";
        let result = extract_json_block(text);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "{\"key\": \"value\"}");
    }

    #[test]
    fn extract_json_block_no_block_returns_none() {
        let text = "plain text";
        assert!(extract_json_block(text).is_none());
    }

    #[test]
    fn extract_json_block_only_json() {
        let text = "```json\n[1, 2, 3]\n```";
        let result = extract_json_block(text);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "[1, 2, 3]");
    }

    #[test]
    fn parse_memory_agent_output_valid() {
        let json = r#"{"settle":{"new_attention":[{"focus":"test topic","content":"description"}],"retired_focus":[]}}"#;
        let output = parse_memory_agent_output(json);
        assert_eq!(output.settle.new_attention.len(), 1);
        assert_eq!(output.settle.new_attention[0].focus, "test topic");
        assert!(output.settle.retired_focus.is_empty());
    }

    #[test]
    fn parse_memory_agent_output_empty() {
        let output = parse_memory_agent_output("");
        assert!(output.settle.new_attention.is_empty());
        assert!(output.settle.retired_focus.is_empty());
    }

    #[test]
    fn parse_memory_agent_output_in_code_block() {
        let text = "```json\n{\"settle\":{\"new_attention\":[{\"focus\":\"f1\",\"content\":\"c1\"}],\"retired_focus\":[\"old\"]}}\n```";
        let output = parse_memory_agent_output(text);
        assert_eq!(output.settle.new_attention.len(), 1);
        assert_eq!(output.settle.retired_focus.len(), 1);
        assert_eq!(output.settle.retired_focus[0], "old");
    }

    #[test]
    fn parse_memory_agent_output_garbage_returns_default() {
        let output = parse_memory_agent_output("not json at all");
        assert!(output.settle.new_attention.is_empty());
        assert!(output.settle.retired_focus.is_empty());
    }
}
