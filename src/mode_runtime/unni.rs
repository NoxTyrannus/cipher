use super::mode::{Mode, ModeContext, ModeResponse};
use crate::common::AgentError;
use async_trait::async_trait;

#[derive(Debug, Default, Clone)]
pub struct UnniMode;

impl UnniMode {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Mode for UnniMode {
    fn name(&self) -> &'static str {
        "UNNI"
    }

    fn description(&self) -> &'static str {
        "UNNI 模式: 协同思考、执行与自然对话 (默认模式)"
    }

    async fn enter(&mut self, _ctx: &mut ModeContext) -> Result<(), AgentError> {
        tracing::info!("UNNI mode entered");
        Ok(())
    }

    async fn exit(&mut self, _ctx: &mut ModeContext) -> Result<(), AgentError> {
        tracing::info!("UNNI mode exited");
        Ok(())
    }

    async fn handle_input(
        &mut self,
        input: &str,
        _ctx: &mut ModeContext,
        factory: &crate::agent::thinking::ThinkingFactory,
    ) -> Result<ModeResponse, AgentError> {
        let _ = factory;
        Ok(ModeResponse::text(format!("[UNNI] {}", input)))
    }

    fn render_status(&self) -> String {
        "[UNNI]".to_string()
    }

    fn gate_awaiting(&self, _output: &crate::agent::output::AgentOutput) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unni_mode_basic_metadata() {
        let m = UnniMode::new();
        assert_eq!(m.name(), "UNNI");
        assert!(m.description().contains("协同"));
        assert_eq!(m.render_status(), "[UNNI]");
    }

    #[tokio::test]
    async fn unni_handle_input_does_not_invent_approval_state() {
        let mut m = UnniMode::new();
        let mut ctx = ModeContext::default();
        let factory = crate::agent::thinking::ThinkingFactory::new();
        let r = m
            .handle_input("build a CLI tool", &mut ctx, &factory)
            .await
            .unwrap();
        assert!(!r.awaiting_confirmation);
    }

    #[tokio::test]
    async fn unni_enter_exit_no_panic() {
        let mut m = UnniMode::new();
        let mut ctx = ModeContext::default();
        m.enter(&mut ctx).await.unwrap();
        m.exit(&mut ctx).await.unwrap();
    }
}
