#[cfg(test)]
use super::base::BaseCapability;
use super::base::Schema;
use crate::common::{AgentError, Result};
use crate::data::duckdb::loader::BaseCapabilityRow;
use crate::data::duckdb::Registry;
#[cfg(test)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    wasm_modules_dir: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    duckdb: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
    reload_tx: Option<mpsc::Sender<ReloadEvent>>,
}

const ALLOWED_TABLES: &[&str] = &[
    "base_capability",
    "composite_capability",
    "usage_method",
    "agent",
];

impl CapabilityExecutor {
    pub fn set_wasm(&mut self, wasm_dir: &Path, workspace_root: &Path) {
        self.wasm_modules_dir = Some(wasm_dir.to_path_buf());
        self.workspace_root = Some(workspace_root.to_path_buf());
    }
    fn execute_builtin(&self, _id: &str, builtin_name: &str, input: &Schema) -> Result<Schema> {
        match builtin_name {
            "db.insert" => self.builtin_db_insert(input),
            "db.update" => self.builtin_db_update(input),
            "db.delete" => self.builtin_db_delete(input),
            "db.query" => self.builtin_db_query(input),
            _ => Err(AgentError::NotFound(format!(
                "builtin executor: {builtin_name}"
            ))),
        }
    }
    pub fn new() -> Self {
        Self {
            #[cfg(test)]
            registry: HashMap::new(),
            wasm_modules_dir: None,
            workspace_root: None,
            duckdb: None,
            reload_tx: None,
        }
    }

    pub fn set_duckdb(&mut self, db: Arc<std::sync::Mutex<duckdb::Connection>>) {
        self.duckdb = Some(db);
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
            if let Some(module_name) = row.executor.strip_prefix("wasm:") {
                return self.execute_wasm(id, row, input, module_name);
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

    fn execute_wasm(
        &self,
        _id: &str,
        _row: &BaseCapabilityRow,
        input: &Schema,
        module_name: &str,
    ) -> Result<Schema> {
        let wasm_dir = self
            .wasm_modules_dir
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("wasm_modules_dir not configured".into()))?;
        let ws_root = self.workspace_root.clone().unwrap_or_default();
        let module_path = wasm_dir.join(format!("{}.wat", module_name.replace('.', "_")));
        if !module_path.exists() {
            return Err(AgentError::NotFound(format!(
                "wasm module not found: {:?}",
                module_path
            )));
        }
        let runtime = crate::logic::script::WasmRuntime::new()
            .map_err(|e| AgentError::Script(format!("runtime init: {e}")))?;
        let host_ctx = crate::logic::script::host_context::HostContext {
            permission: crate::logic::script::host_context::PermissionSnapshot {
                file_read_roots: vec![ws_root.clone()],
                file_write_roots: vec![ws_root.clone()],
                shell_exec_allowed: true,
                ..Default::default()
            },
            budget: crate::logic::script::host_context::BudgetSnapshot::default(),
            duckdb: None,
            triviumdb: None,
        };
        let input_str = serde_json::to_string(input)
            .map_err(|e| AgentError::Script(format!("serialize input: {e}")))?;
        let output_str = runtime
            .run_with_host(&module_path, &input_str, host_ctx)
            .map_err(|e| AgentError::Script(format!("wasm run: {e}")))?;
        serde_json::from_str(&output_str)
            .map_err(|e| AgentError::Script(format!("parse output: {e}")))
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
    fn wasm_file_read_repro() {
        use crate::data::duckdb::loader::BaseCapabilityRow;
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
                executor: "wasm:file.read".into(),
                version: "1.0.0".into(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        let mut ex = CapabilityExecutor::new();
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        ex.set_wasm(&repo_root.join("data/wasm"), repo_root);
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
