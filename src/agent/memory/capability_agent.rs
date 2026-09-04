use crate::agent::capability_protocol::{
    parse_capability_output, CapabilityAction, CapabilityInvocation,
};
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
pub struct CapabilityCallRecord {
    pub capability_id: String,
    pub capability_name: String,
    pub arguments: serde_json::Value,
    pub output: serde_json::Value,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CapabilityLoopOutcome {
    pub summary: String,
    pub turns: u32,
    pub calls: Vec<CapabilityCallRecord>,
    pub logs: Vec<String>,
    pub completed: bool,
}

pub struct CapabilityLoopRequest {
    pub actor_id: String,
    pub system_prompt: String,
    /// 多段 assistant 上下文：原样连续、一次 LLM 调用，不做段内包装（2.0.3 记忆中台
    /// 三输出——思考引擎/执行中台/洞察中台；无 User 段）。
    pub assistant_segments: Vec<String>,
    /// 平台指令（role=system，与主 system 自动合并）。
    pub user_prompt: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_capability_loop(
    provider: &Arc<dyn LlmProvider>,
    model_row: &ModelRow,
    api_key: &SecretString,
    registry: &Registry,
    executor: &Arc<CapabilityExecutor>,
    req: CapabilityLoopRequest,
) -> Result<CapabilityLoopOutcome> {
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

    // 2.0.3：合并输入 = 多段 assistant 原样连续、一次 LLM 调用（无 User 段、无段内包装）。
    let mut messages = vec![ChatMessage::System {
        text: req.system_prompt,
        kind: SystemKind::Primary,
    }];
    for segment in &req.assistant_segments {
        messages.push(ChatMessage::Assistant {
            text: segment.clone(),
        });
    }
    messages.push(ChatMessage::System {
        text: req.user_prompt,
        kind: SystemKind::Primary,
    });

    let mut calls = Vec::new();
    let mut logs = Vec::new();
    let mut summary = String::new();
    // v0.5.0 批级工作区快照：本能力循环（记忆 agent 一次处理批）开始时固化默认
    // 工作区，批内全部调用复用同一快照（任务书 §7 运行中任务保持旧快照）。
    let frozen_host = executor.current_host_context();

    for turn in 0..max_turns {
        let request = LlmRequest::from_model_row(model_row, messages.clone(), api_key.clone())
            .map_err(|e| {
                crate::common::AgentError::Llm(format!("memory capability loop request: {e}"))
            })?;
        let response = provider.call(&request).await.map_err(|e| {
            crate::common::AgentError::Llm(format!("memory capability loop call: {e}"))
        })?;

        match parse_capability_output(&response.content) {
            CapabilityAction::Done { summary: done } => {
                summary = done.clone();
                logs.push(format!("DONE: {done}"));
                return Ok(CapabilityLoopOutcome {
                    summary,
                    turns: turn + 1,
                    calls,
                    logs,
                    completed: true,
                });
            }
            CapabilityAction::LegacyArguments { .. } => {
                let reason = "本 agent 拥有多个能力，调用时必须输出 {\"capability_call\":{\"capability_id\":\"<能力id>\",\"arguments\":{...}}}";
                logs.push(format!("INVALID output (turn {turn}): {reason}"));
                messages.push(ChatMessage::User {
                    text: reason.to_string(),
                });
                continue;
            }
            CapabilityAction::Invalid(reason) => {
                logs.push(format!("INVALID output (turn {turn}): {reason}"));
                messages.push(ChatMessage::User {
                    text: format!(
                        "你的输出无法解析: {reason}\n只输出 JSON: {{\"capability_call\":{{\"capability_id\":\"<能力id>\",\"arguments\":{{...}}}}}} 或 {{\"done\":true,\"summary\":\"...\"}}"
                    ),
                });
            }
            other => {
                // 单能力 capability_call 或多能力 capability_calls 数组：按声明顺序执行。
                let invocations = other
                    .into_calls()
                    .expect("CapabilityCall/CapabilityCalls 才能展开为调用列表");
                for invocation in invocations {
                    let (capability_id, capability_name) =
                        match resolve_capability_identity(registry, &invocation) {
                            Ok(v) => v,
                            Err(e) => {
                                logs.push(format!("INVALID capability (turn {turn}): {e}"));
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
                        capability_name: invocation
                            .capability_name
                            .clone()
                            .unwrap_or_else(|| capability_name.clone()),
                        arguments: invocation.arguments.clone(),
                    };
                    let outcome =
                        CapabilityService::new_with_host(registry, executor, &frozen_host)
                            .and_then(|service| service.execute_for_agent(&req.actor_id, &call))
                            .map(|result| result.output);
                    let record = match outcome {
                        Ok(output) => {
                            let truncated = crate::common::json_util::truncate_head_tail(
                                &output.to_string(),
                                4000,
                            );
                            logs.push(format!("OK {capability_id}: {truncated}"));
                            messages.push(ChatMessage::Assistant {
                                text: serde_json::json!({
                                    "capability_call": {
                                        "capability_id": capability_id,
                                        "capability_name": capability_name,
                                        "arguments": call.arguments
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
                    };
                    calls.push(record);
                }
            }
        }
    }

    logs.push(format!("EXCEEDED max_turns={max_turns}"));
    Ok(CapabilityLoopOutcome {
        summary,
        turns: max_turns,
        calls,
        logs,
        completed: false,
    })
}

/// 按 capability_id 解析注册表中的权威 (id, name)。
///
/// 不使用 provider 原生函数调用；
/// 调用方提交的 capability_name 若存在，交由服务层校验一致性。
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
