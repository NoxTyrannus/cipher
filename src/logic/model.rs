pub mod anthropic;
pub mod api_key;
pub mod capability;
pub mod error;
pub mod openai;
pub mod prompts;
pub mod provider;
pub mod registry;
pub mod responses;
pub mod stream;

pub use anthropic::AnthropicProvider;
pub use error::map_reqwest_error;
pub use openai::OpenAiProvider;
pub use provider::{
    LlmProvider, LlmRequest, LlmResponse, Message, MessageRole, ToolCall, ToolCallFormat, Usage,
};
pub use registry::ProviderRegistry;
pub use responses::ResponsesProvider;
pub use stream::StreamChunk;

#[cfg(test)]
mod tests;
