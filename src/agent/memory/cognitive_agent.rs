use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;

use serde_json::Value;

use crate::data::duckdb::Registry;
use crate::data::triviumdb::TriviumDb;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::capability::service::{CapabilityCall, CapabilityService};
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest};

#[allow(dead_code)]
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

    async fn process_cognitive_update(&self) -> crate::common::Result<()> {
        let base = match &self.prompts_dir {
            Some(dir) => read_platform_prompt(dir, "memory_cognitive.md"),
            None => String::from("Update cognitive graph based on recent thought summaries."),
        };

        let api_key = match &self.api_key {
            Some(k) => k.clone(),
            None => return Ok(()),
        };

        let prompt = format!(
            "{}\n\n## Task\nUpdate the cognitive graph through the capability protocol.\n\n             Output ONLY one JSON object:\n             {{\n  \"nodes\": [{{\"action\": \"upsert|delete\", \"node_id\": \"可选\", \"insight\": \"概念/规则\", \"context\": \"背景\"}}],\n             \"edges\": [{{\"action\": \"upsert|delete\", \"from\": \"节点A\", \"to\": \"节点B\", \"relation\": \"关系\"}}]\n}}\n\n             节点不超过100字，边关系不超过30字。",
            base,
        );

        let summaries = self.recent_thought_summaries(29);
        let current_graph = self.current_cognitive_graph().await;
        let mut user_parts = Vec::new();
        if !summaries.is_empty() {
            user_parts.push(format!("## Recent Thought Summaries\n{summaries}"));
        }
        if !current_graph.is_empty() {
            user_parts.push(format!("## Current Cognitive Graph\n{current_graph}"));
        }
        user_parts.push("Update cognitive graph now. Output ONLY the JSON.".to_string());
        let user_content = user_parts.join("\n\n");

        let messages = vec![
            ChatMessage::System {
                text: prompt,
                kind: SystemKind::Primary,
            },
            ChatMessage::User { text: user_content },
        ];

        let req = LlmRequest::from_model_row(&self.model_row, messages, api_key)?;
        let resp = self.provider.call(&req).await?;

        let output: serde_json::Value = serde_json::from_str(&resp.content).unwrap_or(Value::Null);
        let (node_updates, edge_updates) = cognitive_updates_from_output(output);
        if node_updates.is_empty() && edge_updates.is_empty() {
            return Ok(());
        }

        let (Some(registry), Some(executor)) = (&self.registry, &self.executor) else {
            tracing::warn!("cognitive agent: capability runtime not configured; update skipped");
            return Ok(());
        };

        let args = serde_json::json!({
            "nodes": node_updates,
            "edges": edge_updates,
        });
        let call = CapabilityCall {
            capability_id: "memory.cognitive.update".to_string(),
            capability_name: "Update Cognitive Graph".to_string(),
            arguments: args,
        };
        CapabilityService::new(registry, executor)?.execute_for_agent("cognitive-agent", &call)?;
        tracing::debug!(
            "cognitive agent: capability update committed (nodes={}, edges={})",
            node_updates.len(),
            edge_updates.len()
        );

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

fn cognitive_updates_from_output(output: Value) -> (Vec<Value>, Vec<Value>) {
    match output {
        Value::Array(fragments) => {
            // 兼容旧版 entity/relation/target 三元组：按边导入。
            let mut edges = Vec::new();
            for fragment in fragments {
                let Some(from) = fragment.get("entity").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(relation) = fragment.get("relation").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(to) = fragment.get("target").and_then(|v| v.as_str()) else {
                    continue;
                };
                if from.trim().is_empty() || relation.trim().is_empty() || to.trim().is_empty() {
                    continue;
                }
                edges.push(serde_json::json!({
                    "action": "upsert",
                    "from": from,
                    "relation": relation,
                    "to": to,
                }));
            }
            (Vec::new(), edges)
        }
        Value::Object(obj) => {
            let nodes = obj
                .get("nodes")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let edges = obj
                .get("edges")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            (nodes, edges)
        }
        _ => (Vec::new(), Vec::new()),
    }
}
