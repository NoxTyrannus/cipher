use crate::common::AgentError;
use crate::mode_runtime::ModeResponse;
use async_trait::async_trait;

#[async_trait]
pub trait UiBackend: Send + Sync {
    async fn show_mode_status(&mut self, mode_name: &str, status: &str) -> Result<(), AgentError>;

    async fn show_response(&mut self, response: &ModeResponse) -> Result<(), AgentError>;

    async fn show_error(&mut self, error: &str) -> Result<(), AgentError>;

    async fn wait_for_input(&mut self) -> Result<String, AgentError>;

    fn check_cancel(&self) -> bool;
}
