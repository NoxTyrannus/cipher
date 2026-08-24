//! 能力记忆 agent（洞察域常驻节点）。
//!
//! 设计（v0.4.2）：
//! - 输入：洞察中台发布的散文正文流（每轮洞察完成后异步投递）；
//! - 常驻、**滑动窗口**：固定 token 预算（4K），超出按轮次阶段丢弃最远（仍超出再丢）；
//! - 工具：`usage_method.observe`（原子能力，服务层校验出口已存在）；
//! - 协议：复用 `run_capability_loop`（无专属提示词文件；system=compose(空 base)+可用能力；
//!   指令内联在 user_prompt）；失败不回环重试（指令禁止 + max_turns 上限兜底）。

use crate::agent::memory::capability_agent::{run_capability_loop, CapabilityLoopRequest};
use crate::agent::memory::memory_capability_entries;
use crate::data::duckdb::Registry;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::model::prompts::compose_agent_capability_prompt;
use crate::logic::model::provider::LlmProvider;
use secrecy::SecretString;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc;

pub const CAPABILITY_MEMORY_ACTOR_ID: &str = "capability-memory-agent";

/// 滑动窗口 token 预算（4K）。
const WINDOW_TOKEN_BUDGET: usize = 4096;

pub struct CapabilityMemoryAgent {
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    registry: Registry,
    executor: Arc<CapabilityExecutor>,
    inbox_rx: mpsc::Receiver<String>,
    window: VecDeque<String>,
}

/// 按 token 预算滑动：从最旧丢弃，直到窗口总量 ≤ 预算。
/// token 估算：字符数 / 2（CJK/英文混排近似；4K 预算为软上限，容许估算误差）。
fn trim_window(window: &mut VecDeque<String>) {
    let budget_chars = WINDOW_TOKEN_BUDGET.saturating_mul(2);
    let mut total: usize = window.iter().map(|s| s.chars().count()).sum();
    while total > budget_chars {
        let Some(oldest) = window.pop_front() else {
            break;
        };
        total = total.saturating_sub(oldest.chars().count());
    }
}

impl CapabilityMemoryAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: SecretString,
        registry: Registry,
        executor: Arc<CapabilityExecutor>,
        inbox_rx: mpsc::Receiver<String>,
    ) -> Self {
        Self {
            provider,
            model_row,
            api_key,
            registry,
            executor,
            inbox_rx,
            window: VecDeque::new(),
        }
    }

    pub async fn run(mut self) {
        while let Some(insight) = self.inbox_rx.recv().await {
            if insight.trim().is_empty() {
                continue;
            }
            self.window.push_back(insight);
            trim_window(&mut self.window);
            if let Err(e) = self.process().await {
                tracing::warn!("capability-memory-agent: process failed: {e}");
            }
        }
        tracing::info!("capability-memory-agent: inbox closed, exiting");
    }

    async fn process(&self) -> crate::common::Result<()> {
        let available =
            memory_capability_entries(&self.registry, &self.executor, CAPABILITY_MEMORY_ACTOR_ID);
        let system_prompt = compose_agent_capability_prompt("", &available);
        let assistant_segments: Vec<String> = self.window.iter().cloned().collect();
        let user_prompt = "以下是最新的洞察结果（按时间旧→新）。如果其中有值得沉淀的工具使用观察（能力调用的问题/经验/建议），调用 usage_method.observe 写入（capability_id 必须来自文中实际提到的能力，observation 描述问题/经验，suggestion 给出改进建议）；没有值得沉淀的内容就输出 done。不要重试失败的调用，失败时直接 done 说明原因。开始执行。".to_string();

        let outcome = run_capability_loop(
            &self.provider,
            &self.model_row,
            &self.api_key,
            &self.registry,
            &self.executor,
            CapabilityLoopRequest {
                actor_id: CAPABILITY_MEMORY_ACTOR_ID.to_string(),
                system_prompt,
                assistant_segments,
                user_prompt,
            },
        )
        .await?;

        for line in &outcome.logs {
            tracing::info!("capability-memory-agent: {line}");
        }
        if !outcome.completed {
            tracing::warn!(
                "capability-memory-agent: did not finish within max_turns ({} calls)",
                outcome.calls.len()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_of(entries: &[&str]) -> VecDeque<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn trim_window_drops_oldest_until_budget() {
        let mut window = window_of(&["旧", "旧", "旧", "最新"]);
        trim_window(&mut window);
        // 预算 4K token = 8K 字符；上面的窗口远小于预算 → 不丢弃。
        assert_eq!(window.len(), 4);

        // 超预算：8K+ 字符 → 丢最旧直到 ≤8K 字符。
        let big = "x".repeat(3000);
        let mut window = window_of(&["旧", &big, &big, &big]); // 9002 chars
        trim_window(&mut window);
        let total: usize = window.iter().map(|s| s.chars().count()).sum();
        assert!(total <= 8000);
        // 最旧的「旧」被丢弃（3×3000=9000 已超，需再丢 3000 → 只剩 2 段 big？）
        // 9002 > 8000 → pop「旧」(1) → 9001 > 8000 → pop big(3000) → 6001 ≤ 8000 → [big, big]
        assert_eq!(window.len(), 2);
        assert_eq!(window.front().map(|s| s.len()), Some(3000));
    }

    #[test]
    fn trim_window_empty_is_noop() {
        let mut window: VecDeque<String> = VecDeque::new();
        trim_window(&mut window);
        assert!(window.is_empty());
    }
}
