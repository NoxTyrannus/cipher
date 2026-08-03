use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;

use crate::agent::communication::{AttentionRetireBatch, ExperienceFragment};
use crate::data::triviumdb::TriviumDb;
use crate::data::ModelRow;
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest, Message, MessageRole};

#[allow(dead_code)]
pub struct ExperienceMemoryAgent {
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: Option<SecretString>,
    triviumdb: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,
    prompts_dir: Option<PathBuf>,
    inbox_rx: tokio::sync::mpsc::Receiver<AttentionRetireBatch>,
}

impl ExperienceMemoryAgent {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: Option<SecretString>,
        triviumdb: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,
        prompts_dir: Option<PathBuf>,
        inbox_rx: tokio::sync::mpsc::Receiver<AttentionRetireBatch>,
    ) -> Self {
        Self {
            provider,
            model_row,
            api_key,
            triviumdb,
            prompts_dir,
            inbox_rx,
        }
    }

    #[allow(clippy::while_let_loop)]
    pub async fn run(mut self) {
        loop {
            match self.inbox_rx.recv().await {
                Some(batch) => {
                    if let Err(e) = self.process_batch(batch).await {
                        tracing::warn!("experience memory agent failed: {e}");
                    }
                }
                None => break,
            }
        }
        tracing::info!("experience memory agent: inbox closed, exiting");
    }

    async fn process_batch(&self, batch: AttentionRetireBatch) -> crate::common::Result<()> {
        let base = match &self.prompts_dir {
            Some(dir) => read_platform_prompt(dir, "memory_experience.md"),
            None => String::from(
                "Extract experience memories from the following retired attention entries.",
            ),
        };

        if batch.retired_focus.is_empty() {
            return Ok(());
        }

        let focus_list = batch.retired_focus.join("\n- ");
        let prompt = format!(
            "{}\n\n## Retired Attention Entries\n- {}\n\n## Task\nExtract experience memories. Output a JSON array of experience entries, each with \"title\" and \"summary\" fields.\n\nRespond with ONLY the JSON array.",
            base, focus_list,
        );

        let api_key = match &self.api_key {
            Some(k) => k.clone(),
            None => return Ok(()),
        };

        let messages = vec![
            Message {
                role: MessageRole::System,
                content: prompt,
            },
            Message {
                role: MessageRole::User,
                content: "Extract experience memories now. Output ONLY the JSON.".to_string(),
            },
        ];

        let req = LlmRequest::from_model_row(&self.model_row, messages, api_key)?;
        let resp = self.provider.call(&req).await?;

        if let Ok(fragments) = serde_json::from_str::<Vec<ExperienceFragment>>(&resp.content) {
            if !fragments.is_empty() {
                if let Some(ref db) = self.triviumdb {
                    let mut db = db.lock().await;
                    for fragment in &fragments {
                        let mut payload = match serde_json::to_value(fragment) {
                            Ok(serde_json::Value::Object(p)) => p,
                            _ => continue,
                        };
                        payload.insert(
                            "_memory_type".to_string(),
                            serde_json::Value::String("experience".to_string()),
                        );
                        let zero_vec = vec![0.0_f32; db.db().dim()];
                        let _ = db
                            .db_mut()
                            .insert(&zero_vec, serde_json::Value::Object(payload));
                    }
                    let _ = db.flush();
                }
            }
        }

        Ok(())
    }
}
