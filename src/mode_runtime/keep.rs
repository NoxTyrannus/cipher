use super::mode::{Mode, ModeContext, ModeResponse};
use crate::common::AgentError;
use async_trait::async_trait;

#[derive(Debug, Default)]
pub struct KeepMode;

impl KeepMode {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Mode for KeepMode {
    fn name(&self) -> &'static str {
        "KEEP"
    }

    fn description(&self) -> &'static str {
        "KEEP 模式: AI 主导执行, 单一任务, 完成后回报"
    }

    async fn enter(&mut self, _ctx: &mut ModeContext) -> Result<(), AgentError> {
        tracing::info!("KEEP mode entered");
        Ok(())
    }

    async fn exit(&mut self, _ctx: &mut ModeContext) -> Result<(), AgentError> {
        tracing::info!("KEEP mode exited");
        Ok(())
    }

    async fn handle_input(
        &mut self,
        input: &str,
        _ctx: &mut ModeContext,
        factory: &crate::agent::thinking::ThinkingFactory,
    ) -> Result<ModeResponse, AgentError> {
        let _ = factory;
        Ok(ModeResponse::text(format!("[KEEP] {}", input)))
    }

    fn render_status(&self) -> String {
        "[KEEP]".to_string()
    }

    fn gate_awaiting(&self, _output: &crate::agent::output::AgentOutput) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_mode_basic_metadata() {
        let m = KeepMode::new();
        assert_eq!(m.name(), "KEEP");
        assert!(m.description().contains("主导"));
        assert_eq!(m.render_status(), "[KEEP]");
    }

    #[tokio::test]
    async fn keep_handle_input_does_not_invent_approval_state() {
        let mut m = KeepMode::new();
        let mut ctx = ModeContext::default();
        let factory = crate::agent::thinking::ThinkingFactory::new();
        let r = m.handle_input("do X", &mut ctx, &factory).await.unwrap();
        assert!(!r.awaiting_confirmation);
    }

    #[tokio::test]
    async fn keep_enter_exit_no_panic() {
        let mut m = KeepMode::new();
        let mut ctx = ModeContext::default();
        m.enter(&mut ctx).await.unwrap();
        m.exit(&mut ctx).await.unwrap();
    }
}
