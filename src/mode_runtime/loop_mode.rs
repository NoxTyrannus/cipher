use super::mode::{Mode, ModeContext, ModeResponse};
use crate::common::AgentError;
use async_trait::async_trait;

#[derive(Debug, Default, Clone)]
pub struct LoopMode {
    iteration_count: u32,
    max_iterations: u32,
    idle_rounds: u32,
    max_idle_rounds: u32,
}

impl LoopMode {
    pub fn new() -> Self {
        Self {
            idle_rounds: 0,
            max_idle_rounds: 3,
            ..Self::default()
        }
    }
    pub fn iteration_count(&self) -> u32 {
        self.iteration_count
    }
    pub fn note_noop(&mut self) {
        self.idle_rounds += 1;
    }
    pub fn reset_idle(&mut self) {
        self.idle_rounds = 0;
    }
    pub fn should_stop_idle(&self) -> bool {
        self.idle_rounds >= self.max_idle_rounds
    }
}

#[async_trait]
impl Mode for LoopMode {
    fn name(&self) -> &'static str {
        "LOOP"
    }

    fn description(&self) -> &'static str {
        "LOOP 模式: 自主目标 + 飞轮迭代, 无硬截断"
    }

    async fn enter(&mut self, ctx: &mut ModeContext) -> Result<(), AgentError> {
        tracing::info!(iterations = self.iteration_count, "LOOP mode entered");

        self.iteration_count = 0;

        self.max_iterations = ctx.user_preferences.max_iterations;
        Ok(())
    }

    async fn exit(&mut self, _ctx: &mut ModeContext) -> Result<(), AgentError> {
        tracing::info!(iterations = self.iteration_count, "LOOP mode exited");
        Ok(())
    }

    async fn handle_input(
        &mut self,
        input: &str,
        _ctx: &mut ModeContext,
        factory: &crate::agent::thinking::ThinkingFactory,
    ) -> Result<ModeResponse, AgentError> {
        let _ = factory;
        self.iteration_count += 1;
        Ok(ModeResponse::text(format!(
            "[LOOP 第 {} 轮] {}",
            self.iteration_count, input
        )))
    }

    fn render_status(&self) -> String {
        format!("[LOOP · {} 轮]", self.iteration_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_mode_basic_metadata() {
        let m = LoopMode::new();
        assert_eq!(m.name(), "LOOP");
        assert!(m.description().contains("飞轮"));
        assert_eq!(m.render_status(), "[LOOP · 0 轮]");
    }

    #[tokio::test]
    async fn loop_handle_input_no_awaiting() {
        let mut m = LoopMode::new();
        let mut ctx = ModeContext::default();
        let factory = crate::agent::thinking::ThinkingFactory::new();
        let r = m
            .handle_input("iterate on X", &mut ctx, &factory)
            .await
            .unwrap();
        assert!(!r.awaiting_confirmation, "LOOP 不应 awaiting 审批");
    }

    #[tokio::test]
    async fn loop_tracks_iteration_count() {
        let mut m = LoopMode::new();
        let mut ctx = ModeContext::default();
        let factory = crate::agent::thinking::ThinkingFactory::new();
        m.enter(&mut ctx).await.unwrap();
        m.handle_input("iterate", &mut ctx, &factory).await.unwrap();
        m.handle_input("iterate again", &mut ctx, &factory)
            .await
            .unwrap();
        assert_eq!(m.iteration_count(), 2);
    }

    #[test]
    fn loop_idle_convergence_stops_after_three_noops() {
        let mut m = LoopMode::new();
        assert!(!m.should_stop_idle());
        m.note_noop();
        m.note_noop();
        assert!(!m.should_stop_idle());
        m.note_noop();
        assert!(m.should_stop_idle());
        m.reset_idle();
        assert!(!m.should_stop_idle());
    }

    #[tokio::test]
    async fn loop_enter_resets_count() {
        let mut m = LoopMode::new();
        let mut ctx = ModeContext::default();
        let factory = crate::agent::thinking::ThinkingFactory::new();
        m.enter(&mut ctx).await.unwrap();
        m.handle_input("a", &mut ctx, &factory).await.unwrap();
        m.handle_input("b", &mut ctx, &factory).await.unwrap();
        assert_eq!(m.iteration_count(), 2);

        m.exit(&mut ctx).await.unwrap();
        m.enter(&mut ctx).await.unwrap();
        assert_eq!(m.iteration_count(), 0);
    }
}
