use crate::agent::tool_protocol::{parse_tool_loop_output, ToolLoopAction};
use crate::common::Result;
use crate::data::duckdb::Registry;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::capability::service::{CapabilityCall, CapabilityService};
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::provider::{LlmProvider, LlmRequest};
use secrecy::SecretString;
use std::sync::Arc;

const DEFAULT_MAX_TURNS: u32 = 8;

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub capability_id: String,
    pub capability_name: String,
    pub arguments: serde_json::Value,
    pub output: serde_json::Value,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolLoopOutcome {
    pub summary: String,
    pub turns: u32,
    pub calls: Vec<ToolCallRecord>,
    pub logs: Vec<String>,
    pub completed: bool,
}

pub struct ToolLoopRequest {
    pub actor_id: String,
    pub system_prompt: String,
    pub user_prompt: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tool_loop(
    provider: &Arc<dyn LlmProvider>,
    model_row: &ModelRow,
    api_key: &SecretString,
    registry: &Registry,
    executor: &Arc<CapabilityExecutor>,
    req: ToolLoopRequest,
) -> Result<ToolLoopOutcome> {
    let max_turns = registry
        .agents
        .get(&req.actor_id)
        .and_then(|agent| {
            agent
                .config
                .as_ref()
                .and_then(|c| c.get("max_turns"))
                .and_then(|v| v.as_u64())
        })
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(DEFAULT_MAX_TURNS);

    let mut messages = vec![
        ChatMessage::System {
            text: req.system_prompt,
            kind: SystemKind::Primary,
        },
        ChatMessage::User {
            text: req.user_prompt,
        },
    ];

    let mut calls = Vec::new();
    let mut logs = Vec::new();
    let mut summary = String::new();

    for turn in 0..max_turns {
        let request = LlmRequest::from_model_row(model_row, messages.clone(), api_key.clone())
            .map_err(|e| {
                crate::common::AgentError::Llm(format!("memory tool loop request: {e}"))
            })?;
        let response = provider
            .call(&request)
            .await
            .map_err(|e| crate::common::AgentError::Llm(format!("memory tool loop call: {e}")))?;

        match parse_tool_loop_output(&response.content) {
            ToolLoopAction::Done { summary: done } => {
                summary = done.clone();
                logs.push(format!("DONE: {done}"));
                return Ok(ToolLoopOutcome {
                    summary,
                    turns: turn + 1,
                    calls,
                    logs,
                    completed: true,
                });
            }
            ToolLoopAction::Arguments { .. } => {
                let reason = "本 agent 拥有多个能力，调用时必须输出 {\"tool_call\":{\"name\":\"<能力id>\",\"arguments\":{...}}}";
                logs.push(format!("INVALID output (turn {turn}): {reason}"));
                messages.push(ChatMessage::User {
                    text: reason.to_string(),
                });
                continue;
            }
            ToolLoopAction::ToolCall { name, arguments } => {
                let (capability_id, capability_name) =
                    match resolve_capability_identity(registry, executor, &req.actor_id, &name) {
                        Ok(v) => v,
                        Err(e) => {
                            logs.push(format!("INVALID tool (turn {turn}): {e}"));
                            messages.push(ChatMessage::User {
                                text: format!(
                                    "能力调用被拒绝: {e}\n请使用可用能力并重试，或输出 done 结束。"
                                ),
                            });
                            continue;
                        }
                    };
                let call = CapabilityCall {
                    capability_id: capability_id.clone(),
                    capability_name: capability_name.clone(),
                    arguments: arguments.clone(),
                };
                let outcome = CapabilityService::new(registry, executor)
                    .and_then(|service| service.execute_for_agent(&req.actor_id, &call))
                    .map(|result| result.output);
                let record = match outcome {
                    Ok(output) => {
                        let truncated =
                            crate::common::json_util::truncate_head_tail(&output.to_string(), 4000);
                        logs.push(format!("OK {capability_id}: {truncated}"));
                        messages.push(ChatMessage::Assistant {
                            text: serde_json::json!({
                                "tool_call": {"name": name, "arguments": arguments}
                            })
                            .to_string(),
                            tool_calls: vec![],
                        });
                        messages.push(ChatMessage::User {
                            text: format!("能力 {capability_id} 执行结果: {truncated}"),
                        });
                        ToolCallRecord {
                            capability_id,
                            capability_name,
                            arguments,
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
                        ToolCallRecord {
                            capability_id,
                            capability_name,
                            arguments,
                            output: serde_json::Value::Null,
                            ok: false,
                            error: Some(e.to_string()),
                        }
                    }
                };
                calls.push(record);
            }
            ToolLoopAction::Invalid(reason) => {
                logs.push(format!("INVALID output (turn {turn}): {reason}"));
                messages.push(ChatMessage::User {
                    text: format!(
                        "你的输出无法解析: {reason}\n只输出 JSON: {{\"tool_call\":{{\"name\":\"<能力id>\",\"arguments\":{{...}}}}}} 或 {{\"done\":true,\"summary\":\"...\"}}"
                    ),
                });
            }
        }
    }

    logs.push(format!("EXCEEDED max_turns={max_turns}"));
    Ok(ToolLoopOutcome {
        summary,
        turns: max_turns,
        calls,
        logs,
        completed: false,
    })
}

fn resolve_capability_identity(
    registry: &Registry,
    executor: &Arc<CapabilityExecutor>,
    actor_id: &str,
    requested_name: &str,
) -> std::result::Result<(String, String), String> {
    if let Some(row) = registry.base_capabilities.get(requested_name) {
        return Ok((row.id.clone(), row.name.clone()));
    }
    if let Some(row) = registry.composite_capabilities.get(requested_name) {
        return Ok((row.id.clone(), row.name.clone()));
    }
    let service = CapabilityService::new(registry, executor).map_err(|e| e.to_string())?;
    let tools = service
        .provider_tools_for_agent(actor_id)
        .map_err(|e| e.to_string())?;
    tools
        .normalize(requested_name, serde_json::Value::Null)
        .map(|call| (call.capability_id, call.capability_name))
        .map_err(|e| e.to_string())
}
