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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum ReloadEvent {
    CapabilityTable(String),
    Agent,
}

/// subagent spawn hook（v0.3.1 §4.3）：TB runtime 由 TC 安装到 executor。
///
/// 执行中台受理 `subagent.create / subagent.run` 后通过本 hook 通知外部 runtime；
/// hook 未安装时分子仍完成持久化并正常返回。
pub trait SubagentSpawnHook: Send + Sync {
    /// 同步通知（runtime 侧如需异步，自行 spawn）。
    fn notify(&self, event: SubagentSpawnEvent);
}

/// spawn hook 事件。
#[derive(Debug, Clone)]
pub enum SubagentSpawnEvent {
    /// `subagent.create` 完成，实例已持久化并初始化记忆文件。
    Created { subagent_id: String },
    /// `subagent.run` 受理：携带冻结定义快照、本轮输入与 invocation 事实引用。
    RunAccepted {
        definition: crate::agent::execution_types::SubagentDefinition,
        task_input: String,
        invocation_id: String,
    },
}

pub struct CapabilityExecutor {
    #[cfg(test)]
    registry: HashMap<String, Arc<dyn BaseCapability>>,
    host_context: HostContext,
    duckdb: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
    triviumdb: Option<Arc<std::sync::Mutex<TriviumDb>>>,
    thought_store: Option<Arc<ThoughtStore>>,
    reload_tx: Option<mpsc::Sender<ReloadEvent>>,
    /// `<storage_root>`（全局 invocation 日志与 subagent 记忆文件的根目录）。
    storage_root: Option<PathBuf>,
    /// subagent spawn hook（TC 接线安装 TB runtime）。
    ///
    /// 使用读写锁共享引用：TC 在把 executor 包装为 Arc 之后仍可安装 runtime hook。
    subagent_spawn_hook: std::sync::RwLock<Option<Arc<dyn SubagentSpawnHook>>>,
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

    fn execute_builtin(
        &self,
        actor_id: &str,
        _id: &str,
        builtin_name: &str,
        input: &Schema,
    ) -> Result<Schema> {
        let host = &self.host_context;
        let result: std::result::Result<Schema, String> = match builtin_name {
            "path.exists" => crate::logic::builtin::host_functions::host_path_exists(host, input),
            "file.glob" => crate::logic::builtin::host_functions::host_file_glob(host, input),
            "json.validate" => {
                crate::logic::builtin::host_functions::host_json_validate(host, input)
            }
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
            name if name.starts_with("subagent.") || name == "usage_method.observe" => {
                return self.execute_subagent_molecule(name, input)
            }
            "permission.grant" | "permission.revoke" => {
                return self.execute_permission(actor_id, builtin_name, input)
            }
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
            storage_root: None,
            subagent_spawn_hook: std::sync::RwLock::new(None),
        }
    }

    pub fn set_duckdb(&mut self, db: Arc<std::sync::Mutex<duckdb::Connection>>) {
        self.duckdb = Some(db);
    }

    /// 设置全局 invocation 日志与 subagent 记忆文件的根目录（`<storage_root>`）。
    pub fn set_storage_root(&mut self, storage_root: &Path) {
        self.storage_root = Some(storage_root.to_path_buf());
    }

    /// 安装 subagent spawn hook（Arc 共享安装入口；未安装时分子仍完成持久化）。
    pub fn set_subagent_spawn_hook(&self, hook: Arc<dyn SubagentSpawnHook>) {
        if let Ok(mut slot) = self.subagent_spawn_hook.write() {
            *slot = Some(hook);
        }
    }

    pub fn set_triviumdb(&mut self, db: Arc<std::sync::Mutex<TriviumDb>>) {
        self.triviumdb = Some(db);
    }

    pub fn set_thought_store(&mut self, store: Arc<ThoughtStore>) {
        self.thought_store = Some(store);
    }

    /// 从 duckdb 重新加载注册表；自扩展能力导入后调用，使当前进程可见新能力。
    pub fn reload_registry(&self) -> Option<crate::data::duckdb::Registry> {
        let db = self.duckdb.as_ref()?;
        let conn = db.lock().ok()?;
        crate::data::duckdb::loader::load_all_into_memory(&conn).ok()
    }

    pub fn set_reload_tx(&mut self, tx: mpsc::Sender<ReloadEvent>) {
        self.reload_tx = Some(tx);
    }

    #[cfg(test)]
    pub fn register(&mut self, cap: Arc<dyn BaseCapability>) {
        self.registry.insert(cap.id().to_string(), cap);
    }

    /// 执行一个 base capability 的 executor 入口。
    ///
    /// `actor_id` 是调用方 agent（授权审计的 granter/执行者身份）；`id` 为
    /// registry 契约 id；`registry` 为只读注册表快照；`input` 为已通过
    /// schema_in 校验的参数。
    pub fn execute(
        &self,
        actor_id: &str,
        id: &str,
        registry: &Registry,
        input: &Schema,
    ) -> Result<Schema> {
        if let Some(row) = registry.base_capabilities.get(id) {
            if let Some(builtin_name) = row.executor.strip_prefix("builtin:") {
                return self.execute_builtin(actor_id, id, builtin_name, input);
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

    /// 分发 `permission.grant` / `permission.revoke` 到 permission 模块。
    ///
    /// `actor_id` 即 granter/revoker（审计 granter_agent 字段）；成功路径触发
    /// agent 表 reload（授权叠加/回收后，subagent 下一次 run 刷新 registry 即生效）。
    fn execute_permission(
        &self,
        actor_id: &str,
        builtin_name: &str,
        input: &Schema,
    ) -> Result<Schema> {
        let db = self.duckdb.as_ref().ok_or_else(|| {
            AgentError::NotFound(format!("{builtin_name}: duckdb not configured"))
        })?;
        let conn = db.lock().map_err(|e| {
            AgentError::Script(format!("builtin {builtin_name}: lock poisoned: {e}"))
        })?;
        let result = super::permission::execute(&conn, actor_id, builtin_name, input);
        if result.is_ok() {
            self.trigger_reload("agent");
        }
        result.map_err(|e| AgentError::Script(format!("builtin {builtin_name}: {e}")))
    }

    /// 回收钩子（`CapabilityService` 判定 one-shot 已用 / ttl 已过期后调用）：
    /// 从目标 allowlist 移除能力 + 审计行置终态（used/expired）+ 触发 reload。
    ///
    /// 失败仅记录（tracing::warn），不反向污染已经成功的调用——授权回收是
    /// 持久层维护职责，判定层已按快照放行/拒绝。
    pub fn reclaim_permission_grant(
        &self,
        target_agent: &str,
        capability_id: &str,
        status: &str,
    ) -> Result<()> {
        let db = self.duckdb.as_ref().ok_or_else(|| {
            AgentError::NotFound("permission reclaim: duckdb not configured".into())
        })?;
        let conn = db
            .lock()
            .map_err(|e| AgentError::Script(format!("permission reclaim: lock poisoned: {e}")))?;
        super::permission::reclaim(&conn, target_agent, capability_id, status)
            .map_err(|e| AgentError::Script(format!("permission reclaim: {e}")))?;
        self.trigger_reload("agent");
        Ok(())
    }

    /// 分发六个 `subagent.*` 分子与 `usage_method.observe` 到 `subagent_capability` 模块。
    fn execute_subagent_molecule(&self, builtin_name: &str, input: &Schema) -> Result<Schema> {
        let db = self.duckdb.as_ref().ok_or_else(|| {
            AgentError::NotFound(format!("{builtin_name}: duckdb not configured"))
        })?;
        let storage_root = self.storage_root.as_ref().ok_or_else(|| {
            AgentError::NotFound(format!("{builtin_name}: storage_root not configured"))
        })?;
        let hook = self
            .subagent_spawn_hook
            .read()
            .ok()
            .and_then(|slot| slot.clone());
        // 修法 2（任务书 §3.3）：分子内部自行短锁，本层不再持 duckdb 锁调用分子。
        crate::agent::subagent_capability::execute_subagent_capability(
            db,
            storage_root,
            builtin_name,
            input,
            hook.as_deref(),
        )
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
        let out = ex
            .execute("actor", "echo", &Registry::new(), &input)
            .unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn executor_unknown_id_returns_not_found() {
        let ex = CapabilityExecutor::new();
        let r = ex.execute("actor", "nope", &Registry::new(), &serde_json::json!({}));
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
            "actor",
            "file.read",
            &reg,
            &serde_json::json!({"path": "Cargo.toml"}),
        );
        assert!(out.is_ok(), "正确 path 应可读: {:?}", out.err());

        let bad = ex.execute(
            "actor",
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
