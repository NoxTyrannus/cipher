//! 统一消息 IR：provider 无关的内部消息结构 + 共享规整 pass（normalize）。
//! 出站前统一走 `normalize`，三端适配器只做机械映射、不拍板语义决策。

use super::provider::ToolCall;

/// 内部统一消息 IR（与任何 API 无关，不落盘）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChatMessage {
    /// 指令性内容：主提示词 / 记忆条目 / 平台 Meta 消息（kind 供规整分层）。
    System {
        text: String,
        kind: SystemKind,
    },
    User {
        text: String,
    },
    /// tool_calls 常态为空，保留扩展位。
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    /// 工具结果（envelope 语义内生化，替代 JSON 字符串约定）。
    ToolResult {
        id: String,
        name: String,
        text: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SystemKind {
    /// 调用方的主 system。
    Primary,
    /// 记忆条目（认知/注意力/经验/偏好）。
    Memory(MemoryKind),
    /// 平台 echo / mode trigger / capability result。
    Meta,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemoryKind {
    Cognitive,
    Attention,
    Experience,
    Preference,
}

/// normalize 产物：system 文本 + 纯对话历史（System 已全部抽取）。
#[derive(Debug, Clone)]
pub struct Normalized {
    pub system: String,
    pub messages: Vec<ChatMessage>,
    /// 恒 true（本轮实现 anthropic 缓存断点）。
    pub cache_after_system: bool,
}

const MEMORY_KINDS_ORDERED: [MemoryKind; 4] = [
    MemoryKind::Cognitive,
    MemoryKind::Attention,
    MemoryKind::Experience,
    MemoryKind::Preference,
];

fn memory_title(kind: &MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Cognitive => "认知记忆",
        MemoryKind::Attention => "注意力",
        MemoryKind::Experience => "经验",
        MemoryKind::Preference => "偏好",
    }
}

fn memory_index(kind: &MemoryKind) -> usize {
    match kind {
        MemoryKind::Cognitive => 0,
        MemoryKind::Attention => 1,
        MemoryKind::Experience => 2,
        MemoryKind::Preference => 3,
    }
}

/// 共享规整 pass（规则严格按序）：
/// 1. Primary System 按序 `\n\n` 拼接为主 system 基底
/// 2. Memory 按 cognitive→attention→experience→preference 固定顺序分组，
///    每组一节 `## 认知记忆`/`## 注意力`/`## 经验`/`## 偏好`，节内逐条一行，
///    合并拼到主 system 后（`\n\n` 分隔）；无记忆条目则不加
/// 3. Meta System 转 User（保留原文含 `[xxx]` 前缀），保持序列相对位置
/// 4. User/Assistant/ToolResult 原样保留顺序；剔除全空白文本消息
/// 5. 孤儿 ToolResult（前一条不是含对应 tool_call id 的 Assistant）替换为
///    合成错误结果并打 warn
/// 6. cache_after_system 恒 true
pub fn normalize(messages: Vec<ChatMessage>) -> Normalized {
    let mut primary_parts: Vec<String> = Vec::new();
    let mut memory_groups: [Vec<String>; 4] = Default::default();
    let mut history: Vec<ChatMessage> = Vec::new();

    for msg in messages {
        match msg {
            ChatMessage::System { text, kind } => match kind {
                SystemKind::Primary => primary_parts.push(text),
                SystemKind::Memory(kind) => {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        memory_groups[memory_index(&kind)].push(text);
                    }
                }
                SystemKind::Meta => {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        history.push(ChatMessage::User { text });
                    }
                }
            },
            ChatMessage::User { text } => {
                if !text.trim().is_empty() {
                    history.push(ChatMessage::User { text });
                }
            }
            ChatMessage::Assistant { text, tool_calls } => {
                if !text.trim().is_empty() || !tool_calls.is_empty() {
                    history.push(ChatMessage::Assistant { text, tool_calls });
                }
            }
            other @ ChatMessage::ToolResult { .. } => history.push(other),
        }
    }

    let mut synthesized: Vec<(usize, String)> = Vec::new();
    for i in 0..history.len() {
        let current_id = match &history[i] {
            ChatMessage::ToolResult { id, .. } => id,
            _ => continue,
        };
        let mut paired = false;
        let mut j = i;
        while j > 0 {
            j -= 1;
            match &history[j] {
                ChatMessage::ToolResult { .. } => continue,
                ChatMessage::Assistant { tool_calls, .. } => {
                    paired = tool_calls.iter().any(|tc| &tc.id == current_id);
                    break;
                }
                _ => break,
            }
        }
        if !paired {
            synthesized.push((i, current_id.clone()));
        }
    }
    for (i, id) in synthesized {
        if let ChatMessage::ToolResult { text, is_error, .. } = &mut history[i] {
            *text = "No result provided（孤儿工具结果合成）".to_string();
            *is_error = true;
            tracing::warn!("normalize: 孤儿 ToolResult 合成错误结果 id={id}");
        }
    }

    let mut system = primary_parts.join("\n\n");
    let memory_sections: Vec<String> = MEMORY_KINDS_ORDERED
        .iter()
        .enumerate()
        .filter(|(idx, _)| !memory_groups[*idx].is_empty())
        .map(|(idx, kind)| {
            format!(
                "## {}\n{}",
                memory_title(kind),
                memory_groups[idx].join("\n")
            )
        })
        .collect();
    if !memory_sections.is_empty() {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(&memory_sections.join("\n\n"));
    }

    Normalized {
        system,
        messages: history,
        cache_after_system: true,
    }
}

/// 适配器入口：把 req.system 作为 Primary 前缀与 messages 一起规整。
pub fn normalize_with_system(system: Option<&str>, messages: &[ChatMessage]) -> Normalized {
    let mut msgs = Vec::with_capacity(messages.len() + 1);
    if let Some(s) = system {
        msgs.push(ChatMessage::System {
            text: s.to_string(),
            kind: SystemKind::Primary,
        });
    }
    msgs.extend_from_slice(messages);
    normalize(msgs)
}

impl ChatMessage {
    pub fn text(&self) -> &str {
        match self {
            ChatMessage::System { text, .. } => text,
            ChatMessage::User { text } => text,
            ChatMessage::Assistant { text, .. } => text,
            ChatMessage::ToolResult { text, .. } => text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary(text: &str) -> ChatMessage {
        ChatMessage::System {
            text: text.to_string(),
            kind: SystemKind::Primary,
        }
    }

    fn memory(text: &str, kind: MemoryKind) -> ChatMessage {
        ChatMessage::System {
            text: text.to_string(),
            kind: SystemKind::Memory(kind),
        }
    }

    fn meta(text: &str) -> ChatMessage {
        ChatMessage::System {
            text: text.to_string(),
            kind: SystemKind::Meta,
        }
    }

    fn user(text: &str) -> ChatMessage {
        ChatMessage::User {
            text: text.to_string(),
        }
    }

    fn assistant(text: &str, tool_calls: Vec<ToolCall>) -> ChatMessage {
        ChatMessage::Assistant {
            text: text.to_string(),
            tool_calls,
        }
    }

    fn tool_result(id: &str, text: &str) -> ChatMessage {
        ChatMessage::ToolResult {
            id: id.to_string(),
            name: "cap_x".to_string(),
            text: text.to_string(),
            is_error: false,
        }
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "cap_x".to_string(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn primary_memory_meta_are_layered() {
        let n = normalize(vec![
            memory("记忆A", MemoryKind::Cognitive),
            user("hi"),
            meta("[memory echo]\n沉淀 4 条"),
            primary("你是助手"),
        ]);
        assert!(n.system.starts_with("你是助手"));
        assert!(n.system.contains("## 认知记忆\n记忆A"));
        assert_eq!(n.messages.len(), 2);
        assert!(matches!(&n.messages[0], ChatMessage::User { text } if text == "hi"));
        assert!(matches!(
            &n.messages[1],
            ChatMessage::User { text } if text == "[memory echo]\n沉淀 4 条"
        ));
        assert!(n.cache_after_system);
    }

    #[test]
    fn memory_sections_fixed_order_with_titles() {
        let n = normalize(vec![
            primary("p"),
            memory("偏好1", MemoryKind::Preference),
            memory("经验1", MemoryKind::Experience),
            memory("注意力1", MemoryKind::Attention),
            memory("认知1", MemoryKind::Cognitive),
        ]);
        let system = &n.system;
        let cog = system.find("## 认知记忆\n认知1").unwrap();
        let att = system.find("## 注意力\n注意力1").unwrap();
        let exp = system.find("## 经验\n经验1").unwrap();
        let pre = system.find("## 偏好\n偏好1").unwrap();
        assert!(cog < att && att < exp && exp < pre, "{system}");
    }

    #[test]
    fn empty_memory_adds_no_section() {
        let n = normalize(vec![primary("p"), user("hi")]);
        assert_eq!(n.system, "p");
        assert!(!n.system.contains("##"));
        assert_eq!(n.messages.len(), 1);
    }

    #[test]
    fn meta_becomes_user_keeping_position() {
        let n = normalize(vec![
            user("a"),
            meta("[capability result: file.read]\ncontent"),
            user("b"),
        ]);
        let roles: Vec<&str> = n
            .messages
            .iter()
            .map(|m| match m {
                ChatMessage::User { .. } => "user",
                _ => "other",
            })
            .collect();
        assert_eq!(roles, vec!["user", "user", "user"]);
        assert!(n.messages[1].to_debug_contains("[capability result: file.read]"));
        assert!(matches!(&n.messages[1], ChatMessage::User { text } if text.contains("file.read")));
        assert_eq!(n.system, "");
    }

    #[test]
    fn blank_text_messages_are_dropped() {
        let n = normalize(vec![user("   "), assistant("", vec![]), user("real")]);
        assert_eq!(n.messages.len(), 1);
        assert!(matches!(&n.messages[0], ChatMessage::User { text } if text == "real"));
    }

    #[test]
    fn blank_assistant_with_tool_calls_is_kept() {
        let n = normalize(vec![assistant("", vec![tool_call("c1")])]);
        assert_eq!(n.messages.len(), 1);
        assert!(matches!(
            &n.messages[0],
            ChatMessage::Assistant { tool_calls, .. } if tool_calls.len() == 1
        ));
    }

    #[test]
    fn orphan_tool_result_is_synthesized_with_error() {
        let n = normalize(vec![user("u"), tool_result("c9", "raw")]);
        assert!(matches!(
            &n.messages[1],
            ChatMessage::ToolResult { id, text, is_error, .. }
                if id == "c9" && text.contains("孤儿工具结果合成") && *is_error
        ));
    }

    #[test]
    fn paired_tool_result_is_kept_unchanged() {
        let n = normalize(vec![
            assistant("我来读文件", vec![tool_call("c1")]),
            tool_result("c1", "file body"),
        ]);
        assert!(matches!(
            &n.messages[1],
            ChatMessage::ToolResult { id, text, is_error, .. }
                if id == "c1" && text == "file body" && !*is_error
        ));
    }

    #[test]
    fn plain_dialogue_unchanged() {
        let input = vec![user("q1"), assistant("a1", vec![]), user("q2")];
        let n = normalize(input.clone());
        assert_eq!(n.system, "");
        assert_eq!(n.messages, input);
    }

    #[test]
    fn multiple_primaries_joined() {
        let n = normalize(vec![primary("sys1"), user("u"), primary("sys2")]);
        assert!(n.system.starts_with("sys1\n\nsys2"), "{}", n.system);
        assert_eq!(n.messages.len(), 1);
    }

    #[test]
    fn normalize_with_system_prepends_primary() {
        let n = normalize_with_system(Some("base sys"), &[user("hi")]);
        assert!(n.system.starts_with("base sys"));
        assert_eq!(n.messages.len(), 1);
        let n2 = normalize_with_system(None, &[user("hi")]);
        assert_eq!(n2.system, "");
    }

    impl ChatMessage {
        fn to_debug_contains(&self, needle: &str) -> bool {
            format!("{self:?}").contains(needle)
        }
    }
}
