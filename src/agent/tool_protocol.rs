//! 服务层能力调用式的文本 JSON 协议。
//!
//! 模型不直接产生 provider 原生 tool_calls，而是按两段式协议输出：
//! - `{"arguments": {...}}`：调用一个能力；
//! - `{"done": true, "summary": "..."}`：结束本轮工具循环。
//!
//! 该协议已经过执行平台 subagent 验证（历史 A'/IR 回归），记忆 agent 复用同一契约。

#[derive(Debug, Clone)]
pub enum ToolLoopAction {
    /// 单能力场景的历史协议（执行平台 subagent 继续使用）。
    Arguments {
        arguments: serde_json::Value,
    },
    /// 多能力场景：显式声明要调用的能力名。
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    Done {
        summary: String,
    },
    Invalid(String),
}

pub fn parse_tool_loop_output(content: &str) -> ToolLoopAction {
    let mut specific_reason: Option<String> = None;
    for candidate in tool_loop_parse_candidates(content) {
        match parse_tool_loop_action_json(&candidate) {
            Ok(action) => return action,
            Err(ToolLoopParseError::ArgumentsNotObject(reason)) => {
                if specific_reason.is_none() {
                    specific_reason = Some(reason);
                }
            }
            Err(ToolLoopParseError::NotAction) => {}
        }
        let repaired = crate::common::json_util::repair_json(&candidate);
        if repaired != candidate {
            if let Ok(action) = parse_tool_loop_action_json(&repaired) {
                return action;
            }
        }
    }
    ToolLoopAction::Invalid(specific_reason.unwrap_or_else(|| {
        format!(
            "输出缺少 arguments 或 done 字段或 JSON 非法: {}",
            crate::common::json_util::truncate_utf8_boundary(content, 120)
        )
    }))
}

fn tool_loop_parse_candidates(content: &str) -> Vec<String> {
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
    candidates
}

enum ToolLoopParseError {
    NotAction,
    ArgumentsNotObject(String),
}

fn parse_tool_loop_action_json(
    json_text: &str,
) -> std::result::Result<ToolLoopAction, ToolLoopParseError> {
    let value: serde_json::Value =
        serde_json::from_str(json_text).map_err(|_| ToolLoopParseError::NotAction)?;
    if value.get("arguments").is_some() {
        let args = value.get("arguments").unwrap();
        if args.is_object() {
            return Ok(ToolLoopAction::Arguments {
                arguments: args.clone(),
            });
        }
        return Err(ToolLoopParseError::ArgumentsNotObject(
            "arguments 必须是 JSON 对象".to_string(),
        ));
    }
    if value.get("tool_call").is_some() {
        let tc = value.get("tool_call").unwrap();
        let name = tc
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.trim().is_empty() {
            return Err(ToolLoopParseError::ArgumentsNotObject(
                "tool_call.name 必须是非空字符串".to_string(),
            ));
        }
        if let Some(args) = tc.get("arguments") {
            if args.is_object() {
                return Ok(ToolLoopAction::ToolCall {
                    name,
                    arguments: args.clone(),
                });
            }
        }
        return Err(ToolLoopParseError::ArgumentsNotObject(
            "tool_call.arguments 必须是 JSON 对象".to_string(),
        ));
    }
    if value.get("done").and_then(|v| v.as_bool()) == Some(true) {
        let summary = value
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(ToolLoopAction::Done { summary });
    }
    Err(ToolLoopParseError::NotAction)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arguments_and_done() {
        assert!(matches!(
            parse_tool_loop_output(r#"{"arguments": {"command": "ls"}}"#),
            ToolLoopAction::Arguments { .. }
        ));
        assert!(matches!(
            parse_tool_loop_output(r#"{"done": true, "summary": "ok"}"#),
            ToolLoopAction::Done { .. }
        ));
    }

    #[test]
    fn strips_reasoning_preamble() {
        let content = "<think>let me check</think>\n{\"done\": true, \"summary\": \"ok\"}";
        assert!(matches!(
            parse_tool_loop_output(content),
            ToolLoopAction::Done { .. }
        ));
    }

    #[test]
    fn rejects_non_object_arguments() {
        assert!(matches!(
            parse_tool_loop_output(r#"{"arguments": "ls"}"#),
            ToolLoopAction::Invalid(_)
        ));
    }

    #[test]
    fn parses_named_tool_call() {
        let content =
            r#"{"tool_call":{"name":"memory.list","arguments":{"memory_type":"attention"}}}"#;
        match parse_tool_loop_output(content) {
            ToolLoopAction::ToolCall { name, arguments } => {
                assert_eq!(name, "memory.list");
                assert_eq!(arguments["memory_type"], "attention");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }
}
