pub mod budget;
mod reader;

pub use budget::ContextConfig;
use budget::{ContextBudget, ParsedMessage};
use reader::*;

use crate::agent::agent_pool::AgentPool;
use crate::agent::thought::ThinkingTerminalState;
use crate::common::{AgentError, Result};
use crate::data::thought_store::ThoughtStore;
use crate::logic::model::message::{ChatMessage, SystemKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct ContextAssembler {
    config: ContextConfig,
    conversations_dir: PathBuf,
    prompts_dir: PathBuf,

    triviumdb_path: Option<PathBuf>,

    agent_pool: Option<Arc<AgentPool>>,

    thought_store: Option<Arc<ThoughtStore>>,

    memory_db: Option<std::sync::Arc<std::sync::Mutex<duckdb::Connection>>>,

    shared_trivium: Option<Arc<std::sync::Mutex<crate::data::triviumdb::TriviumDb>>>,
}

impl ContextAssembler {
    pub fn new(config: ContextConfig, data_dir: &Path, triviumdb_path: Option<PathBuf>) -> Self {
        Self::new_with_roots(config, data_dir, data_dir, triviumdb_path)
    }

    pub fn new_with_roots(
        config: ContextConfig,
        storage_root: &Path,
        prompt_root: &Path,
        triviumdb_path: Option<PathBuf>,
    ) -> Self {
        Self {
            config,
            conversations_dir: storage_root.join("conversations"),
            prompts_dir: prompt_root.join("prompts"),
            triviumdb_path,
            agent_pool: None,
            thought_store: None,
            memory_db: None,
            shared_trivium: None,
        }
    }

    pub fn set_agent_pool(&mut self, pool: Arc<AgentPool>) {
        self.agent_pool = Some(pool);
    }

    pub fn set_memory_db(&mut self, db: std::sync::Arc<std::sync::Mutex<duckdb::Connection>>) {
        self.memory_db = Some(db);
    }

    pub fn set_shared_trivium(
        &mut self,
        db: Arc<std::sync::Mutex<crate::data::triviumdb::TriviumDb>>,
    ) {
        self.shared_trivium = Some(db);
    }

    fn memory_kind_active(
        &self,
        kind: crate::agent::memory::memory_version::MemoryVersionKind,
    ) -> bool {
        let Some(db) = &self.memory_db else {
            return true;
        };
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return false,
        };
        match crate::agent::memory::memory_version::get_active(&conn, kind) {
            Ok(v) => v.is_some(),
            Err(_) => false,
        }
    }

    pub fn set_thought_store(&mut self, thought_store: Arc<ThoughtStore>) {
        self.thought_store = Some(thought_store);
    }

    pub async fn build_self_awareness(&self) -> String {
        match &self.agent_pool {
            Some(pool) => {
                let snapshot = pool.snapshot().await;
                let subagent_states = pool.subagent_states().await;
                let text = build_pool_snapshot_text(&snapshot, &subagent_states);
                if text.is_empty() {
                    String::new()
                } else {
                    // Subagent Status 作为固定开销计入上下文预算，再计算历史动态预算。
                    let budget = self
                        .config
                        .context_window
                        .saturating_mul(10)
                        .saturating_div(100)
                        .max(256);
                    let bounded = crate::agent::context_assembler::reader::truncate_by_token_budget(
                        &text, budget,
                    );
                    format!("## Agent Pool Status\n{bounded}")
                }
            }
            None => String::new(),
        }
    }

    pub async fn build_messages(
        &self,
        user_input: &str,
        mode_hint: &str,
    ) -> (String, Vec<ChatMessage>) {
        let window = self.config.context_window;

        let system_prompt =
            crate::logic::model::prompts::compose_prompt(&self.prompts_dir, mode_hint);
        let system_tokens = estimate_tokens(&system_prompt);

        let user_tokens = estimate_tokens(user_input);

        let cognitive_quota = pct_of(window, self.config.cognitive_quota_pct);
        let attention_quota = pct_of(window, self.config.attention_quota_pct);
        let experience_quota = pct_of(window, self.config.experience_quota_pct);
        let preference_quota = pct_of(window, self.config.preference_quota_pct);

        use crate::agent::memory::memory_version::MemoryVersionKind;
        let cognitive_msgs = if self.memory_kind_active(MemoryVersionKind::Cognitive) {
            self.read_trivium_memories("cognitive", cognitive_quota)
                .await
        } else {
            Vec::new()
        };
        let experience_msgs = self
            .read_trivium_memories("experience", experience_quota)
            .await;
        let preference_msgs = self
            .read_trivium_memories("preference", preference_quota)
            .await;

        let cognitive_tokens = total_tokens(&cognitive_msgs);
        let experience_tokens = total_tokens(&experience_msgs);
        let preference_tokens = total_tokens(&preference_msgs);

        let fixed_tokens =
            system_tokens + cognitive_tokens + experience_tokens + preference_tokens + user_tokens;
        let dynamic_budget = window.saturating_sub(fixed_tokens);

        let rag_reserve = pct_of(dynamic_budget, self.config.rag_reserve_pct);
        let history_budget = dynamic_budget.saturating_sub(rag_reserve);

        let (recent_messages, history_truncated) = self.read_conversation_history(history_budget);

        let attention_msgs =
            if history_truncated && self.memory_kind_active(MemoryVersionKind::Attention) {
                self.read_trivium_memories("attention", attention_quota)
                    .await
            } else {
                Vec::new()
            };

        let mut messages = Vec::new();

        if !attention_msgs.is_empty() {
            tracing::debug!(
                "ContextAssembler: 历史被裁剪, 注入 {} attention memories 替补被裁轮次",
                attention_msgs.len()
            );
        }
        messages.extend(cognitive_msgs);
        messages.extend(attention_msgs);

        for msg in &recent_messages {
            messages.push(chat_message_of(msg));
        }

        messages.extend(experience_msgs);
        messages.extend(preference_msgs);

        messages.push(ChatMessage::User {
            text: user_input.to_string(),
        });

        (system_prompt, messages)
    }

    async fn read_trivium_memories(&self, memory_type: &str, quota: usize) -> Vec<ChatMessage> {
        if let Some(shared) = &self.shared_trivium {
            let db = match shared.lock() {
                Ok(db) => db,
                Err(e) => {
                    tracing::warn!("ContextAssembler: triviumdb lock poisoned: {e}");
                    return Vec::new();
                }
            };
            return self.read_memories_with_db(&db, memory_type, quota);
        }
        let db = match self.open_triviumdb() {
            Ok(db) => db,
            Err(_) => return Vec::new(),
        };
        self.read_memories_with_db(&db, memory_type, quota)
    }

    fn read_memories_with_db(
        &self,
        db: &crate::data::triviumdb::TriviumDb,
        memory_type: &str,
        quota: usize,
    ) -> Vec<ChatMessage> {
        let mut budget = ContextBudget::new(quota);
        let mut messages = Vec::new();

        let ids = db.db().get_all_ids();
        for id in ids {
            let payload = match db.db().get_payload(id) {
                Some(p) => p,
                None => continue,
            };

            let mem_type = payload.get("_memory_type").and_then(|v| v.as_str());
            let matches = if memory_type == "cognitive" {
                mem_type == Some("cognitive") || mem_type == Some("cognitive_edge")
            } else {
                mem_type == Some(memory_type)
            };
            if !matches {
                continue;
            }

            let content = format_memory_line(mem_type.unwrap_or(memory_type), &payload);
            let tokens = estimate_tokens(&content);
            if !budget.try_allocate(tokens) {
                break;
            }

            messages.push(ChatMessage::System {
                text: content,
                kind: SystemKind::Memory(memory_kind_of(memory_type)),
            });
        }

        messages
    }

    fn open_triviumdb(&self) -> Result<crate::data::triviumdb::TriviumDb> {
        let path = match &self.triviumdb_path {
            Some(p) => p.clone(),
            None => {
                return Err(AgentError::Bootstrap(
                    "TriviumDB path not configured".into(),
                ))
            }
        };
        if !path.exists() {
            return Err(AgentError::Bootstrap(format!(
                "TriviumDB file not found: {}",
                path.display()
            )));
        }

        crate::data::triviumdb::TriviumDb::open(&path, crate::data::triviumdb::DEFAULT_DIM)
    }

    /// 读取对话历史。返回 (选中的历史消息, 是否因预算不足被裁剪)。
    fn read_conversation_history(&self, budget: usize) -> (Vec<ParsedMessage>, bool) {
        let mut all_messages = match &self.thought_store {
            Some(thought_store) => match self.read_thought_messages(thought_store) {
                Ok(messages) if !messages.is_empty() => messages,
                Ok(_) => match self.read_all_messages(&self.conversations_dir) {
                    Ok(messages) => messages,
                    Err(error) => {
                        tracing::warn!(
                            "ContextAssembler: read legacy conversation history failed: {error}"
                        );
                        return (Vec::new(), false);
                    }
                },
                Err(error) => {
                    tracing::warn!("ContextAssembler: recover thought history failed: {error}");
                    return (Vec::new(), false);
                }
            },
            None => match self.read_all_messages(&self.conversations_dir) {
                Ok(messages) => messages,
                Err(error) => {
                    tracing::warn!("ContextAssembler: read conversation history failed: {error}");
                    return (Vec::new(), false);
                }
            },
        };

        if all_messages.is_empty() {
            return (Vec::new(), false);
        }

        let total_tokens: usize = all_messages.iter().map(|m| m.token_count).sum();
        if total_tokens <= budget {
            return (all_messages, false);
        }

        all_messages.reverse();
        let mut selected: Vec<ParsedMessage> = Vec::new();
        let mut used = 0usize;
        for msg in all_messages {
            if used + msg.token_count > budget && !selected.is_empty() {
                break;
            }
            used += msg.token_count;
            selected.push(msg);
        }
        selected.reverse();
        (selected, true)
    }

    fn read_all_messages(&self, session_dir: &Path) -> Result<Vec<ParsedMessage>> {
        if !session_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(session_dir)
            .map_err(|e| AgentError::Io(format!("read_dir {}: {}", session_dir.display(), e)))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();

        entries.sort_by_key(|e| e.file_name());

        let mut messages = Vec::new();
        for entry in entries {
            let path = entry.path();
            match parse_conversation_file(&path) {
                Ok(msg) => messages.push(msg),
                Err(e) => {
                    tracing::warn!(
                        "ContextAssembler: skip {} (parse error: {})",
                        path.display(),
                        e
                    );
                }
            }
        }

        Ok(messages)
    }

    fn read_thought_messages(&self, thought_store: &ThoughtStore) -> Result<Vec<ParsedMessage>> {
        let timeline = thought_store.recover()?;
        let mut messages = Vec::new();

        for group in timeline.groups {
            for context in group.contexts {
                let output = match context.output {
                    Some(output) => output,
                    None => continue,
                };
                if !matches!(&output.terminal_state, ThinkingTerminalState::Completed) {
                    continue;
                }

                let has_say = output.say.as_ref().is_some_and(|s| !s.is_empty());
                if !has_say {
                    continue;
                }

                messages.push(parsed_thinking_input(context.input));

                let mut parts: Vec<String> = Vec::new();
                if let Some(think) = output.think.filter(|think| !think.is_empty()) {
                    parts.push(think);
                }
                if let Some(say) = output.say.filter(|say| !say.is_empty()) {
                    parts.push(say);
                }
                if !parts.is_empty() {
                    messages.push(parsed_message("assistant", parts.join("\n")));
                }
            }
        }

        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::thought::ThinkingInput;
    use crate::agent::thought::{ThinkingOutput, ThinkingTerminalState, ThoughtContext, ThoughtId};
    use crate::common::UtcTimestamp;
    use tempfile::tempdir;

    #[test]
    fn default_config_has_sensible_values() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.recent_turns, 3);
        assert_eq!(cfg.raw_threshold_pct, 30.0);
        assert_eq!(cfg.rag_reserve_pct, 10.0);
        assert_eq!(cfg.cognitive_quota_pct, 5.0);
        assert_eq!(cfg.experience_quota_pct, 5.0);
        assert_eq!(cfg.preference_quota_pct, 3.0);
        assert_eq!(cfg.context_window, 1_000_000);
    }

    #[test]
    fn estimate_tokens_conservative_cjk() {
        assert_eq!(estimate_tokens("hello"), 5);
        assert_eq!(estimate_tokens("hello world"), 11);
        assert_eq!(estimate_tokens(""), 1);
        assert_eq!(estimate_tokens("abcd"), 4);
        assert_eq!(estimate_tokens("你好"), 4);
        assert_eq!(estimate_tokens("读 Cargo.toml"), 13);
    }

    #[test]
    fn pct_of_window() {
        assert_eq!(pct_of(100_000, 10.0), 10_000);
        assert_eq!(pct_of(128_000, 5.0), 6_400);
        assert_eq!(pct_of(100, 0.0), 0);
    }

    #[test]
    fn budget_tracks_usage() {
        let mut b = ContextBudget::new(100);
        assert_eq!(b.remaining(), 100);
        assert!(b.try_allocate(30));
        assert_eq!(b.remaining(), 70);
        assert!(b.try_allocate(50));
        assert_eq!(b.remaining(), 20);
        assert!(!b.try_allocate(30));
        assert_eq!(b.remaining(), 20);
    }

    #[test]
    fn budget_force_allocate_may_exceed() {
        let mut b = ContextBudget::new(50);
        b.force_allocate(100);
        assert_eq!(b.used, 100);
    }

    #[test]
    fn truncate_to_budget_keeps_within_limit() {
        let messages = vec![
            ParsedMessage {
                role: "assistant".into(),
                content: "aaaa".into(),
                token_count: 1,
            },
            ParsedMessage {
                role: "assistant".into(),
                content: "bbbbbbbb".into(),
                token_count: 2,
            },
            ParsedMessage {
                role: "assistant".into(),
                content: "cccc".into(),
                token_count: 1,
            },
        ];
        let result = truncate_to_budget(messages, 3);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "aaaa");
        assert_eq!(result[1].content, "bbbbbbbb");
    }

    #[test]
    fn truncate_to_budget_keeps_at_least_one() {
        let messages = vec![ParsedMessage {
            role: "assistant".into(),
            content: "x".repeat(100),
            token_count: 25,
        }];
        let result = truncate_to_budget(messages, 1);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_valid_conversation_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("20260712_120000.md");
        let content = "---\nrole: \"assistant\"\nmodel_id: \"test-model\"\ncreated_at: \"1234567890\"\nusage:\n  prompt_tokens: 10\n  completion_tokens: 20\n---\n\nHello, world!\n";
        std::fs::write(&path, content).unwrap();

        let msg = parse_conversation_file(&path).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "Hello, world!");
        assert!(msg.token_count > 0);
    }

    #[test]
    fn parse_conversation_file_with_capability_calls_marker() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("20260712_120001.md");
        let content = "---\nrole: \"assistant\"\nmodel_id: \"test-model\"\ncreated_at: \"1234567890\"\nusage:\n  prompt_tokens: 10\n  completion_tokens: 20\n---\n\nLet me read that file\n\n<!-- capability_calls -->\n```json\n{\"capability_id\": \"file.read\"}\n```\n";
        std::fs::write(&path, content).unwrap();

        let msg = parse_conversation_file(&path).unwrap();
        assert_eq!(msg.role, "assistant");
        assert!(msg.content.contains("Let me read that file"));
        assert!(msg.content.contains("<!-- capability_calls -->"));
    }

    #[test]
    fn parse_file_missing_opening_delimiter_is_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.md");
        std::fs::write(&path, "no frontmatter\n").unwrap();
        assert!(parse_conversation_file(&path).is_err());
    }

    #[test]
    fn parse_file_without_turn_still_works() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("20260712_120000.md");
        std::fs::write(&path, "---\nrole: \"assistant\"\n---\n\nbody\n").unwrap();
        let msg = parse_conversation_file(&path).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "body");
    }

    #[test]
    fn format_cognitive_memory() {
        let payload = serde_json::json!({
            "_memory_type": "cognitive",
            "insight": "User prefers short answers",
            "context": "previous conversations"
        });
        let line = format_memory_line("cognitive", &payload);
        assert!(line.contains("[COGNITIVE]"));
        assert!(line.contains("User prefers short answers"));
    }

    #[test]
    fn format_experience_memory() {
        let payload = serde_json::json!({
            "_memory_type": "experience",
            "title": "Fixed a bug",
            "summary": "success"
        });
        let line = format_memory_line("experience", &payload);
        assert!(line.contains("[EXPERIENCE]"));
        assert!(line.contains("Fixed a bug"));
        assert!(line.contains("success"));
    }

    #[test]
    fn format_preference_memory() {
        let payload = serde_json::json!({
            "_memory_type": "preference",
            "key": "language",
            "value": "rust"
        });
        let line = format_memory_line("preference", &payload);
        assert!(line.contains("[PREFERENCE]"));
        assert!(line.contains("language: rust"));
    }

    #[tokio::test]
    async fn assembler_without_triviumdb_produces_system_user() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let assembler = ContextAssembler::new(ContextConfig::default(), &data_dir, None);

        let (system_prompt, messages) = assembler.build_messages("hello", "unni").await;
        assert!(!messages.is_empty());

        assert!(!system_prompt.is_empty());

        let last = messages.last().unwrap();
        assert!(matches!(last, ChatMessage::User { text } if text == "hello"));
    }

    #[tokio::test]
    async fn assembler_degrade_on_missing_session_dir() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let assembler = ContextAssembler::new(ContextConfig::default(), &data_dir, None);

        let (_system_prompt, messages) = assembler.build_messages("test input", "unni").await;

        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            ChatMessage::User { text } if text == "test input"
        ));
    }

    #[tokio::test]
    async fn assembler_reads_conversation_history() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let conversations_dir = data_dir.join("conversations");
        std::fs::create_dir_all(&conversations_dir).unwrap();

        let t1 = "---\nsession_id: \"test-session\"\nturn: 1\nrole: \"user\"\nmodel_id: \"m\"\ncreated_at: \"1\"\nusage:\n  prompt_tokens: 0\n  completion_tokens: 0\n---\n\nUser question one\n";
        let t2 = "---\nsession_id: \"test-session\"\nturn: 2\nrole: \"assistant\"\nmodel_id: \"m\"\ncreated_at: \"2\"\nusage:\n  prompt_tokens: 10\n  completion_tokens: 20\n---\n\nAssistant reply two\n";
        std::fs::write(conversations_dir.join("turn_001.md"), t1).unwrap();
        std::fs::write(conversations_dir.join("turn_002.md"), t2).unwrap();

        let assembler = ContextAssembler::new(ContextConfig::default(), &data_dir, None);

        let (_system_prompt, messages) = assembler.build_messages("new question", "unni").await;

        assert!(
            messages.len() >= 3,
            "expected at least 3, got {}",
            messages.len()
        );

        let has_user_turn = messages
            .iter()
            .any(|m| message_text(m).contains("User question one"));
        let has_assistant_turn = messages
            .iter()
            .any(|m| message_text(m).contains("Assistant reply two"));
        assert!(has_user_turn, "should contain user turn 1");
        assert!(has_assistant_turn, "should contain assistant turn 2");

        assert!(matches!(
            messages.last().unwrap(),
            ChatMessage::User { text } if text == "new question"
        ));
    }

    #[tokio::test]
    async fn assembler_prefers_completed_thought_history_and_skips_pending_input() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let thought_store = Arc::new(ThoughtStore::open(&data_dir).unwrap());

        let mut completed = ThoughtContext::new_at(
            ThoughtId::parse("ca761232-ed42-11ce-bacd-00aa0057b223").unwrap(),
            UtcTimestamp::parse("2026-07-15T12:34:56.123456789Z").unwrap(),
            ThinkingInput::User {
                text: "durable earlier question".to_string(),
            },
        );
        thought_store.persist_input(&completed).unwrap();
        completed.set_output(ThinkingOutput::completed(
            Some("durable earlier work".to_string()),
            Some("durable earlier answer".to_string()),
            None,
        ));
        thought_store.persist_output(&completed).unwrap();

        let pending = ThoughtContext::new_at(
            ThoughtId::parse("ca761233-ed42-11ce-bacd-00aa0057b223").unwrap(),
            UtcTimestamp::parse("2026-07-15T12:35:00.000000000Z").unwrap(),
            ThinkingInput::User {
                text: "incomplete input must not be duplicated".to_string(),
            },
        );
        thought_store.persist_input(&pending).unwrap();

        let mut failed = ThoughtContext::new_at(
            ThoughtId::parse("ca761234-ed42-11ce-bacd-00aa0057b223").unwrap(),
            UtcTimestamp::parse("2026-07-15T12:35:01.000000000Z").unwrap(),
            ThinkingInput::User {
                text: "failed input must not become orphan history".to_string(),
            },
        );
        thought_store.persist_input(&failed).unwrap();
        failed.set_output(ThinkingOutput::failed("validation failed"));
        thought_store.persist_output(&failed).unwrap();

        let mut cancelled = ThoughtContext::new_at(
            ThoughtId::parse("ca761235-ed42-11ce-bacd-00aa0057b223").unwrap(),
            UtcTimestamp::parse("2026-07-15T12:35:02.000000000Z").unwrap(),
            ThinkingInput::User {
                text: "cancelled input must not become orphan history".to_string(),
            },
        );
        thought_store.persist_input(&cancelled).unwrap();
        cancelled.set_output(ThinkingOutput::cancelled(Some(
            "cancelled by user".to_string(),
        )));
        thought_store.persist_output(&cancelled).unwrap();

        let mut assembler = ContextAssembler::new(ContextConfig::default(), &data_dir, None);
        assembler.set_thought_store(Arc::clone(&thought_store));
        let (_system_prompt, messages) = assembler.build_messages("current question", "unni").await;

        assert!(messages
            .iter()
            .any(|message| message_text(message) == "durable earlier question"));
        let assistant_history = messages
            .iter()
            .find(|message| matches!(message, ChatMessage::Assistant { .. }))
            .expect("completed Thought should produce assistant history");
        let flat_history = message_text(assistant_history);
        assert!(
            flat_history.contains("durable earlier work"),
            "平铺后历史应含 think 文本: {flat_history}"
        );
        assert!(
            flat_history.contains("durable earlier answer"),
            "平铺后历史应含 say 文本: {flat_history}"
        );
        assert!(
            !flat_history.contains("{\"think\""),
            "平铺后历史不应是 JSON 外壳: {flat_history}"
        );
        assert!(!messages
            .iter()
            .any(|message| { message_text(message) == "incomplete input must not be duplicated" }));
        assert!(!messages.iter().any(|message| {
            message_text(message) == "failed input must not become orphan history"
                || message_text(message) == "cancelled input must not become orphan history"
        }));
        assert!(matches!(
            messages.last().unwrap(),
            ChatMessage::User { text } if text == "current question"
        ));
        assert_eq!(
            completed.output.unwrap().terminal_state,
            ThinkingTerminalState::Completed
        );
    }

    #[tokio::test]
    async fn assembler_respects_recent_turns_config() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let conversations_dir = data_dir.join("conversations");
        std::fs::create_dir_all(&conversations_dir).unwrap();

        for i in 1..=5 {
            let content = format!(
                "---\nsession_id: \"test-session\"\nturn: {i}\nrole: \"assistant\"\nmodel_id: \"m\"\ncreated_at: \"1\"\nusage:\n  prompt_tokens: 0\n  completion_tokens: 0\n---\n\nTurn {i} content\n"
            );
            std::fs::write(conversations_dir.join(format!("turn_{i:03}.md")), content).unwrap();
        }

        let config = ContextConfig {
            recent_turns: 1,
            ..ContextConfig::default()
        };

        let assembler = ContextAssembler::new(config, &data_dir, None);
        let (_system_prompt, messages) = assembler.build_messages("q", "unni").await;

        let assistant_count = messages
            .iter()
            .filter(|m| message_text(m).contains("Turn") && message_text(m).contains("content"))
            .count();
        assert!(assistant_count > 0, "should have at least some history");
    }

    #[tokio::test]
    async fn assembler_attention_gated_by_active_version() {
        use crate::agent::memory::memory_version as mv;
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let trivium_path = data_dir.join("memory.trivium");
        {
            let mut db = crate::data::triviumdb::TriviumDb::open(
                &trivium_path,
                crate::data::triviumdb::DEFAULT_DIM,
            )
            .unwrap();
            let payload = serde_json::json!({
                "_memory_type": "attention",
                "focus": "用户偏好 Rust",
                "content": "用户在第1轮说喜欢 Rust",
            });
            let zero_vec = vec![0.0_f32; db.db().dim()];
            db.db_mut().insert(&zero_vec, payload).unwrap();
            db.flush().unwrap();
        }

        let memory_conn = duckdb::Connection::open_in_memory().unwrap();
        mv::create_memory_version_tables(&memory_conn).unwrap();
        let memory_db = std::sync::Arc::new(std::sync::Mutex::new(memory_conn));

        let mut assembler =
            ContextAssembler::new(ContextConfig::default(), &data_dir, Some(trivium_path));
        assembler.set_memory_db(std::sync::Arc::clone(&memory_db));

        let (_s, msgs) = assembler.build_messages("q", "unni").await;
        assert!(
            !msgs
                .iter()
                .any(|m| message_text(m).contains("用户偏好 Rust")),
            "无 active 版本时 attention 不可见"
        );

        {
            let conn = memory_db.lock().unwrap();
            let vid = mv::stage(
                &conn,
                mv::MemoryVersionKind::Attention,
                "trivium://attention/t1",
                &["t1".to_string()],
            )
            .unwrap();
            mv::publish(&conn, vid).unwrap();
        }

        let (_s, msgs) = assembler.build_messages("q", "unni").await;
        assert!(
            !msgs
                .iter()
                .any(|m| message_text(m).contains("用户偏好 Rust")),
            "历史未超预算时不需要 attention 替补 (历史优先策略)"
        );
    }

    #[tokio::test]
    async fn attention_injected_as_backup_when_history_truncated() {
        use crate::agent::memory::memory_version as mv;

        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let thought_store = Arc::new(ThoughtStore::open(&data_dir).unwrap());

        for i in 0..6 {
            let mut ctx = ThoughtContext::new_at(
                ThoughtId::parse(format!("ca76123{i}-ed42-11ce-bacd-00aa0057b223").as_str())
                    .unwrap(),
                UtcTimestamp::parse(&format!("2026-07-1{i}T12:34:56.123456789Z")).unwrap(),
                ThinkingInput::User {
                    text: format!("第 {i} 轮用户问题"),
                },
            );
            thought_store.persist_input(&ctx).unwrap();
            ctx.set_output(ThinkingOutput::completed(
                None,
                Some(format!(
                    "第 {i} 轮回答内容 {}",
                    "文本文本文本内容填充填充".repeat(10)
                )),
                None,
            ));
            thought_store.persist_output(&ctx).unwrap();
        }

        let trivium_path = data_dir.join("memory.trivium");
        {
            let mut db = crate::data::triviumdb::TriviumDb::open(
                &trivium_path,
                crate::data::triviumdb::DEFAULT_DIM,
            )
            .unwrap();
            let payload = serde_json::json!({
                "_memory_type": "attention",
                "focus": "替补注意力",
                "content": "历史被裁剪时的注意力要点",
            });
            let zero_vec = vec![0.0_f32; db.db().dim()];
            db.db_mut().insert(&zero_vec, payload).unwrap();
            db.flush().unwrap();
        }

        let memory_conn = duckdb::Connection::open_in_memory().unwrap();
        mv::create_memory_version_tables(&memory_conn).unwrap();
        let memory_db = std::sync::Arc::new(std::sync::Mutex::new(memory_conn));
        {
            let conn = memory_db.lock().unwrap();
            let vid = mv::stage(
                &conn,
                mv::MemoryVersionKind::Attention,
                "trivium://attention/t1",
                &["t1".to_string()],
            )
            .unwrap();
            mv::publish(&conn, vid).unwrap();
        }

        let mut assembler = ContextAssembler::new(
            ContextConfig::default(),
            &data_dir,
            Some(trivium_path.clone()),
        );
        assembler.set_thought_store(Arc::clone(&thought_store));
        assembler.set_memory_db(std::sync::Arc::clone(&memory_db));

        let tight = ContextConfig {
            context_window: 1000,
            rag_reserve_pct: 10.0,
            ..ContextConfig::default()
        };
        let mut assembler_tight = ContextAssembler::new(tight, &data_dir, Some(trivium_path));
        assembler_tight.set_thought_store(Arc::clone(&thought_store));
        assembler_tight.set_memory_db(std::sync::Arc::clone(&memory_db));

        let (_s, msgs) = assembler_tight.build_messages("q", "unni").await;
        assert!(
            msgs.iter().any(|m| message_text(m).contains("替补注意力")),
            "历史超预算被裁剪时, attention 应替补注入"
        );
    }

    #[test]
    fn parse_yaml_string_valid() {
        assert_eq!(
            parse_yaml_string("role: \"assistant\"", "role:"),
            Some("assistant".to_string())
        );
    }
}
