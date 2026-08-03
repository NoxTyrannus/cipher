use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModeKind {
    #[default]
    Unni,
    Keep,
    Loop,
}

impl ModeKind {
    pub const ALL: [ModeKind; 3] = [ModeKind::Unni, ModeKind::Keep, ModeKind::Loop];

    pub fn next(self) -> Self {
        match self {
            ModeKind::Unni => ModeKind::Keep,
            ModeKind::Keep => ModeKind::Loop,
            ModeKind::Loop => ModeKind::Unni,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            ModeKind::Unni => ModeKind::Loop,
            ModeKind::Keep => ModeKind::Unni,
            ModeKind::Loop => ModeKind::Keep,
        }
    }

    pub fn cycle(self) -> Self {
        self.next()
    }

    pub fn name(self) -> &'static str {
        match self {
            ModeKind::Unni => "UNNI",
            ModeKind::Keep => "KEEP",
            ModeKind::Loop => "LOOP",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ModeKind::Unni => "unni",
            ModeKind::Keep => "keep",
            ModeKind::Loop => "loop",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            ModeKind::Unni => "UNNI 模式: 协同思考、执行与自然对话 (默认模式)",
            ModeKind::Keep => "KEEP 模式: AI 主导执行, 单一任务, 完成后回报",
            ModeKind::Loop => "LOOP 模式: 自主目标 + 飞轮迭代, 无硬截断 (per ADR-053)",
        }
    }
}

impl std::str::FromStr for ModeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "unni" => Ok(ModeKind::Unni),
            "keep" => Ok(ModeKind::Keep),
            "loop" => Ok(ModeKind::Loop),
            _ => Err(format!("unknown mode: {}", s)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeResponse {
    pub text: String,

    pub blocks: Vec<RenderBlock>,

    pub awaiting_confirmation: bool,

    pub instance_id: Option<String>,

    pub think: Option<String>,
}

impl ModeResponse {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            blocks: Vec::new(),
            awaiting_confirmation: false,
            instance_id: None,
            think: None,
        }
    }

    pub fn with_awaiting_confirmation(mut self) -> Self {
        self.awaiting_confirmation = true;
        self
    }

    pub fn with_block(mut self, block: RenderBlock) -> Self {
        self.blocks.push(block);
        self
    }

    pub fn with_instance_id(mut self, id: impl Into<String>) -> Self {
        self.instance_id = Some(id.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderBlock {
    Text(String),
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Code {
        lang: String,
        source: String,
    },
    Image {
        path: String,
        alt: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub agent_id: String,

    pub agent_name: String,
}

impl AgentRef {
    pub fn new(agent_id: impl Into<String>, agent_name: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            agent_name: agent_name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppState {
    pub theme: String,

    pub viewport: (u16, u16),

    pub message_count: u32,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            viewport: (80, 24),
            message_count: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OutputType {
    #[default]
    Message,

    PermissionRequest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssembledContext {
    pub system_role: String,
    pub mode_prompt: String,
    pub user_input: String,
    pub history: Vec<String>,
    pub long_term_memory: Vec<String>,
    pub rag_results: Vec<String>,
    pub tool_state: Vec<String>,
    pub scratchpad: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModeContext {
    pub mode_name: ModeKind,
    pub mode_hint: String,
    pub user_preferences: UserPreferences,
    pub agent: Option<AgentRef>,
    pub app_state: AppState,
    pub output_type: OutputType,
    pub request_permission: bool,
    pub context: Option<AssembledContext>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserPreferences {
    pub default_mode: String,

    pub language: String,

    pub max_iterations: u32,
}

#[async_trait]
pub trait Mode: Send + Sync {
    fn name(&self) -> &'static str;

    fn description(&self) -> &'static str;

    async fn enter(&mut self, ctx: &mut ModeContext) -> Result<(), crate::common::AgentError>;

    async fn exit(&mut self, ctx: &mut ModeContext) -> Result<(), crate::common::AgentError>;

    async fn handle_input(
        &mut self,
        input: &str,
        ctx: &mut ModeContext,
        factory: &crate::agent::thinking::ThinkingFactory,
    ) -> Result<ModeResponse, crate::common::AgentError>;

    fn render_status(&self) -> String;

    fn gate_awaiting(&self, _output: &crate::agent::output::AgentOutput) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_response_text_constructor() {
        let r = ModeResponse::text("hello");
        assert_eq!(r.text, "hello");
        assert!(r.blocks.is_empty());
        assert!(!r.awaiting_confirmation);
    }

    #[test]
    fn mode_response_with_awaiting() {
        let r = ModeResponse::text("approve? (y/n)").with_awaiting_confirmation();
        assert!(r.awaiting_confirmation);
    }

    #[test]
    fn mode_response_with_block() {
        let r = ModeResponse::text("see table").with_block(RenderBlock::Table {
            headers: vec!["a".into(), "b".into()],
            rows: vec![vec!["1".into(), "2".into()]],
        });
        assert_eq!(r.blocks.len(), 1);
    }

    #[test]
    fn mode_context_default_is_empty() {
        let ctx = ModeContext::default();
        assert_eq!(ctx.mode_name, ModeKind::default());
        assert_eq!(ctx.mode_hint, "");
        assert_eq!(ctx.user_preferences.default_mode, "");
        assert!(ctx.agent.is_none());
        assert_eq!(ctx.app_state.theme, "dark");
        assert_eq!(ctx.output_type, OutputType::Message);
        assert!(!ctx.request_permission);
        assert!(ctx.context.is_none());
    }

    #[test]
    fn user_preferences_default_is_zero_values() {
        let p = UserPreferences::default();
        assert_eq!(p.default_mode, "");
        assert_eq!(p.language, "");
        assert_eq!(p.max_iterations, 0);
    }

    #[test]
    fn agent_ref_new_sets_fields() {
        let r = AgentRef::new("agent-001", "main-agent");
        assert_eq!(r.agent_id, "agent-001");
        assert_eq!(r.agent_name, "main-agent");
    }

    #[test]
    fn app_state_default_is_dark_80x24() {
        let s = AppState::default();
        assert_eq!(s.theme, "dark");
        assert_eq!(s.viewport, (80, 24));
        assert_eq!(s.message_count, 0);
    }

    #[test]
    fn output_type_default_is_message() {
        let t = OutputType::default();
        assert_eq!(t, OutputType::Message);
    }

    #[test]
    fn assembled_context_default_is_empty() {
        let c = AssembledContext::default();
        assert!(c.system_role.is_empty());
        assert!(c.history.is_empty());
        assert_eq!(c.user_input, "");
    }

    #[test]
    fn mode_kind_next_cycles_unni_keep_loop() {
        assert_eq!(ModeKind::Unni.next(), ModeKind::Keep);
        assert_eq!(ModeKind::Keep.next(), ModeKind::Loop);
        assert_eq!(ModeKind::Loop.next(), ModeKind::Unni);
    }

    #[test]
    fn mode_kind_prev_cycles_reverse() {
        assert_eq!(ModeKind::Unni.prev(), ModeKind::Loop);
        assert_eq!(ModeKind::Loop.prev(), ModeKind::Keep);
        assert_eq!(ModeKind::Keep.prev(), ModeKind::Unni);
    }

    #[test]
    fn mode_kind_name_uppercase() {
        assert_eq!(ModeKind::Unni.name(), "UNNI");
        assert_eq!(ModeKind::Keep.name(), "KEEP");
        assert_eq!(ModeKind::Loop.name(), "LOOP");
    }

    struct MockMode;
    #[async_trait]
    impl Mode for MockMode {
        fn name(&self) -> &'static str {
            "MOCK"
        }
        fn description(&self) -> &'static str {
            "mock mode"
        }
        async fn enter(&mut self, _ctx: &mut ModeContext) -> Result<(), crate::common::AgentError> {
            Ok(())
        }
        async fn exit(&mut self, _ctx: &mut ModeContext) -> Result<(), crate::common::AgentError> {
            Ok(())
        }
        async fn handle_input(
            &mut self,
            _input: &str,
            _ctx: &mut ModeContext,
            _factory: &crate::agent::thinking::ThinkingFactory,
        ) -> Result<ModeResponse, crate::common::AgentError> {
            Ok(ModeResponse::text("mock response"))
        }
        fn render_status(&self) -> String {
            "[MOCK]".to_string()
        }
    }

    #[test]
    fn mock_mode_satisfies_trait_shape() {
        let m = MockMode;
        assert_eq!(m.name(), "MOCK");
        assert_eq!(m.description(), "mock mode");
        assert_eq!(m.render_status(), "[MOCK]");
    }
}
