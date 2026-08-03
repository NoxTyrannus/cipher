use crate::data::triviumdb::TriviumDb;
use crate::data::ModelRow;
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest, Message, MessageRole};
use secrecy::SecretString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::agent_pool::AgentPool;
use super::communication::{
    AgentMessage, AttentionFragment, AttentionRetireBatch, ExecutionOutput, ExperienceFragment,
    InsightOutput, MemoryOutput, NodeStatus,
};

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

    shared_trivium: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,

    memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,

    prompts_dir: Option<PathBuf>,

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
        shared_trivium: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,
        memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
        prompts_dir: Option<PathBuf>,
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
            experience_tx,
            preference_tx,
            cognitive_tx,
        }
    }

    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("memory_platform: started, polling rx");

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
                            .update_platform_status(|s| s.memory_active = None)
                            .await;
                    }
                    other => {
                        tracing::warn!("memory_platform: unexpected message: {:?}", other);
                    }
                }

                self.pool.snapshot_detailed().await;
            }

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
            10,
        )
        .await;

        let prompt = build_attention_prompt(
            &base_prompt,
            &ctx.thinking.goal,
            insight,
            execution,
            &existing_attention,
        );

        let messages = vec![
            Message {
                role: MessageRole::System,
                content: prompt,
            },
            Message {
                role: MessageRole::User,
                content: "Extract attention memories now. Output ONLY the JSON.".to_string(),
            },
        ];

        let req = match LlmRequest::from_model_row(&self.model_row, messages, self.api_key.clone())
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    "memory_platform: failed to build LLM request for turn_id={turn_id}: {e}"
                );
                let output = fallback_memory(execution);
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

        let result = match self.provider.call(&req).await {
            Ok(resp) => {
                let output = parse_memory_agent_output(&resp.content);
                Some(output)
            }
            Err(e) => {
                tracing::error!("memory_platform: LLM call failed for turn_id={turn_id}: {e}");
                None
            }
        };

        let output = match result {
            Some(agent_output) => {
                if !agent_output.settle.new_attention.is_empty() {
                    let attention_count = count_memories(
                        self.shared_trivium.as_ref(),
                        self.triviumdb_path.as_deref(),
                        "attention",
                    )
                    .await;
                    if attention_count >= MAX_ATTENTION_ENTRIES {
                        tracing::warn!(
                            "memory_platform: attention 已达上限 {} 条, 拒绝本轮 {} 条新注意力 \
                             (依赖 retired_focus 淘汰释放; 若持续满需检查淘汰链)",
                            MAX_ATTENTION_ENTRIES,
                            agent_output.settle.new_attention.len()
                        );
                    } else {
                        write_to_triviumdb(
                            self.shared_trivium.as_ref(),
                            self.triviumdb_path.as_deref(),
                            "attention",
                            &agent_output.settle.new_attention,
                        )
                        .await;
                    }
                }

                self.publish_attention_version(turn_id);

                if !agent_output.settle.retired_focus.is_empty() {
                    remove_attention_by_focus(
                        self.shared_trivium.as_ref(),
                        self.triviumdb_path.as_deref(),
                        &agent_output.settle.retired_focus,
                    )
                    .await;
                    let batch = AttentionRetireBatch {
                        retired_focus: agent_output.settle.retired_focus.clone(),
                    };
                    if let Some(ref tx) = self.experience_tx {
                        let _ = tx.try_send(batch.clone());
                    }
                    if let Some(ref tx) = self.preference_tx {
                        let _ = tx.try_send(batch);
                    }
                }

                MemoryOutput {
                    attention: agent_output.settle.new_attention,
                    experience: vec![],
                    preference: vec![],
                    cognitive: vec![],
                }
            }
            None => {
                tracing::warn!("memory_platform: LLM failed, using fallback for turn_id={turn_id}");
                fallback_memory(execution)
            }
        };

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

async fn rag_retrieve(
    shared_trivium: Option<&Arc<tokio::sync::Mutex<TriviumDb>>>,
    triviumdb_path: Option<&Path>,
    memory_type: &str,
    limit: usize,
) -> String {
    if let Some(shared) = shared_trivium {
        let db = shared.lock().await;
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

const MAX_ATTENTION_ENTRIES: usize = 2000;

async fn count_memories(
    shared_trivium: Option<&Arc<tokio::sync::Mutex<TriviumDb>>>,
    triviumdb_path: Option<&Path>,
    memory_type: &str,
) -> usize {
    if let Some(shared) = shared_trivium {
        let db = shared.lock().await;
        return count_memories_with_db(&db, memory_type);
    }
    let Some(path) = triviumdb_path else { return 0 };
    match TriviumDb::open(path, crate::data::triviumdb::DEFAULT_DIM) {
        Ok(db) => count_memories_with_db(&db, memory_type),
        Err(_) => 0,
    }
}

fn count_memories_with_db(db: &TriviumDb, memory_type: &str) -> usize {
    let mut count = 0usize;
    for id in db.db().get_all_ids() {
        let mtype = db
            .db()
            .get_payload(id)
            .and_then(|p| p.get("_memory_type").cloned())
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        if mtype == memory_type {
            count += 1;
        }
    }
    count
}

async fn remove_attention_by_focus(
    shared_trivium: Option<&Arc<tokio::sync::Mutex<TriviumDb>>>,
    triviumdb_path: Option<&Path>,
    retired_focus: &[String],
) {
    if retired_focus.is_empty() {
        return;
    }
    if let Some(shared) = shared_trivium {
        let mut db = shared.lock().await;
        remove_attention_by_focus_with_db(&mut db, retired_focus);
        return;
    }
    let Some(path) = triviumdb_path else { return };
    match TriviumDb::open(path, crate::data::triviumdb::DEFAULT_DIM) {
        Ok(mut db) => remove_attention_by_focus_with_db(&mut db, retired_focus),
        Err(e) => tracing::warn!("memory_platform: open trivium for retire-delete failed: {e}"),
    }
}

fn remove_attention_by_focus_with_db(db: &mut TriviumDb, retired_focus: &[String]) {
    let mut removed = 0usize;
    for id in db.db().get_all_ids() {
        let Some(payload) = db.db().get_payload(id) else {
            continue;
        };
        if payload.get("_memory_type").and_then(|v| v.as_str()) != Some("attention") {
            continue;
        }
        let focus = payload.get("focus").and_then(|v| v.as_str()).unwrap_or("");
        if retired_focus.iter().any(|r| r == focus) {
            let _ = db.db_mut().delete(id);
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::debug!("memory_platform: retired {removed} attention entries (retire-delete)");
    }
}

async fn write_to_triviumdb<T: serde::Serialize>(
    shared_trivium: Option<&Arc<tokio::sync::Mutex<TriviumDb>>>,
    triviumdb_path: Option<&Path>,
    memory_type: &str,
    fragments: &[T],
) {
    if let Some(shared) = shared_trivium {
        let mut db = shared.lock().await;
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
        for nr in &exec.node_results {
            if nr.status == NodeStatus::Failed {
                let title = format!("Node {} failed", nr.node_id);
                let error_reason = nr.error.as_deref().unwrap_or("unknown error");
                let summary = format!(
                    "Execution node '{}' failed with error: {}. Tool calls: {}. This failure should be analyzed for root cause and the system should learn to handle similar situations.",
                    nr.node_id,
                    error_reason,
                    nr.tool_call_count
                );
                experience.push(ExperienceFragment { title, summary });
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
struct SettleAction {
    new_attention: Vec<AttentionFragment>,
    retired_focus: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MemoryAgentOutput {
    settle: SettleAction,
}

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

fn build_attention_prompt(
    base_prompt: &str,
    goal: &str,
    insight: &InsightOutput,
    execution: Option<&ExecutionOutput>,
    existing_attention: &str,
) -> String {
    let insight_summary = format!(
        "Boundary Check: crossed={}, violations={:?}, analysis={}\n\
         Goal Alignment: aligned={}, deviation={:?}, analysis={}\n\
         Growth Check: growth_detected={}, growth_type={:?}, analysis={}\n\
         Needs Followup: {}",
        insight.insight.boundary_check.crossed,
        insight.insight.boundary_check.violations,
        insight.insight.boundary_check.analysis,
        insight.insight.goal_alignment.aligned,
        insight.insight.goal_alignment.deviation,
        insight.insight.goal_alignment.analysis,
        insight.insight.growth_check.growth_detected,
        insight.insight.growth_check.growth_type,
        insight.insight.growth_check.analysis,
        insight.insight.needs_followup,
    );

    let execution_summary = match execution {
        Some(exec) => {
            let mut lines = vec![format!("Overall Status: {:?}", exec.status)];
            for nr in &exec.node_results {
                let status_str = match nr.status {
                    NodeStatus::Completed => "Completed",
                    NodeStatus::Failed => "Failed",
                    NodeStatus::Skipped => "Skipped",
                };
                lines.push(format!(
                    "  - node_id={} {} (tool_calls={})",
                    nr.node_id, status_str, nr.tool_call_count
                ));
                if let Some(ref err) = nr.error {
                    lines.push(format!("    error: {}", err));
                }
            }
            lines.join("\n")
        }
        None => "No execution data available.".to_string(),
    };

    format!(
        "{}\n\n## Goal\n{}\n\n## Insight Analysis\n{}\n\n## Execution Results\n{}\n\n## Existing Attention\n{}",
        base_prompt, goal, insight_summary, execution_summary, existing_attention,
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pool: Arc<AgentPool>,
    rx: mpsc::Receiver<AgentMessage>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    triviumdb_path: Option<PathBuf>,
    shared_trivium: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,
    memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
    prompts_dir: Option<PathBuf>,
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
        BoundaryCheck, ExecutionDag, ExecutionOutput, ExecutionStatus, GoalAlignment, GrowthCheck,
        InsightOutput, InsightResult, NodeResult, NodeStatus,
    };
    use super::*;

    #[allow(dead_code)]
    fn make_insight_output() -> InsightOutput {
        InsightOutput {
            insight: InsightResult {
                boundary_check: BoundaryCheck {
                    crossed: false,
                    violations: vec![],
                    analysis: "all within bounds".into(),
                },
                goal_alignment: GoalAlignment {
                    aligned: true,
                    deviation: None,
                    analysis: "on track".into(),
                },
                growth_check: GrowthCheck {
                    growth_detected: false,
                    growth_type: None,
                    analysis: "steady".into(),
                },
                needs_followup: false,
                followup_hint: None,
            },
            tool_memory: vec![],
        }
    }

    #[test]
    fn fallback_memory_extracts_failure_experience() {
        let execution = ExecutionOutput {
            dag: ExecutionDag::Single {
                template_kind: "normal".into(),
                capability_ids: vec![],
                task_context: "test".into(),
            },
            node_results: vec![
                NodeResult {
                    node_id: "n1".into(),
                    status: NodeStatus::Completed,
                    summary: "done".into(),
                    error: None,
                    tool_call_count: 1,
                    tool_call_logs: vec![],
                },
                NodeResult {
                    node_id: "n2".into(),
                    status: NodeStatus::Failed,
                    summary: "".into(),
                    error: Some("timeout".into()),
                    tool_call_count: 3,
                    tool_call_logs: vec!["retry 1 failed".into(), "retry 2 failed".into()],
                },
            ],
            status: ExecutionStatus::PartialFailure,
        };

        let result = fallback_memory(Some(&execution));
        assert_eq!(
            result.experience.len(),
            1,
            "should have 1 failure experience"
        );
        assert!(result.experience[0].title.contains("n2"));
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
            dag: ExecutionDag::Single {
                template_kind: "normal".into(),
                capability_ids: vec![],
                task_context: "test".into(),
            },
            node_results: vec![NodeResult {
                node_id: "n1".into(),
                status: NodeStatus::Completed,
                summary: "done".into(),
                error: None,
                tool_call_count: 1,
                tool_call_logs: vec![],
            }],
            status: ExecutionStatus::Success,
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
