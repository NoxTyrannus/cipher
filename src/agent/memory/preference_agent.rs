use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;

use crate::agent::communication::AttentionRetireBatch;
use crate::agent::memory::capability_agent::{run_capability_loop, CapabilityLoopRequest};
use crate::agent::memory::memory_capability_entries;
use crate::data::duckdb::Registry;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::model::prompts::{
    compose_agent_capability_prompt, read_platform_prompt, MEMORY_PREFERENCE_DEFAULT,
};
use crate::logic::model::provider::LlmProvider;

const ACTOR_ID: &str = "preference-agent";

pub struct PreferenceMemoryAgent {
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: Option<SecretString>,
    prompts_dir: Option<PathBuf>,
    inbox_rx: tokio::sync::mpsc::Receiver<AttentionRetireBatch>,
    registry: Option<Registry>,
    executor: Option<Arc<CapabilityExecutor>>,
}

impl PreferenceMemoryAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: Option<SecretString>,
        prompts_dir: Option<PathBuf>,
        inbox_rx: tokio::sync::mpsc::Receiver<AttentionRetireBatch>,
        registry: Option<Registry>,
        executor: Option<Arc<CapabilityExecutor>>,
    ) -> Self {
        Self {
            provider,
            model_row,
            api_key,
            prompts_dir,
            inbox_rx,
            registry,
            executor,
        }
    }

    #[allow(clippy::while_let_loop)]
    pub async fn run(mut self) {
        loop {
            match self.inbox_rx.recv().await {
                Some(batch) => {
                    if let Err(e) = self.process_batch(batch).await {
                        tracing::warn!("preference memory agent failed: {e}");
                    }
                }
                None => break,
            }
        }
        tracing::info!("preference memory agent: inbox closed, exiting");
    }

    /// T3：工具调用式交错思维链（与 attention-agent 同构）。
    /// 输入 = 退休注意力条目段（focus + source_refs）；agent 在能力循环内先查证
    /// （memory.evidence.lookup），再写入（memory.preference.write），全部完成后输出 done。
    async fn process_batch(&self, batch: AttentionRetireBatch) -> crate::common::Result<()> {
        if batch.retired_focus.is_empty() {
            return Ok(());
        }

        let base_prompt = match &self.prompts_dir {
            Some(dir) => read_platform_prompt(dir, "memory_preference.md"),
            None => MEMORY_PREFERENCE_DEFAULT.to_string(),
        };

        let (Some(registry), Some(executor)) = (&self.registry, &self.executor) else {
            tracing::warn!("{ACTOR_ID}: capability runtime not configured; skipping batch");
            return Ok(());
        };
        let Some(api_key) = &self.api_key else {
            return Ok(());
        };

        let focus_list = batch
            .retired_focus
            .iter()
            .zip(batch.source_refs.iter())
            .map(|(focus, refs)| {
                if refs.is_empty() {
                    format!("- {focus}")
                } else {
                    format!("- {focus} (source_refs: {})", refs.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let input_segment = format!("## Retired Attention Entries\n{focus_list}");

        let system_prompt = compose_agent_capability_prompt(
            &base_prompt,
            &memory_capability_entries(registry, executor, ACTOR_ID),
        );

        let req = CapabilityLoopRequest {
            actor_id: ACTOR_ID.to_string(),
            system_prompt,
            assistant_segments: vec![input_segment],
            user_prompt: "基于以上退休注意力条目，提取并沉淀偏好记忆。先查证（调用 memory.evidence.lookup 按 source_refs 检索原始证据），再调用 memory.preference.write 写入（entries 每项含 key/value 与 source_refs）；无值得提取的内容时输出 done 并简述原因。开始执行。".to_string(),
        };

        match run_capability_loop(
            &self.provider,
            &self.model_row,
            api_key,
            registry,
            executor,
            req,
        )
        .await
        {
            Ok(trace) => {
                for line in &trace.logs {
                    tracing::info!("{ACTOR_ID}: {line}");
                }
                if !trace.completed {
                    tracing::warn!(
                        "{ACTOR_ID}: did not finish within max_turns ({} calls)",
                        trace.calls.len()
                    );
                }
            }
            Err(e) => {
                tracing::warn!("{ACTOR_ID} loop failed: {e}");
            }
        }

        Ok(())
    }
}
