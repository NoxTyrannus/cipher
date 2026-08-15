#[cfg(test)]
use super::base::BaseCapability;
use super::base::Schema;
use crate::common::{AgentError, Result};
use crate::data::duckdb::Registry;
use crate::data::thought_store::ThoughtStore;
use crate::data::triviumdb::TriviumDb;
use crate::logic::builtin::host_context::HostContext;
#[cfg(test)]
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum ReloadEvent {
    CapabilityTable(String),
    Agent,
}

pub struct CapabilityExecutor {
    #[cfg(test)]
    registry: HashMap<String, Arc<dyn BaseCapability>>,
    host_context: HostContext,
    duckdb: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
    triviumdb: Option<Arc<std::sync::Mutex<TriviumDb>>>,
    thought_store: Option<Arc<ThoughtStore>>,
    reload_tx: Option<mpsc::Sender<ReloadEvent>>,
}

const ALLOWED_TABLES: &[&str] = &[
    "base_capability",
    "composite_capability",
    "usage_method",
    "agent",
];

impl CapabilityExecutor {
    pub fn set_workspace_root(&mut self, workspace_root: &Path) {
        self.host_context = HostContext::for_workspace(workspace_root.to_path_buf());
    }

    fn execute_builtin(&self, _id: &str, builtin_name: &str, input: &Schema) -> Result<Schema> {
        let host = &self.host_context;
        let result: std::result::Result<Schema, String> = match builtin_name {
            "file.read" => crate::logic::builtin::host_functions::host_file_read(host, input),
            "file.write" => crate::logic::builtin::host_functions::host_file_write(host, input),
            "file.list" => crate::logic::builtin::host_functions::host_file_list(host, input),
            "file.delete" => crate::logic::builtin::host_functions::host_file_delete(host, input),
            "file.move" => crate::logic::builtin::host_functions::host_file_move(host, input),
            "file.chunk_read" => {
                crate::logic::builtin::host_functions::host_file_chunk_read(host, input)
            }
            "text.grep" => crate::logic::builtin::host_functions::host_text_grep(host, input),
            "shell.exec" | "powershell.exec" => {
                crate::logic::builtin::host_functions::host_shell_exec(host, input)
            }
            "code.exec" => crate::logic::builtin::host_functions::host_code_exec(host, input),
            "capability.import" => {
                let conn = self.duckdb.as_ref().ok_or_else(|| {
                    AgentError::NotFound("capability.import: duckdb not configured".into())
                })?;
                let guard = conn.lock().map_err(|e| {
                    AgentError::Script(format!("builtin capability.import: lock poisoned: {e}"))
                })?;
                return super::import::capability_import(&guard, &self.reload_tx, input)
                    .map_err(|e| AgentError::Script(format!("builtin capability.import: {e}")));
            }
            "db.insert" => return self.builtin_db_insert(input),
            "db.update" => return self.builtin_db_update(input),
            "db.delete" => return self.builtin_db_delete(input),
            "db.query" => return self.builtin_db_query(input),
            name if name.starts_with("memory.") => return self.execute_memory(name, input),
            _ => {
                return Err(AgentError::NotFound(format!(
                    "builtin executor: {builtin_name}"
                )))
            }
        };
        result.map_err(|e| AgentError::Script(format!("builtin {builtin_name}: {e}")))
    }
    pub fn new() -> Self {
        Self {
            #[cfg(test)]
            registry: HashMap::new(),
            host_context: HostContext::deny_all(),
            duckdb: None,
            triviumdb: None,
            thought_store: None,
            reload_tx: None,
        }
    }

    pub fn set_duckdb(&mut self, db: Arc<std::sync::Mutex<duckdb::Connection>>) {
        self.duckdb = Some(db);
    }

    pub fn set_triviumdb(&mut self, db: Arc<std::sync::Mutex<TriviumDb>>) {
        self.triviumdb = Some(db);
    }

    pub fn set_thought_store(&mut self, store: Arc<ThoughtStore>) {
        self.thought_store = Some(store);
    }

    pub fn set_reload_tx(&mut self, tx: mpsc::Sender<ReloadEvent>) {
        self.reload_tx = Some(tx);
    }

    #[cfg(test)]
    pub fn register(&mut self, cap: Arc<dyn BaseCapability>) {
        self.registry.insert(cap.id().to_string(), cap);
    }

    pub fn execute(&self, id: &str, registry: &Registry, input: &Schema) -> Result<Schema> {
        if let Some(row) = registry.base_capabilities.get(id) {
            if let Some(builtin_name) = row.executor.strip_prefix("builtin:") {
                return self.execute_builtin(id, builtin_name, input);
            }

            #[cfg(test)]
            {
                if let Some(cap) = self.registry.get(&row.executor) {
                    return cap.execute(input);
                }
            }
            return Err(AgentError::NotFound(format!(
                "base capability executor: {}",
                row.executor
            )));
        }
        #[cfg(test)]
        if let Some(cap) = self.registry.get(id) {
            return cap.execute(input);
        }
        Err(AgentError::NotFound(format!("base capability: {}", id)))
    }

    fn execute_memory(&self, builtin_name: &str, input: &Schema) -> Result<Schema> {
        let db = self.triviumdb.as_ref().ok_or_else(|| {
            AgentError::NotFound("memory capability: triviumdb not configured".into())
        })?;
        let mut guard = db.lock().map_err(|e| {
            AgentError::Script(format!(
                "builtin {builtin_name}: triviumdb lock poisoned: {e}"
            ))
        })?;
        let raw = match builtin_name {
            "memory.list" => super::memory::memory_list(&guard, input),
            "memory.retrieve" => super::memory::memory_retrieve(&guard, input),
            "memory.delete" => super::memory::memory_delete(&mut guard, input),
            "memory.attention.write" => super::memory::memory_attention_write(&mut guard, input),
            "memory.attention.retire" => super::memory::memory_attention_retire(&mut guard, input),
            "memory.experience.write" => super::memory::memory_experience_write(&mut guard, input),
            "memory.preference.write" => super::memory::memory_preference_write(&mut guard, input),
            "memory.cognitive.update" => super::memory::memory_cognitive_update(&mut guard, input),
            "memory.evidence.lookup" => {
                return super::memory::memory_evidence_lookup(self.thought_store.as_ref(), input)
                    .map_err(|e| AgentError::Script(format!("builtin {builtin_name}: {e}")))
            }
            other => return Err(AgentError::NotFound(format!("builtin executor: {other}"))),
        };
        raw.map_err(|e| AgentError::Script(format!("builtin {builtin_name}: {e}")))
    }

    fn builtin_db_insert(&self, input: &Schema) -> Result<Schema> {
        let table = input
            .get("table")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Parse("db.insert: missing 'table'".into()))?;
        if !ALLOWED_TABLES.contains(&table) {
            return Err(AgentError::NotFound(format!(
                "db.insert: table '{table}' not allowed"
            )));
        }
        let data = input
            .get("data")
            .ok_or_else(|| AgentError::Parse("db.insert: missing 'data'".into()))?;
        let db = self
            .duckdb
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("db.insert: duckdb not configured".into()))?;
        let conn = db.lock().unwrap();
        let obj = data
            .as_object()
            .ok_or_else(|| AgentError::Parse("db.insert: data must be object".into()))?;
        let columns: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        let placeholders: Vec<String> = (0..columns.len()).map(|i| format!("${}", i + 1)).collect();
        let sql = format!(
            "INSERT OR REPLACE INTO {table} ({}) VALUES ({})",
            columns.join(", "),
            placeholders.join(", ")
        );
        let params: Vec<String> = obj
            .values()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        let param_refs: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
        let rows = conn
            .execute(&sql, param_refs.as_slice())
            .map_err(|e| AgentError::Bootstrap(format!("db.insert: {e}")))?;
        self.trigger_reload(table);
        Ok(serde_json::json!({"rows_affected": rows}))
    }

    fn builtin_db_update(&self, input: &Schema) -> Result<Schema> {
        let table = input
            .get("table")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Parse("db.update: missing 'table'".into()))?;
        if !ALLOWED_TABLES.contains(&table) {
            return Err(AgentError::NotFound(format!(
                "db.update: table '{table}' not allowed"
            )));
        }
        let data = input
            .get("data")
            .ok_or_else(|| AgentError::Parse("db.update: missing 'data'".into()))?;
        let where_clause = input.get("where").and_then(|v| v.as_str()).unwrap_or("1=1");
        let db = self
            .duckdb
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("db.update: duckdb not configured".into()))?;
        let conn = db.lock().unwrap();
        let obj = data
            .as_object()
            .ok_or_else(|| AgentError::Parse("db.update: data must be object".into()))?;
        let set_clause: Vec<String> = obj
            .keys()
            .enumerate()
            .map(|(i, k)| format!("{k} = ${}", i + 1))
            .collect();
        let sql = format!(
            "UPDATE {table} SET {} WHERE {where_clause}",
            set_clause.join(", ")
        );
        let params: Vec<String> = obj
            .values()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        let param_refs: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
        let rows = conn
            .execute(&sql, param_refs.as_slice())
            .map_err(|e| AgentError::Bootstrap(format!("db.update: {e}")))?;
        self.trigger_reload(table);
        Ok(serde_json::json!({"rows_affected": rows}))
    }

    fn builtin_db_delete(&self, input: &Schema) -> Result<Schema> {
        let table = input
            .get("table")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Parse("db.delete: missing 'table'".into()))?;
        if !ALLOWED_TABLES.contains(&table) {
            return Err(AgentError::NotFound(format!(
                "db.delete: table '{table}' not allowed"
            )));
        }
        let where_clause = input
            .get("where")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Parse("db.delete: missing 'where'".into()))?;
        let db = self
            .duckdb
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("db.delete: duckdb not configured".into()))?;
        let conn = db.lock().unwrap();
        let sql = format!("DELETE FROM {table} WHERE {where_clause}");
        let rows = conn
            .execute(&sql, [])
            .map_err(|e| AgentError::Bootstrap(format!("db.delete: {e}")))?;
        self.trigger_reload(table);
        Ok(serde_json::json!({"rows_affected": rows}))
    }

    fn builtin_db_query(&self, input: &Schema) -> Result<Schema> {
        let table = input
            .get("table")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Parse("db.query: missing 'table'".into()))?;
        if !ALLOWED_TABLES.contains(&table) {
            return Err(AgentError::NotFound(format!(
                "db.query: table '{table}' not allowed"
            )));
        }
        let where_clause = input.get("where").and_then(|v| v.as_str()).unwrap_or("1=1");
        let db = self
            .duckdb
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("db.query: duckdb not configured".into()))?;
        let conn = db.lock().unwrap();
        let sql = format!("SELECT * FROM {table} WHERE {where_clause}");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AgentError::Bootstrap(format!("db.query prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let n = row.as_ref().column_count();
                let mut obj = serde_json::Map::new();
                for i in 0..n {
                    let name = row
                        .as_ref()
                        .column_name(i)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|_| "unknown".to_string());
                    let val: String = row.get::<_, String>(i).unwrap_or_default();
                    obj.insert(name, serde_json::Value::String(val));
                }
                Ok(serde_json::Value::Object(obj))
            })
            .map_err(|e| AgentError::Bootstrap(format!("db.query: {e}")))?;
        let results: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();
        Ok(serde_json::json!({"rows": results}))
    }

    fn trigger_reload(&self, table: &str) {
        if let Some(tx) = &self.reload_tx {
            let event = if table == "agent" {
                ReloadEvent::Agent
            } else {
                ReloadEvent::CapabilityTable(table.to_string())
            };
            let _ = tx.try_send(event);
        }
    }
}

impl Default for CapabilityExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::duckdb::loader::BaseCapabilityRow;

    struct EchoCap;
    impl BaseCapability for EchoCap {
        fn id(&self) -> &'static str {
            "echo"
        }
        fn name(&self) -> &'static str {
            "Echo"
        }
        fn execute(&self, input: &Schema) -> Result<Schema> {
            Ok(input.clone())
        }
    }

    #[test]
    fn executor_register_and_execute() {
        let mut ex = CapabilityExecutor::new();
        ex.register(Arc::new(EchoCap));
        let input = serde_json::json!({"x": 1});
        let out = ex.execute("echo", &Registry::new(), &input).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn executor_unknown_id_returns_not_found() {
        let ex = CapabilityExecutor::new();
        let r = ex.execute("nope", &Registry::new(), &serde_json::json!({}));
        assert!(matches!(r, Err(AgentError::NotFound(_))));
    }

    #[test]
    fn builtin_file_read_repro() {
        let mut reg = Registry::new();
        reg.base_capabilities.insert(
            "file.read".into(),
            BaseCapabilityRow {
                id: "file.read".into(),
                name: "Read File".into(),
                cap_type: "function".into(),
                description: "read".into(),
                schema_in: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                schema_out: serde_json::json!({}),
                executor: "builtin:file.read".into(),
                version: "1.0.0".into(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        let mut ex = CapabilityExecutor::new();
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        ex.set_workspace_root(repo_root);
        let out = ex.execute(
            "file.read",
            &reg,
            &serde_json::json!({"path": "Cargo.toml"}),
        );
        assert!(out.is_ok(), "正确 path 应可读: {:?}", out.err());

        let bad = ex.execute(
            "file.read",
            &reg,
            &serde_json::json!({"path": "读一下 Cargo.toml"}),
        );
        let bad = bad.expect("散文 path 现在返回结构化错误而非 trap");
        assert_eq!(
            bad.get("success").and_then(|v| v.as_bool()),
            Some(false),
            "散文 path 必须返回 success=false: {bad}"
        );
        assert!(
            bad.get("error").and_then(|v| v.as_str()).is_some(),
            "结构化错误必须含 error 字段: {bad}"
        );
    }
}
