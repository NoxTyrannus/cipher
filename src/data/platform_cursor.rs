use crate::common::types::ThoughtId;
use crate::common::{AgentError, Result, UtcTimestamp};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const CURSORS_DIR: &str = "cursors";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformCursor {
    pub platform: String,
    pub processed_through_ts: UtcTimestamp,
    #[serde(default)]
    pub processed_through_ids: BTreeSet<String>,
    pub updated_at: UtcTimestamp,
}

impl PlatformCursor {
    pub fn new(platform: &str) -> Self {
        Self {
            platform: platform.to_string(),
            processed_through_ts: UtcTimestamp::parse("1970-01-01T00:00:00.000000000Z")
                .expect("epoch is valid"),
            processed_through_ids: BTreeSet::new(),
            updated_at: UtcTimestamp::now(),
        }
    }

    pub fn should_process(&self, thought_id: &ThoughtId, occurred_at: &UtcTimestamp) -> bool {
        if occurred_at > &self.processed_through_ts {
            return true;
        }
        if occurred_at == &self.processed_through_ts {
            return !self.processed_through_ids.contains(&thought_id.to_string());
        }
        false
    }

    pub fn advance(&mut self, max_ts: &UtcTimestamp, processed_ids: &[ThoughtId]) {
        if max_ts > &self.processed_through_ts {
            self.processed_through_ts = max_ts.clone();
            self.processed_through_ids = processed_ids.iter().map(|id| id.to_string()).collect();
        } else if max_ts == &self.processed_through_ts {
            for id in processed_ids {
                self.processed_through_ids.insert(id.to_string());
            }
        }
        self.updated_at = UtcTimestamp::now();
    }
}

pub struct CursorStore {
    path: PathBuf,
}

impl CursorStore {
    pub fn open(data_dir: &Path, platform: &str) -> Result<Self> {
        let dir = data_dir.join(CURSORS_DIR);
        crate::data::permissions::ensure_private_directory(&dir)?;
        let path = dir.join(format!("{platform}.json"));
        Ok(Self { path })
    }

    pub fn load(&self, platform: &str) -> Result<PlatformCursor> {
        if !self.path.exists() {
            return Ok(PlatformCursor::new(platform));
        }
        let content = fs::read_to_string(&self.path)
            .map_err(|e| AgentError::Io(format!("read cursor {}: {e}", self.path.display())))?;
        let cursor: PlatformCursor = serde_json::from_str(&content)
            .map_err(|e| AgentError::Parse(format!("parse cursor {}: {e}", self.path.display())))?;
        Ok(cursor)
    }

    pub fn save(&self, cursor: &PlatformCursor) -> Result<()> {
        let content = serde_json::to_string_pretty(cursor)
            .map_err(|e| AgentError::Parse(format!("serialize cursor: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, content.as_bytes())
            .map_err(|e| AgentError::Io(format!("write cursor tmp {}: {e}", tmp.display())))?;
        crate::data::permissions::secure_existing_file(&tmp)?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| AgentError::Io(format!("rename cursor {}: {e}", self.path.display())))?;
        crate::data::permissions::secure_existing_file(&self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cursor_processes_everything() {
        let cursor = PlatformCursor::new("execution");
        let ts = UtcTimestamp::now();
        let id = ThoughtId::new();
        assert!(cursor.should_process(&id, &ts));
    }

    #[test]
    fn cursor_skips_already_processed() {
        let ts = UtcTimestamp::parse("2026-07-24T10:00:00.000000000Z").unwrap();
        let id = ThoughtId::new();
        let mut cursor = PlatformCursor::new("execution");
        cursor.advance(&ts, std::slice::from_ref(&id));

        assert!(!cursor.should_process(&id, &ts));
    }

    #[test]
    fn cursor_handles_same_timestamp_different_id() {
        let ts = UtcTimestamp::parse("2026-07-24T10:00:00.000000000Z").unwrap();
        let id1 = ThoughtId::new();
        let id2 = ThoughtId::new();
        let mut cursor = PlatformCursor::new("execution");

        cursor.advance(&ts, std::slice::from_ref(&id1));
        assert!(!cursor.should_process(&id1, &ts));
        assert!(cursor.should_process(&id2, &ts));

        cursor.advance(&ts, std::slice::from_ref(&id2));
        assert!(!cursor.should_process(&id2, &ts));
    }

    #[test]
    fn cursor_advances_to_newer_timestamp_resets_ids() {
        let ts1 = UtcTimestamp::parse("2026-07-24T10:00:00.000000000Z").unwrap();
        let ts2 = UtcTimestamp::parse("2026-07-24T11:00:00.000000000Z").unwrap();
        let id1 = ThoughtId::new();
        let mut cursor = PlatformCursor::new("execution");

        cursor.advance(&ts1, std::slice::from_ref(&id1));
        assert!(!cursor.should_process(&id1, &ts1));

        let id2 = ThoughtId::new();
        cursor.advance(&ts2, std::slice::from_ref(&id2));
        assert!(cursor.processed_through_ids.len() == 1);
        assert!(!cursor.processed_through_ids.contains(&id1.to_string()));
    }

    #[test]
    fn cursor_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CursorStore::open(dir.path(), "execution").unwrap();

        let loaded = store.load("execution").unwrap();
        assert!(loaded.processed_through_ids.is_empty());

        let ts = UtcTimestamp::now();
        let id = ThoughtId::new();
        let mut cursor = loaded;
        cursor.advance(&ts, std::slice::from_ref(&id));
        store.save(&cursor).unwrap();

        let reloaded = store.load("execution").unwrap();
        assert_eq!(reloaded.processed_through_ts, ts);
        assert!(reloaded.processed_through_ids.contains(&id.to_string()));
        assert!(!reloaded.should_process(&id, &ts));
    }
}
