pub mod error;
pub mod json_util;
/// #13（v0.5.6）：API key 单行掩码输入组件（CLI 向导与 TUI 面板共用，一处真源）。
pub mod masked_input;
pub mod time;
pub mod types;
pub mod watchdog;

pub use error::{AgentError, Result};
pub use time::UtcTimestamp;
pub use types::unix_timestamp_now;
