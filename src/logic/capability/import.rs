use crate::common::AgentError;
use serde_json::Value;
use std::collections::HashSet;

pub const BUILTIN_EXECUTORS: &[&str] = &[
    "builtin:path.exists",
    "builtin:file.glob",
    "builtin:json.validate",
    "builtin:file.read",
    "builtin:file.write",
    "builtin:file.list",
    "builtin:file.delete",
    "builtin:file.move",
    "builtin:file.chunk_read",
    "builtin:text.grep",
    "builtin:shell.exec",
    "builtin:powershell.exec",
    "builtin:code.exec",
    "builtin:db.insert",
    "builtin:db.update",
    "builtin:db.delete",
    "builtin:db.query",
    "builtin:capability.import",
    "builtin:memory.list",
    "builtin:memory.retrieve",
    "builtin:memory.delete",
    "builtin:memory.attention.write",
    "builtin:memory.attention.retire",
    "builtin:memory.experience.write",
    "builtin:memory.preference.write",
    "builtin:memory.cognitive.update",
    "builtin:memory.evidence.lookup",
    "builtin:subagent.create",
    "builtin:subagent.update",
    "builtin:subagent.run",
    "builtin:subagent.sleep",
    "builtin:subagent.wake",
    "builtin:subagent.delete",
    "builtin:usage_method.observe",
    "builtin:method.invoke",
    "builtin:web.fetch.public",
];

fn fail(msg: impl Into<String>) -> Result<Value, String> {
    Ok(serde_json::json!({"success": false, "error": msg.into()}))
}

fn array(args: &Value, field: &str) -> Vec<Value> {
    args.get(field)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn string_field<'a>(row: &'a Value, field: &str) -> Result<&'a str, String> {
    row.get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("capability.import: row missing string field '{field}'"))
}

fn validate_schema_json(value: &Value, field: &str) -> Result<(), String> {
    if !value.is_object() {
        return Err(format!("capability.import: {field} must be an object"));
    }
    jsonschema::validator_for(value)
        .map_err(|_| format!("capability.import: {field} is not a valid JSON Schema"))?;
    Ok(())
}

fn validate_dag_json(value: &Value, field: &str) -> Result<Vec<Value>, String> {
    let nodes: Vec<Value> = serde_json::from_value(value.clone())
        .map_err(|_| format!("capability.import: {field} must be a JSON array"))?;
    for node in &nodes {
        if node.get("id").and_then(|v| v.as_str()).is_none() {
            return Err(format!("capability.import: {field} node missing id"));
        }
        if node
            .get("base_capability")
            .and_then(|v| v.as_str())
            .is_none()
        {
            return Err(format!(
                "capability.import: {field} node missing base_capability"
            ));
        }
    }
    Ok(nodes)
}

pub fn capability_import(
    conn: &duckdb::Connection,
    reload_tx: &Option<tokio::sync::mpsc::Sender<crate::logic::capability::executor::ReloadEvent>>,
    args: &Value,
) -> Result<Value, String> {
    let base_rows = array(args, "base_capabilities");
    let comp_rows = array(args, "composite_capabilities");
    let usage_rows = array(args, "usage_methods");
    if base_rows.is_empty() && comp_rows.is_empty() && usage_rows.is_empty() {
        return fail("capability.import: at least one definition array must be non-empty");
    }

    // 1) 先校验，全部通过才写入（原子导入）。
    let allowed_builtins: HashSet<&str> = BUILTIN_EXECUTORS.iter().copied().collect();
    let mut base_ids: HashSet<String> = existing_executable_ids(conn)?;
    let mut imported_base: Vec<Value> = Vec::new();
    for row in &base_rows {
        let id = string_field(row, "id")?.to_string();
        let executor = string_field(row, "executor")?.to_string();
        if executor.starts_with("builtin:") && !allowed_builtins.contains(executor.as_str()) {
            return fail(format!(
                "capability.import: base '{id}' executor '{executor}' is not an allowed builtin"
            ));
        }
        if !executor.starts_with("builtin:") {
            return fail(format!(
                "capability.import: base '{id}' executor must use a builtin:* executor"
            ));
        }
        validate_schema_json(
            row.get("schema_in")
                .unwrap_or(&serde_json::json!({"type": "object"})),
            &format!("base '{id}' schema_in"),
        )?;
        validate_schema_json(
            row.get("schema_out")
                .unwrap_or(&serde_json::json!({"type": "object"})),
            &format!("base '{id}' schema_out"),
        )?;
        base_ids.insert(id.clone());
        imported_base.push(row.clone());
    }

    let mut imported_comp: Vec<(Value, Vec<Value>)> = Vec::new();
    for row in &comp_rows {
        let id = string_field(row, "id")?.to_string();
        let dag = validate_dag_json(
            row.get("dag").unwrap_or(&Value::Null),
            &format!("composite '{id}' dag"),
        )?;
        for node in &dag {
            let base = node
                .get("base_capability")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !base_ids.contains(base) {
                return fail(format!(
                    "capability.import: composite '{id}' references unknown base capability '{base}'"
                ));
            }
        }
        validate_schema_json(
            row.get("schema_in")
                .unwrap_or(&serde_json::json!({"type": "object"})),
            &format!("composite '{id}' schema_in"),
        )?;
        validate_schema_json(
            row.get("schema_out")
                .unwrap_or(&serde_json::json!({"type": "object"})),
            &format!("composite '{id}' schema_out"),
        )?;
        base_ids.insert(id.clone());
        imported_comp.push((row.clone(), dag));
    }

    let mut imported_usage: Vec<Value> = Vec::new();
    for row in &usage_rows {
        let id = string_field(row, "id")?.to_string();
        let capability_id = string_field(row, "capability_id")?.to_string();
        if !base_ids.contains(&capability_id) {
            return fail(format!(
                "capability.import: usage_method '{id}' references unknown capability_id '{capability_id}'"
            ));
        }
        imported_usage.push(row.clone());
    }

    // 2) 写入。
    for row in &imported_base {
        insert_base(conn, row).map_err(|e| e.to_string())?;
    }
    for (row, dag) in &imported_comp {
        insert_composite(conn, row, dag).map_err(|e| e.to_string())?;
    }
    for row in &imported_usage {
        insert_usage(conn, row).map_err(|e| e.to_string())?;
    }

    // 3) 可选授权：导入的能力同时授予指定 agent（自迭代闭环）。
    let grant_to_agent = args.get("grant_to_agent").and_then(|v| v.as_str());
    if let Some(agent_id) = grant_to_agent.filter(|s| !s.trim().is_empty()) {
        let mut caps: Vec<String> = imported_base
            .iter()
            .filter_map(|row| {
                row.get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .chain(imported_comp.iter().filter_map(|(row, _)| {
                row.get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            }))
            .collect();
        for cap in load_agent_capability_allowlist(conn, agent_id)? {
            if !caps.iter().any(|c| c == &cap) {
                caps.push(cap);
            }
        }
        let caps_json = serde_json::to_string(&caps)
            .map_err(|e| format!("capability.import: serialize grants: {e}"))?;
        conn.execute(
            "UPDATE agent SET capability_allowlist = CAST(? AS JSON) WHERE id = ?",
            duckdb::params![caps_json, agent_id],
        )
        .map_err(|e| format!("capability.import: grant to {agent_id}: {e}"))?;
    }

    if let Some(tx) = reload_tx {
        let _ = tx.try_send(
            crate::logic::capability::executor::ReloadEvent::CapabilityTable(
                "base_capability".to_string(),
            ),
        );
        let _ = tx.try_send(
            crate::logic::capability::executor::ReloadEvent::CapabilityTable(
                "composite_capability".to_string(),
            ),
        );
        if grant_to_agent.is_some() {
            let _ = tx.try_send(crate::logic::capability::executor::ReloadEvent::Agent);
        }
    }

    Ok(serde_json::json!({
        "success": true,
        "granted_to_agent": grant_to_agent,
        "imported": {
            "base_capabilities": imported_base.len(),
            "composite_capabilities": imported_comp.len(),
            "usage_methods": imported_usage.len(),
        }
    }))
}

fn existing_executable_ids(conn: &duckdb::Connection) -> Result<HashSet<String>, String> {
    let mut ids = HashSet::new();
    for table in ["base_capability", "composite_capability"] {
        let sql = format!("SELECT id FROM {table} WHERE enabled = true AND tombstoned_at IS NULL");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("capability.import prepare {table}: {e}"))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("capability.import query {table}: {e}"))?;
        for row in rows {
            ids.insert(row.map_err(|e| format!("capability.import row {table}: {e}"))?);
        }
    }
    Ok(ids)
}

fn load_agent_capability_allowlist(
    conn: &duckdb::Connection,
    agent_id: &str,
) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = ?")
        .map_err(|e| format!("capability.import prepare agent caps: {e}"))?;
    let mut rows = stmt
        .query_map([agent_id], |row| row.get::<_, String>(0))
        .map_err(|e| format!("capability.import query agent caps: {e}"))?;
    let Some(row) = rows.next() else {
        return Ok(Vec::new());
    };
    let text = row.map_err(|e| format!("capability.import read agent caps: {e}"))?;
    serde_json::from_str::<Vec<String>>(&text)
        .map_err(|e| format!("capability.import parse agent caps: {e}"))
}

fn insert_base(conn: &duckdb::Connection, row: &Value) -> Result<(), AgentError> {
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let name = row.get("name").and_then(|v| v.as_str()).unwrap_or(id);
    let cap_type = row
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("function");
    let description = row
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let schema_in = row
        .get("schema_in")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
    let schema_out = row
        .get("schema_out")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
    let executor = row.get("executor").and_then(|v| v.as_str()).unwrap_or("");
    let version = row
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0");
    let enabled = row.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let metadata = row.get("metadata").cloned().unwrap_or_else(|| {
        serde_json::json!({"partition": row.get("partition").and_then(|v| v.as_str()).unwrap_or("user")})
    });
    conn.execute(
        "INSERT OR REPLACE INTO base_capability \
         (id, name, type, description, schema_in, schema_out, executor, version, enabled, metadata) \
         VALUES (?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, ?, ?, CAST(? AS JSON))",
        duckdb::params![
            id,
            name,
            cap_type,
            description,
            schema_in.to_string(),
            schema_out.to_string(),
            executor,
            version,
            enabled,
            metadata.to_string(),
        ],
    )
    .map_err(|e| AgentError::Bootstrap(format!("capability.import insert base '{id}': {e}")))?;
    Ok(())
}

fn insert_composite(
    conn: &duckdb::Connection,
    row: &Value,
    dag: &[Value],
) -> Result<(), AgentError> {
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let name = row.get("name").and_then(|v| v.as_str()).unwrap_or(id);
    let description = row
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let schema_in = row
        .get("schema_in")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
    let schema_out = row
        .get("schema_out")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
    let version = row
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0");
    let enabled = row.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let metadata = row.get("metadata").cloned().unwrap_or_else(|| {
        serde_json::json!({"partition": row.get("partition").and_then(|v| v.as_str()).unwrap_or("user")})
    });
    conn.execute(
        "INSERT OR REPLACE INTO composite_capability \
         (id, name, description, schema_in, schema_out, executor, dag, version, enabled, metadata) \
         VALUES (?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), 'dag', CAST(? AS JSON), ?, ?, CAST(? AS JSON))",
        duckdb::params![
            id,
            name,
            description,
            schema_in.to_string(),
            schema_out.to_string(),
            serde_json::to_string(dag).unwrap_or_else(|_| "[]".to_string()),
            version,
            enabled,
            metadata.to_string(),
        ],
    )
    .map_err(|e| AgentError::Bootstrap(format!("capability.import insert composite '{id}': {e}")))?;
    Ok(())
}

fn insert_usage(conn: &duckdb::Connection, row: &Value) -> Result<(), AgentError> {
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let capability_id = row
        .get("capability_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = row.get("name").and_then(|v| v.as_str()).unwrap_or(id);
    let prompt = row.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    let examples = row.get("examples").cloned().unwrap_or(Value::Null);
    let metadata = row.get("metadata").cloned().unwrap_or(Value::Null);
    conn.execute(
        "INSERT OR REPLACE INTO usage_method \
         (id, capability_id, name, prompt, examples, metadata) \
         VALUES (?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON))",
        duckdb::params![
            id,
            capability_id,
            name,
            prompt,
            examples.to_string(),
            metadata.to_string(),
        ],
    )
    .map_err(|e| AgentError::Bootstrap(format!("capability.import insert usage '{id}': {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_validation_rejects_unknown_builtin() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        let out = capability_import(
            &conn,
            &None,
            &serde_json::json!({
                "base_capabilities": [{
                    "id": "bad.exec",
                    "name": "Bad",
                    "description": "bad",
                    "schema_in": {"type":"object"},
                    "schema_out": {"type":"object"},
                    "executor": "builtin:bad.exec",
                    "version": "1.0.0"
                }]
            }),
        )
        .unwrap();
        assert_eq!(out["success"], false);
    }

    #[test]
    fn import_atom_into_registry() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO base_capability (id,name,type,description,schema_in,schema_out,executor,version,enabled) \
             VALUES ('file.read','Read File','function','read','{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"]}','{\"type\":\"object\"}','builtin:file.read','1.0.0',true);",
        )
        .unwrap();
        let out = capability_import(
            &conn,
            &None,
            &serde_json::json!({
                "composite_capabilities": [{
                    "id": "file.read_once",
                    "name": "Read Once",
                    "description": "read once",
                    "schema_in": {"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},
                    "schema_out": {"type":"object","properties":{"capability_id":{"type":"string"}}},
                    "dag": [{"id":"read","base_capability":"file.read","depends_on":[]}]
                }]
            }),
        )
        .unwrap();
        assert_eq!(out["success"], true);
        assert_eq!(out["imported"]["composite_capabilities"], 1);
    }

    #[test]
    fn import_grants_to_agent() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::schema::create_all_tables(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO agent (id,name,mode,capability_allowlist,is_default) VALUES ('agent','Agent','unni','[]',true);",
        )
        .unwrap();
        let out = capability_import(
            &conn,
            &None,
            &serde_json::json!({
                "grant_to_agent": "agent",
                "base_capabilities": [{
                    "id": "echo.test",
                    "name": "Echo Test",
                    "description": "echo",
                    "schema_in": {"type":"object"},
                    "schema_out": {"type":"object"},
                    "executor": "builtin:shell.exec",
                    "version": "1.0.0"
                }]
            }),
        )
        .unwrap();
        assert_eq!(out["success"], true);
        let caps = load_agent_capability_allowlist(&conn, "agent").unwrap();
        assert!(caps.contains(&"echo.test".to_string()));
    }
}
