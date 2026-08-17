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
            AgentError::Bootstrap(format!("seed cognitive node {id}: {e}", id = node.node_id))
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
    // 主 agent 上下文排除内部管理分子：subagent.* 由执行中台专属平台 agent 使用，
    // usage_method.observe 由洞察平台专属 agent 使用，二者都不进入主 agent 上下文。
    let agent_capability_allowlist: Vec<String> = capability_ids
        .iter()
        .filter(|id| {
            !id.starts_with("memory.")
                && !id.starts_with("db.")
                && !id.starts_with("subagent.")
                && *id != "usage_method.observe"
        })
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

    let caps_json = serde_json::to_string(&agent_capability_allowlist)
        .map_err(|e| AgentError::Parse(format!("serialize capability_allowlist: {e}")))?;
    conn.execute(
        "INSERT INTO agent (id, name, mode, capability_allowlist, is_default) \
         SELECT 'agent', 'Agent', 'unni', CAST(? AS JSON), true \
         WHERE NOT EXISTS (SELECT 1 FROM agent WHERE id = 'agent')",
        duckdb::params![caps_json],
    )
    .map_err(|e| AgentError::Bootstrap(format!("seed agent: {e}")))?;
    conn.execute(
        "UPDATE agent SET capability_allowlist = CAST(? AS JSON) WHERE id = 'agent'",
        duckdb::params![caps_json],
    )
    .map_err(|e| AgentError::Bootstrap(format!("update agent capability_allowlist: {e}")))?;
    conn.execute(
        "UPDATE agent SET config = '{\"max_turns\": 8}' \
         WHERE id = 'agent' AND (config IS NULL OR config = 'null')",
        [],
    )
    .map_err(|e| AgentError::Bootstrap(format!("seed agent config: {e}")))?;

    seed_memory_agents(conn)?;
    seed_subagent_templates(conn)?;
    seed_platform_agents(conn)?;

    tracing::info!(
        "import_factory_defaults: {} base + agent capability_allowlist={}",
        base_rows.len(),
        agent_capability_allowlist.len()
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
        let caps_json = serde_json::to_string(caps).map_err(|e| {
            AgentError::Parse(format!("serialize capability_allowlist for {id}: {e}"))
        })?;
        conn.execute(
            "INSERT INTO agent (id, name, mode, capability_allowlist, display_name, is_default) \
             VALUES (?, ?, 'unni', CAST(? AS JSON), ?, false) \
             ON CONFLICT (id) DO NOTHING",
            duckdb::params![id, name, caps_json, name],
        )
        .map_err(|e| AgentError::Bootstrap(format!("seed memory agent {id}: {e}")))?;
    }
    Ok(())
}

/// 四个 subagent 模板行（§5.1 / §14）。
///
/// `mode = 'subagent_template'`，可读不可改；capability_allowlist 非空（安全集合）；
/// config.subagent 含 lifecycle/startup/trigger/memory_window_pct/briefing/预算字段。
/// ON CONFLICT DO NOTHING：重复执行不破坏已有行（含用户改动）。
pub fn seed_subagent_templates(conn: &duckdb::Connection) -> Result<()> {
    // (id, display_name, lifecycle_kind, startup, allowlist)
    let templates: &[(&str, &str, &str, &str, &[&str])] = &[
        (
            "subagent.template.normal",
            "Normal Subagent Template",
            "temporary",
            "normal",
            &["file.read", "file.list", "path.exists", "text.grep"],
        ),
        (
            "subagent.template.resident",
            "Resident Subagent Template",
            "resident",
            "normal",
            &["file.read", "file.list", "path.exists", "text.grep"],
        ),
        (
            "subagent.template.scheduled",
            "Scheduled Subagent Template",
            "temporary",
            "scheduled",
            &["file.read", "file.list", "path.exists", "text.grep"],
        ),
        (
            "subagent.template.condition",
            "Condition Subagent Template",
            "resident",
            "condition",
            &["file.read", "file.list", "path.exists", "text.grep"],
        ),
    ];
    for (id, name, lifecycle, startup, allowlist) in templates {
        let trigger = match *startup {
            "scheduled" => serde_json::json!({
                "type": "schedule",
                "cron": "* * * * *",
                "description": "定时触发范例（v0.3.1 不实现调度器）"
            }),
            "condition" => serde_json::json!({
                "type": "condition",
                "description": "条件触发范例（v0.3.1 不实现调度器）"
            }),
            _ => serde_json::Value::Null,
        };
        let config = serde_json::json!({
            "subagent": {
                "lifecycle": lifecycle,
                "startup": startup,
                "trigger": trigger,
                "memory_window_pct": 80,
                "briefing": true,
                "max_retries": 0,
                "attempt_timeout_seconds": 600,
                "total_timeout_seconds": 3600,
            }
        });
        let prompt = format!(
            "你是 subagent「{name}」，一个有最小记忆的异步工作单元。\n             ## 角色与任务边界\n             - 只处理执行中台分配的任务，不越界，不访问未授权能力，不创建/删除其他 subagent。\n             - 完成分配任务后立即以简报合同收口，不额外继续工作。\n             ## 能力调用规范\n             - 能力调用规范见系统提供的统一片段（capability_call.md 由运行时按需拼接），不要自行重写调用协议。\n             - 你只能调用 available_capabilities 中已授权的能力。\n             ## 运行方式\n             - 每轮从固定能力组选择 0/1/多个能力，多个调用按声明顺序依次执行；能力调用结果不回到本轮 LLM。\n             - 每轮只做一次模型调用。\n             ## 简报输出合同\n             - 任务完成时输出：{{\"done\": true, \"summary\": \"简明结果\"}}"
        );
        let allowlist_json = serde_json::to_string(&allowlist.to_vec()).map_err(|error| {
            AgentError::Parse(format!("serialize template allowlist for {id}: {error}"))
        })?;
        conn.execute(
            "INSERT INTO agent (id, name, mode, prompt, capability_allowlist, config, display_name, is_default) \
             VALUES (?, ?, 'subagent_template', ?, CAST(? AS JSON), CAST(? AS JSON), ?, false) \
             ON CONFLICT (id) DO NOTHING",
            duckdb::params![
                id,
                name,
                prompt,
                allowlist_json,
                config.to_string(),
                name,
            ],
        )
        .map_err(|error| AgentError::Bootstrap(format!("seed subagent template {id}: {error}")))?;
    }
    Ok(())
}

/// 核心平台 agent 表内预授权（§4.1）：执行中台六个 subagent.*、洞察平台 usage_method.observe。
///
/// INSERT ON CONFLICT DO NOTHING，幂等；已存在的行（含用户手工改动）不被覆盖。
pub fn seed_platform_agents(conn: &duckdb::Connection) -> Result<()> {
    let agents: &[(&str, &str, &[&str])] = &[
        (
            "execution-platform",
            "Execution Platform",
            &[
                "subagent.create",
                "subagent.run",
                "subagent.update",
                "subagent.sleep",
                "subagent.wake",
                "subagent.delete",
            ],
        ),
        (
            "insight-platform",
            "Insight Platform",
            &["usage_method.observe"],
        ),
    ];
    for (id, name, caps) in agents {
        let caps_json = serde_json::to_string(caps).map_err(|error| {
            AgentError::Parse(format!("serialize capability_allowlist for {id}: {error}"))
        })?;
        conn.execute(
            "INSERT INTO agent (id, name, mode, capability_allowlist, display_name, is_default) \
             VALUES (?, ?, 'platform', CAST(? AS JSON), ?, false) \
             ON CONFLICT (id) DO NOTHING",
            duckdb::params![id, name, caps_json, name],
        )
        .map_err(|error| AgentError::Bootstrap(format!("seed platform agent {id}: {error}")))?;
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

    #[test]
    fn seed_subagent_templates_and_platform_agents_are_idempotent() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        let template_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent WHERE mode = 'subagent_template'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(template_count, 4);
        let platform_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent WHERE mode = 'platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(platform_count, 2);

        // 幂等：再跑一遍不增加行、不破坏已有行。
        import_factory_defaults(&conn, dir.path()).unwrap();
        let template_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent WHERE mode = 'subagent_template'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(template_count_after, 4);
        let platform_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent WHERE mode = 'platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(platform_count_after, 2);

        // 模板 allowlist 非空安全集合。
        let allowlist_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent                  WHERE id = 'subagent.template.normal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let allowlist: serde_json::Value = serde_json::from_str(&allowlist_text.unwrap()).unwrap();
        assert!(
            !allowlist.as_array().unwrap().is_empty(),
            "template allowlist must be non-empty"
        );
    }

    #[test]
    fn seed_preserves_existing_agent_rows() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO agent (id, name, mode, capability_allowlist, is_default) \
             VALUES ('user-keep', 'User', 'unni', '[\"file.read\"]', false);",
        )
        .unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        let keep_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = 'user-keep'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(keep_text.unwrap(), "[\"file.read\"]");
    }

    #[test]
    fn main_agent_allowlist_excludes_subagent_and_observe() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        let allowlist_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = 'agent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let allowlist: serde_json::Value = serde_json::from_str(&allowlist_text.unwrap()).unwrap();
        let ids: Vec<&str> = allowlist
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert!(!ids.iter().any(|id| id.starts_with("subagent.")));
        assert!(!ids.contains(&"usage_method.observe"));
        assert!(ids.contains(&"file.read"));
    }

    #[test]
    fn platform_agents_hold_molecule_allowlists() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        let exec_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent                  WHERE id = 'execution-platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let exec: serde_json::Value = serde_json::from_str(&exec_text.unwrap()).unwrap();
        let exec_ids: Vec<&str> = exec
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        for expected in [
            "subagent.create",
            "subagent.run",
            "subagent.update",
            "subagent.sleep",
            "subagent.wake",
            "subagent.delete",
        ] {
            assert!(
                exec_ids.contains(&expected),
                "execution-platform missing {expected}"
            );
        }

        let insight_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent                  WHERE id = 'insight-platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let insight: serde_json::Value = serde_json::from_str(&insight_text.unwrap()).unwrap();
        let insight_ids: Vec<&str> = insight
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert_eq!(insight_ids, vec!["usage_method.observe"]);
    }
}
