//! 统一消息 IR：provider 无关的内部消息结构 + 共享规整 pass（normalize）。
//! 业务链路使用文本能力协议，不再保留 provider 原生工具消息形态。

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
    Assistant {
        text: String,
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
    /// 恒 true（System 段缓存断点，本轮不细分 provider）。
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
/// 2. Memory 按 cognitive→attention→experience→preference 固定顺序分组
/// 3. Meta System 保序保留为 System（不转变、不合并，序列相对位置不变）
/// 4. User/Assistant 原样保留；剔除全空白文本消息
/// 5. cache_after_system 恒 true
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
                        history.push(ChatMessage::System {
                            text,
                            kind: SystemKind::Meta,
                        });
                    }
                }
            },
            ChatMessage::User { text } => {
                if !text.trim().is_empty() {
                    history.push(ChatMessage::User { text });
                }
            }
            ChatMessage::Assistant { text } => {
                if !text.trim().is_empty() {
                    history.push(ChatMessage::Assistant { text });
                }
            }
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
            ChatMessage::System { text, .. }
            | ChatMessage::User { text }
            | ChatMessage::Assistant { text } => text,
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

    fn assistant(text: &str) -> ChatMessage {
        ChatMessage::Assistant {
            text: text.to_string(),
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
            ChatMessage::System { text, kind: SystemKind::Meta } if text == "[memory echo]\n沉淀 4 条"
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
    fn meta_stays_system_keeping_position() {
        let n = normalize(vec![
            user("a"),
            meta("[capability result: file.read]\ncontent"),
            user("b"),
        ]);
        assert_eq!(n.messages.len(), 3);
        assert!(matches!(&n.messages[0], ChatMessage::User { text } if text == "a"));
        assert!(matches!(
            &n.messages[1],
            ChatMessage::System { text, kind: SystemKind::Meta } if text == "[capability result: file.read]\ncontent"
        ));
        assert!(matches!(&n.messages[2], ChatMessage::User { text } if text == "b"));
        assert_eq!(n.system, "");
    }

    #[test]
    fn meta_not_merged_into_memory_or_primary() {
        // Meta 保持独立 system 段：不并入 Primary system、不并入 Memory 分组。
        let n = normalize(vec![
            primary("sys"),
            memory("记忆A", MemoryKind::Cognitive),
            meta("[mode trigger: keep]\nreason"),
            user("hi"),
        ]);
        assert_eq!(n.system, "sys\n\n## 认知记忆\n记忆A");
        assert!(!n.system.contains("mode trigger"));
        assert_eq!(n.messages.len(), 2);
        assert!(matches!(
            &n.messages[0],
            ChatMessage::System {
                kind: SystemKind::Meta,
                ..
            }
        ));
        assert!(matches!(&n.messages[1], ChatMessage::User { text } if text == "hi"));
    }

    #[test]
    fn blank_text_messages_are_dropped() {
        let n = normalize(vec![user("   "), assistant(""), user("real")]);
        assert_eq!(n.messages.len(), 1);
        assert!(matches!(&n.messages[0], ChatMessage::User { text } if text == "real"));
    }

    #[test]
    fn plain_dialogue_unchanged() {
        let input = vec![user("q1"), assistant("a1"), user("q2")];
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
}
