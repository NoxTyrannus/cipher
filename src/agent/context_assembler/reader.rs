use super::budget::ParsedMessage;
use crate::agent::agent_pool::registry::{AgentEntry, AgentIdentity, AgentStatus};
use crate::agent::thought::ThinkingInput;
use crate::common::{AgentError, Result};
use crate::logic::model::message::{ChatMessage, MemoryKind, SystemKind};
use std::path::Path;

pub(super) fn parsed_message(role: &str, content: String) -> ParsedMessage {
    ParsedMessage {
        role: role.to_string(),
        token_count: estimate_tokens(&content),
        content,
    }
}

pub(super) fn parsed_thinking_input(input: ThinkingInput) -> ParsedMessage {
    match input {
        ThinkingInput::User { text } => parsed_message("user", text),
        ThinkingInput::PlatformInsight {
            summary,
            has_subagent_result,
        } => {
            let result_flag = if has_subagent_result {
                " (含 subagent 结果段)"
            } else {
                ""
            };
            parsed_message(
                "system",
                format!("[洞察回环轮 | 洞察中台报告{result_flag}]\n{summary}"),
            )
        }
        ThinkingInput::ModeTrigger { mode, reason } => {
            parsed_message("system", format!("[mode trigger: {mode}]\n{reason}"))
        }
        ThinkingInput::CapabilityResult {
            capability_id,
            capability_name,
            summary,
            artifact_refs,
        } => {
            let mut content =
                format!("[capability result: {capability_id} / {capability_name}]\n{summary}");
            if !artifact_refs.is_empty() {
                content.push_str("\nartifact refs: ");
                content.push_str(&artifact_refs.join(", "));
            }
            parsed_message("system", content)
        }
        ThinkingInput::LegacyInternal => {
            parsed_message("system", "[legacy internal round]".to_string())
        }
    }
}

pub(super) fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0usize;
    for ch in text.chars() {
        tokens += if ch.is_ascii() { 1 } else { 2 };
    }
    tokens.max(1)
}

pub(super) fn build_pool_snapshot_text(
    snapshot: &[AgentEntry],
    subagent_states: &[crate::agent::execution_types::SubagentRuntimeState],
) -> String {
    use std::collections::HashMap;

    if snapshot.is_empty() && subagent_states.is_empty() {
        return String::new();
    }

    let mut platform_status: HashMap<&'static str, (AgentStatus, f32)> = HashMap::new();
    let mut thinking_count = 0usize;
    let mut subagent_running = 0usize;
    let mut subagent_pending = 0usize;

    for entry in snapshot {
        let heartbeat_age = entry.last_heartbeat.elapsed().as_secs_f32();
        match &entry.identity {
            AgentIdentity::ExecutionPlatform => {
                platform_status.insert("执行中台", (entry.status.clone(), heartbeat_age));
            }
            AgentIdentity::InsightPlatform => {
                platform_status.insert("洞察中台", (entry.status.clone(), heartbeat_age));
            }
            AgentIdentity::MemoryPlatform => {
                platform_status.insert("记忆中台", (entry.status.clone(), heartbeat_age));
            }
            AgentIdentity::ThinkingEngine { .. } => {
                thinking_count += 1;
            }
            AgentIdentity::SubagentRunning { .. } => {
                subagent_running += 1;
            }
            AgentIdentity::SubagentPending { .. } => {
                subagent_pending += 1;
            }
            AgentIdentity::SubagentResident { .. } => {}
        }
    }

    let status_str = |s: &AgentStatus| -> &'static str {
        match s {
            AgentStatus::Idle => "waiting",
            AgentStatus::Running => "running",
            AgentStatus::Pending => "pending",
        }
    };

    let mut lines = vec!["## 当前 Agent 池状态".to_string()];

    let platforms = ["执行中台", "洞察中台", "记忆中台"];
    for name in &platforms {
        let status = platform_status
            .get(name)
            .map(|(status, heartbeat_age)| {
                format!(
                    "{} (heartbeat {:.1}s ago)",
                    status_str(status),
                    heartbeat_age
                )
            })
            .unwrap_or_else(|| "unregistered".to_string());
        lines.push(format!("- {}: {}", name, status));
    }

    if thinking_count > 0 {
        lines.push(format!("- 思考引擎实例: {}", thinking_count));
    }

    if subagent_running > 0 || subagent_pending > 0 {
        let parts = vec![
            if subagent_running > 0 {
                format!("running={}", subagent_running)
            } else {
                String::new()
            },
            if subagent_pending > 0 {
                format!("pending={}", subagent_pending)
            } else {
                String::new()
            },
        ];
        let sub_str = parts
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("- Subagents: {}", sub_str));
    }

    if !subagent_states.is_empty() {
        lines.push("## Subagent Status".to_string());
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
    }

    lines.join("\n")
}

/// 按 token 估算预算截断文本（UTF-8 安全，超出预算时丢弃末尾）。
pub(super) fn truncate_by_token_budget(text: &str, max_tokens: usize) -> String {
    let mut used = 0usize;
    let mut end = text.len();
    for (index, ch) in text.char_indices() {
        let tokens = if ch.is_ascii() { 1 } else { 2 };
        if used + tokens > max_tokens {
            end = index;
            break;
        }
        used += tokens;
    }
    if end >= text.len() {
        text.to_string()
    } else {
        text[..end].to_string()
    }
}

pub(super) fn memory_kind_of(memory_type: &str) -> MemoryKind {
    match memory_type {
        "attention" => MemoryKind::Attention,
        "experience" => MemoryKind::Experience,
        "preference" => MemoryKind::Preference,
        _ => MemoryKind::Cognitive,
    }
}

pub(super) fn chat_message_of(parsed: &ParsedMessage) -> ChatMessage {
    match parsed.role.as_str() {
        "user" => ChatMessage::User {
            text: parsed.content.clone(),
        },
        "assistant" => ChatMessage::Assistant {
            text: parsed.content.clone(),
        },
        "system" => ChatMessage::System {
            text: parsed.content.clone(),
            kind: SystemKind::Meta,
        },
        _ => ChatMessage::User {
            text: parsed.content.clone(),
        },
    }
}

pub(super) fn message_text(m: &ChatMessage) -> &str {
    match m {
        ChatMessage::System { text, .. } => text,
        ChatMessage::User { text } => text,
        ChatMessage::Assistant { text, .. } => text,
    }
}

pub(super) fn pct_of(window: usize, pct: f64) -> usize {
    ((window as f64) * pct / 100.0) as usize
}

pub(super) fn total_tokens(msgs: &[ChatMessage]) -> usize {
    msgs.iter().map(|m| estimate_tokens(message_text(m))).sum()
}

#[allow(dead_code)]
pub(super) fn truncate_to_budget(
    messages: Vec<ParsedMessage>,
    budget: usize,
) -> Vec<ParsedMessage> {
    let mut used = 0usize;
    let mut result = Vec::new();
    for m in messages {
        if used + m.token_count > budget && !result.is_empty() {
            break;
        }
        used += m.token_count;
        result.push(m);
    }
    result
}

pub(super) fn format_memory_line(memory_type: &str, payload: &serde_json::Value) -> String {
    match memory_type {
        "cognitive" => {
            let insight = payload.get("insight").and_then(|v| v.as_str());
            let entity = payload.get("entity").and_then(|v| v.as_str());
            match (insight, entity) {
                (Some(i), _) => {
                    let context = payload
                        .get("context")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    format!("[COGNITIVE] {} (context: {})", i, context)
                }
                (None, Some(e)) => {
                    let relation = payload
                        .get("relation")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let target = payload.get("target").and_then(|v| v.as_str()).unwrap_or("");
                    format!("[COGNITIVE] {} {} {}", e, relation, target)
                }
                _ => "[COGNITIVE]".to_string(),
            }
        }
        "cognitive_edge" => {
            let from = payload
                .get("from_entity")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let to = payload
                .get("to_entity")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let relation = payload
                .get("relation")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("{} ->{}（{}）", from, to, relation)
        }
        "attention" => {
            let focus = payload.get("focus").and_then(|v| v.as_str()).unwrap_or("");
            let content = payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("[ATTENTION] {}: {}", focus, content)
        }
        "experience" => {
            let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let summary = payload
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("[EXPERIENCE] {}: {}", title, summary)
        }
        "preference" => {
            let key = payload.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let value = payload.get("value").and_then(|v| v.as_str()).unwrap_or("");
            format!("[PREFERENCE] {}: {}", key, value)
        }
        _ => format!("[{}] {:?}", memory_type, payload),
    }
}

pub(super) fn parse_conversation_file(path: &Path) -> Result<ParsedMessage> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| AgentError::Io(format!("read {:?}: {}", path, e)))?;

    let mut lines = content.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return Err(AgentError::Parse(format!(
            "{}: missing opening ---",
            path.display()
        )));
    }

    let mut role: Option<String> = None;
    let mut in_body = false;
    let mut body_lines = Vec::new();

    for line in &mut lines {
        let trimmed = line.trim();
        if !in_body && trimmed == "---" {
            in_body = true;
            continue;
        }
        if in_body {
            body_lines.push(line);
        } else if let Some(value) = parse_yaml_string(trimmed, "role:") {
            role = Some(value);
        }
    }

    let role = role.unwrap_or_else(|| "assistant".to_string());
    let body = body_lines.join("\n").trim().to_string();
    let token_count = estimate_tokens(&body);

    Ok(ParsedMessage {
        role,
        content: body,
        token_count,
    })
}

pub(super) fn parse_yaml_string(line: &str, prefix: &str) -> Option<String> {
    line.strip_prefix(prefix)
        .map(|s| s.trim())
        .map(|s| s.trim_matches('"').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::thought::ThinkingInput;

    #[test]
    fn platform_insight_prefix_names_insight_platform() {
        let parsed = parsed_thinking_input(ThinkingInput::PlatformInsight {
            summary: "insight 摘要".to_string(),
            has_subagent_result: false,
        });
        assert_eq!(parsed.role, "system");
        assert!(parsed
            .content
            .starts_with("[洞察回环轮 | 洞察中台报告]\ninsight 摘要"));
    }

    #[test]
    fn platform_insight_prefix_marks_subagent_result() {
        let parsed = parsed_thinking_input(ThinkingInput::PlatformInsight {
            summary: "摘要".to_string(),
            has_subagent_result: true,
        });
        assert!(parsed
            .content
            .starts_with("[洞察回环轮 | 洞察中台报告 (含 subagent 结果段)]\n摘要"));
    }

    #[test]
    fn mode_trigger_and_capability_result_prefixes_unchanged() {
        let mode = parsed_thinking_input(ThinkingInput::ModeTrigger {
            mode: "keep".to_string(),
            reason: "r".to_string(),
        });
        assert!(mode.content.starts_with("[mode trigger: keep]"));
        let cap = parsed_thinking_input(ThinkingInput::CapabilityResult {
            capability_id: "file.read".to_string(),
            capability_name: "读文件".to_string(),
            summary: "s".to_string(),
            artifact_refs: vec![],
        });
        assert!(cap
            .content
            .starts_with("[capability result: file.read / 读文件]"));
    }

    #[test]
    fn chat_message_of_system_maps_to_meta() {
        let parsed = ParsedMessage {
            role: "system".to_string(),
            token_count: 3,
            content: "[洞察回环轮 | 洞察中台报告]\nx".to_string(),
        };
        assert!(matches!(
            chat_message_of(&parsed),
            ChatMessage::System {
                kind: SystemKind::Meta,
                ..
            }
        ));
    }
}
