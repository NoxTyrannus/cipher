use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;

use crate::agent::memory::capability_agent::{run_capability_loop, CapabilityLoopRequest};
use crate::agent::memory::memory_capability_entries;
use crate::data::duckdb::Registry;
use crate::data::triviumdb::TriviumDb;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::model::prompts::{
    compose_agent_capability_prompt, read_platform_prompt, MEMORY_COGNITIVE_DEFAULT,
};
use crate::logic::model::provider::LlmProvider;

const ACTOR_ID: &str = "cognitive-agent";

pub struct CognitiveAgent {
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: Option<SecretString>,
    triviumdb: Option<Arc<std::sync::Mutex<TriviumDb>>>,
    prompts_dir: Option<PathBuf>,
    instance_counter: u64,
    inbox_rx: tokio::sync::mpsc::Receiver<()>,

    memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,

    thought_store: Option<Arc<crate::data::thought_store::ThoughtStore>>,

    registry: Option<Registry>,

    executor: Option<Arc<CapabilityExecutor>>,
}

impl CognitiveAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: Option<SecretString>,
        triviumdb: Option<Arc<std::sync::Mutex<TriviumDb>>>,
        prompts_dir: Option<PathBuf>,
        inbox_rx: tokio::sync::mpsc::Receiver<()>,
        memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
        thought_store: Option<Arc<crate::data::thought_store::ThoughtStore>>,
        registry: Option<Registry>,
        executor: Option<Arc<CapabilityExecutor>>,
    ) -> Self {
        Self {
            provider,
            model_row,
            api_key,
            triviumdb,
            prompts_dir,
            instance_counter: 0,
            inbox_rx,
            memory_db,
            thought_store,
            registry,
            executor,
        }
    }

    #[allow(clippy::while_let_loop)]
    pub async fn run(mut self) {
        loop {
            match self.inbox_rx.recv().await {
                Some(_) => {
                    self.instance_counter += 1;
                    if self.instance_counter >= 29 {
                        self.instance_counter = 0;
                        if let Err(e) = self.process_cognitive_update().await {
                            tracing::warn!("cognitive agent update failed: {e}");
                        }
                    }
                }
                None => break,
            }
        }
        tracing::info!("cognitive agent: inbox closed, exiting");
    }

    /// T3：工具调用式交错思维链（与 attention-agent 同构）。
    /// 输入 = 近期思考摘要 + 当前认知图；agent 在能力循环内先查看（memory.list /
    /// memory.retrieve）、可查证（memory.evidence.lookup），再提交变更
    /// （memory.cognitive.update），全部完成后输出 done。写回在能力循环内完成。
    async fn process_cognitive_update(&self) -> crate::common::Result<()> {
        let base_prompt = match &self.prompts_dir {
            Some(dir) => read_platform_prompt(dir, "memory_cognitive.md"),
            None => MEMORY_COGNITIVE_DEFAULT.to_string(),
        };

        let (Some(registry), Some(executor)) = (&self.registry, &self.executor) else {
            tracing::warn!("{ACTOR_ID}: capability runtime not configured; update skipped");
            return Ok(());
        };
        let Some(api_key) = &self.api_key else {
            return Ok(());
        };

        let summaries = self.recent_thought_summaries(29);
        let current_graph = self.current_cognitive_graph().await;
        let mut input_parts = Vec::new();
        if !summaries.is_empty() {
            input_parts.push(format!("## Recent Thought Summaries\n{summaries}"));
        }
        if !current_graph.is_empty() {
            input_parts.push(format!("## Current Cognitive Graph\n{current_graph}"));
        }
        let input_segment = input_parts.join("\n\n");

        let system_prompt = compose_agent_capability_prompt(
            &base_prompt,
            &memory_capability_entries(registry, executor, ACTOR_ID),
        );

        let req = CapabilityLoopRequest {
            actor_id: ACTOR_ID.to_string(),
            system_prompt,
            assistant_segments: if input_segment.is_empty() {
                vec![]
            } else {
                vec![input_segment]
            },
            user_prompt: "基于以上思考摘要与当前认知图，更新认知图：先用 memory.list / memory.retrieve 查看相关节点与边，可调用 memory.evidence.lookup 查证原始证据，最后调用 memory.cognitive.update（nodes/edges）提交变更；无变更时输出 done 说明。开始执行。".to_string(),
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
                return Ok(());
            }
        }

        self.publish_cognitive_version();

        Ok(())
    }

    fn publish_cognitive_version(&self) {
        let Some(memory_db) = &self.memory_db else {
            return;
        };
        use crate::agent::memory::memory_version as mv;
        if let Ok(conn) = memory_db.lock() {
            let snapshot_ref =
                format!("trivium://cognitive/{}", crate::common::UtcTimestamp::now());
            match mv::stage(&conn, mv::MemoryVersionKind::Cognitive, &snapshot_ref, &[]) {
                Ok(vid) => {
                    if let Err(e) = mv::publish(&conn, vid) {
                        tracing::warn!("cognitive agent: publish version {vid} failed: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!("cognitive agent: stage version failed: {e}");
                }
            }
        }
    }

    async fn current_cognitive_graph(&self) -> String {
        let Some(db) = &self.triviumdb else {
            return String::new();
        };
        let db = match db.lock() {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("cognitive agent: triviumdb lock poisoned: {e}");
                return String::new();
            }
        };
        let mut lines = Vec::new();
        for id in db.db().get_all_ids() {
            let Some(payload) = db.db().get_payload(id) else {
                continue;
            };
            match payload.get("_memory_type").and_then(|v| v.as_str()) {
                Some("cognitive") => {
                    let insight = payload
                        .get("insight")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let context = payload
                        .get("context")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !insight.is_empty() {
                        lines.push(format!("- node: {insight} (context: {context})"));
                    }
                }
                Some("cognitive_edge") => {
                    let from = payload
                        .get("from_entity")
                        .and_then(|v| v.as_str())
                        .or_else(|| payload.get("from").and_then(|v| v.as_str()))
                        .unwrap_or("");
                    let to = payload
                        .get("to_entity")
                        .and_then(|v| v.as_str())
                        .or_else(|| payload.get("to").and_then(|v| v.as_str()))
                        .unwrap_or("");
                    let relation = payload
                        .get("relation")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    lines.push(format!("- edge: {from} -> {to} ({relation})"));
                }
                _ => {}
            }
        }
        lines.join("\n")
    }

    fn recent_thought_summaries(&self, limit: usize) -> String {
        let Some(store) = &self.thought_store else {
            return String::new();
        };
        let timeline = match store.recover() {
            Ok(tl) => tl,
            Err(e) => {
                tracing::warn!("cognitive agent: thought_store recover failed: {e}");
                return String::new();
            }
        };
        let mut lines: Vec<String> = timeline
            .groups
            .iter()
            .flat_map(|g| g.contexts.iter())
            .filter(|ctx| ctx.output.is_some())
            .map(|ctx| {
                let input = match &ctx.input {
                    crate::agent::thought::ThinkingInput::User { text } => truncate_text(text, 60),
                    crate::agent::thought::ThinkingInput::PlatformInsight { summary, .. } => {
                        truncate_text(summary, 60)
                    }
                    crate::agent::thought::ThinkingInput::CapabilityResult { summary, .. } => {
                        truncate_text(summary, 60)
                    }
                    crate::agent::thought::ThinkingInput::ModeTrigger { reason, .. } => {
                        truncate_text(reason, 60)
                    }
                    crate::agent::thought::ThinkingInput::LegacyInternal => {
                        "[legacy internal round]".to_string()
                    }
                };
                let output = ctx
                    .output
                    .as_ref()
                    .and_then(|o| o.say.clone().or(o.think.clone()))
                    .map(|s| truncate_text(&s, 100))
                    .unwrap_or_default();
                format!("- 用户: {} → Agent: {}", input, output)
            })
            .collect();
        if lines.len() > limit {
            lines = lines.split_off(lines.len() - limit);
        }
        lines.join("\n")
    }
}

fn truncate_text(s: &str, max_chars: usize) -> String {
    let truncated: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{truncated}…")
    } else {
        truncated
    }
}
