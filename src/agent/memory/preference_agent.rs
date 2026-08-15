use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;

use crate::agent::communication::{AttentionRetireBatch, PreferenceFragment};
use crate::data::duckdb::Registry;
use crate::data::triviumdb::TriviumDb;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::capability::service::{CapabilityCall, CapabilityService};
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest};

pub struct PreferenceMemoryAgent {
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: Option<SecretString>,
    triviumdb: Option<Arc<std::sync::Mutex<TriviumDb>>>,
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
        triviumdb: Option<Arc<std::sync::Mutex<TriviumDb>>>,
        prompts_dir: Option<PathBuf>,
        inbox_rx: tokio::sync::mpsc::Receiver<AttentionRetireBatch>,
        registry: Option<Registry>,
        executor: Option<Arc<CapabilityExecutor>>,
    ) -> Self {
        Self {
            provider,
            model_row,
            api_key,
            triviumdb,
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

    async fn process_batch(&self, batch: AttentionRetireBatch) -> crate::common::Result<()> {
        let base = match &self.prompts_dir {
            Some(dir) => read_platform_prompt(dir, "memory_preference.md"),
            None => String::from(
                "Extract preference memories from the following retired attention entries.",
            ),
        };

        if batch.retired_focus.is_empty() {
            return Ok(());
        }

        let evidence = self.collect_evidence(&batch).await;
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
        let prompt = format!(
            "{}\n\n## Retired Attention Entries\n{}\n\n## Original Evidence\n{}\n\n## Task\nExtract preference memories. Output a JSON array of preference entries, each with \"key\" and \"value\" fields.\n\nRespond with ONLY the JSON array.",
            base, focus_list, evidence,
        );

        let api_key = match &self.api_key {
            Some(k) => k.clone(),
            None => return Ok(()),
        };

        let messages = vec![
            ChatMessage::System {
                text: prompt,
                kind: SystemKind::Primary,
            },
            ChatMessage::User {
                text: "Extract preference memories now. Output ONLY the JSON.".to_string(),
            },
        ];

        let req = LlmRequest::from_model_row(&self.model_row, messages, api_key)?;
        let resp = self.provider.call(&req).await?;

        let mut fragments: Vec<PreferenceFragment> =
            serde_json::from_str(&resp.content).unwrap_or_default();
        for (index, fragment) in fragments.iter_mut().enumerate() {
            if fragment.source_refs.is_empty() {
                fragment.source_refs = batch.source_refs.get(index).cloned().unwrap_or_default();
            }
        }
        if fragments.is_empty() {
            return Ok(());
        }

        if let (Some(registry), Some(executor)) = (&self.registry, &self.executor) {
            let args = serde_json::json!({
                "entries": fragments
                    .iter()
                    .map(|f| serde_json::json!({
                        "key": f.key,
                        "value": f.value,
                        "source_refs": f.source_refs,
                    }))
                    .collect::<Vec<_>>()
            });
            let call = CapabilityCall {
                capability_id: "memory.preference.write".to_string(),
                capability_name: "Write Preference Memory".to_string(),
                arguments: args,
            };
            CapabilityService::new(registry, executor)?
                .execute_for_agent("preference-agent", &call)?;
            return Ok(());
        }

        if let Some(ref db) = self.triviumdb {
            let mut db = db.lock().map_err(|e| {
                crate::common::AgentError::Io(format!("preference trivium lock poisoned: {e}"))
            })?;
            for fragment in &fragments {
                let mut payload = match serde_json::to_value(fragment) {
                    Ok(serde_json::Value::Object(p)) => p,
                    _ => continue,
                };
                payload.insert(
                    "_memory_type".to_string(),
                    serde_json::Value::String("preference".to_string()),
                );
                let zero_vec = vec![0.0_f32; db.db().dim()];
                let _ = db
                    .db_mut()
                    .insert(&zero_vec, serde_json::Value::Object(payload));
            }
            let _ = db.flush();
        }

        Ok(())
    }

    async fn collect_evidence(&self, batch: &AttentionRetireBatch) -> String {
        let (Some(registry), Some(executor)) = (&self.registry, &self.executor) else {
            return "No evidence runtime configured.".to_string();
        };
        let mut parts = Vec::new();
        for (focus, refs) in batch.retired_focus.iter().zip(batch.source_refs.iter()) {
            if refs.is_empty() {
                continue;
            }
            let call = CapabilityCall {
                capability_id: "memory.evidence.lookup".to_string(),
                capability_name: "Lookup Memory Evidence".to_string(),
                arguments: serde_json::json!({"source_refs": refs}),
            };
            match CapabilityService::new(registry, executor)
                .and_then(|service| service.execute_for_agent("preference-agent", &call))
            {
                Ok(result) => {
                    parts.push(format!("## Evidence for {focus}\n{}", result.output));
                }
                Err(e) => {
                    tracing::warn!("preference agent evidence lookup failed for {focus}: {e}");
                }
            }
        }
        if parts.is_empty() {
            "No original evidence available.".to_string()
        } else {
            parts.join("\n\n")
        }
    }
}
