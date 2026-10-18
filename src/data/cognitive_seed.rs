use crate::common::{AgentError, Result};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use crate::data::triviumdb::TriviumDb;
use duckdb::OptionalExt;
use serde::Deserialize;
use std::fs;
use std::path::Path;

const SEED_DIR: &str = "seed/cognitive";

const MAX_SEED_COGNITIVE_NODES: usize = 50;
const SEEDED_MARKER: &str = ".seeded";
const CAPABILITY_SEED_DIR: &str = "seed/capabilities";

/// capability seed 版本（v0.4.7）：今后种子变更 +1。
///
/// 版本判定只作用于 `base_capabilities.json`（唯一版本化的种子文件）：
/// - 新格式：`{"seed_version": N, "capabilities": [...]}`；
/// - 旧格式（v0.4.6 及以前）：纯数组，视为 version 1 → 触发重写。
///
/// v4（v0.5.11 B2/B3）：新增 ledger.read / ledger.write 能力行（完成状态台账）。
/// v5（v0.5.11 B4/B1）：新增 evidence.check 能力行；ledger.write 的 schema_in
/// 去掉 `additionalProperties:false`（composite DAG 数据流会把 composite 输入键
/// 合并进每个节点 args，闭 schema 使 ledger.write 无法进入任何 composite——
/// 诚实合同的实际强制在 ledger.rs 状态机与证据校验，不依赖该 schema 收口）。
pub const CAPABILITY_SEED_VERSION: u32 = 5;

/// composite seed 版本（v0.5.11 B1a）：种子文件从纯数组升级为版本化对象
/// `{"seed_version": N, "capabilities": [...]}`，与 base 同构。
/// v2：新增 capability.self-register（自注册闭环 composite）。
pub const COMPOSITE_SEED_VERSION: u32 = 2;

/// usage_method seed 版本（v0.5.11 B1b）：同 composite 的版本化升级。
/// v2：新增 um_capability_self_register（自注册方法文档）。
pub const USAGE_SEED_VERSION: u32 = 2;

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

/// 库内 cognitive 条目数（节点 + 边，含 seeded 与习得）。
fn count_cognitive_entries(db: &TriviumDb) -> usize {
    db.db()
        .get_all_ids()
        .into_iter()
        .filter(|id| is_cognitive_payload(db.db().get_payload(*id).as_ref()))
        .count()
}

/// payload 是否为认知条目（节点或边）。
fn is_cognitive_payload(payload: Option<&serde_json::Value>) -> bool {
    matches!(
        payload
            .and_then(|p| p.get("_memory_type"))
            .and_then(|v| v.as_str()),
        Some("cognitive") | Some("cognitive_edge")
    )
}

/// 重播清理：删除库内全部 factory 播种条目（`_memory_type ∈ {cognitive, cognitive_edge}`
/// 且 `seeded == true`）。旧重播直接重插会造成 seeded 条目重复；习得节点（无 seeded
/// 标记）不受影响。
fn purge_seeded_cognitive_entries(db: &mut TriviumDb) -> Result<()> {
    let stale_ids: Vec<triviumdb::NodeId> = db
        .db()
        .get_all_ids()
        .into_iter()
        .filter(|id| {
            db.db()
                .get_payload(*id)
                .filter(|payload| {
                    is_cognitive_payload(Some(payload))
                        && payload.get("seeded").and_then(|v| v.as_bool()) == Some(true)
                })
                .is_some()
        })
        .collect();
    let purged = stale_ids.len();
    for id in stale_ids {
        db.db_mut().delete(id).map_err(|e| {
            AgentError::Bootstrap(format!("purge seeded cognitive entry {id}: {e}"))
        })?;
    }
    if purged > 0 {
        tracing::info!("cognitive seed: purged {purged} stale seeded entries before replay");
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
                if count_cognitive_entries(db) == 0 {
                    // 库空自愈：marker 残留但记忆库已无认知条目（如用户清空记忆库），
                    // 继续执行完整播种让出厂认知重生。
                    tracing::info!(
                        "cognitive seed: marker matches but memory has no cognitive entries, reseeding (schema_version={})",
                        manifest.schema_version
                    );
                } else {
                    tracing::debug!(
                        "cognitive seed: already seeded (schema_version={})",
                        manifest.schema_version
                    );
                    return Ok(());
                }
            }
        }
    }

    tracing::info!(
        "cognitive seed: seeding (schema_version={})",
        manifest.schema_version
    );

    purge_seeded_cognitive_entries(db)?;

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

/// 读取已落盘种子文件的 seed_version（base/composite/usage 通用）：
/// - 新格式对象 `{"seed_version": N, ...}` → N；
/// - 旧格式纯数组 → 1（v0.4.6 及以前写入的版本）；
/// - 缺失 / 不可解析 / 无 seed_version 字段 → 0（触发重写）。
fn seed_file_version(path: &Path) -> u32 {
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return 0;
    };
    match value {
        serde_json::Value::Array(_) => 1,
        serde_json::Value::Object(obj) => obj
            .get("seed_version")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(0),
        _ => 0,
    }
}

pub fn ensure_default_capabilities(data_dir: &Path) -> Result<()> {
    let seed_root = data_dir.join(CAPABILITY_SEED_DIR);
    ensure_private_directory(&seed_root)?;

    let files: &[(&str, &str, u32)] = &[
        (
            "base_capabilities.json",
            CAPABILITY_SEED_BASE,
            CAPABILITY_SEED_VERSION,
        ),
        (
            "composite_capabilities.json",
            CAPABILITY_SEED_COMPOSITE,
            COMPOSITE_SEED_VERSION,
        ),
        (
            "usage_methods.json",
            CAPABILITY_SEED_USAGE,
            USAGE_SEED_VERSION,
        ),
    ];

    for (name, content, version) in files {
        let path = seed_root.join(name);
        // v0.4.7 覆盖判定：base_capabilities.json 版本化（缺失 / seed_version 落后 / 旧纯数组
        // 格式 → 以内置最新重写）。v0.5.11 B1 起 composite/usage 同样版本化：存量目录
        // 种子文件版本落后时重写，保证升级后拿到新增行（此前「缺失才写」使存量安装
        // 永远拿不到新 composite/usage 行）。`wasm:` 特判已删除。
        if seed_file_version(&path) < *version {
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
    // v0.4.7 读侧兼容两种格式：对象（新格式，取 capabilities 字段）与纯数组（旧格式，
    // 视为 version 1 处理）。旧文件在 ensure_default_capabilities 中已被重写为最新格式，
    // 此处兼容保证「重写失败/手工旧文件」等异常路径仍可导入。
    let base_value: serde_json::Value = serde_json::from_str(&base_text)
        .map_err(|e| AgentError::Parse(format!("parse base_capabilities.json: {e}")))?;
    let base_rows: Vec<serde_json::Value> = match base_value {
        serde_json::Value::Array(rows) => rows,
        serde_json::Value::Object(mut obj) => obj
            .remove("capabilities")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default(),
        other => {
            return Err(AgentError::Parse(format!(
                "parse base_capabilities.json: unexpected top-level type {}",
                json_type_name(&other)
            )));
        }
    };
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
    // usage_method.observe 由洞察平台专属 agent 使用，permission.* 由执行中台（或
    // 获递归授权的 subagent）使用——三者都不进入主 agent 上下文。
    // v0.4.6 追加：web.* 网络能力（web.fetch.public）同样不默认授权（安全默认——
    // 白名单 + permission.grant 双重把关后才可调用）。
    let agent_capability_allowlist: Vec<String> = capability_ids
        .iter()
        .filter(|id| {
            !id.starts_with("memory.")
                && !id.starts_with("db.")
                && !id.starts_with("subagent.")
                && !id.starts_with("permission.")
                && !id.starts_with("web.")
                && *id != "usage_method.observe"
        })
        .cloned()
        .collect();

    let comp_text = fs::read_to_string(seed_root.join("composite_capabilities.json"))
        .map_err(|e| AgentError::Io(format!("read composite_capabilities.json: {e}")))?;
    let comp_rows = read_seed_rows(&comp_text, "composite_capabilities.json")?;
    for row in &comp_rows {
        let id = row["id"].as_str().unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let enabled = row["enabled"].as_bool().unwrap_or(true);
        let metadata = serde_json::json!({
            "partition": row["partition"].as_str().unwrap_or("system"),
        });
        // v0.5.11 B1：composite 走 delta 语义（DB 无该 id 才插）——INSERT OR REPLACE
        // 会覆盖用户对同 id 工厂行的修改；升级目标是「拿到新增行」，不夺回用户改动。
        conn.execute(
            "INSERT INTO composite_capability \
             (id, name, description, schema_in, schema_out, executor, dag, version, enabled, metadata) \
             SELECT ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, CAST(? AS JSON), ?, ?, CAST(? AS JSON) \
             WHERE NOT EXISTS (SELECT 1 FROM composite_capability WHERE id = ?)",
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
                id,
            ],
        )
        .map_err(|e| AgentError::Bootstrap(format!("import composite_capability {id}: {e}")))?;
    }

    let usage_text = fs::read_to_string(seed_root.join("usage_methods.json"))
        .map_err(|e| AgentError::Io(format!("read usage_methods.json: {e}")))?;
    let usage_rows = read_seed_rows(&usage_text, "usage_methods.json")?;
    for row in &usage_rows {
        let id = row["id"].as_str().unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        // v0.5.11 B1：usage 同 composite 走 delta 语义（DB 无该 id 才插）。
        conn.execute(
            "INSERT INTO usage_method \
             (id, capability_id, name, prompt, examples, metadata) \
             SELECT ?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON) \
             WHERE NOT EXISTS (SELECT 1 FROM usage_method WHERE id = ?)",
            duckdb::params![
                id,
                row["capability_id"].as_str().unwrap_or(""),
                row["name"].as_str().unwrap_or(id),
                row["prompt"].as_str().unwrap_or(""),
                row["examples"].to_string(),
                row["metadata"].to_string(),
                id,
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

/// serde_json::Value 顶层类型名（错误信息用）。
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// 读侧兼容（v0.5.11 B1）：composite/usage 种子文件两种格式——
/// 新格式对象 `{"seed_version": N, "capabilities": [...]}`（取 capabilities）与
/// 旧格式纯数组（视为完整行集）。与 base 的读侧兼容策略同构。
fn read_seed_rows(text: &str, name: &str) -> Result<Vec<serde_json::Value>> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| AgentError::Parse(format!("parse {name}: {e}")))?;
    match value {
        serde_json::Value::Array(rows) => Ok(rows),
        serde_json::Value::Object(mut obj) => Ok(obj
            .remove("capabilities")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()),
        other => Err(AgentError::Parse(format!(
            "parse {name}: unexpected top-level type {}",
            json_type_name(&other)
        ))),
    }
}

/// 记忆 agent 内置定义（id, display_name, allowlist）——seed 与 upgrade_seed_deltas 共用。
const MEMORY_AGENT_DEFS: &[(&str, &str, &[&str])] = &[
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

/// 记忆 agent 全部入表：Desktop 阶段用户可以直接查看/改造/创建这些 agent。
/// 使用 ON CONFLICT DO NOTHING，已有行（含用户手工修改）不被覆盖。
pub fn seed_memory_agents(conn: &duckdb::Connection) -> Result<()> {
    for (id, name, caps) in MEMORY_AGENT_DEFS {
        insert_memory_agent_row(conn, id, name, caps)?;
    }
    Ok(())
}

fn insert_memory_agent_row(
    conn: &duckdb::Connection,
    id: &str,
    name: &str,
    caps: &[&str],
) -> Result<()> {
    let caps_json = serde_json::to_string(caps)
        .map_err(|e| AgentError::Parse(format!("serialize capability_allowlist for {id}: {e}")))?;
    conn.execute(
        "INSERT INTO agent (id, name, mode, capability_allowlist, display_name, is_default) \
         VALUES (?, ?, 'unni', CAST(? AS JSON), ?, false) \
         ON CONFLICT (id) DO NOTHING",
        duckdb::params![id, name, caps_json, name],
    )
    .map_err(|e| AgentError::Bootstrap(format!("seed memory agent {id}: {e}")))?;
    Ok(())
}

/// 七 subagent 模板内置定义（id, display_name, lifecycle_kind, startup, allowlist）——
/// seed 与 upgrade_seed_deltas 共用。
///
/// v0.5.11 B2 起 allowlist 按**模板**差异化：per-template 宽集是唯一可行的授权
/// 路线（permission.grant 中转链断于 execution-platform，E1 实测）。前四个任务模板
/// 共用 [`TEMPLATE_WIDE_ALLOWLIST`]；register/verify/inspect 三个专用模板持窄集。
const SUBAGENT_TEMPLATE_DEFS: &[(&str, &str, &str, &str, &[&str])] = &[
    (
        "subagent.template.normal",
        "Normal Subagent Template",
        "temporary",
        "normal",
        TEMPLATE_WIDE_ALLOWLIST,
    ),
    (
        "subagent.template.resident",
        "Resident Subagent Template",
        "resident",
        "normal",
        TEMPLATE_WIDE_ALLOWLIST,
    ),
    (
        "subagent.template.scheduled",
        "Scheduled Subagent Template",
        "temporary",
        "scheduled",
        TEMPLATE_WIDE_ALLOWLIST,
    ),
    (
        "subagent.template.condition",
        "Condition Subagent Template",
        "resident",
        "condition",
        TEMPLATE_WIDE_ALLOWLIST,
    ),
    // v0.5.11 B2 三个专用模板（均 resident）：注册（自迭代能力导入）、验证
    // （只读查询核对导入结果）、检查（记忆检索 + 台账读写 + 只读文件集）。
    (
        "subagent.template.register",
        "Register Subagent Template",
        "resident",
        "normal",
        &["capability.import"],
    ),
    (
        "subagent.template.verify",
        "Verify Subagent Template",
        "resident",
        "normal",
        &["db.query"],
    ),
    (
        "subagent.template.inspect",
        "Inspect Subagent Template",
        "resident",
        "normal",
        &[
            "memory.retrieve",
            "ledger.read",
            "ledger.write",
            "evidence.check",
            "file.read",
            "file.list",
            "path.exists",
            "text.grep",
        ],
    ),
];

/// 任务模板宽安全集（模板 = 类型封装 + 宽集；实例只做子集裁剪）。
/// v0.5.11 B2：追加 file.chunk_read（E3 实证：REA 轮次因 chunk_read 不在宽集导致
/// 8k 缺口不可恢复；只读能力，风险面与 file.read 同级）。
const TEMPLATE_WIDE_ALLOWLIST: &[&str] = &[
    "file.read",
    "file.list",
    "file.write",
    "file.chunk_read",
    "path.exists",
    "text.grep",
    "shell.exec",
];

/// 按内置定义构建模板行的（prompt, allowlist_json, config_json）——seed 与
/// upgrade_seed_deltas 共用，保证两条路径写入的定义一致。
/// `allowlist` 为该模板的授权宽集（v0.5.11 B2 起 per-template 差异化）。
fn build_subagent_template_row(
    id: &str,
    name: &str,
    lifecycle: &str,
    startup: &str,
    allowlist: &[&str],
) -> Result<(String, String, String)> {
    let trigger = match startup {
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
    Ok((prompt, allowlist_json, config.to_string()))
}

/// 七个 subagent 模板行（§5.1 / §14）。
///
/// `mode = 'subagent_template'`，可读不可改；capability_allowlist 为该模板的授权
/// **宽集**（任务模板四件套同宽集，register/verify/inspect 专用模板持窄集——
/// v0.5.11 B2：per-template 宽集是唯一可行授权路线，实例创建时由实例 allowlist
/// 只做子集裁剪，保守性由实例设计裁剪承担）；config.subagent 含
/// lifecycle/startup/trigger/memory_window_pct/briefing/预算字段。
/// 升级策略：`INSERT ... ON CONFLICT (id) DO UPDATE SET capability_allowlist`——
/// 旧数据目录模板行在 seed 时自动升级为当前宽集（幂等，仅对已知模板 id，
/// 不改用户自建模板行）。
pub fn seed_subagent_templates(conn: &duckdb::Connection) -> Result<()> {
    for (id, name, lifecycle, startup, allowlist) in SUBAGENT_TEMPLATE_DEFS {
        let (prompt, allowlist_json, config_json) =
            build_subagent_template_row(id, name, lifecycle, startup, allowlist)?;
        conn.execute(
            "INSERT INTO agent (id, name, mode, prompt, capability_allowlist, config, display_name, is_default) \
             VALUES (?, ?, 'subagent_template', ?, CAST(? AS JSON), CAST(? AS JSON), ?, false) \
             ON CONFLICT (id) DO UPDATE SET capability_allowlist = excluded.capability_allowlist",
            duckdb::params![id, name, prompt, allowlist_json, config_json, name],
        )
        .map_err(|error| AgentError::Bootstrap(format!("seed subagent template {id}: {error}")))?;
    }
    Ok(())
}

/// 平台 agent 内置定义（insight-platform / capability-memory-agent）——seed 与
/// upgrade_seed_deltas 共用。execution-platform 单独处理（追加合并语义见
/// seed_execution_platform；upgrade_seed_deltas 仅缺 id 插入，见 insert_execution_platform_row）。
const PLATFORM_AGENT_DEFS: &[(&str, &str, &[&str], Option<&str>)] = &[
    (
        "insight-platform",
        "Insight Platform",
        &["usage_method.observe"],
        None,
    ),
    (
        "capability-memory-agent",
        "Capability Memory Agent",
        &["usage_method.observe"],
        Some(r#"{"max_turns":2}"#),
    ),
];

fn insert_platform_agent_row(
    conn: &duckdb::Connection,
    id: &str,
    name: &str,
    caps: &[&str],
    config: Option<&str>,
) -> Result<()> {
    let caps_json = serde_json::to_string(caps).map_err(|error| {
        AgentError::Parse(format!("serialize capability_allowlist for {id}: {error}"))
    })?;
    let config_json = config.unwrap_or("null").to_string();
    conn.execute(
        "INSERT INTO agent (id, name, mode, capability_allowlist, config, display_name, is_default) \
         VALUES (?, ?, 'platform', CAST(? AS JSON), CAST(? AS JSON), ?, false) \
         ON CONFLICT (id) DO NOTHING",
        duckdb::params![id, name, caps_json, config_json, name],
    )
    .map_err(|error| AgentError::Bootstrap(format!("seed platform agent {id}: {error}")))?;
    Ok(())
}

/// 核心平台 agent 表内预授权（§4.1）：执行中台六个 subagent.*、洞察平台 usage_method.observe、
/// 洞察域能力记忆 agent（usage_method.observe，常驻滑动窗口节点；config.max_turns=2 限制
/// 失败回环重试上限）。
///
/// INSERT ON CONFLICT DO NOTHING，幂等；已存在的行（含用户手工改动）不被覆盖。
/// execution-platform 单独处理（v0.4.4 追加 permission.grant/revoke，见
/// `seed_execution_platform`）：旧数据目录平台行必须升级拿到两能力。
pub fn seed_platform_agents(conn: &duckdb::Connection) -> Result<()> {
    for (id, name, caps, config) in PLATFORM_AGENT_DEFS {
        insert_platform_agent_row(conn, id, name, caps, *config)?;
    }

    seed_execution_platform(conn)
}

/// execution-platform 内置能力集
/// （v0.4.4 起：六个 subagent.* + permission.grant/revoke；
///   v0.5.11 追加 ledger.read/write——IT-2 实测主 Agent 的台账读写经平台中转，
///   平台无此二能力则收口/更新交付状态的编排动作整条被 capability_not_authorized 拒绝）。
const EXECUTION_PLATFORM_CAPS: &[&str] = &[
    "subagent.create",
    "subagent.run",
    "subagent.update",
    "subagent.sleep",
    "subagent.wake",
    "subagent.delete",
    "permission.grant",
    "permission.revoke",
    "method.invoke",
    "ledger.read",
    "ledger.write",
];

/// execution-platform 平台行（v0.4.4）：六个 subagent.* + permission.grant/revoke。
///
/// 升级策略：读现有 allowlist → 追加合并（去重）→ UPSERT（ON CONFLICT DO UPDATE 仅改
/// capability_allowlist，保留用户对 name/config 的手工改动；追加语义保证旧数据目录
/// 升级后拿到两能力，同时不覆盖用户已授权的其他叠加能力）。
fn seed_execution_platform(conn: &duckdb::Connection) -> Result<()> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent \
             WHERE id = 'execution-platform'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            AgentError::Bootstrap(format!("seed execution-platform read allowlist: {error}"))
        })?;
    let mut allowlist: Vec<String> = match existing {
        Some(text) => serde_json::from_str(&text).map_err(|error| {
            AgentError::Parse(format!(
                "parse execution-platform capability_allowlist: {error}"
            ))
        })?,
        None => Vec::new(),
    };
    for capability_id in EXECUTION_PLATFORM_CAPS {
        if !allowlist.iter().any(|value| value == capability_id) {
            allowlist.push((*capability_id).to_string());
        }
    }
    let caps_json = serde_json::to_string(&allowlist).map_err(|error| {
        AgentError::Parse(format!(
            "serialize execution-platform capability_allowlist: {error}"
        ))
    })?;
    conn.execute(
        "INSERT INTO agent (id, name, mode, capability_allowlist, display_name, is_default) \
         VALUES ('execution-platform', 'Execution Platform', 'platform', CAST(? AS JSON), \
                 'Execution Platform', false) \
         ON CONFLICT (id) DO UPDATE SET capability_allowlist = excluded.capability_allowlist",
        duckdb::params![caps_json],
    )
    .map_err(|error| AgentError::Bootstrap(format!("seed execution-platform: {error}")))?;
    Ok(())
}

/// 注册表缺 id 判定（upgrade_seed_deltas 使用）。
fn agent_row_exists(conn: &duckdb::Connection, id: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent WHERE id = ?",
            duckdb::params![id],
            |row| row.get(0),
        )
        .map_err(|e| AgentError::Bootstrap(format!("check agent row {id}: {e}")))?;
    Ok(count > 0)
}

/// 缺 id 时插入 execution-platform 内置行（仅 upgrade_seed_deltas 使用；已存在 → 不动，
/// 不叠加用户改动）。seed_execution_platform 负责「已存在行的追加升级」。
fn insert_execution_platform_row(conn: &duckdb::Connection) -> Result<()> {
    let caps_json = serde_json::to_string(&EXECUTION_PLATFORM_CAPS.to_vec()).map_err(|error| {
        AgentError::Parse(format!(
            "serialize execution-platform capability_allowlist: {error}"
        ))
    })?;
    conn.execute(
        "INSERT INTO agent (id, name, mode, capability_allowlist, display_name, is_default) \
         VALUES ('execution-platform', 'Execution Platform', 'platform', CAST(? AS JSON), \
                 'Execution Platform', false) \
         ON CONFLICT (id) DO NOTHING",
        duckdb::params![caps_json],
    )
    .map_err(|error| AgentError::Bootstrap(format!("insert execution-platform: {error}")))?;
    Ok(())
}

/// 缺 id 时插入 subagent 模板内置行（仅 upgrade_seed_deltas 使用；已存在 → 不动，
/// 含用户对 allowlist/config/prompt 的修改全部保留）。seed_subagent_templates 负责
/// 「已存在行的宽集升级」。
fn insert_subagent_template_row(
    conn: &duckdb::Connection,
    id: &str,
    name: &str,
    lifecycle: &str,
    startup: &str,
    allowlist: &[&str],
) -> Result<()> {
    let (prompt, allowlist_json, config_json) =
        build_subagent_template_row(id, name, lifecycle, startup, allowlist)?;
    conn.execute(
        "INSERT INTO agent (id, name, mode, prompt, capability_allowlist, config, display_name, is_default) \
         VALUES (?, ?, 'subagent_template', ?, CAST(? AS JSON), CAST(? AS JSON), ?, false) \
         ON CONFLICT (id) DO NOTHING",
        duckdb::params![id, name, prompt, allowlist_json, config_json, name],
    )
    .map_err(|error| AgentError::Bootstrap(format!("insert subagent template {id}: {error}")))?;
    Ok(())
}

/// agent/模板缺失补插（v0.4.7，TA-C）：在 import_factory_defaults 之后调用。
///
/// 遍历内置清单——平台 agents（execution-platform/insight-platform/capability-memory-agent）、
/// 四记忆 agent、七 subagent 模板（v0.5.11 B2 含 register/verify/inspect）——注册表
/// 缺 id → 按内置定义插入行（含 config/allowlist）；
/// 已存在 → 不动（含用户手工修改，与 seed_* 的升级语义分离）；内置已删除的 id → 不动。
/// 幂等（重复启动无副作用）。
pub fn upgrade_seed_deltas(conn: &duckdb::Connection, _data_dir: &Path) -> Result<()> {
    // 1) 平台 agents。
    for (id, name, caps, config) in PLATFORM_AGENT_DEFS {
        if !agent_row_exists(conn, id)? {
            insert_platform_agent_row(conn, id, name, caps, *config)?;
            tracing::info!("upgrade_seed_deltas: inserted missing platform agent {id}");
        }
    }
    if !agent_row_exists(conn, "execution-platform")? {
        insert_execution_platform_row(conn)?;
        tracing::info!("upgrade_seed_deltas: inserted missing platform agent execution-platform");
    }
    // 2) 四记忆 agent。
    for (id, name, caps) in MEMORY_AGENT_DEFS {
        if !agent_row_exists(conn, id)? {
            insert_memory_agent_row(conn, id, name, caps)?;
            tracing::info!("upgrade_seed_deltas: inserted missing memory agent {id}");
        }
    }
    // 3) 七 subagent 模板（v0.5.11 B2：含 register/verify/inspect 三个专用模板；
    //    缺 id 插入——已初始化的旧库经此升级路径拿到新模板，已有行不动）。
    for (id, name, lifecycle, startup, allowlist) in SUBAGENT_TEMPLATE_DEFS {
        if !agent_row_exists(conn, id)? {
            insert_subagent_template_row(conn, id, name, lifecycle, startup, allowlist)?;
            tracing::info!("upgrade_seed_deltas: inserted missing subagent template {id}");
        }
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
        assert_eq!(manifest.schema_version, 2);
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
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "2");

        let zero_vec = vec![0.0_f32; 4];
        let results = db.db().search(&zero_vec, 200, 0, 0.0).unwrap();
        let nodes: Vec<SeedNode> = serde_json::from_str(COGNITIVE_SEED_NODES).unwrap();
        let edges: Vec<SeedEdge> = serde_json::from_str(COGNITIVE_SEED_EDGES).unwrap();
        let expected = nodes.len() + edges.len();
        assert!(
            results.len() >= expected,
            "expected {expected} entries (nodes + edges), got {}",
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

    /// 重播清理（修复）：schema_version 升版触发重播时，旧 seeded 条目先被删除再重插，
    /// 认知条目总数恰为 seed 清单条数（无重复）；习得节点（无 seeded 标记）不受清理影响。
    #[test]
    fn seed_cognitive_memory_replay_purges_stale_seeded_entries_exactly_once() {
        let dir = tempdir().unwrap();
        ensure_default_cognitive_seed(dir.path()).unwrap();

        let db_path = dir.path().join("cognitive.trivium");
        let mut db = TriviumDb::open(&db_path, 4).unwrap();

        seed_cognitive_memory(dir.path(), &mut db).unwrap();

        let nodes: Vec<SeedNode> = serde_json::from_str(COGNITIVE_SEED_NODES).unwrap();
        let edges: Vec<SeedEdge> = serde_json::from_str(COGNITIVE_SEED_EDGES).unwrap();
        let expected = nodes.len() + edges.len();
        assert_eq!(count_cognitive_entries(&db), expected, "首次播种条目数");

        // 习得节点：无 seeded 标记，重播清理不得删除。
        let zero_vec = vec![0.0_f32; 4];
        db.db_mut()
            .insert(
                &zero_vec,
                serde_json::json!({
                    "_memory_type": "cognitive",
                    "node_id": "learned-1",
                    "insight": "learned insight",
                    "context": "acquired at runtime",
                }),
            )
            .unwrap();

        // 升版触发重播。
        let manifest_path = dir.path().join(SEED_DIR).join("manifest.json");
        std::fs::write(
            &manifest_path,
            r#"{"schema_version":3,"description":"test","nodes_file":"nodes.json","edges_file":"edges.json"}"#,
        )
        .unwrap();

        seed_cognitive_memory(dir.path(), &mut db).unwrap();

        assert_eq!(
            count_cognitive_entries(&db),
            expected + 1,
            "重播应清理旧 seeded 条目且恰好重插一次，习得节点保留"
        );
        let learned_survived = db.db().get_all_ids().iter().any(|id| {
            db.db()
                .get_payload(*id)
                .map(|p| p.get("node_id").and_then(|v| v.as_str()) == Some("learned-1"))
                .unwrap_or(false)
        });
        assert!(learned_survived, "习得节点不得被重播清理删除");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(SEED_DIR).join(SEEDED_MARKER))
                .unwrap()
                .trim(),
            "3"
        );
    }

    /// 库空自愈（修复）：marker 匹配但记忆库中认知条目为 0（如用户清空记忆库而数据目录
    /// marker 残留）时，仍执行完整播种，出厂认知重生。
    #[test]
    fn seed_cognitive_memory_self_heals_when_marker_matches_but_library_emptied() {
        let dir = tempdir().unwrap();
        ensure_default_cognitive_seed(dir.path()).unwrap();

        let db_path = dir.path().join("cognitive.trivium");
        let mut db = TriviumDb::open(&db_path, 4).unwrap();

        seed_cognitive_memory(dir.path(), &mut db).unwrap();

        let marker_path = dir.path().join(SEED_DIR).join(SEEDED_MARKER);
        assert_eq!(std::fs::read_to_string(&marker_path).unwrap().trim(), "2");

        // 模拟用户清空记忆库：删除全部条目，保留数据目录与 .seeded marker。
        for id in db.db().get_all_ids() {
            db.db_mut().delete(id).unwrap();
        }
        assert_eq!(count_cognitive_entries(&db), 0, "库应已清空");

        // marker 仍匹配 manifest 版本 → 修复前直接 return；修复后自愈重播。
        seed_cognitive_memory(dir.path(), &mut db).unwrap();

        let nodes: Vec<SeedNode> = serde_json::from_str(COGNITIVE_SEED_NODES).unwrap();
        let edges: Vec<SeedEdge> = serde_json::from_str(COGNITIVE_SEED_EDGES).unwrap();
        assert_eq!(
            count_cognitive_entries(&db),
            nodes.len() + edges.len(),
            "清库后出厂认知应完整重生"
        );
        assert_eq!(std::fs::read_to_string(&marker_path).unwrap().trim(), "2");
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
        assert_eq!(template_count, 7);
        let platform_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent WHERE mode = 'platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(platform_count, 3);

        // 幂等：再跑一遍不增加行、不破坏已有行。
        import_factory_defaults(&conn, dir.path()).unwrap();
        let template_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent WHERE mode = 'subagent_template'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(template_count_after, 7);
        let platform_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent WHERE mode = 'platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(platform_count_after, 3);

        // 模板 allowlist 为宽安全集（四任务模板同宽集，实例只做子集裁剪；
        // register/verify/inspect 专用窄集见专门测试）。
        let allowlist_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent                  WHERE id = 'subagent.template.normal'",
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
        for expected in [
            "file.read",
            "file.list",
            "file.write",
            "file.chunk_read",
            "path.exists",
            "text.grep",
            "shell.exec",
        ] {
            assert!(ids.contains(&expected), "normal 模板宽集缺少 {expected}");
        }
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
    fn seed_templates_wide_allowlist_upgrades_legacy_narrow_rows() {
        // 升级路径：旧数据目录模板行（窄只读集）在 seed 后被宽化为当前宽集。
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        // 模拟旧数据：把 resident/scheduled/condition 三模板行改为只读窄集。
        for id in [
            "subagent.template.resident",
            "subagent.template.scheduled",
            "subagent.template.condition",
        ] {
            conn.execute(
                "UPDATE agent SET capability_allowlist = CAST(? AS JSON) WHERE id = ?",
                duckdb::params![r#"["file.read","file.list","path.exists","text.grep"]"#, id],
            )
            .unwrap();
        }

        // 重新 seed：DO UPDATE 把窄集行宽化回当前宽集。
        import_factory_defaults(&conn, dir.path()).unwrap();

        let wide: serde_json::Value = serde_json::from_str(
            r#"["file.read","file.list","file.write","file.chunk_read","path.exists","text.grep","shell.exec"]"#,
        )
        .unwrap();
        for id in [
            "subagent.template.normal",
            "subagent.template.resident",
            "subagent.template.scheduled",
            "subagent.template.condition",
        ] {
            let text: Option<String> = conn
                .query_row(
                    "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = ?",
                    duckdb::params![id],
                    |row| row.get(0),
                )
                .unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&text.unwrap()).unwrap();
            assert_eq!(parsed, wide, "模板 {id} 应被宽化为宽集");
        }
    }

    /// v0.5.11 B2：seed 后三个专用模板（register/verify/inspect）存在且持各自
    /// 窄集；四任务模板持含 file.chunk_read 的宽集；全部 mode='subagent_template'。
    #[test]
    fn seed_registers_specialized_templates_with_per_template_allowlists() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        let expected: &[(&str, &[&str])] = &[
            ("subagent.template.register", &["capability.import"]),
            ("subagent.template.verify", &["db.query"]),
            (
                "subagent.template.inspect",
                &[
                    "memory.retrieve",
                    "ledger.read",
                    "ledger.write",
                    "evidence.check",
                    "file.read",
                    "file.list",
                    "path.exists",
                    "text.grep",
                ],
            ),
        ];
        for (id, allowlist) in expected {
            let (mode, caps_text, config_text): (String, String, String) = conn
                .query_row(
                    "SELECT mode, CAST(capability_allowlist AS VARCHAR), \
                     COALESCE(CAST(config AS VARCHAR), '') FROM agent WHERE id = ?",
                    duckdb::params![id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap_or_else(|e| panic!("{id} 应存在: {e}"));
            assert_eq!(mode, "subagent_template", "{id} mode");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&caps_text).unwrap(),
                serde_json::to_value(allowlist).unwrap(),
                "{id} allowlist 应为专用窄集"
            );
            let config: serde_json::Value = serde_json::from_str(&config_text).unwrap();
            assert_eq!(
                config["subagent"]["lifecycle"], "resident",
                "{id} 应为 resident"
            );
        }

        // normal 模板宽集含 file.chunk_read（E3 缺口修复）。
        let caps_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = 'subagent.template.normal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let allowlist: serde_json::Value = serde_json::from_str(&caps_text.unwrap()).unwrap();
        let ids: Vec<&str> = allowlist
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(ids.contains(&"file.chunk_read"), "宽集缺 file.chunk_read");
    }

    /// v0.5.11 B2 升级路径：已初始化的旧库（无三个新模板）经 upgrade_seed_deltas
    /// 补插新模板；已有行（含用户改过的 allowlist）不动。
    #[test]
    fn upgrade_seed_deltas_inserts_missing_specialized_templates_keeps_user_rows() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        // 模拟旧库：删除三个新模板；并给 normal 一个用户自定义窄集。
        conn.execute_batch(
            "DELETE FROM agent WHERE id IN \
             ('subagent.template.register', 'subagent.template.verify', 'subagent.template.inspect');",
        )
        .unwrap();
        conn.execute(
            "UPDATE agent SET capability_allowlist = CAST(? AS JSON) \
             WHERE id = 'subagent.template.normal'",
            duckdb::params![r#"["file.read"]"#],
        )
        .unwrap();

        upgrade_seed_deltas(&conn, dir.path()).unwrap();

        // 新模板补插且 allowlist 为内置窄集。
        let caps_text: String = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent \
                 WHERE id = 'subagent.template.register'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(caps_text, r#"["capability.import"]"#);

        // 用户改过的 normal 行：upgrade 路径不覆盖（宽化由 seed_subagent_templates
        // 的 DO UPDATE 负责，两条路径语义分离）。
        let normal: String = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent \
                 WHERE id = 'subagent.template.normal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(normal, r#"["file.read"]"#, "已有行不得被 upgrade 覆盖");

        // 幂等。
        upgrade_seed_deltas(&conn, dir.path()).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent WHERE mode = 'subagent_template'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 7);
    }

    #[test]
    fn seed_templates_do_not_touch_custom_template_rows() {
        // 仅已知内置模板 id 宽化/升级：用户自建模板行（同 mode，不同 id）不受影响。
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        conn.execute(
            "INSERT INTO agent (id, name, mode, capability_allowlist, is_default) \
             VALUES ('subagent.template.custom', 'My Template', 'subagent_template', '[\"file.read\"]', false);",
            [],
        )
        .unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        let text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = 'subagent.template.custom'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(text.unwrap(), "[\"file.read\"]", "用户自建模板行不得被宽化");
    }

    #[test]
    fn seed_upgrades_legacy_execution_platform_with_permission_abilities() {
        // 旧数据目录升级：execution-platform 行仅含 6 个 subagent.*（+用户叠加的 shell.exec）
        // → seed 后追加 permission.grant/revoke，保留既有叠加能力（追加合并，不覆盖用户改动）。
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        conn.execute(
            "INSERT INTO agent (id, name, mode, capability_allowlist, display_name, is_default) \
             VALUES ('execution-platform', 'Execution Platform', 'platform', \
                     CAST(? AS JSON), 'Execution Platform', false)",
            duckdb::params![r#"["subagent.create","subagent.run","subagent.update","subagent.sleep","subagent.wake","subagent.delete","shell.exec"]"#],
        )
        .unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        let text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent \
                 WHERE id = 'execution-platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let allowlist: serde_json::Value = serde_json::from_str(&text.unwrap()).unwrap();
        let ids: Vec<&str> = allowlist
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
            "permission.grant",
            "permission.revoke",
            "method.invoke",
            "shell.exec",
        ] {
            assert!(
                ids.contains(&expected),
                "execution-platform 升级后缺少 {expected}"
            );
        }
        // 幂等：再 seed 一次不重复追加。
        import_factory_defaults(&conn, dir.path()).unwrap();
        let text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent \
                 WHERE id = 'execution-platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let allowlist: serde_json::Value = serde_json::from_str(&text.unwrap()).unwrap();
        assert_eq!(
            allowlist.as_array().unwrap().len(),
            12,
            "再次 seed 不得重复追加能力"
        );
    }

    #[test]
    fn main_agent_allowlist_excludes_subagent_observe_and_permission() {
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
        assert!(!ids.iter().any(|id| id.starts_with("permission.")));
        assert!(!ids.iter().any(|id| id.starts_with("web.")));
        assert!(!ids.contains(&"usage_method.observe"));
        assert!(ids.contains(&"file.read"));
    }

    #[test]
    fn web_fetch_public_registered_but_not_in_any_allowlist() {
        // v0.4.6：web.fetch.public 注册为 enabled base capability（permission.grant 可授），
        // 但不得 seed 进任何 allowlist（主 agent / 平台 / 模板 / 记忆 agent 均不加）。
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        let (count, enabled): (i64, bool) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(enabled), false) FROM base_capability \
                 WHERE id = 'web.fetch.public'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "web.fetch.public 应注册为 base capability");
        assert!(enabled, "enabled=true 才能被 permission.grant 授予");

        let mut stmt = conn
            .prepare("SELECT CAST(capability_allowlist AS VARCHAR) FROM agent")
            .unwrap();
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>();
        for text in rows {
            let allowlist: serde_json::Value = serde_json::from_str(&text).unwrap();
            let ids: Vec<&str> = allowlist
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|value| value.as_str())
                .collect();
            assert!(
                !ids.contains(&"web.fetch.public"),
                "任何 agent allowlist 都不得默认含 web.fetch.public"
            );
        }
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
            "permission.grant",
            "permission.revoke",
            "method.invoke",
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

    /// 新目录（v0.4.7）：ensure_default_capabilities 写入含 seed_version 的新格式对象，
    /// 导入后注册表含 web.fetch.public（v0.4.6 缺口在旧目录的唯一修复路径）。
    #[test]
    fn new_dir_writes_versioned_format_and_registers_web_fetch_public() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();

        let base_path = dir.path().join("seed/capabilities/base_capabilities.json");
        let text = std::fs::read_to_string(&base_path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["seed_version"].as_u64(),
            Some(CAPABILITY_SEED_VERSION as u64),
            "新目录应写入版本化对象格式"
        );
        let caps = value["capabilities"].as_array().expect("capabilities 数组");
        assert!(caps.iter().any(|c| c["id"] == "web.fetch.public"));
        assert!(seed_file_version(&base_path) >= CAPABILITY_SEED_VERSION);

        import_factory_defaults(&conn, dir.path()).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM base_capability WHERE id = 'web.fetch.public'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "web.fetch.public 应进入注册表");
    }

    /// 旧目录升级：base_capabilities.json 为旧纯数组格式（version 1，且缺 web.fetch.public）
    /// → 启动重写为最新版本化格式 → 导入补齐新能力（v0.4.6 缺口修复验证）。
    #[test]
    fn legacy_flat_array_dir_rewritten_and_capabilities_registered() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        let seed_root = dir.path().join("seed/capabilities");
        std::fs::create_dir_all(&seed_root).unwrap();

        // 模拟 v0.4.6 旧文件：纯数组、无 web.fetch.public。
        let builtin: serde_json::Value = serde_json::from_str(CAPABILITY_SEED_BASE).unwrap();
        let legacy_caps: Vec<serde_json::Value> = builtin["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["id"] != "web.fetch.public")
            .cloned()
            .collect();
        std::fs::write(
            seed_root.join("base_capabilities.json"),
            serde_json::to_string(&legacy_caps).unwrap(),
        )
        .unwrap();
        assert_eq!(
            seed_file_version(&seed_root.join("base_capabilities.json")),
            1,
            "纯数组应判定为 version 1"
        );

        // 启动链：ensure → 重写；composite/usage 缺失 → 补写。
        ensure_default_capabilities(dir.path()).unwrap();

        let rewritten = std::fs::read_to_string(seed_root.join("base_capabilities.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(
            value["seed_version"].as_u64(),
            Some(CAPABILITY_SEED_VERSION as u64),
            "旧纯数组文件应被重写为最新版本化格式"
        );
        assert!(
            rewritten.contains("web.fetch.public"),
            "重写后含 web.fetch.public"
        );

        import_factory_defaults(&conn, dir.path()).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM base_capability WHERE id = 'web.fetch.public'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "旧目录升级后注册表补齐 web.fetch.public");
    }

    /// 最新目录（当前版本文件）：不重写——文件保持原样（用户/历史内容不被覆盖）。
    #[test]
    fn latest_version_dir_not_rewritten() {
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        let base_path = dir.path().join("seed/capabilities/base_capabilities.json");

        // 篡改为另一个当前版本内容（不含 ledger.read）——模拟已升级目录的本地状态。
        let custom = serde_json::json!({
            "seed_version": CAPABILITY_SEED_VERSION,
            "capabilities": [
                {"id": "file.read", "name": "Read File", "type": "function",
                 "description": "custom", "schema_in": {}, "schema_out": {},
                 "executor": "builtin:file.read", "version": "1.0.0",
                 "enabled": true, "partition": "system"}
            ]
        });
        std::fs::write(&base_path, serde_json::to_string(&custom).unwrap()).unwrap();

        ensure_default_capabilities(dir.path()).unwrap();

        let text = std::fs::read_to_string(&base_path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["seed_version"].as_u64(),
            Some(CAPABILITY_SEED_VERSION as u64),
            "最新版本文件保持原版本号"
        );
        assert!(!text.contains("ledger.read"), "最新版本文件不得被重写");
    }

    /// v2 → v3 升级（v0.4.8 种子版本迭代）：version 2 文件（旧 code.exec 描述、
    /// 无最新描述变更）应被重写为 version 3 官方内容（含新描述），新描述在
    /// import_factory_defaults 中以 INSERT OR REPLACE 落库（种子权威语义，不保留
    /// 用户改过的描述——用户已确认基础建设更新无条件覆盖）。
    #[test]
    fn version2_dir_rewritten_to_latest_with_new_description() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        let seed_root = dir.path().join("seed/capabilities");
        std::fs::create_dir_all(&seed_root).unwrap();

        // 模拟 v0.4.7 用户的 version 2 文件：旧 code.exec 描述。
        let custom = serde_json::json!({
            "seed_version": 2,
            "capabilities": [
                {"id": "code.exec", "name": "Execute Code", "type": "function",
                 "description": "在沙箱内执行代码片段", "schema_in": {}, "schema_out": {},
                 "executor": "builtin:code.exec", "version": "1.0.0",
                 "enabled": true, "partition": "system"},
                {"id": "file.read", "name": "Read File", "type": "function",
                 "description": "读取文件内容", "schema_in": {}, "schema_out": {},
                 "executor": "builtin:file.read", "version": "1.0.0",
                 "enabled": true, "partition": "system"}
            ]
        });
        std::fs::write(
            seed_root.join("base_capabilities.json"),
            serde_json::to_string(&custom).unwrap(),
        )
        .unwrap();
        assert_eq!(
            seed_file_version(&seed_root.join("base_capabilities.json")),
            2,
            "v2 文件应判定为 version 2"
        );

        // 启动链：ensure → 重写为内置最新（version 3）。
        ensure_default_capabilities(dir.path()).unwrap();

        let rewritten = std::fs::read_to_string(seed_root.join("base_capabilities.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(
            value["seed_version"].as_u64(),
            Some(CAPABILITY_SEED_VERSION as u64),
            "v2 文件应被重写为最新版本"
        );
        assert!(
            rewritten.contains("执行代码片段（受工作区权限约束）"),
            "重写后 code.exec 描述应为新描述"
        );

        // import 落库：INSERT OR REPLACE 无条件覆盖 → code.exec 新描述进库。
        import_factory_defaults(&conn, dir.path()).unwrap();
        let desc: String = conn
            .query_row(
                "SELECT description FROM base_capability WHERE id = 'code.exec'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            desc, "执行代码片段（受工作区权限约束）",
            "code.exec 描述应升级为新描述"
        );
    }

    /// import_factory_defaults 读侧兼容：旧纯数组格式（version 1 文件）也能完整导入
    /// （base/composite/usage 三份种子文件的旧数组格式读侧兼容）。
    #[test]
    fn import_factory_defaults_reads_legacy_array_format() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        let seed_root = dir.path().join("seed/capabilities");
        std::fs::create_dir_all(&seed_root).unwrap();
        // 三份种子都写旧格式纯数组（直接写内置 capabilities 数组内容）。
        for (file, builtin) in [
            ("base_capabilities.json", CAPABILITY_SEED_BASE),
            ("composite_capabilities.json", CAPABILITY_SEED_COMPOSITE),
            ("usage_methods.json", CAPABILITY_SEED_USAGE),
        ] {
            let builtin: serde_json::Value = serde_json::from_str(builtin).unwrap();
            std::fs::write(
                seed_root.join(file),
                serde_json::to_string(builtin["capabilities"].as_array().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(
                seed_file_version(&seed_root.join(file)),
                1,
                "{file} 纯数组应判定为 version 1"
            );
        }

        import_factory_defaults(&conn, dir.path()).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM base_capability WHERE id = 'web.fetch.public'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "旧数组格式读侧应兼容导入（含 web.fetch.public）");
        let comp_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM composite_capability WHERE id = 'capability.self-register'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(comp_count, 1, "旧数组格式 composite 也应完整导入");
        let agent_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent", [], |row| row.get(0))
            .unwrap();
        assert!(agent_count >= 4, "记忆 agent 正常导入: {agent_count}");
    }

    /// v0.5.11 B1 升级通路：旧版本 composite/usage 种子文件（纯数组、无新行）在
    /// ensure_default_capabilities 时被重写为最新版本化内容；import 落库采用
    /// delta 语义——新工厂行（capability.self-register / um_capability_self_register）
    /// 落库，用户自定义行与对已有工厂行的修改均不被破坏。
    #[test]
    fn legacy_composite_usage_seed_upgraded_with_delta_import() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        let seed_root = dir.path().join("seed/capabilities");
        std::fs::create_dir_all(&seed_root).unwrap();

        // 模拟旧版目录：composite/usage 为纯数组且不含新行；base 落后一版。
        let comp_builtin: serde_json::Value =
            serde_json::from_str(CAPABILITY_SEED_COMPOSITE).unwrap();
        let legacy_comp: Vec<serde_json::Value> = comp_builtin["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["id"] != "capability.self-register")
            .cloned()
            .collect();
        std::fs::write(
            seed_root.join("composite_capabilities.json"),
            serde_json::to_string(&legacy_comp).unwrap(),
        )
        .unwrap();
        let usage_builtin: serde_json::Value = serde_json::from_str(CAPABILITY_SEED_USAGE).unwrap();
        let legacy_usage: Vec<serde_json::Value> = usage_builtin["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["id"] != "um_capability_self_register")
            .cloned()
            .collect();
        std::fs::write(
            seed_root.join("usage_methods.json"),
            serde_json::to_string(&legacy_usage).unwrap(),
        )
        .unwrap();
        let base_builtin: serde_json::Value = serde_json::from_str(CAPABILITY_SEED_BASE).unwrap();
        let mut legacy_base = base_builtin.clone();
        legacy_base["seed_version"] = serde_json::json!(CAPABILITY_SEED_VERSION - 1);
        std::fs::write(
            seed_root.join("base_capabilities.json"),
            serde_json::to_string(&legacy_base).unwrap(),
        )
        .unwrap();

        // 启动链：版本落后 → 三份种子文件重写为内置最新。
        ensure_default_capabilities(dir.path()).unwrap();
        for (file, version) in [
            ("composite_capabilities.json", COMPOSITE_SEED_VERSION),
            ("usage_methods.json", USAGE_SEED_VERSION),
            ("base_capabilities.json", CAPABILITY_SEED_VERSION),
        ] {
            assert_eq!(
                seed_file_version(&seed_root.join(file)),
                version,
                "{file} 应被重写为最新版本"
            );
        }
        let rewritten =
            std::fs::read_to_string(seed_root.join("composite_capabilities.json")).unwrap();
        assert!(rewritten.contains("capability.self-register"));

        // DB 预置：用户自定义行 + 对已有工厂行的修改（改 prompt）。
        conn.execute(
            "INSERT INTO composite_capability \
             (id, name, description, schema_in, schema_out, executor, dag, version, enabled) \
             VALUES ('user.custom.comp', 'Custom', 'user row', CAST(? AS JSON), CAST(? AS JSON), 'dag', CAST(? AS JSON), '1.0.0', true)",
            duckdb::params![r#"{"type":"object"}"#, r#"{"type":"object"}"#, "[]"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_method (id, capability_id, name, prompt) \
             VALUES ('user.custom.um', 'capability.register', 'Custom UM', 'user prompt')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_method (id, capability_id, name, prompt) \
             VALUES ('capability.list', 'capability.register', 'List', '工厂行原文')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE usage_method SET prompt = '用户改过的工厂行' WHERE id = 'capability.list'",
            [],
        )
        .unwrap();

        // 启动导入：delta 语义。
        import_factory_defaults(&conn, dir.path()).unwrap();

        // 新工厂行落库（升级拿到 self-register 与方法行）。
        let (comp_n, um_n): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM composite_capability WHERE id = 'capability.self-register'), \
                        (SELECT COUNT(*) FROM usage_method WHERE id = 'um_capability_self_register')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (comp_n, um_n),
            (1, 1),
            "存量安装升级后应拿到新 composite/usage 行"
        );

        // 用户自定义行与对工厂行的修改未被破坏。
        let user_um: Option<String> = conn
            .query_row(
                "SELECT prompt FROM usage_method WHERE id = 'user.custom.um'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(user_um.as_deref(), Some("user prompt"));
        let modified_factory: Option<String> = conn
            .query_row(
                "SELECT prompt FROM usage_method WHERE id = 'capability.list'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            modified_factory.as_deref(),
            Some("用户改过的工厂行"),
            "delta 语义：用户对同 id 工厂行的修改不被覆盖"
        );

        // 幂等：重复启动不重复插入、不破坏。
        import_factory_defaults(&conn, dir.path()).unwrap();
        let comp_total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM composite_capability WHERE id = 'capability.self-register'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(comp_total, 1);
    }

    /// 最新版本目录：composite/usage 种子文件不被重写（用户/历史内容保持）。
    #[test]
    fn current_version_composite_usage_seed_not_rewritten() {
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        let seed_root = dir.path().join("seed/capabilities");
        for file in ["composite_capabilities.json", "usage_methods.json"] {
            let path = seed_root.join(file);
            std::fs::write(&path, "{\"seed_version\":2,\"capabilities\":[]}").unwrap();
        }

        ensure_default_capabilities(dir.path()).unwrap();

        for file in ["composite_capabilities.json", "usage_methods.json"] {
            let text = std::fs::read_to_string(seed_root.join(file)).unwrap();
            assert_eq!(
                text, "{\"seed_version\":2,\"capabilities\":[]}",
                "{file} 版本匹配时不得被重写"
            );
        }
    }

    /// upgrade_seed_deltas：缺 id 插入（平台/记忆/模板三类）、已有不覆盖（含用户改的
    /// allowlist 保持）、内置已删除的 id 不动、幂等。
    #[test]
    fn upgrade_seed_deltas_inserts_missing_keeps_existing_and_idempotent() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let dir = tempdir().unwrap();
        ensure_default_capabilities(dir.path()).unwrap();
        import_factory_defaults(&conn, dir.path()).unwrap();

        // 用户改 insight-platform 的 allowlist（自定义叠加，不覆盖）。
        conn.execute(
            "UPDATE agent SET capability_allowlist = CAST(? AS JSON) WHERE id = 'insight-platform'",
            duckdb::params![r#"["usage_method.observe","shell.exec"]"#],
        )
        .unwrap();
        // 模拟缺失：删三类各一行 + 一记忆 agent 行。
        conn.execute(
            "DELETE FROM agent WHERE id IN \
             ('execution-platform', 'attention-agent', 'subagent.template.scheduled')",
            [],
        )
        .unwrap();

        upgrade_seed_deltas(&conn, dir.path()).unwrap();

        // 缺失行被补回。
        for id in [
            "execution-platform",
            "attention-agent",
            "subagent.template.scheduled",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM agent WHERE id = ?",
                    duckdb::params![id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{id} 应被补插");
        }
        // 补插行含内置 allowlist（execution-platform 9 能力）。
        let exec_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = 'execution-platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let exec_value: serde_json::Value = serde_json::from_str(&exec_text.unwrap()).unwrap();
        let exec_ids: Vec<&str> = exec_value
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for expected in [
            "subagent.create",
            "subagent.run",
            "permission.grant",
            "permission.revoke",
            "method.invoke",
        ] {
            assert!(
                exec_ids.contains(&expected),
                "execution-platform 缺 {expected}"
            );
        }
        // 用户改的 allowlist 保持。
        let insight_text: Option<String> = conn
            .query_row(
                "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = 'insight-platform'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            insight_text.unwrap(),
            r#"["usage_method.observe","shell.exec"]"#,
            "已存在行（含用户修改）不得被覆盖"
        );

        // 幂等：再跑一遍，行数与 allowlist 均不变。
        let total_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent", [], |row| row.get(0))
            .unwrap();
        upgrade_seed_deltas(&conn, dir.path()).unwrap();
        let total_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total_before, total_after, "重复启动无副作用");
    }
}
