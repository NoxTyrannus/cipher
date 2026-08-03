use super::TriviumDb;
use crate::common::{unix_timestamp_now, AgentError, Result};

pub fn insert_raw_file_node(
    db: &mut TriviumDb,
    node_id: &str,
    path: &str,
    mime: &str,
    size: u64,
    source: &str,
) -> Result<u64> {
    let payload = serde_json::json!({
        "_memory_type": "raw_files",
        "node_id": node_id,
        "path": path,
        "mime": mime,
        "size": size,
        "source": source,
    });

    let zero_vec = vec![0.0_f32; db.db().dim()];
    let id = db
        .db_mut()
        .insert(&zero_vec, payload)
        .map_err(|e| AgentError::Bootstrap(format!("insert raw_file {}: {}", node_id, e)))?;
    Ok(id)
}

pub fn insert_attention_table_node(
    db: &mut TriviumDb,
    focus: &str,
    doc_node_id: u64,
) -> Result<u64> {
    let payload = serde_json::json!({
        "_memory_type": "attention",
        "node_type": "table",
        "focus": focus,
        "doc_node_id": doc_node_id,
        "ts": unix_timestamp_now(),
    });
    let zero_vec = vec![0.0_f32; db.db().dim()];
    let id = db
        .db_mut()
        .insert(&zero_vec, payload)
        .map_err(|e| AgentError::Bootstrap(format!("insert attention table node: {}", e)))?;
    Ok(id)
}

pub fn insert_attention_doc_node(
    db: &mut TriviumDb,
    filename: &str,
    content: &str,
    source_turns: &[u64],
) -> Result<u64> {
    let turns_json: Vec<serde_json::Value> =
        source_turns.iter().map(|t| serde_json::json!(t)).collect();
    let payload = serde_json::json!({
        "_memory_type": "attention",
        "node_type": "doc",
        "filename": filename,
        "content": content,
        "source_turns": turns_json,
    });
    let zero_vec = vec![0.0_f32; db.db().dim()];
    let id = db
        .db_mut()
        .insert(&zero_vec, payload)
        .map_err(|e| AgentError::Bootstrap(format!("insert attention doc node: {}", e)))?;
    Ok(id)
}

pub fn insert_experience_node(db: &mut TriviumDb, summary: &str, outcome: &str) -> Result<u64> {
    let payload = serde_json::json!({
        "_memory_type": "experience",
        "summary": summary,
        "outcome": outcome,
        "ts": unix_timestamp_now(),
    });
    let zero_vec = vec![0.0_f32; db.db().dim()];
    let id = db
        .db_mut()
        .insert(&zero_vec, payload)
        .map_err(|e| AgentError::Bootstrap(format!("insert experience node: {}", e)))?;
    Ok(id)
}

pub fn insert_preference_node(db: &mut TriviumDb, key: &str, value: &str) -> Result<u64> {
    let payload = serde_json::json!({
        "_memory_type": "preference",
        "key": key,
        "value": value,
    });
    let zero_vec = vec![0.0_f32; db.db().dim()];
    let id = db
        .db_mut()
        .insert(&zero_vec, payload)
        .map_err(|e| AgentError::Bootstrap(format!("insert preference node: {}", e)))?;
    Ok(id)
}

pub fn insert_cognitive_node(db: &mut TriviumDb, insight: &str, context: &str) -> Result<u64> {
    let payload = serde_json::json!({
        "_memory_type": "cognitive",
        "insight": insight,
        "context": context,
        "ts": unix_timestamp_now(),
    });
    let zero_vec = vec![0.0_f32; db.db().dim()];
    let id = db
        .db_mut()
        .insert(&zero_vec, payload)
        .map_err(|e| AgentError::Bootstrap(format!("insert cognitive node: {}", e)))?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn insert_and_read_raw_file_node() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.trivium");
        let mut db = TriviumDb::open(&db_path, 128).unwrap();

        let id = insert_raw_file_node(
            &mut db,
            "conv:sess1:turn_1",
            "/home/user/.cipher/conversations/sess1/turn_1.md",
            "text/markdown",
            1234,
            "conversation",
        )
        .unwrap();

        let payload = db.db().get_payload(id).unwrap();
        assert_eq!(payload["_memory_type"], "raw_files");
        assert_eq!(payload["source"], "conversation");
        assert_eq!(payload["mime"], "text/markdown");
        assert_eq!(payload["size"], 1234);
    }

    #[test]
    fn insert_attention_dual_node() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.trivium");
        let mut db = TriviumDb::open(&db_path, 128).unwrap();

        let doc_id = insert_attention_doc_node(
            &mut db,
            "stage2 memory design",
            "Full context about stage 2 memory platform design discussion",
            &[1, 2, 3],
        )
        .unwrap();

        let doc_payload = db.db().get_payload(doc_id).unwrap();
        assert_eq!(doc_payload["_memory_type"], "attention");
        assert_eq!(doc_payload["node_type"], "doc");
        assert_eq!(doc_payload["filename"], "stage2 memory design");
        assert_eq!(doc_payload["source_turns"][0], 1);
        assert_eq!(doc_payload["source_turns"][1], 2);
        assert_eq!(doc_payload["source_turns"][2], 3);

        let table_id =
            insert_attention_table_node(&mut db, "stage2 memory design", doc_id).unwrap();

        let table_payload = db.db().get_payload(table_id).unwrap();
        assert_eq!(table_payload["_memory_type"], "attention");
        assert_eq!(table_payload["node_type"], "table");
        assert_eq!(table_payload["focus"], "stage2 memory design");
        assert_eq!(table_payload["doc_node_id"], doc_id as u64);
    }

    #[test]
    fn insert_experience_node_roundtrip() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.trivium");
        let mut db = TriviumDb::open(&db_path, 128).unwrap();

        let id = insert_experience_node(&mut db, "Test experience", "success").unwrap();
        let payload = db.db().get_payload(id).unwrap();
        assert_eq!(payload["_memory_type"], "experience");
        assert_eq!(payload["summary"], "Test experience");
        assert_eq!(payload["outcome"], "success");
    }

    #[test]
    fn insert_preference_node_roundtrip() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.trivium");
        let mut db = TriviumDb::open(&db_path, 128).unwrap();

        let id = insert_preference_node(&mut db, "theme", "dark").unwrap();
        let payload = db.db().get_payload(id).unwrap();
        assert_eq!(payload["_memory_type"], "preference");
        assert_eq!(payload["key"], "theme");
        assert_eq!(payload["value"], "dark");
    }

    #[test]
    fn insert_cognitive_node_roundtrip() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.trivium");
        let mut db = TriviumDb::open(&db_path, 128).unwrap();

        let id =
            insert_cognitive_node(&mut db, "Key insight", "Context about the insight").unwrap();
        let payload = db.db().get_payload(id).unwrap();
        assert_eq!(payload["_memory_type"], "cognitive");
        assert_eq!(payload["insight"], "Key insight");
        assert_eq!(payload["context"], "Context about the insight");
    }

    #[test]
    fn all_memory_types_have_distinct_type_field() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.trivium");
        let mut db = TriviumDb::open(&db_path, 128).unwrap();

        let raw_id = insert_raw_file_node(&mut db, "n1", "/p", "text/plain", 10, "conv").unwrap();
        let att_id = insert_attention_doc_node(&mut db, "f", "c", &[]).unwrap();
        let exp_id = insert_experience_node(&mut db, "s", "o").unwrap();
        let pre_id = insert_preference_node(&mut db, "k", "v").unwrap();
        let cog_id = insert_cognitive_node(&mut db, "i", "c").unwrap();

        assert_eq!(
            db.db().get_payload(raw_id).unwrap()["_memory_type"],
            "raw_files"
        );
        assert_eq!(
            db.db().get_payload(att_id).unwrap()["_memory_type"],
            "attention"
        );
        assert_eq!(
            db.db().get_payload(exp_id).unwrap()["_memory_type"],
            "experience"
        );
        assert_eq!(
            db.db().get_payload(pre_id).unwrap()["_memory_type"],
            "preference"
        );
        assert_eq!(
            db.db().get_payload(cog_id).unwrap()["_memory_type"],
            "cognitive"
        );
    }
}
