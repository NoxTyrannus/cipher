//! 服务层能力调用式的文本 JSON 协议（capability_protocol）。
//!
//! 模型不直接产生 provider 原生函数调用，而是按固定格式能力协议输出：
//! - `{"capability_call": {"capability_id":"...", "capability_name":"可选", "arguments":{...}}}`：调用一个能力；
//! - `{"capability_calls": [{...}, ...]}`：按数组声明顺序调用多个能力；
//! - `{"done": true, "summary": "..."}`：结束本轮能力循环。
//!
//! 协议字段只使用 `capability_id`（最小许可）/ 可选 `capability_name` / `arguments`；
//! `{"arguments":{...}}` 单能力历史形态仅作为兼容分支保留。

/// 单条能力调用声明（协议最小单位）。
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityInvocation {
    /// 能力注册表权威 id（最小许可：填对即可启动）。
    pub capability_id: String,
    /// 可选能力名称；提交时服务层校验一致性，不提交时解析权威定义。
    pub capability_name: Option<String>,
    /// 调用参数（必须是 JSON 对象）。
    pub arguments: serde_json::Value,
}

/// 单轮模型输出的解析结果。
#[derive(Debug, Clone, PartialEq)]
pub enum CapabilityAction {
    /// 单能力场景：`{"capability_call": {...}}`。
    CapabilityCall(CapabilityInvocation),
    /// 多能力场景：`{"capability_calls": [...]}`，按数组顺序执行。
    CapabilityCalls(Vec<CapabilityInvocation>),
    /// 旧协议单能力场景（无 capability_id，无法作为能力调用）：`{"arguments": {...}}`。
    LegacyArguments { arguments: serde_json::Value },
    /// 结束本轮：`{"done": true, "summary": "..."}`。
    Done { summary: String },
    /// 无法解析。
    Invalid(String),
}

impl CapabilityAction {
    /// 把单/多能力调用统一展开为按声明顺序的调用列表。
    /// `LegacyArguments / Done / Invalid` 返回 `None`。
    pub fn into_calls(self) -> Option<Vec<CapabilityInvocation>> {
        match self {
            CapabilityAction::CapabilityCall(invocation) => Some(vec![invocation]),
            CapabilityAction::CapabilityCalls(calls) => Some(calls),
            CapabilityAction::LegacyArguments { .. }
            | CapabilityAction::Done { .. }
            | CapabilityAction::Invalid(_) => None,
        }
    }
}

/// 解析模型单轮输出为 `CapabilityAction`（顺序解析，容忍围栏/推理前缀）。
pub fn parse_capability_output(content: &str) -> CapabilityAction {
    let mut specific_reason: Option<String> = None;
    for candidate in capability_parse_candidates(content) {
        match parse_capability_action_json(&candidate) {
            Ok(action) => return action,
            Err(CapabilityParseError::ArgumentsNotObject(reason)) => {
                if specific_reason.is_none() {
                    specific_reason = Some(reason);
                }
            }
            Err(CapabilityParseError::NotAction) => {}
        }
        let repaired = crate::common::json_util::repair_json(&candidate);
        if repaired != candidate {
            if let Ok(action) = parse_capability_action_json(&repaired) {
                return action;
            }
        }
    }
    CapabilityAction::Invalid(specific_reason.unwrap_or_else(|| {
        format!(
            "输出缺少 capability_call / capability_calls / done 字段或 JSON 非法: {}",
            crate::common::json_util::truncate_utf8_boundary(content, 120)
        )
    }))
}

fn capability_parse_candidates(content: &str) -> Vec<String> {
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

enum CapabilityParseError {
    NotAction,
    ArgumentsNotObject(String),
}

fn parse_capability_action_json(
    json_text: &str,
) -> std::result::Result<CapabilityAction, CapabilityParseError> {
    let value: serde_json::Value =
        serde_json::from_str(json_text).map_err(|_| CapabilityParseError::NotAction)?;

    // 新协议：单能力调用。
    if let Some(call) = value.get("capability_call") {
        let invocation = parse_invocation(call, "capability_call")?;
        return Ok(CapabilityAction::CapabilityCall(invocation));
    }
    // 新协议：多能力数组（按声明顺序解析）。
    if let Some(calls) = value.get("capability_calls") {
        let array = calls.as_array().ok_or_else(|| {
            CapabilityParseError::ArgumentsNotObject(
                "capability_calls 必须是 JSON 数组".to_string(),
            )
        })?;
        let mut parsed = Vec::with_capacity(array.len());
        for (index, element) in array.iter().enumerate() {
            match parse_invocation(element, "capability_calls") {
                Ok(invocation) => parsed.push(invocation),
                Err(error) => {
                    return Err(match error {
                        CapabilityParseError::ArgumentsNotObject(reason) => {
                            CapabilityParseError::ArgumentsNotObject(format!(
                                "capability_calls[{index}]: {reason}"
                            ))
                        }
                        CapabilityParseError::NotAction => CapabilityParseError::NotAction,
                    });
                }
            }
        }
        return Ok(CapabilityAction::CapabilityCalls(parsed));
    }
    // 旧协议兼容：无具名单能力调用（无 capability_id）。
    if value.get("arguments").is_some() {
        let arguments = parse_arguments_object(value.get("arguments"), "arguments")?;
        return Ok(CapabilityAction::LegacyArguments { arguments });
    }
    // 结束：done.summary。
    if value.get("done").and_then(|v| v.as_bool()) == Some(true) {
        let summary = value
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(CapabilityAction::Done { summary });
    }
    Err(CapabilityParseError::NotAction)
}

fn parse_invocation(
    value: &serde_json::Value,
    context: &str,
) -> std::result::Result<CapabilityInvocation, CapabilityParseError> {
    if !value.is_object() {
        return Err(CapabilityParseError::ArgumentsNotObject(format!(
            "{context} 必须是 JSON 对象"
        )));
    }
    let capability_id = value
        .get("capability_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if capability_id.trim().is_empty() {
        return Err(CapabilityParseError::ArgumentsNotObject(
            "{context}.capability_id 必须是非空字符串".replace("{context}", context),
        ));
    }
    let capability_name = value
        .get("capability_name")
        .and_then(|v| v.as_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string);
    let arguments =
        parse_arguments_object(value.get("arguments"), &format!("{context}.arguments"))?;
    Ok(CapabilityInvocation {
        capability_id,
        capability_name,
        arguments,
    })
}

fn parse_arguments_object(
    value: Option<&serde_json::Value>,
    context: &str,
) -> std::result::Result<serde_json::Value, CapabilityParseError> {
    match value {
        Some(args) if args.is_object() => Ok(args.clone()),
        Some(_) => Err(CapabilityParseError::ArgumentsNotObject(format!(
            "{context} 必须是 JSON 对象"
        ))),
        // 最小许可（任务书 §1.3）：capability_id 填对即可启动；
        // arguments 缺失按空对象处理，必填参数由能力 schema 校验报普通错误。
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_capability_call_and_done() {
        match parse_capability_output(
            r#"{"capability_call": {"capability_id": "file.read", "arguments": {"path": "/tmp"}}}"#,
        ) {
            CapabilityAction::CapabilityCall(invocation) => {
                assert_eq!(invocation.capability_id, "file.read");
                assert_eq!(invocation.capability_name, None);
                assert_eq!(invocation.arguments["path"], "/tmp");
            }
            other => panic!("expected CapabilityCall, got {other:?}"),
        }
        assert!(matches!(
            parse_capability_output(r#"{"done": true, "summary": "ok"}"#),
            CapabilityAction::Done { .. }
        ));
    }

    #[test]
    fn parses_capability_call_with_optional_name() {
        match parse_capability_output(
            r#"{"capability_call": {"capability_id": "file.read", "capability_name": "Read File", "arguments": {}}}"#,
        ) {
            CapabilityAction::CapabilityCall(invocation) => {
                assert_eq!(invocation.capability_id, "file.read");
                assert_eq!(invocation.capability_name.as_deref(), Some("Read File"));
            }
            other => panic!("expected CapabilityCall, got {other:?}"),
        }
    }

    #[test]
    fn parses_capability_calls_array_in_declaration_order() {
        match parse_capability_output(
            r#"{"capability_calls": [
                {"capability_id": "file.list", "arguments": {}},
                {"capability_id": "file.read", "capability_name": "Read", "arguments": {"path": "/a"}}
            ]}"#,
        ) {
            CapabilityAction::CapabilityCalls(calls) => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].capability_id, "file.list");
                assert_eq!(calls[1].capability_id, "file.read");
                assert_eq!(calls[1].capability_name.as_deref(), Some("Read"));
            }
            other => panic!("expected CapabilityCalls, got {other:?}"),
        }
    }

    #[test]
    fn capability_calls_array_element_invalid_fails_whole_parse() {
        match parse_capability_output(
            r#"{"capability_calls": [{"capability_id": "file.list"}, {"arguments": {}}]}"#,
        ) {
            CapabilityAction::Invalid(reason) => {
                assert!(reason.contains("capability_calls[1]"), "got: {reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn parses_legacy_arguments_for_compat() {
        assert!(matches!(
            parse_capability_output(r#"{"arguments": {"command": "ls"}}"#),
            CapabilityAction::LegacyArguments { .. }
        ));
    }

    #[test]
    fn into_calls_flattens_single_and_array() {
        let single = CapabilityAction::CapabilityCall(CapabilityInvocation {
            capability_id: "a".to_string(),
            capability_name: None,
            arguments: serde_json::json!({}),
        });
        assert_eq!(single.clone().into_calls().unwrap().len(), 1);
        let multiple = CapabilityAction::CapabilityCalls(vec![
            CapabilityInvocation {
                capability_id: "a".to_string(),
                capability_name: None,
                arguments: serde_json::json!({}),
            },
            CapabilityInvocation {
                capability_id: "b".to_string(),
                capability_name: None,
                arguments: serde_json::json!({}),
            },
        ]);
        assert_eq!(multiple.into_calls().unwrap().len(), 2);
        assert!(CapabilityAction::Done {
            summary: "x".to_string()
        }
        .into_calls()
        .is_none());
    }

    #[test]
    fn strips_reasoning_preamble() {
        let content = " thinkinglet me check response\n{\"done\": true, \"summary\": \"ok\"}";
        assert!(matches!(
            parse_capability_output(content),
            CapabilityAction::Done { .. }
        ));
    }

    #[test]
    fn rejects_non_object_arguments() {
        match parse_capability_output(
            r#"{"capability_call": {"capability_id": "x", "arguments": "ls"}}"#,
        ) {
            CapabilityAction::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_capability_id() {
        match parse_capability_output(r#"{"capability_call": {"arguments": {}}}"#) {
            CapabilityAction::Invalid(reason) => {
                assert!(reason.contains("capability_id"), "got: {reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}
