use crate::common::{AgentError, Result};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use crate::data::triviumdb::TriviumDb;
use serde::Deserialize;
use std::fs;
use std::path::Path;

const SEED_DIR: &str = "seed/cognitive";

const MAX_SEED_COGNITIVE_NODES: usize = 50;
const SEEDED_MARKER: &str = ".seeded";
const CAPABILITY_SEED_DIR: &str = "seed/capabilities";

pub const COGNITIVE_SEED_MANIFEST: &str = include_str!("../../data/seed/cognitive/manifest.json");
pub const COGNITIVE_SEED_NODES: &str = include_str!("../../data/seed/cognitive/nodes.json");
pub const COGNITIVE_SEED_EDGES: &str = include_str!("../../data/seed/cognitive/edges.json");
pub const CAPABILITY_SEED_BASE: &str =
    include_str!("../../data/seed/capabilities/base_capabilities.json");
pub const CAPABILITY_SEED_COMPOSITE: &str =
    include_str!("../../data/seed/capabilities/composite_capabilities.json");
pub const CAPABILITY_SEED_USAGE: &str =
    include_str!("../../data/seed/capabilities/usage_methods.json");

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    nodes_file: String,
    edges_file: String,
}

#[derive(Debug, Deserialize)]
struct SeedNode {
    node_id: String,
    insight: String,
    context: String,
}

#[derive(Debug, Deserialize)]
struct SeedEdge {
    from: String,
    to: String,
    relation: String,
}

pub fn ensure_default_cognitive_seed(data_dir: &Path) -> Result<()> {
    let seed_root = data_dir.join(SEED_DIR);
    ensure_private_directory(&seed_root)?;

    let files: &[(&str, &str)] = &[
        ("manifest.json", COGNITIVE_SEED_MANIFEST),
        ("nodes.json", COGNITIVE_SEED_NODES),
        ("edges.json", COGNITIVE_SEED_EDGES),
    ];

    for (name, content) in files {
        let path = seed_root.join(name);
        if !path.exists() {
            fs::write(&path, content)
                .map_err(|e| AgentError::Io(format!("write cognitive seed {name}: {e}")))?;
            secure_existing_file(&path)?;
        }
    }

    Ok(())
}

pub fn seed_cognitive_memory(data_dir: &Path, db: &mut TriviumDb) -> Result<()> {
    let seed_root = data_dir.join(SEED_DIR);
    let marker = seed_root.join(SEEDED_MARKER);

    let manifest_path = seed_root.join("manifest.json");
    let manifest_content = fs::read_to_string(&manifest_path)
        .map_err(|e| AgentError::Io(format!("read cognitive manifest: {e}")))?;
    let manifest: Manifest = serde_json::from_str(&manifest_content)
        .map_err(|e| AgentError::Parse(format!("parse cognitive manifest: {e}")))?;

    if let Ok(marker_content) = fs::read_to_string(&marker) {
        if let Ok(seeded_version) = marker_content.trim().parse::<u32>() {
            if seeded_version == manifest.schema_version {
                tracing::debug!(
                    "cognitive seed: already seeded (schema_version={})",
                    manifest.schema_version
                );
                return Ok(());
            }
        }
    }

    tracing::info!(
        "cognitive seed: seeding (schema_version={})",
        manifest.schema_version
    );

    let nodes_path = seed_root.join(&manifest.nodes_file);
    let nodes_content = fs::read_to_string(&nodes_path)
        .map_err(|e| AgentError::Io(format!("read cognitive nodes: {e}")))?;
    let nodes: Vec<SeedNode> = serde_json::from_str(&nodes_content)
        .map_err(|e| AgentError::Parse(format!("parse cognitive nodes: {e}")))?;

    if nodes.len() > MAX_SEED_COGNITIVE_NODES {
        return Err(AgentError::Bootstrap(format!(
            "cognitive seed 节点数 {} 超过上限 {MAX_SEED_COGNITIVE_NODES}",
            nodes.len()
        )));
    }

    for node in &nodes {
        let payload = serde_json::json!({
            "_memory_type": "cognitive",
            "node_id": node.node_id,
            "insight": node.insight,
            "context": node.context,
            "seeded": true,
        });
        let zero_vec = vec![0.0_f32; db.db().dim()];
        db.db_mut().insert(&zero_vec, payload).map_err(|e| {
            AgentError::Bootstrap(format!("seed cognitive node {}: {e}", node.node_id))
        })?;
    }

    let edges_path = seed_root.join(&manifest.edges_file);
    let edges_content = fs::read_to_string(&edges_path)
        .map_err(|e| AgentError::Io(format!("read cognitive edges: {e}")))?;
    let edges: Vec<SeedEdge> = serde_json::from_str(&edges_content)
        .map_err(|e| AgentError::Parse(format!("parse cognitive edges: {e}")))?;

    for edge in &edges {
        let payload = serde_json::json!({
            "_memory_type": "cognitive_edge",
            "from": edge.from,
            "to": edge.to,
            "relation": edge.relation,
            "seeded": true,
        });
        let zero_vec = vec![0.0_f32; db.db().dim()];
        db.db_mut().insert(&zero_vec, payload).map_err(|e| {
            AgentError::Bootstrap(format!(
                "seed cognitive edge {}->{}: {e}",
                edge.from, edge.to
            ))
        })?;
    }

    db.flush()?;

    fs::write(&marker, manifest.schema_version.to_string())
        .map_err(|e| AgentError::Io(format!("write cognitive .seeded: {e}")))?;
    secure_existing_file(&marker)?;

    tracing::info!(
        "cognitive seed: {} nodes + {} edges seeded",
        nodes.len(),
        edges.len()
    );

    Ok(())
}

pub fn ensure_default_capabilities(data_dir: &Path) -> Result<()> {
    let seed_root = data_dir.join(CAPABILITY_SEED_DIR);
    ensure_private_directory(&seed_root)?;

    let files: &[(&str, &str)] = &[
        ("base_capabilities.json", CAPABILITY_SEED_BASE),
        ("composite_capabilities.json", CAPABILITY_SEED_COMPOSITE),
        ("usage_methods.json", CAPABILITY_SEED_USAGE),
    ];

    for (name, content) in files {
        let path = seed_root.join(name);
        let should_write = match fs::read_to_string(&path) {
            Ok(existing) => existing.contains("wasm:"),
            Err(_) => true,
        };
        if should_write {
            fs::write(&path, content)
                .map_err(|e| AgentError::Io(format!("write capability seed {name}: {e}")))?;
            secure_existing_file(&path)?;
        }
    }
    Ok(())
}

pub fn import_factory_defaults(conn: &duckdb::Connection, data_dir: &Path) -> Result<()> {
    let seed_root = data_dir.join(CAPABILITY_SEED_DIR);

    let base_text = fs::read_to_string(seed_root.join("base_capabilities.json"))
        .map_err(|e| AgentError::Io(format!("read base_capabilities.json: {e}")))?;
    let base_rows: Vec<serde_json::Value> = serde_json::from_str(&base_text)
        .map_err(|e| AgentError::Parse(format!("parse base_capabilities.json: {e}")))?;
    let mut capability_ids: Vec<String> = Vec::new();
    for row in &base_rows {
        let id = row["id"].as_str().unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let enabled = row["enabled"].as_bool().unwrap_or(true);
        let metadata = serde_json::json!({
            "partition": row["partition"].as_str().unwrap_or("system"),
        });
        conn.execute(
            "INSERT OR REPLACE INTO base_capability \
             (id, name, type, description, schema_in, schema_out, executor, version, enabled, metadata) \
             VALUES (?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, ?, ?, CAST(? AS JSON))",
            duckdb::params![
                id,
                row["name"].as_str().unwrap_or(id),
                row["type"].as_str().unwrap_or("function"),
                row["description"].as_str().unwrap_or(""),
                row["schema_in"].to_string(),
                row["schema_out"].to_string(),
                row["executor"].as_str().unwrap_or(""),
                row["version"].as_str().unwrap_or(""),
                enabled,
                metadata.to_string(),
            ],
        )
        .map_err(|e| AgentError::Bootstrap(format!("import base_capability {id}: {e}")))?;
        if enabled {
            capability_ids.push(id.to_string());
        }
    }
    let agent_tool_caps: Vec<String> = capability_ids
        .iter()
        .filter(|id| !id.starts_with("memory.") && !id.starts_with("db."))
        .cloned()
        .collect();

    let comp_text = fs::read_to_string(seed_root.join("composite_capabilities.json"))
        .map_err(|e| AgentError::Io(format!("read composite_capabilities.json: {e}")))?;
    let comp_rows: Vec<serde_json::Value> = serde_json::from_str(&comp_text)
        .map_err(|e| AgentError::Parse(format!("parse composite_capabilities.json: {e}")))?;
    for row in &comp_rows {
        let id = row["id"].as_str().unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let enabled = row["enabled"].as_bool().unwrap_or(true);
        let metadata = serde_json::json!({
            "partition": row["partition"].as_str().unwrap_or("system"),
        });
        conn.execute(
            "INSERT OR REPLACE INTO composite_capability \
             (id, name, description, schema_in, schema_out, executor, dag, version, enabled, metadata) \
             VALUES (?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, CAST(? AS JSON), ?, ?, CAST(? AS JSON))",
            duckdb::params![
                id,
                row["name"].as_str().unwrap_or(id),
                row["description"].as_str().unwrap_or(""),
                row["schema_in"].to_string(),
                row["schema_out"].to_string(),
                row["executor"].as_str().unwrap_or("dag"),
                row["dag"].to_string(),
                row["version"].as_str().unwrap_or(""),
                enabled,
                metadata.to_string(),
            ],
        )
        .map_err(|e| AgentError::Bootstrap(format!("import composite_capability {id}: {e}")))?;
    }

    let usage_text = fs::read_to_string(seed_root.join("usage_methods.json"))
        .map_err(|e| AgentError::Io(format!("read usage_methods.json: {e}")))?;
    let usage_rows: Vec<serde_json::Value> = serde_json::from_str(&usage_text)
        .map_err(|e| AgentError::Parse(format!("parse usage_methods.json: {e}")))?;
    for row in &usage_rows {
        let id = row["id"].as_str().unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT OR REPLACE INTO usage_method \
             (id, capability_id, name, prompt, examples, metadata) \
             VALUES (?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON))",
            duckdb::params![
                id,
                row["capability_id"].as_str().unwrap_or(""),
                row["name"].as_str().unwrap_or(id),
                row["prompt"].as_str().unwrap_or(""),
                row["examples"].to_string(),
                row["metadata"].to_string(),
            ],
        )
        .map_err(|e| AgentError::Bootstrap(format!("import usage_method {id}: {e}")))?;
    }

    let caps_json = serde_json::to_string(&agent_tool_caps)
        .map_err(|e| AgentError::Parse(format!("serialize tool_caps: {e}")))?;
    conn.execute(
        "INSERT INTO agent (id, name, mode, tool_caps, is_default) \
         SELECT 'agent', 'Agent', 'unni', CAST(? AS JSON), true \
         WHERE NOT EXISTS (SELECT 1 FROM agent WHERE id = 'agent')",
        duckdb::params![caps_json],
    )
    .map_err(|e| AgentError::Bootstrap(format!("seed agent: {e}")))?;
    conn.execute(
        "UPDATE agent SET tool_caps = CAST(? AS JSON) WHERE id = 'agent'",
        duckdb::params![caps_json],
    )
    .map_err(|e| AgentError::Bootstrap(format!("update agent tool_caps: {e}")))?;
    conn.execute(
        "UPDATE agent SET config = '{\"max_turns\": 6}' \
         WHERE id = 'agent' AND (config IS NULL OR config = 'null')",
        [],
    )
    .map_err(|e| AgentError::Bootstrap(format!("seed agent config: {e}")))?;

    seed_memory_agents(conn)?;

    tracing::info!(
        "import_factory_defaults: {} base + agent tool_caps={}",
        base_rows.len(),
        agent_tool_caps.len()
    );
    Ok(())
}

/// 记忆 agent 全部入表：Desktop 阶段用户可以直接查看/改造/创建这些 agent。
/// 使用 ON CONFLICT DO NOTHING，已有行（含用户手工修改）不被覆盖。
pub fn seed_memory_agents(conn: &duckdb::Connection) -> Result<()> {
    let agents: &[(&str, &str, &[&str])] = &[
        (
            "attention-agent",
            "Attention Agent",
            &[
                "memory.list",
                "memory.retrieve",
                "memory.delete",
                "memory.attention.write",
                "memory.attention.retire",
            ],
        ),
        (
            "experience-agent",
            "Experience Agent",
            &[
                "memory.list",
                "memory.retrieve",
                "memory.evidence.lookup",
                "memory.experience.write",
            ],
        ),
        (
            "preference-agent",
            "Preference Agent",
            &[
                "memory.list",
                "memory.retrieve",
                "memory.evidence.lookup",
                "memory.preference.write",
            ],
        ),
        (
            "cognitive-agent",
            "Cognitive Agent",
            &[
                "memory.list",
                "memory.retrieve",
                "memory.evidence.lookup",
                "memory.cognitive.update",
            ],
        ),
    ];
    for (id, name, caps) in agents {
        let caps_json = serde_json::to_string(caps)
            .map_err(|e| AgentError::Parse(format!("serialize tool_caps for {id}: {e}")))?;
        conn.execute(
            "INSERT INTO agent (id, name, mode, tool_caps, display_name, is_default) \
             VALUES (?, ?, 'unni', CAST(? AS JSON), ?, false) \
             ON CONFLICT (id) DO NOTHING",
            duckdb::params![id, name, caps_json, name],
        )
        .map_err(|e| AgentError::Bootstrap(format!("seed memory agent {id}: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ensure_default_cognitive_seed_writes_files() {
        let dir = tempdir().unwrap();
        ensure_default_cognitive_seed(dir.path()).unwrap();

        let seed_root = dir.path().join(SEED_DIR);
        assert!(seed_root.join("manifest.json").exists());
        assert!(seed_root.join("nodes.json").exists());
        assert!(seed_root.join("edges.json").exists());

        let manifest: Manifest = serde_json::from_str(
            &std::fs::read_to_string(seed_root.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.schema_version, 1);
    }

    #[test]
    fn ensure_default_cognitive_seed_idempotent() {
        let dir = tempdir().unwrap();
        ensure_default_cognitive_seed(dir.path()).unwrap();

        let nodes_path = dir.path().join(SEED_DIR).join("nodes.json");
        std::fs::write(&nodes_path, "[]").unwrap();

        ensure_default_cognitive_seed(dir.path()).unwrap();

        let content = std::fs::read_to_string(&nodes_path).unwrap();
        assert_eq!(content, "[]", "user edit should be preserved");
    }

    #[test]
    fn seed_cognitive_memory_inserts_nodes_and_edges() {
        let dir = tempdir().unwrap();
        ensure_default_cognitive_seed(dir.path()).unwrap();

        let db_path = dir.path().join("cognitive.trivium");
        let mut db = TriviumDb::open(&db_path, 4).unwrap();

        seed_cognitive_memory(dir.path(), &mut db).unwrap();

        let marker = dir.path().join(SEED_DIR).join(SEEDED_MARKER);
        assert!(marker.exists());
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "1");

        let zero_vec = vec![0.0_f32; 4];
        let results = db.db().search(&zero_vec, 100, 0, 0.0).unwrap();
        assert!(
            results.len() >= 29,
            "expected 9 nodes + 20 edges = 29 entries, got {}",
            results.len()
        );

        let mut found_divergence = false;
        let mut found_edge = false;
        for result in &results {
            if let Some(payload) = db.db().get_payload(result.id) {
                if payload.get("node_id").and_then(|v| v.as_str()) == Some("seed-divergence") {
                    found_divergence = true;
                }
                if payload.get("_memory_type").and_then(|v| v.as_str()) == Some("cognitive_edge") {
                    found_edge = true;
                }
            }
        }
        assert!(found_divergence, "seed-divergence node not found");
        assert!(found_edge, "cognitive_edge entries not found");
    }

    #[test]
    fn seed_cognitive_memory_idempotent() {
        let dir = tempdir().unwrap();
        ensure_default_cognitive_seed(dir.path()).unwrap();

        let db_path = dir.path().join("cognitive.trivium");
        let mut db = TriviumDb::open(&db_path, 4).unwrap();

        seed_cognitive_memory(dir.path(), &mut db).unwrap();

        let zero_vec = vec![0.0_f32; 4];
        let count_after_first = db.db().search(&zero_vec, 100, 0, 0.0).unwrap().len();

        seed_cognitive_memory(dir.path(), &mut db).unwrap();
        let count_after_second = db.db().search(&zero_vec, 100, 0, 0.0).unwrap().len();

        assert_eq!(
            count_after_first, count_after_second,
            "re-seed with .seeded marker should not add entries"
        );
    }

    #[test]
    fn seed_cognitive_memory_reseeds_on_schema_version_change() {
        let dir = tempdir().unwrap();
        ensure_default_cognitive_seed(dir.path()).unwrap();

        let db_path = dir.path().join("cognitive.trivium");
        let mut db = TriviumDb::open(&db_path, 4).unwrap();

        seed_cognitive_memory(dir.path(), &mut db).unwrap();

        let manifest_path = dir.path().join(SEED_DIR).join("manifest.json");
        std::fs::write(
            &manifest_path,
            r#"{"schema_version":2,"description":"test","nodes_file":"nodes.json","edges_file":"edges.json"}"#,
        )
        .unwrap();

        seed_cognitive_memory(dir.path(), &mut db).unwrap();

        let marker = dir.path().join(SEED_DIR).join(SEEDED_MARKER);
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "2");
    }

    #[test]
    #[cfg(unix)]
    fn seed_files_have_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        ensure_default_cognitive_seed(dir.path()).unwrap();

        let seed_root = dir.path().join(SEED_DIR);
        for name in ["manifest.json", "nodes.json", "edges.json"] {
            let path = seed_root.join(name);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "wrong permissions for {name}: {mode:o}");
        }
    }
}
