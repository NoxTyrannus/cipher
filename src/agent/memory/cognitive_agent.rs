use std::path::PathBuf;

const MAX_DYNAMIC_COGNITIVE_NODES: usize = 1000;
use std::sync::Arc;

use secrecy::SecretString;

use serde_json::Value;

use crate::agent::communication::CognitiveFragment;
use crate::data::triviumdb::TriviumDb;
use crate::data::ModelRow;
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest};

#[allow(dead_code)]
pub struct CognitiveAgent {
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: Option<SecretString>,
    triviumdb: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,
    prompts_dir: Option<PathBuf>,
    instance_counter: u64,
    inbox_rx: tokio::sync::mpsc::Receiver<()>,

    memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,

    thought_store: Option<Arc<crate::data::thought_store::ThoughtStore>>,
}

impl CognitiveAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: Option<SecretString>,
        triviumdb: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,
        prompts_dir: Option<PathBuf>,
        inbox_rx: tokio::sync::mpsc::Receiver<()>,
        memory_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
        thought_store: Option<Arc<crate::data::thought_store::ThoughtStore>>,
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
            "{}\n\n## Task\nUpdate the cognitive graph. Output a JSON array of cognitive entries, each with \"entity\", \"relation\", and \"target\" fields.\n\nRespond with ONLY the JSON array.",
            base,
        );

        let summaries = self.recent_thought_summaries(29);
        let user_content = if summaries.is_empty() {
            "Update cognitive graph now. Output ONLY the JSON.".to_string()
        } else {
            format!(
                "## Recent Thought Summaries\n{}\n\nUpdate cognitive graph based on these. Output ONLY the JSON.",
                summaries
            )
        };

        let messages = vec![
            ChatMessage::System {
                text: prompt,
                kind: SystemKind::Primary,
            },
            ChatMessage::User { text: user_content },
        ];

        let req = LlmRequest::from_model_row(&self.model_row, messages, api_key)?;
        let resp = self.provider.call(&req).await?;

        let output: serde_json::Value =
            serde_json::from_str(&resp.content).unwrap_or(serde_json::Value::Null);

        let (nodes, edges) = match &output {
            Value::Array(_arr) => {
                let fragments: Vec<CognitiveFragment> =
                    serde_json::from_value(output.clone()).unwrap_or_default();
                (fragments, Vec::new())
            }
            Value::Object(obj) => {
                let nodes = obj
                    .get("nodes")
                    .and_then(|v| serde_json::from_value::<Vec<CognitiveFragment>>(v.clone()).ok())
                    .unwrap_or_default();
                let edges = obj
                    .get("edges")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|e| {
                                Some((
                                    e.get("from")?.as_str()?.to_string(),
                                    e.get("to")?.as_str()?.to_string(),
                                    e.get("relation")?.as_str()?.to_string(),
                                ))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                (nodes, edges)
            }
            _ => (Vec::new(), Vec::new()),
        };

        if !nodes.is_empty() || !edges.is_empty() {
            if let Some(ref db) = self.triviumdb {
                let mut db = db.lock().await;
                let zero_vec = vec![0.0_f32; db.db().dim()];

                let dynamic_count = {
                    let mut count = 0usize;
                    for id in db.db().get_all_ids() {
                        let Some(payload) = db.db().get_payload(id) else {
                            continue;
                        };
                        if payload.get("_memory_type").and_then(|v| v.as_str()) != Some("cognitive")
                        {
                            continue;
                        }
                        if payload.get("entity").is_some() {
                            count += 1;
                        }
                    }
                    count
                };
                let dynamic_budget = MAX_DYNAMIC_COGNITIVE_NODES.saturating_sub(dynamic_count);
                if dynamic_budget == 0 {
                    tracing::warn!(
                        "cognitive_agent: 动态认知节点达上限 {dynamic_count}/{} — 拒绝本轮新增 \
                         (nodes={}, edges={})",
                        MAX_DYNAMIC_COGNITIVE_NODES,
                        nodes.len(),
                        edges.len()
                    );
                }

                for fragment in nodes.iter().take(dynamic_budget) {
                    let mut payload = match serde_json::to_value(fragment) {
                        Ok(Value::Object(p)) => p,
                        _ => continue,
                    };
                    payload.insert(
                        "_memory_type".to_string(),
                        Value::String("cognitive".to_string()),
                    );
                    let _ = db.db_mut().insert(&zero_vec, Value::Object(payload));
                }

                for (from, to, relation) in &edges {
                    let edge_payload = serde_json::json!({
                        "_memory_type": "cognitive_edge",
                        "from_entity": from,
                        "to_entity": to,
                        "relation": relation,
                    });
                    let _ = db.db_mut().insert(&zero_vec, edge_payload);
                }

                let _ = db.flush();

                if let Some(ref memory_db) = self.memory_db {
                    use crate::agent::memory::memory_version as mv;
                    if let Ok(conn) = memory_db.lock() {
                        let snapshot_ref =
                            format!("trivium://cognitive/{}", crate::common::UtcTimestamp::now());
                        match mv::stage(&conn, mv::MemoryVersionKind::Cognitive, &snapshot_ref, &[])
                        {
                            Ok(vid) => {
                                if let Err(e) = mv::publish(&conn, vid) {
                                    tracing::warn!(
                                        "cognitive agent: publish version {vid} failed: {e}"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!("cognitive agent: stage version failed: {e}")
                            }
                        }
                    }
                }
            }
        }

        Ok(())
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
                    crate::agent::thought::ThinkingInput::PlatformEcho { summary, .. } => {
                        truncate_text(summary, 60)
                    }
                    crate::agent::thought::ThinkingInput::CapabilityResult { summary, .. } => {
                        truncate_text(summary, 60)
                    }
                    crate::agent::thought::ThinkingInput::ModeTrigger { reason, .. } => {
                        truncate_text(reason, 60)
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
