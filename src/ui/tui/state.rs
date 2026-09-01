use crate::mode_runtime::ModeKind;

use super::config_panel::ConfigPanel;
use super::status_line::StatusLineState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    Normal,

    Config,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiMessage {
    User(String),

    Assistant(String),

    Streaming {
        id: String,
        content: String,
        finished: bool,
        error: Option<String>,
    },

    Think {
        id: String,
        text: String,
    },

    Request(String),
}

#[derive(Debug, Clone)]
pub struct TuiState {
    pub mode: TuiMode,

    pub messages: Vec<TuiMessage>,

    pub current_mode: ModeKind,

    pub current_error: Option<String>,

    pub input: String,

    pub agent_name: String,

    pub config_panel: ConfigPanel,

    pub status_line: StatusLineState,

    pub scroll_offset: usize,

    /// 全局思考面板显示开关（来自 `[ui] show_think`，缺省 true=显示）。
    pub ui_show_think: bool,

    /// UNNI per-mode 思考显示覆盖（来自 `[mode_styles.unni] show_think`，None=跟随全局）。
    pub unni_show_think: Option<bool>,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            mode: TuiMode::Normal,
            messages: Vec::new(),
            current_mode: ModeKind::Unni,
            current_error: None,
            input: String::new(),
            agent_name: "cipher".to_string(),
            config_panel: ConfigPanel::new(),
            status_line: StatusLineState::new(),
            scroll_offset: 0,
            ui_show_think: true,
            unni_show_think: None,
        }
    }

    pub fn enter_config(&mut self) {
        self.mode = TuiMode::Config;
        self.config_panel = ConfigPanel::new();
    }

    pub fn exit_config(&mut self) {
        self.mode = TuiMode::Normal;
    }

    pub fn push_user(&mut self, text: String) {
        self.scroll_offset = 0;
        self.current_error = None;
        self.messages.push(TuiMessage::User(text));
    }

    pub fn push_assistant(&mut self, text: String) {
        self.scroll_offset = 0;
        self.messages.push(TuiMessage::Assistant(text));
    }

    pub fn push_streaming(&mut self, id: String) {
        self.scroll_offset = 0;
        self.messages.push(TuiMessage::Streaming {
            id,
            content: String::new(),
            finished: false,
            error: None,
        });
    }

    pub fn last_user_message(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find_map(|m| {
                if let TuiMessage::User(t) = m {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub fn scroll_to_tail(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn push_request(&mut self, text: String) {
        self.messages.push(TuiMessage::Request(text));
    }

    pub fn set_error(&mut self, msg: String) {
        self.current_error = Some(msg);
    }

    fn find_streaming_mut(&mut self, id: &str) -> Option<&mut TuiMessage> {
        self.messages
            .iter_mut()
            .rev()
            .find(|m| matches!(m, TuiMessage::Streaming { id: mid, .. } if mid == id))
    }

    pub fn append_delta(&mut self, id: &str, text: &str) {
        if let Some(TuiMessage::Streaming { content: t, .. }) = self.find_streaming_mut(id) {
            t.push_str(text);
        }
    }

    pub fn push_think(&mut self, id: &str, text: &str) {
        self.scroll_offset = 0;
        if let Some(existing) = self
            .messages
            .iter_mut()
            .find(|m| matches!(m, TuiMessage::Think { id: mid, .. } if mid == id))
        {
            if let TuiMessage::Think { text: t, .. } = existing {
                *t = text.to_string();
            }
            return;
        }
        let think = TuiMessage::Think {
            id: id.to_string(),
            text: text.to_string(),
        };
        match self
            .messages
            .iter()
            .rposition(|m| matches!(m, TuiMessage::Streaming { id: mid, .. } if mid == id))
        {
            Some(index) => self.messages.insert(index, think),
            None => self.messages.push(think),
        }
    }

    pub fn finalize_stream(&mut self, id: &str) {
        let Some(index) = self.messages.iter().rposition(
            |message| matches!(message, TuiMessage::Streaming { id: mid, .. } if mid == id),
        ) else {
            return;
        };

        let text = match &mut self.messages[index] {
            TuiMessage::Streaming { content, .. } => std::mem::take(content),
            _ => unreachable!("streaming index must reference a streaming message"),
        };
        if text.is_empty() {
            self.messages.remove(index);
        } else {
            self.messages[index] = TuiMessage::Assistant(text);
        }
    }

    pub fn mark_cancelled(&mut self, id: &str) {
        if let Some(TuiMessage::Streaming {
            content, finished, ..
        }) = self.find_streaming_mut(id)
        {
            *finished = true;
            content.clear();
            content.push_str("[已中断]");
        }
    }

    pub fn mark_error(&mut self, id: &str, err: &str) {
        if let Some(TuiMessage::Streaming {
            content,
            error,
            finished,
            ..
        }) = self.find_streaming_mut(id)
        {
            content.clear();
            *error = Some(err.to_string());
            *finished = true;
        }
    }

    pub fn input_push(&mut self, c: char) {
        self.input.push(c);
    }

    pub fn input_backspace(&mut self) {
        self.input.pop();
    }

    pub fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_is_empty_unni() {
        let s = TuiState::new();
        assert!(s.messages.is_empty());
        assert_eq!(s.current_mode, ModeKind::Unni);
        assert!(s.current_error.is_none());
        assert!(s.input.is_empty());
    }

    #[test]
    fn push_user_clears_error() {
        let mut s = TuiState::new();
        s.set_error("网络错误".into());
        assert!(s.current_error.is_some());
        s.push_user("你好".into());
        assert!(s.current_error.is_none(), "push_user 应清错误态");
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0], TuiMessage::User("你好".into()));
    }

    #[test]
    fn push_assistant_and_request_append() {
        let mut s = TuiState::new();
        s.push_assistant("回复".into());
        s.push_request("用递归还是迭代?".into());
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0], TuiMessage::Assistant("回复".into()));
        assert_eq!(s.messages[1], TuiMessage::Request("用递归还是迭代?".into()));
    }

    #[test]
    fn streaming_finalize_uses_persisted_say_text() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.append_delta("inst-1", "Hel");
        s.append_delta("inst-1", "lo");
        s.finalize_stream("inst-1");
        match &s.messages[0] {
            TuiMessage::Assistant(text) => assert_eq!(text, "Hello"),
            other => panic!("expected Assistant after finalize, got {:?}", other),
        }
    }

    #[test]
    fn streaming_finalize_removes_think_only_placeholder() {
        let mut s = TuiState::new();
        s.push_streaming("inst-2".into());
        s.finalize_stream("inst-2");
        assert!(s.messages.is_empty());
    }

    #[test]
    fn streaming_finalize_never_reparses_model_json() {
        let mut s = TuiState::new();
        s.push_streaming("inst-3".into());
        s.append_delta("inst-3", "confirm?");
        s.finalize_stream("inst-3");
        match &s.messages[0] {
            TuiMessage::Assistant(text) => assert_eq!(text, "confirm?"),
            other => panic!("expected Assistant, got {:?}", other),
        }
    }

    #[test]
    fn streaming_mark_cancelled_appends_suffix() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.append_delta("inst-1", "partial");
        s.mark_cancelled("inst-1");
        match &s.messages[0] {
            TuiMessage::Streaming {
                content, finished, ..
            } => {
                assert!(content.contains("[已中断]"));
                assert!(!content.contains("partial"));
                assert!(finished);
            }
            _ => panic!("expected Streaming"),
        }
    }

    #[test]
    fn streaming_mark_error() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.append_delta("inst-1", "unvalidated model text");
        s.mark_error("inst-1", "network timeout");
        match &s.messages[0] {
            TuiMessage::Streaming {
                content,
                error,
                finished,
                ..
            } => {
                assert!(content.is_empty());
                assert_eq!(error.as_deref(), Some("network timeout"));
                assert!(finished);
            }
            _ => panic!("expected Streaming"),
        }
    }

    #[test]
    fn streaming_append_delta_stale_id_is_noop() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.append_delta("inst-999", "ghost");
        match &s.messages[0] {
            TuiMessage::Streaming { content, .. } => assert!(content.is_empty()),
            _ => panic!("expected Streaming"),
        }
    }

    #[test]
    fn streaming_push_creates_empty_bubble() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        match &s.messages[0] {
            TuiMessage::Streaming {
                id,
                content,
                finished,
                error,
            } => {
                assert_eq!(id, "inst-1");
                assert_eq!(content, "");
                assert!(!finished);
                assert!(error.is_none());
            }
            _ => panic!("expected Streaming"),
        }
    }

    #[test]
    fn push_think_inserts_before_same_id_streaming_bubble() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.push_think("inst-1", "deep thoughts");
        assert_eq!(s.messages.len(), 2);
        match &s.messages[0] {
            TuiMessage::Think { id, text } => {
                assert_eq!(id, "inst-1");
                assert_eq!(text, "deep thoughts");
            }
            other => panic!("expected Think before streaming bubble, got {other:?}"),
        }
        assert!(matches!(&s.messages[1], TuiMessage::Streaming { .. }));
    }

    #[test]
    fn push_think_appends_when_no_streaming_bubble() {
        let mut s = TuiState::new();
        s.push_think("inst-1", "standalone thought");
        assert_eq!(s.messages.len(), 1);
        assert!(matches!(&s.messages[0], TuiMessage::Think { .. }));
    }

    #[test]
    fn push_think_same_id_replaces_text() {
        let mut s = TuiState::new();
        s.push_think("inst-1", "v1");
        s.push_think("inst-1", "v2");
        assert_eq!(s.messages.len(), 1, "same id must not duplicate Think");
        match &s.messages[0] {
            TuiMessage::Think { text, .. } => assert_eq!(text, "v2"),
            _ => panic!("expected Think"),
        }
    }

    #[test]
    fn think_only_finalize_keeps_think_message() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.push_think("inst-1", "internal plan");
        s.finalize_stream("inst-1");
        assert_eq!(s.messages.len(), 1, "empty bubble removed, Think stays");
        match &s.messages[0] {
            TuiMessage::Think { text, .. } => assert_eq!(text, "internal plan"),
            other => panic!("think-only: Think must survive finalize_stream, got {other:?}"),
        }
    }

    #[test]
    fn think_then_say_finalize_keeps_both_in_order() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.push_think("inst-1", "plan");
        s.append_delta("inst-1", "answer");
        s.finalize_stream("inst-1");
        assert_eq!(s.messages.len(), 2);
        assert!(matches!(&s.messages[0], TuiMessage::Think { .. }));
        match &s.messages[1] {
            TuiMessage::Assistant(text) => assert_eq!(text, "answer"),
            other => panic!("expected Assistant after Think, got {other:?}"),
        }
    }

    #[test]
    fn input_buffer_ops() {
        let mut s = TuiState::new();
        s.input_push('h');
        s.input_push('i');
        assert_eq!(s.input, "hi");
        s.input_backspace();
        assert_eq!(s.input, "h");
        let taken = s.take_input();
        assert_eq!(taken, "h");
        assert!(s.input.is_empty(), "take 后缓冲清空");
    }
}
