use crate::common::Result;
use crate::data::ModelRow;
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest, Message, MessageRole};
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::agent_pool::registry::{AgentEntry, AgentIdentity, AgentStatus};
use super::agent_pool::AgentPool;
use super::communication::{
    AgentMessage, BoundaryCheck, ExecutionOutput, GoalAlignment, GrowthCheck, InsightOutput,
    InsightResult, NodeResult, NodeStatus, ToolMemoryUpdate,
};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct InsightRawOutput {
    #[serde(default)]
    insight: Option<InsightResult>,

    #[serde(default)]
    tool_memory: Vec<ToolMemoryUpdate>,
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

    if let Ok(insight) = serde_json::from_str::<InsightResult>(content) {
        return Ok(InsightRawOutput {
            insight: Some(insight),
            tool_memory: vec![],
        });
    }
    if let Some(json_block) = extract_json_block(content) {
        if let Ok(insight) = serde_json::from_str::<InsightResult>(&json_block) {
            return Ok(InsightRawOutput {
                insight: Some(insight),
                tool_memory: vec![],
            });
        }
    }

    tracing::warn!(
        "insight_platform: failed to parse InsightRawOutput from LLM content: {}",
        crate::common::json_util::truncate_utf8_boundary(content, 200)
    );
    Ok(InsightRawOutput {
        insight: None,
        tool_memory: vec![],
    })
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

    tool_memory_tx: mpsc::Sender<Vec<ToolMemoryUpdate>>,

    prompts_dir: Option<PathBuf>,
}

impl InsightPlatform {
    pub fn new(
        insight_rx: mpsc::Receiver<AgentMessage>,
        pool: Arc<AgentPool>,
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: SecretString,
        tool_memory_tx: mpsc::Sender<Vec<ToolMemoryUpdate>>,
        prompts_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            insight_rx,
            pool,
            provider,
            model_row,
            api_key,
            tool_memory_tx,
            prompts_dir,
        }
    }

    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("insight_platform: started, polling rx");

            while let Some(msg) = self.insight_rx.recv().await {
                let pending = self.insight_rx.len();
                self.pool
                    .update_platform_status(move |s| s.insight_pending = pending)
                    .await;
                match msg {
                    AgentMessage::ExecutionDone { turn_id } => {
                        tracing::debug!("insight_platform: received ExecutionDone({turn_id})");
                        self.pool
                            .update_platform_status(|s| s.insight_active = Some(turn_id.clone()))
                            .await;
                        self.handle_insight(&turn_id).await;
                        self.pool
                            .update_platform_status(|s| s.insight_active = None)
                            .await;
                    }
                    AgentMessage::MessageDeliver { turn_id, message } => {
                        tracing::debug!(
                            "insight_platform: received MessageDeliver(turn_id={turn_id}, message_len={})",
                            message.len()
                        );
                    }
                    AgentMessage::Cancel { turn_id } => {
                        tracing::debug!("insight_platform: received Cancel({turn_id}), ignoring (insight runs after execution)");
                    }
                    other => {
                        tracing::warn!("insight_platform: unexpected message: {:?}", other);
                    }
                }

                self.pool.snapshot_detailed().await;
            }

            tracing::info!("insight_platform: rx closed, shutting down");
        })
    }

    async fn handle_insight(&self, turn_id: &str) {
        let ctx = match self.pool.get_turn_context(turn_id).await {
            Some(ctx) => ctx,
            None => {
                tracing::warn!("insight_platform: TurnContext not found for turn_id={turn_id}");
                return;
            }
        };

        let execution = match &ctx.execution {
            Some(exec) => exec,
            None => {
                tracing::warn!(
                    "insight_platform: no execution output for turn_id={turn_id}, skipping"
                );
                return;
            }
        };

        let pool_snapshot = self.pool.snapshot().await;

        tracing::debug!(
            "insight_platform: analyzing turn_id={turn_id}, execution_status={:?}, {} nodes, pool_agents={}",
            execution.status,
            execution.node_results.len(),
            pool_snapshot.len()
        );

        let prompt = build_insight_prompt(
            &ctx.thinking.goal,
            &ctx.thinking.constraints,
            execution,
            &pool_snapshot,
            self.prompts_dir.as_deref(),
        );

        let (insight_result, tool_memory_updates) = match self.call_llm_for_insight(&prompt).await {
            Ok(raw) => {
                let insight = match raw.insight {
                    Some(i) => i,
                    None => {
                        tracing::warn!(
                            "insight_platform: LLM returned no insight for turn_id={turn_id}, using fallback"
                        );
                        fallback_insight(execution)
                    }
                };
                (insight, raw.tool_memory)
            }
            Err(e) => {
                tracing::error!("insight_platform: LLM call failed for turn_id={turn_id}: {e}");

                (fallback_insight(execution), vec![])
            }
        };

        let output = InsightOutput {
            insight: insight_result,
            tool_memory: tool_memory_updates.clone(),
        };
        self.pool.set_insight(turn_id, output).await;
        if let Err(e) = self.pool.send_insight_done(turn_id).await {
            tracing::warn!("insight_platform: send_insight_done failed: {e}");
        }

        if let Err(e) = self.pool.send_trigger(turn_id, "insight_complete").await {
            tracing::warn!("insight_platform: send_trigger insight_complete failed: {e}");
        }
        self.pool
            .publish_event("insight_complete", turn_id.to_string());

        if !tool_memory_updates.is_empty() {
            tracing::debug!(
                "insight_platform: routing {} tool_memory update(s) to service layer for turn_id={turn_id}",
                tool_memory_updates.len()
            );
            if let Err(e) = self.tool_memory_tx.try_send(tool_memory_updates) {
                tracing::warn!(
                    "insight_platform: tool_memory_tx send error turn_id={turn_id}, error={e}"
                );
            }
        }

        tracing::debug!(
            "insight_platform: turn_id={turn_id} insight done, InsightDone DM sent, tool_memory routed"
        );
    }

    async fn call_llm_for_insight(&self, prompt: &str) -> Result<InsightRawOutput> {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: prompt.to_string(),
            },
            Message {
                role: MessageRole::User,
                content: "Perform the three-question self-check now. Output ONLY the JSON."
                    .to_string(),
            },
        ];

        let req = LlmRequest::from_model_row(&self.model_row, messages, self.api_key.clone())?;

        let resp = self.provider.call(&req).await?;
        match parse_insight_output(&resp.content) {
            Ok(raw) => Ok(raw),
            Err(first_error) => {
                tracing::warn!("insight_platform: 洞察输出解析失败, 重试 1 次: {first_error}");
                let retry_prompt = format!(
                    "{prompt}\n\n## 上次输出解析失败\n{first_error}\n\
                     请输出**单个完整 JSON 对象** (analysis 每段 ≤ 2 句简洁, 总输出 ≤ 600 字符, 不要截断)。"
                );
                let retry_messages = vec![
                    Message {
                        role: MessageRole::System,
                        content: retry_prompt,
                    },
                    Message {
                        role: MessageRole::User,
                        content: "Retry: output ONLY the complete JSON.".to_string(),
                    },
                ];
                let retry_req = LlmRequest::from_model_row(
                    &self.model_row,
                    retry_messages,
                    self.api_key.clone(),
                )?;
                let retry_resp = self.provider.call(&retry_req).await?;
                parse_insight_output(&retry_resp.content)
            }
        }
    }
}

fn build_insight_prompt(
    goal: &str,
    constraints: &[String],
    execution: &ExecutionOutput,
    pool_snapshot: &[AgentEntry],
    prompts_dir: Option<&std::path::Path>,
) -> String {
    let base = if let Some(dir) = prompts_dir {
        read_platform_prompt(dir, "insight_platform.md")
    } else {
        String::from("You are the Insight Platform. Perform a three-question self-check on the execution results.")
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

    let nodes_summary = execution
        .node_results
        .iter()
        .map(format_node_result)
        .collect::<Vec<_>>()
        .join("\n---\n");

    let failed_nodes: Vec<&NodeResult> = execution
        .node_results
        .iter()
        .filter(|nr| nr.status == NodeStatus::Failed)
        .collect();

    let failure_summary = if failed_nodes.is_empty() {
        "No failures detected.".to_string()
    } else {
        let mut lines = vec![format!("{} node(s) failed:", failed_nodes.len())];
        for nr in &failed_nodes {
            lines.push(format!(
                "  - node_id={}, error={}",
                nr.node_id,
                nr.error.as_deref().unwrap_or("unknown")
            ));
        }
        lines.join("\n")
    };

    let pool_summary = build_pool_snapshot_summary(pool_snapshot);

    format!(
        "{}\n\n## Task Input\n\n**Goal:** {}\n\n**Constraints:**\n{}\n\n## Execution Results\n\n**Overall Status:** {:?}\n\n**Node Results:**\n{}\n\n## Failure Summary\n\n{}\n\n## Agent Pool Status\n\n{}",
        base,
        goal,
        constraints_str,
        execution.status,
        nodes_summary,
        failure_summary,
        pool_summary,
    )
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
        "  Platforms: execution={}, insight={}, memory={}",
        execution_count, insight_count, memory_count
    ));
    lines.push(format!("  Thinking engines (active): {}", thinking_count));
    lines.push(format!(
        "  Subagents: running={}, pending={}, resident={}",
        subagent_running, subagent_pending, subagent_resident
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
        "  By status: idle={}, running={}, pending={}",
        idle, running, pending
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

fn format_node_result(nr: &NodeResult) -> String {
    let mut lines = vec![
        format!("  node_id: {}", nr.node_id),
        format!("  status: {:?}", nr.status),
        format!("  summary: {}", nr.summary),
        format!("  tool_call_count: {}", nr.tool_call_count),
    ];

    if let Some(ref err) = nr.error {
        lines.push(format!("  error: {}", err));
    }

    if !nr.tool_call_logs.is_empty() {
        lines.push("  tool_call_logs:".to_string());
        for log in &nr.tool_call_logs {
            lines.push(format!("    - {}", log));
        }
    }

    lines.join("\n")
}

fn fallback_insight(execution: &ExecutionOutput) -> InsightResult {
    let has_failures = execution
        .node_results
        .iter()
        .any(|nr| nr.status == NodeStatus::Failed);

    let all_failed = execution
        .node_results
        .iter()
        .all(|nr| nr.status == NodeStatus::Failed);

    let boundary_check = if all_failed {
        BoundaryCheck {
            crossed: true,
            violations: vec!["execution_platform: all nodes failed".into()],
            analysis: "insight_platform: all execution nodes failed — likely a constraint or capability issue".into(),
        }
    } else if has_failures {
        BoundaryCheck {
            crossed: false,
            violations: vec![],
            analysis:
                "insight_platform: partial failure detected, but no clear constraint violation"
                    .into(),
        }
    } else {
        BoundaryCheck {
            crossed: false,
            violations: vec![],
            analysis: "insight_platform: all nodes completed successfully".into(),
        }
    };

    let goal_alignment = if all_failed {
        GoalAlignment {
            aligned: false,
            deviation: Some("insight_platform: no nodes completed successfully".into()),
            analysis: "insight_platform: execution failed to produce any results".into(),
        }
    } else if has_failures {
        GoalAlignment {
            aligned: true,
            deviation: Some(
                "insight_platform: some nodes failed, partial results may still be useful".into(),
            ),
            analysis: "insight_platform: partial execution, review failed nodes".into(),
        }
    } else {
        GoalAlignment {
            aligned: true,
            deviation: None,
            analysis: "insight_platform: execution completed successfully".into(),
        }
    };

    let growth_check = if has_failures {
        GrowthCheck {
            growth_detected: true,
            growth_type: Some("failure_lesson".into()),
            analysis: "insight_platform: failures detected — potential learning opportunity".into(),
        }
    } else {
        GrowthCheck {
            growth_detected: false,
            growth_type: None,
            analysis: "insight_platform: no failures, steady execution".into(),
        }
    };

    InsightResult {
        boundary_check,
        goal_alignment,
        growth_check,
        needs_followup: has_failures,
        followup_hint: if has_failures {
            Some("insight_platform: review failed nodes for root cause".into())
        } else {
            None
        },
    }
}

pub async fn run(
    pool: Arc<AgentPool>,
    rx: mpsc::Receiver<AgentMessage>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    tool_memory_tx: mpsc::Sender<Vec<ToolMemoryUpdate>>,
    prompts_dir: Option<PathBuf>,
) {
    let platform = InsightPlatform::new(
        rx,
        pool,
        provider,
        model_row,
        api_key,
        tool_memory_tx,
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
    use super::super::communication::{
        ExecutionDag, ExecutionOutput, ExecutionStatus, NodeResult, NodeStatus,
    };
    use super::*;
    use std::time::Instant;

    fn make_agent_entry(id: &str, identity: AgentIdentity, status: AgentStatus) -> AgentEntry {
        AgentEntry {
            id: id.into(),
            identity,
            status,
            created_at: Instant::now(),
        }
    }

    #[test]
    fn parse_valid_insight_raw_output() {
        let json = r#"{"insight":{"boundary_check":{"crossed":false,"violations":[],"analysis":"all good"},"goal_alignment":{"aligned":true,"deviation":null,"analysis":"on track"},"growth_check":{"growth_detected":false,"growth_type":null,"analysis":"steady"},"needs_followup":false,"followup_hint":null},"tool_memory":[]}"#;
        let raw = parse_insight_output(json).unwrap();
        let insight = raw.insight.unwrap();
        assert!(!insight.boundary_check.crossed);
        assert!(insight.goal_alignment.aligned);
        assert!(!insight.needs_followup);
        assert!(raw.tool_memory.is_empty());
    }

    #[test]
    fn parse_insight_raw_with_tool_memory() {
        let json = r#"{"insight":{"boundary_check":{"crossed":true,"violations":["timeout"],"analysis":"exceeded"},"goal_alignment":{"aligned":false,"deviation":"partial","analysis":"off"},"growth_check":{"growth_detected":true,"growth_type":"failure_lesson","analysis":"learn"},"needs_followup":true,"followup_hint":"retry"},"tool_memory":[{"capability_id":"http_client","description_patch":"add: handles connection_timeout","rating":"degraded","note":"HTTP client timed out on /api endpoint"}]}"#;
        let raw = parse_insight_output(json).unwrap();
        let insight = raw.insight.unwrap();
        assert!(insight.boundary_check.crossed);
        assert!(!insight.goal_alignment.aligned);
        assert!(insight.growth_check.growth_detected);
        assert_eq!(raw.tool_memory.len(), 1);
        assert_eq!(raw.tool_memory[0].capability_id, "http_client");
        assert_eq!(raw.tool_memory[0].rating, "degraded");
    }

    #[test]
    fn parse_insight_in_code_block() {
        let text = "some text\n```json\n{\"insight\":{\"boundary_check\":{\"crossed\":true,\"violations\":[\"timeout\"],\"analysis\":\"exceeded\"},\"goal_alignment\":{\"aligned\":false,\"deviation\":\"partial\",\"analysis\":\"off\"},\"growth_check\":{\"growth_detected\":true,\"growth_type\":\"failure_lesson\",\"analysis\":\"learn\"},\"needs_followup\":true,\"followup_hint\":\"retry\"},\"tool_memory\":[]}\n```\nmore";
        let raw = parse_insight_output(text).unwrap();
        let insight = raw.insight.unwrap();
        assert!(insight.boundary_check.crossed);
        assert!(!insight.goal_alignment.aligned);
        assert!(insight.growth_check.growth_detected);
    }

    #[test]
    fn parse_insight_old_format_compat() {
        let json = r#"{"boundary_check":{"crossed":false,"violations":[],"analysis":"all good"},"goal_alignment":{"aligned":true,"deviation":null,"analysis":"on track"},"growth_check":{"growth_detected":false,"growth_type":null,"analysis":"steady"},"needs_followup":false,"followup_hint":null}"#;
        let raw = parse_insight_output(json).unwrap();
        let insight = raw.insight.unwrap();
        assert!(!insight.boundary_check.crossed);
        assert!(raw.tool_memory.is_empty());
    }

    #[test]
    fn parse_insight_output_invalid_returns_none_insight() {
        let raw = parse_insight_output("garbage").unwrap();
        assert!(raw.insight.is_none());
        assert!(raw.tool_memory.is_empty());
    }

    #[test]
    fn format_node_result_includes_error_and_logs() {
        let nr = NodeResult {
            node_id: "n1".into(),
            status: NodeStatus::Failed,
            summary: "failed".into(),
            error: Some("timeout".into()),
            tool_call_count: 3,
            tool_call_logs: vec!["call 1: failed".into(), "call 2: timeout".into()],
        };
        let formatted = format_node_result(&nr);
        assert!(formatted.contains("n1"));
        assert!(formatted.contains("Failed"));
        assert!(formatted.contains("timeout"));
        assert!(formatted.contains("tool_call_logs"));
        assert!(formatted.contains("call 1: failed"));
    }

    #[test]
    fn build_insight_prompt_contains_failure_info() {
        let execution = ExecutionOutput {
            dag: ExecutionDag::Single {
                template_kind: "normal".into(),
                capability_ids: vec![],
                task_context: "test".into(),
            },
            node_results: vec![NodeResult {
                node_id: "n1".into(),
                status: NodeStatus::Failed,
                summary: "".into(),
                error: Some("connection refused".into()),
                tool_call_count: 1,
                tool_call_logs: vec!["HTTP GET /api → connection refused".into()],
            }],
            status: ExecutionStatus::Failure,
        };

        let prompt = build_insight_prompt("test goal", &[], &execution, &[], None);
        assert!(prompt.contains("test goal"));
        assert!(prompt.contains("Failure"));
        assert!(prompt.contains("n1"));
        assert!(prompt.contains("connection refused"));
        assert!(prompt.contains("HTTP GET"));
    }

    #[test]
    fn build_insight_prompt_includes_pool_snapshot() {
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

        let snapshot = vec![
            make_agent_entry(
                "exec-plat",
                AgentIdentity::ExecutionPlatform,
                AgentStatus::Running,
            ),
            make_agent_entry(
                "insight-plat",
                AgentIdentity::InsightPlatform,
                AgentStatus::Idle,
            ),
            make_agent_entry(
                "sub-1",
                AgentIdentity::SubagentRunning {
                    agent_id: "sub-1".into(),
                },
                AgentStatus::Running,
            ),
        ];

        let prompt = build_insight_prompt("test goal", &[], &execution, &snapshot, None);
        assert!(prompt.contains("Agent Pool Status"));
        assert!(prompt.contains("Total agents in pool: 3"));
        assert!(prompt.contains("execution=1"));
        assert!(prompt.contains("insight=1"));
        assert!(prompt.contains("Subagents: running=1"));
        assert!(prompt.contains("ExecutionPlatform"));
        assert!(prompt.contains("SubagentRunning(sub-1)"));
    }

    #[test]
    fn build_pool_snapshot_summary_empty() {
        let summary = build_pool_snapshot_summary(&[]);
        assert!(summary.contains("empty"));
    }

    #[test]
    fn build_pool_snapshot_summary_with_agents() {
        let snapshot = vec![
            make_agent_entry(
                "t1",
                AgentIdentity::ThinkingEngine {
                    instance_id: "inst-1".into(),
                },
                AgentStatus::Running,
            ),
            make_agent_entry("e1", AgentIdentity::ExecutionPlatform, AgentStatus::Running),
        ];
        let summary = build_pool_snapshot_summary(&snapshot);
        assert!(summary.contains("Total agents in pool: 2"));
        assert!(summary.contains("ThinkingEngine(inst-1)"));
        assert!(summary.contains("ExecutionPlatform"));
        assert!(summary.contains("By status: idle=0, running=2, pending=0"));
    }

    #[test]
    fn fallback_insight_detects_failures() {
        let execution = ExecutionOutput {
            dag: ExecutionDag::Single {
                template_kind: "normal".into(),
                capability_ids: vec![],
                task_context: "test".into(),
            },
            node_results: vec![NodeResult {
                node_id: "n1".into(),
                status: NodeStatus::Failed,
                summary: "".into(),
                error: Some("error".into()),
                tool_call_count: 0,
                tool_call_logs: vec![],
            }],
            status: ExecutionStatus::Failure,
        };

        let result = fallback_insight(&execution);
        assert!(result.boundary_check.crossed);
        assert!(!result.goal_alignment.aligned);
        assert!(result.growth_check.growth_detected);
        assert!(result.needs_followup);
    }

    #[test]
    fn fallback_insight_all_success() {
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

        let result = fallback_insight(&execution);
        assert!(!result.boundary_check.crossed);
        assert!(result.goal_alignment.aligned);
        assert!(!result.growth_check.growth_detected);
        assert!(!result.needs_followup);
    }

    #[test]
    fn fallback_insight_partial_failure() {
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
                    tool_call_count: 2,
                    tool_call_logs: vec!["retry exhausted".into()],
                },
            ],
            status: ExecutionStatus::PartialFailure,
        };

        let result = fallback_insight(&execution);

        assert!(!result.boundary_check.crossed);
        assert!(result.goal_alignment.aligned);
        assert!(result.growth_check.growth_detected);
        assert!(result.needs_followup);
    }
}
