use crate::common::AgentError;
use crate::common::UtcTimestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryVersionKind {
    Attention,
    Cognitive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryVersionState {
    Staging,
    Active,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryVersion {
    pub version_id: u64,
    pub kind: MemoryVersionKind,
    pub state: MemoryVersionState,
    pub processed_through_ts: Option<String>,
    pub processed_through_ids: Option<String>,
    pub snapshot_ref: String,
    pub delta_refs: Vec<String>,
    pub source_thought_ids: Vec<String>,
    pub created_at: String,
    pub activated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCursor {
    pub kind: MemoryVersionKind,
    pub processed_through_ts: Option<String>,
    pub processed_through_ids: Option<String>,
}

fn kind_to_string(kind: MemoryVersionKind) -> &'static str {
    match kind {
        MemoryVersionKind::Attention => "attention",
        MemoryVersionKind::Cognitive => "cognitive",
    }
}

fn kind_from_string(s: &str) -> Result<MemoryVersionKind, AgentError> {
    match s {
        "attention" => Ok(MemoryVersionKind::Attention),
        "cognitive" => Ok(MemoryVersionKind::Cognitive),
        other => Err(AgentError::Bootstrap(format!(
            "unknown MemoryVersionKind: '{other}'"
        ))),
    }
}

fn state_from_string(s: &str) -> Result<MemoryVersionState, AgentError> {
    match s {
        "staging" => Ok(MemoryVersionState::Staging),
        "active" => Ok(MemoryVersionState::Active),
        "rejected" => Ok(MemoryVersionState::Rejected),
        other => Err(AgentError::Bootstrap(format!(
            "unknown MemoryVersionState: '{other}'"
        ))),
    }
}

type RawVersion = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    String,
    Option<String>,
);

fn read_version_row(row: &duckdb::Row) -> duckdb::Result<RawVersion> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn raw_to_version(raw: RawVersion) -> Result<MemoryVersion, AgentError> {
    Ok(MemoryVersion {
        version_id: raw.0 as u64,
        kind: kind_from_string(&raw.1)?,
        state: state_from_string(&raw.2)?,
        processed_through_ts: raw.3,
        processed_through_ids: raw.4,
        snapshot_ref: raw.5,
        delta_refs: serde_json::from_str(&raw.6).unwrap_or_default(),
        source_thought_ids: serde_json::from_str(&raw.7).unwrap_or_default(),
        created_at: raw.8,
        activated_at: raw.9,
    })
}

pub fn create_memory_version_tables(conn: &duckdb::Connection) -> Result<(), AgentError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_version (
            version_id BIGINT PRIMARY KEY,
            kind TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'staging',
            processed_through_ts TEXT,
            processed_through_ids TEXT,
            snapshot_ref TEXT NOT NULL,
            delta_refs TEXT NOT NULL DEFAULT '[]',
            source_thought_ids TEXT NOT NULL DEFAULT '[]',
            created_at TEXT NOT NULL,
            activated_at TEXT
        );
        CREATE TABLE IF NOT EXISTS memory_cursor (
            kind TEXT PRIMARY KEY,
            processed_through_ts TEXT,
            processed_through_ids TEXT,
            cognitive_instance_count INTEGER DEFAULT 0
        );",
    )
    .map_err(|e| AgentError::Bootstrap(format!("create memory version tables: {e}")))?;
    Ok(())
}

pub fn stage(
    conn: &duckdb::Connection,
    kind: MemoryVersionKind,
    snapshot_ref: &str,
    source_thought_ids: &[String],
) -> Result<u64, AgentError> {
    let kind_str = kind_to_string(kind);
    let source_json = serde_json::to_string(source_thought_ids)
        .map_err(|e| AgentError::Bootstrap(format!("stage serialize source: {e}")))?;
    let now = UtcTimestamp::now().to_string();

    let mut stmt = conn
        .prepare("SELECT COALESCE(MAX(version_id), 0) + 1 FROM memory_version WHERE kind=?")
        .map_err(|e| AgentError::Bootstrap(format!("stage prepare max: {e}")))?;
    let version_id: i64 = stmt
        .query_row(duckdb::params![kind_str], |row| row.get(0))
        .map_err(|e| AgentError::Bootstrap(format!("stage query max: {e}")))?;

    conn.execute(
        "INSERT INTO memory_version (version_id, kind, state, snapshot_ref, source_thought_ids, created_at) VALUES (?, ?, 'staging', ?, ?, ?)",
        duckdb::params![version_id, kind_str, snapshot_ref, source_json, now],
    )
    .map_err(|e| AgentError::Bootstrap(format!("stage insert: {e}")))?;

    Ok(version_id as u64)
}

pub fn publish(conn: &duckdb::Connection, version_id: u64) -> Result<(), AgentError> {
    let mut stmt = conn
        .prepare(
            "SELECT kind, processed_through_ts, processed_through_ids \
             FROM memory_version WHERE version_id=?",
        )
        .map_err(|e| AgentError::Bootstrap(format!("publish prepare: {e}")))?;
    let (kind, ts, ids): (String, Option<String>, Option<String>) = stmt
        .query_row(duckdb::params![version_id as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|e| AgentError::Bootstrap(format!("publish query version: {e}")))?;

    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(|e| AgentError::Bootstrap(format!("publish begin: {e}")))?;

    let now = UtcTimestamp::now().to_string();
    conn.execute(
        "UPDATE memory_version SET state='active', activated_at=? WHERE version_id=?",
        duckdb::params![now, version_id as i64],
    )
    .map_err(|e| AgentError::Bootstrap(format!("publish update: {e}")))?;

    conn.execute(
        "INSERT OR REPLACE INTO memory_cursor (kind, processed_through_ts, processed_through_ids) VALUES (?, ?, ?)",
        duckdb::params![kind, ts, ids],
    )
    .map_err(|e| AgentError::Bootstrap(format!("publish upsert cursor: {e}")))?;

    conn.execute(
        "DELETE FROM memory_version WHERE kind=? AND version_id NOT IN ( \
            SELECT version_id FROM memory_version WHERE kind=? \
            ORDER BY version_id DESC LIMIT 7 \
        )",
        duckdb::params![kind, kind],
    )
    .map_err(|e| AgentError::Bootstrap(format!("publish prune old versions: {e}")))?;

    conn.execute_batch("COMMIT")
        .map_err(|e| AgentError::Bootstrap(format!("publish commit: {e}")))?;

    Ok(())
}

pub fn reject(conn: &duckdb::Connection, version_id: u64) -> Result<(), AgentError> {
    let affected = conn
        .execute(
            "UPDATE memory_version SET state='rejected' WHERE version_id=?",
            duckdb::params![version_id as i64],
        )
        .map_err(|e| AgentError::Bootstrap(format!("reject: {e}")))?;
    if affected == 0 {
        return Err(AgentError::Bootstrap(format!(
            "version_id {version_id} not found for rejection"
        )));
    }
    Ok(())
}

pub fn get_active(
    conn: &duckdb::Connection,
    kind: MemoryVersionKind,
) -> Result<Option<MemoryVersion>, AgentError> {
    let kind_str = kind_to_string(kind);
    let mut stmt = conn
        .prepare(
            "SELECT version_id, kind, state, processed_through_ts, processed_through_ids, \
             snapshot_ref, delta_refs, source_thought_ids, created_at, activated_at \
             FROM memory_version WHERE kind=? AND state='active' \
             ORDER BY version_id DESC LIMIT 1",
        )
        .map_err(|e| AgentError::Bootstrap(format!("get_active prepare: {e}")))?;

    match stmt.query_row(duckdb::params![kind_str], read_version_row) {
        Ok(raw) => Ok(Some(raw_to_version(raw)?)),
        Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AgentError::Bootstrap(format!("get_active query: {e}"))),
    }
}

pub fn get_staging(
    conn: &duckdb::Connection,
    kind: MemoryVersionKind,
) -> Result<Option<MemoryVersion>, AgentError> {
    let kind_str = kind_to_string(kind);
    let mut stmt = conn
        .prepare(
            "SELECT version_id, kind, state, processed_through_ts, processed_through_ids, \
             snapshot_ref, delta_refs, source_thought_ids, created_at, activated_at \
             FROM memory_version WHERE kind=? AND state='staging' \
             ORDER BY version_id DESC LIMIT 1",
        )
        .map_err(|e| AgentError::Bootstrap(format!("get_staging prepare: {e}")))?;

    match stmt.query_row(duckdb::params![kind_str], read_version_row) {
        Ok(raw) => Ok(Some(raw_to_version(raw)?)),
        Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AgentError::Bootstrap(format!("get_staging query: {e}"))),
    }
}

pub fn get_cursor(
    conn: &duckdb::Connection,
    kind: MemoryVersionKind,
) -> Result<Option<MemoryCursor>, AgentError> {
    let kind_str = kind_to_string(kind);
    let mut stmt = conn
        .prepare(
            "SELECT kind, processed_through_ts, processed_through_ids \
             FROM memory_cursor WHERE kind=?",
        )
        .map_err(|e| AgentError::Bootstrap(format!("get_cursor prepare: {e}")))?;

    match stmt.query_row(duckdb::params![kind_str], |row| {
        let k: String = row.get(0)?;
        Ok((
            k,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    }) {
        Ok((k, ts, ids)) => Ok(Some(MemoryCursor {
            kind: kind_from_string(&k)?,
            processed_through_ts: ts,
            processed_through_ids: ids,
        })),
        Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AgentError::Bootstrap(format!("get_cursor query: {e}"))),
    }
}

pub fn advance_cursor(
    conn: &duckdb::Connection,
    kind: MemoryVersionKind,
    ts: &str,
    ids_json: &str,
) -> Result<(), AgentError> {
    let kind_str = kind_to_string(kind);
    conn.execute(
        "INSERT OR REPLACE INTO memory_cursor (kind, processed_through_ts, processed_through_ids) \
         VALUES (?, ?, ?)",
        duckdb::params![kind_str, ts, ids_json],
    )
    .map_err(|e| AgentError::Bootstrap(format!("advance_cursor: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> duckdb::Connection {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        create_memory_version_tables(&conn).unwrap();
        conn
    }

    fn verify_table_exists(conn: &duckdb::Connection, name: &str) -> bool {
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_name = ?",
            )
            .unwrap();
        let count: i64 = stmt
            .query_row(duckdb::params![name], |row| row.get(0))
            .unwrap();
        count == 1
    }

    #[test]
    fn test_create_tables() {
        let conn = setup();
        assert!(verify_table_exists(&conn, "memory_version"));
        assert!(verify_table_exists(&conn, "memory_cursor"));
    }

    #[test]
    fn test_stage_returns_version_id() {
        let conn = setup();
        let vid = stage(&conn, MemoryVersionKind::Attention, "snap1", &[]).unwrap();
        assert!(vid > 0);
    }

    #[test]
    fn test_get_staging_returns_staged() {
        let conn = setup();
        stage(
            &conn,
            MemoryVersionKind::Cognitive,
            "snap2",
            &["tid-1".to_string()],
        )
        .unwrap();
        let result = get_staging(&conn, MemoryVersionKind::Cognitive).unwrap();
        assert!(result.is_some());
        let v = result.unwrap();
        assert_eq!(v.kind, MemoryVersionKind::Cognitive);
        assert_eq!(v.state, MemoryVersionState::Staging);
        assert_eq!(v.snapshot_ref, "snap2");
        assert_eq!(v.source_thought_ids, vec!["tid-1".to_string()]);
    }

    #[test]
    fn test_publish_activates_version() {
        let conn = setup();
        let vid = stage(&conn, MemoryVersionKind::Attention, "snap3", &[]).unwrap();
        publish(&conn, vid).unwrap();
        let active = get_active(&conn, MemoryVersionKind::Attention).unwrap();
        assert!(active.is_some());
        let v = active.unwrap();
        assert_eq!(v.state, MemoryVersionState::Active);
        assert!(v.activated_at.is_some());
    }

    #[test]
    fn test_cursor_advances_on_publish() {
        let conn = setup();
        let vid = stage(&conn, MemoryVersionKind::Attention, "snap4", &[]).unwrap();
        assert!(get_cursor(&conn, MemoryVersionKind::Attention)
            .unwrap()
            .is_none());
        publish(&conn, vid).unwrap();
        let cursor = get_cursor(&conn, MemoryVersionKind::Attention).unwrap();
        assert!(cursor.is_some());
    }

    #[test]
    fn test_reject_marks_rejected() {
        let conn = setup();
        let vid = stage(&conn, MemoryVersionKind::Attention, "snap5", &[]).unwrap();
        reject(&conn, vid).unwrap();
        assert!(get_active(&conn, MemoryVersionKind::Attention)
            .unwrap()
            .is_none());
        assert!(get_staging(&conn, MemoryVersionKind::Attention)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_get_active_prefers_latest() {
        let conn = setup();
        let v1 = stage(&conn, MemoryVersionKind::Cognitive, "snap6", &[]).unwrap();
        publish(&conn, v1).unwrap();
        let v2 = stage(&conn, MemoryVersionKind::Cognitive, "snap7", &[]).unwrap();
        let active = get_active(&conn, MemoryVersionKind::Cognitive).unwrap();
        assert!(active.is_some());
        assert_eq!(active.unwrap().version_id, v1);
        let _ = v2;
    }

    #[test]
    fn test_stage_increments_version_id() {
        let conn = setup();
        let v1 = stage(&conn, MemoryVersionKind::Attention, "snap8", &[]).unwrap();
        let v2 = stage(&conn, MemoryVersionKind::Attention, "snap9", &[]).unwrap();
        assert!(v2 > v1);
    }

    #[test]
    fn test_reject_nonexistent() {
        let conn = setup();
        let result = reject(&conn, 99999);
        assert!(result.is_err());
    }

    #[test]
    fn test_get_staging_empty() {
        let conn = setup();
        let result = get_staging(&conn, MemoryVersionKind::Cognitive).unwrap();
        assert!(result.is_none());
    }
}
