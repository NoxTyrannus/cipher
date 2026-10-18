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
use std::sync::{Arc, RwLock};
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
    workspace_root: Arc<RwLock<PathBuf>>,
    extra_read_roots: Arc<RwLock<Vec<PathBuf>>>,
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
    /// `web.fetch.public` 域名白名单（来自 `[web] allowed_domains`，缺省空=拒绝全部）。
    web_allowed_domains: Vec<String>,
}

/// db.* 内置能力可达的表白名单（v0.5.11 D3 缺省：移除 `agent` 表——注册表回读走
/// 白名单收缩，agent 表不再经 db.* 可达；grant/seed/capability.import 等主链路
/// 均直写 SQL，不经 db.*）。
const ALLOWED_TABLES: &[&str] = &["base_capability", "composite_capability", "usage_method"];

#[cfg(not(test))]
fn is_allowed_table(table: &str) -> bool {
    ALLOWED_TABLES.contains(&table)
}

/// 测试编译期额外放行 db.* 类型回读基线的探针表（生产构建不可见）。
#[cfg(test)]
fn is_allowed_table(table: &str) -> bool {
    const TEST_TYPE_PROBE_TABLE: &str = "_db_query_type_probe";
    ALLOWED_TABLES.contains(&table) || table == TEST_TYPE_PROBE_TABLE
}

/// 列名标识符校验（v0.5.11 A6）：db.insert 的 JSON key 与 db.update 的 SET 列
/// 直接拼进 SQL，只放行 `[A-Za-z_][A-Za-z0-9_]*`，阻断经列名的 SQL 注入。
fn validate_column_ident(cap: &str, column: &str) -> Result<()> {
    let mut chars = column.chars();
    let valid = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(AgentError::Parse(format!(
            "{cap}: invalid column name '{column}' (must match [A-Za-z_][A-Za-z0-9_]*)"
        )))
    }
}

/// where 子句守卫（v0.5.11 A6）：where 原样拼接进 SQL，SELECT 场景可嵌
/// `IN (SELECT ...)` 子查询读到白名单外表（如 model 表的 key），因此 where 中
/// 出现 "select"（大小写不敏感）即拒绝。这是务实加固，不是完整 SQL 解析。
fn validate_where_clause(cap: &str, where_clause: &str) -> Result<()> {
    if where_clause.to_ascii_lowercase().contains("select") {
        return Err(AgentError::Parse(format!(
            "{cap}: subqueries are not allowed in 'where'"
        )));
    }
    Ok(())
}

/// db.query 行值 → JSON（v0.5.11 A6 类型修复）：按 duckdb `ValueRef` 类型映射，
/// NULL 保持 null（不再变空串）、布尔/数值保型、TIMESTAMP 转 RFC3339（UTC）；
/// 其余类型给可读字符串表示（Blob/Geometry 为 hex，Date 为 `YYYY-MM-DD`，
/// Decimal 用 Display，Enum 用标签，List/Struct/Map 等容器用 Debug）。
/// NaN/Infinity 浮点经 serde_json `From<f64>` 规则映射为 null（JSON 无非有限数）。
fn duckdb_value_ref_to_json(value: duckdb::types::ValueRef<'_>) -> serde_json::Value {
    use chrono::{DateTime, SecondsFormat, Utc};
    use duckdb::types::ValueRef;
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Boolean(v) => serde_json::Value::Bool(v),
        ValueRef::TinyInt(v) => serde_json::Value::from(i64::from(v)),
        ValueRef::SmallInt(v) => serde_json::Value::from(i64::from(v)),
        ValueRef::Int(v) => serde_json::Value::from(i64::from(v)),
        ValueRef::BigInt(v) => serde_json::Value::from(v),
        ValueRef::HugeInt(v) => i64::try_from(v)
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(v.to_string())),
        ValueRef::UTinyInt(v) => serde_json::Value::from(u64::from(v)),
        ValueRef::USmallInt(v) => serde_json::Value::from(u64::from(v)),
        ValueRef::UInt(v) => serde_json::Value::from(u64::from(v)),
        ValueRef::UBigInt(v) => serde_json::Value::from(v),
        ValueRef::UHugeInt(v) => u64::try_from(v)
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(v.to_string())),
        ValueRef::Float(v) => serde_json::Value::from(f64::from(v)),
        ValueRef::Double(v) => serde_json::Value::from(v),
        ValueRef::Text(bytes) => {
            serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned())
        }
        ValueRef::Timestamp(unit, raw) => {
            let micros = unit.to_micros(raw);
            DateTime::<Utc>::from_timestamp_micros(micros)
                .map(|dt| {
                    serde_json::Value::String(dt.to_rfc3339_opts(SecondsFormat::Micros, true))
                })
                .unwrap_or_else(|| {
                    serde_json::Value::String(format!("(timestamp out of range: {raw})"))
                })
        }
        ValueRef::Date32(days) => DateTime::<Utc>::from_timestamp(i64::from(days) * 86_400, 0)
            .map(|dt| serde_json::Value::String(dt.format("%Y-%m-%d").to_string()))
            .unwrap_or_else(|| serde_json::Value::String(format!("(date out of range: {days})"))),
        ValueRef::Blob(bytes) | ValueRef::Geometry(bytes) => {
            serde_json::Value::String(bytes.iter().map(|b| format!("{b:02x}")).collect())
        }
        ValueRef::Decimal(v) => serde_json::Value::String(v.to_string()),
        ValueRef::Enum(..) => serde_json::Value::String(
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|_| format!("{value:?}")),
        ),
        // Time64/Interval/List/Struct/Map/Array/Union 及未来新增类型。
        other => serde_json::Value::String(format!("{other:?}")),
    }
}

impl CapabilityExecutor {
    pub fn set_workspace_root(&self, workspace_root: &Path) {
        if let Ok(mut root) = self.workspace_root.write() {
            *root = workspace_root.to_path_buf();
        }
    }

    /// v0.4.8：把 `[fs] read_roots` 追加为额外文件读根（不改写根）。
    /// 需在 `set_workspace_root` 之后调用；extra 为空时行为与现状一致。
    pub fn set_extra_read_roots(&self, extra: &[PathBuf]) {
        if let Ok(mut roots) = self.extra_read_roots.write() {
            *roots = extra.to_vec();
        }
    }

    /// 当前执行权限根快照（运行时热切换的「当前默认工作区」状态）。
    ///
    /// 单次调用时由 [`Self::execute`] 读取；执行轮/subagent run 的调用方（如
    /// `CapabilityService` 构造）应在轮/run 开始时快照一次并把同一快照贯穿整轮，
    /// 使切换默认工作区只影响之后开始的新任务（任务书 §7）。
    pub fn current_host_context(&self) -> HostContext {
        let root = self
            .workspace_root
            .read()
            .map(|r| r.clone())
            .unwrap_or_default();
        let extra = self
            .extra_read_roots
            .read()
            .map(|r| r.clone())
            .unwrap_or_default();
        HostContext::for_workspace_with_roots(root, extra)
    }

    /// 以调用方提供的 host 快照执行内置能力（轮/run 级快照的落点）。
    fn execute_builtin_with(
        &self,
        actor_id: &str,
        _id: &str,
        builtin_name: &str,
        input: &Schema,
        host: &HostContext,
    ) -> Result<Schema> {
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
            // B3（v0.5.11）：完成状态台账读写分子（deliverable_ledger 第 9 表）。
            "ledger.read" | "ledger.write" => {
                let conn = self.duckdb.as_ref().ok_or_else(|| {
                    AgentError::NotFound(format!("{builtin_name}: duckdb not configured"))
                })?;
                let guard = conn.lock().map_err(|e| {
                    AgentError::Script(format!("builtin {builtin_name}: lock poisoned: {e}"))
                })?;
                let raw = if builtin_name == "ledger.read" {
                    super::ledger::ledger_read(&guard, input)
                } else {
                    super::ledger::ledger_write(&guard, input)
                };
                return raw.map_err(|e| AgentError::Script(format!("builtin {builtin_name}: {e}")));
            }
            // B4（v0.5.11）：多模式证据检索（证据与推断分离，只报告命中片段）。
            // 各依赖可选：缺 conn/storage_root/triviumdb 时对应模式半边降级为
            // hit=false + note，不构成执行失败。
            "evidence.check" => {
                let duckdb_guard = self.duckdb.as_ref().and_then(|db| db.lock().ok());
                let trivium_guard = self.triviumdb.as_ref().and_then(|db| db.lock().ok());
                return super::evidence::evidence_check(
                    self.storage_root.as_deref(),
                    duckdb_guard.as_deref(),
                    trivium_guard.as_deref(),
                    host,
                    &self.web_allowed_domains,
                    input,
                )
                .map_err(|e| AgentError::Script(format!("builtin evidence.check: {e}")));
            }
            "db.insert" => return self.builtin_db_insert(input),
            "db.update" => return self.builtin_db_update(input),
            "db.delete" => return self.builtin_db_delete(input),
            "db.query" => return self.builtin_db_query(input),
            name if name.starts_with("memory.") => return self.execute_memory(name, input),
            name if name.starts_with("subagent.")
                || name == "usage_method.observe"
                || name == "method.invoke" =>
            {
                return self.execute_subagent_molecule(name, input)
            }
            "permission.grant" | "permission.revoke" => {
                return self.execute_permission(actor_id, builtin_name, input)
            }
            "web.fetch.public" => return self.execute_web_fetch(actor_id, input),
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
            workspace_root: Arc::new(RwLock::new(PathBuf::new())),
            extra_read_roots: Arc::new(RwLock::new(Vec::new())),
            duckdb: None,
            triviumdb: None,
            thought_store: None,
            reload_tx: None,
            storage_root: None,
            subagent_spawn_hook: std::sync::RwLock::new(None),
            web_allowed_domains: Vec::new(),
        }
    }

    /// 设置 `web.fetch.public` 域名白名单（`[web] allowed_domains`）。
    pub fn set_web_allowed_domains(&mut self, domains: Vec<String>) {
        self.web_allowed_domains = domains;
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
    ///
    /// host 权限根取「调用时刻」的当前默认工作区快照（无轮/run 固化）。
    /// 需要整轮固化的调用方请用 [`Self::execute_with_host`]。
    pub fn execute(
        &self,
        actor_id: &str,
        id: &str,
        registry: &Registry,
        input: &Schema,
    ) -> Result<Schema> {
        let host = self.current_host_context();
        self.execute_with_host(actor_id, id, registry, input, &host)
    }

    /// 以调用方提供的 host 快照执行一个 base capability（轮/run 级快照入口）。
    ///
    /// 调用方（执行中台每轮、subagent runtime 每次 run）在任务开始时从
    /// `current_host_context()` 固化一次 host，并把同一快照贯穿该任务的所有
    /// 能力调用——运行时切换默认工作区后，已存在的任务继续使用旧快照，
    /// 只有新任务使用新默认工作区（任务书 v0.5.0 §7）。
    pub fn execute_with_host(
        &self,
        actor_id: &str,
        id: &str,
        registry: &Registry,
        input: &Schema,
        host: &HostContext,
    ) -> Result<Schema> {
        if let Some(row) = registry.base_capabilities.get(id) {
            if let Some(builtin_name) = row.executor.strip_prefix("builtin:") {
                return self.execute_builtin_with(actor_id, id, builtin_name, input, host);
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

    /// 分发 `web.fetch.public` 到 builtin 实现（v0.4.6）。
    ///
    /// 执行在 workspace 无关（正文仅返回内存，不落盘）；每次调用全量审计落库
    /// `web_fetch_audit`（纯审计表，不进 Registry）。授权由调用方
    /// （CapabilityService 按 agent allowlist / permission.grant）把关。
    fn execute_web_fetch(&self, actor_id: &str, input: &Schema) -> Result<Schema> {
        let db = self.duckdb.as_ref().ok_or_else(|| {
            AgentError::NotFound("web.fetch.public: duckdb not configured".into())
        })?;
        let conn = db.lock().map_err(|e| {
            AgentError::Script(format!("builtin web.fetch.public: lock poisoned: {e}"))
        })?;
        crate::logic::builtin::web_fetch::execute(&conn, &self.web_allowed_domains, actor_id, input)
            .map_err(|e| AgentError::Script(format!("builtin web.fetch.public: {e}")))
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
        if !is_allowed_table(table) {
            return Err(AgentError::NotFound(format!(
                "db.insert: table '{table}' not allowed"
            )));
        }
        let data = input
            .get("data")
            .ok_or_else(|| AgentError::Parse("db.insert: missing 'data'".into()))?;
        let obj = data
            .as_object()
            .ok_or_else(|| AgentError::Parse("db.insert: data must be object".into()))?;
        // A6：列名拼进 SQL 前做标识符白名单校验，阻断注入。
        for column in obj.keys() {
            validate_column_ident("db.insert", column)?;
        }
        let db = self
            .duckdb
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("db.insert: duckdb not configured".into()))?;
        let conn = db
            .lock()
            .map_err(|e| AgentError::Script(format!("builtin db.insert: lock poisoned: {e}")))?;
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
        if !is_allowed_table(table) {
            return Err(AgentError::NotFound(format!(
                "db.update: table '{table}' not allowed"
            )));
        }
        let data = input
            .get("data")
            .ok_or_else(|| AgentError::Parse("db.update: missing 'data'".into()))?;
        let where_clause = input.get("where").and_then(|v| v.as_str()).unwrap_or("1=1");
        let obj = data
            .as_object()
            .ok_or_else(|| AgentError::Parse("db.update: data must be object".into()))?;
        // A6：SET 列与 where 均拼进 SQL，先做标识符校验与子查询守卫。
        for column in obj.keys() {
            validate_column_ident("db.update", column)?;
        }
        validate_where_clause("db.update", where_clause)?;
        let db = self
            .duckdb
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("db.update: duckdb not configured".into()))?;
        let conn = db
            .lock()
            .map_err(|e| AgentError::Script(format!("builtin db.update: lock poisoned: {e}")))?;
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
        if !is_allowed_table(table) {
            return Err(AgentError::NotFound(format!(
                "db.delete: table '{table}' not allowed"
            )));
        }
        let where_clause = input
            .get("where")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Parse("db.delete: missing 'where'".into()))?;
        // A6：where 拼进 SQL 前做子查询守卫。
        validate_where_clause("db.delete", where_clause)?;
        let db = self
            .duckdb
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("db.delete: duckdb not configured".into()))?;
        let conn = db
            .lock()
            .map_err(|e| AgentError::Script(format!("builtin db.delete: lock poisoned: {e}")))?;
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
        if !is_allowed_table(table) {
            return Err(AgentError::NotFound(format!(
                "db.query: table '{table}' not allowed"
            )));
        }
        let where_clause = input.get("where").and_then(|v| v.as_str()).unwrap_or("1=1");
        // A6：where 拼进 SQL 前做子查询守卫，防 `IN (SELECT ...)` 读白名单外表。
        validate_where_clause("db.query", where_clause)?;
        let db = self
            .duckdb
            .as_ref()
            .ok_or_else(|| AgentError::NotFound("db.query: duckdb not configured".into()))?;
        let conn = db
            .lock()
            .map_err(|e| AgentError::Script(format!("builtin db.query: lock poisoned: {e}")))?;
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
                    // A6：按 ValueRef 类型映射，INTEGER/BOOLEAN/TIMESTAMP/NULL 不再
                    // 被吞成空串（损坏数据当成功返回）。
                    let val = duckdb_value_ref_to_json(row.get_ref(i)?);
                    obj.insert(name, val);
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
    use crate::data::duckdb::schema::create_all_tables;

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
        let ex = CapabilityExecutor::new();
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

    /// TD 接线验收：executor 层 `set_extra_read_roots` 后 file.read 可读追加根；
    /// 未配置时行为与现状一致（追加根内文件不可读）。
    #[test]
    fn builtin_file_read_via_set_extra_read_roots_allows_append_root() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let extra = dir.path().join("extra");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::write(extra.join("note.txt"), "from-extra").unwrap();

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

        let ex = CapabilityExecutor::new();
        ex.set_workspace_root(&ws);

        // 未配置额外读根：追加根内文件不可读（行为与现状一致，返回结构化错误）。
        let denied = ex
            .execute(
                "actor",
                "file.read",
                &reg,
                &serde_json::json!({"path": extra.join("note.txt").to_string_lossy()}),
            )
            .expect("追加根未配置时应返回结构化结果而非 trap");
        assert_eq!(denied.get("success").and_then(|v| v.as_bool()), Some(false));

        // v0.4.8：set_extra_read_roots 接线后，追加根内文件可读。
        ex.set_extra_read_roots(std::slice::from_ref(&extra));
        let ok = ex
            .execute(
                "actor",
                "file.read",
                &reg,
                &serde_json::json!({"path": extra.join("note.txt").to_string_lossy()}),
            )
            .expect("追加根内文件应可读");
        assert_eq!(
            ok.get("content").and_then(|v| v.as_str()),
            Some("from-extra")
        );
    }

    /// v0.5.0 热切换：`set_workspace_root` 后，不经固化的下一次 `execute` 立即用新根
    /// （相对路径以新根为基准解析）。
    #[test]
    fn builtin_file_read_follows_hot_switched_root_on_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("ws-a");
        let root_b = dir.path().join("ws-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::write(root_a.join("note.txt"), "from-a").unwrap();
        std::fs::write(root_b.join("note.txt"), "from-b").unwrap();

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

        let ex = CapabilityExecutor::new();
        ex.set_workspace_root(&root_a);
        let before = ex
            .execute(
                "actor",
                "file.read",
                &reg,
                &serde_json::json!({"path": "note.txt"}),
            )
            .expect("a-root read should be a structured result");
        assert_eq!(
            before.get("content").and_then(|v| v.as_str()),
            Some("from-a")
        );

        ex.set_workspace_root(&root_b);
        let after = ex
            .execute(
                "actor",
                "file.read",
                &reg,
                &serde_json::json!({"path": "note.txt"}),
            )
            .expect("b-root read should be a structured result");
        assert_eq!(
            after.get("content").and_then(|v| v.as_str()),
            Some("from-b"),
            "热切换后下一次调用应使用新工作区根"
        );
    }

    /// v0.5.0 轮/run 级快照：`execute_with_host` 用调用方固化快照执行；
    /// 固化后热切换不影响已固化调用，新快照才落到新根。
    #[test]
    fn execute_with_host_freezes_old_snapshot_across_hot_switch() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("ws-a");
        let root_b = dir.path().join("ws-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::write(root_a.join("note.txt"), "from-a").unwrap();
        std::fs::write(root_b.join("note.txt"), "from-b").unwrap();

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

        let ex = CapabilityExecutor::new();
        ex.set_workspace_root(&root_a);
        // 轮开始：固化 host（旧工作区快照）。
        let frozen = ex.current_host_context();
        let frozen_read = ex
            .execute_with_host(
                "actor",
                "file.read",
                &reg,
                &serde_json::json!({"path": "note.txt"}),
                &frozen,
            )
            .expect("frozen read should be structured");
        assert_eq!(
            frozen_read.get("content").and_then(|v| v.as_str()),
            Some("from-a")
        );

        // 热切换到 B：运行中任务（固化快照）仍读 A。
        ex.set_workspace_root(&root_b);
        let old_task = ex
            .execute_with_host(
                "actor",
                "file.read",
                &reg,
                &serde_json::json!({"path": "note.txt"}),
                &frozen,
            )
            .expect("old snapshot read should stay structured");
        assert_eq!(
            old_task.get("content").and_then(|v| v.as_str()),
            Some("from-a"),
            "运行中任务保持旧工作区快照"
        );

        // 新任务（新固化）落到新根 B。
        let fresh = ex.current_host_context();
        let new_task = ex
            .execute_with_host(
                "actor",
                "file.read",
                &reg,
                &serde_json::json!({"path": "note.txt"}),
                &fresh,
            )
            .expect("new snapshot read should be structured");
        assert_eq!(
            new_task.get("content").and_then(|v| v.as_str()),
            Some("from-b"),
            "新任务使用新默认工作区"
        );
    }

    /// db.* 基线共用：内存库 + 全表 schema + 接线到 executor。
    fn db_executor() -> CapabilityExecutor {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        create_all_tables(&conn).unwrap();
        let mut ex = CapabilityExecutor::new();
        ex.set_duckdb(Arc::new(std::sync::Mutex::new(conn)));
        ex
    }

    /// A6 类型回读基线：INTEGER→number、BOOLEAN→bool、TIMESTAMP→RFC3339 字符串、
    /// VARCHAR→string、NULL→null（不再变空串）。
    #[test]
    fn db_query_maps_column_types_to_json_types() {
        let ex = db_executor();
        {
            let db = ex.duckdb.as_ref().unwrap();
            let conn = db.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE _db_query_type_probe \
                 (n INTEGER, b BOOLEAN, t TIMESTAMP, s VARCHAR, maybe_null INTEGER); \
                 INSERT INTO _db_query_type_probe \
                 VALUES (42, true, CAST('2026-10-18 12:34:56.789' AS TIMESTAMP), 'hello', NULL);",
            )
            .unwrap();
        }
        let out = ex
            .builtin_db_query(&serde_json::json!({"table": "_db_query_type_probe"}))
            .unwrap();
        let rows = out["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["n"].as_i64(), Some(42), "INTEGER 必须回读为 number");
        assert_eq!(row["b"], serde_json::json!(true), "BOOLEAN 必须回读为 bool");
        let ts = row["t"].as_str().unwrap_or_default();
        assert!(
            ts.starts_with("2026-10-18T12:34:56.789"),
            "TIMESTAMP 必须回读为 RFC3339 字符串: {ts}"
        );
        assert_eq!(
            row["s"].as_str(),
            Some("hello"),
            "VARCHAR 必须回读为 string"
        );
        assert!(row["maybe_null"].is_null(), "NULL 必须回读为 null 而非空串");
    }

    /// db.insert / db.query 正常路径基线：白名单表可写可读，且缺省 TIMESTAMP/NULL
    /// 列的类型回读正确（usage_method 的 NOT NULL 列全为 TEXT，规避 VARCHAR→JSON
    /// 参数绑定的隐式转换变量）。
    #[test]
    fn db_insert_then_query_roundtrip_on_allowed_table() {
        let ex = db_executor();
        let out = ex
            .builtin_db_insert(&serde_json::json!({
                "table": "usage_method",
                "data": {
                    "id": "happy.usage",
                    "capability_id": "file.read",
                    "name": "Happy",
                    "prompt": "how to read"
                }
            }))
            .unwrap();
        assert_eq!(out["rows_affected"], 1);

        let out = ex
            .builtin_db_query(&serde_json::json!({
                "table": "usage_method",
                "where": "id = 'happy.usage'"
            }))
            .unwrap();
        let rows = out["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], serde_json::json!("happy.usage"));
        assert_eq!(
            rows[0]["prompt"].as_str(),
            Some("how to read"),
            "TEXT 列必须回读为 string"
        );
        assert!(rows[0]["examples"].is_null(), "NULL 列必须回读为 null");
        assert!(
            rows[0]["created_at"]
                .as_str()
                .unwrap_or_default()
                .starts_with("20"),
            "TIMESTAMP 缺省必须回读为 RFC3339 字符串"
        );
    }

    /// A6 列名注入：db.insert 的 JSON key / db.update 的 SET 列非
    /// `[A-Za-z_][A-Za-z0-9_]*` 时拒绝，表不被破坏。
    #[test]
    fn db_insert_and_update_reject_invalid_column_names() {
        let ex = db_executor();
        let err = ex
            .builtin_db_insert(&serde_json::json!({
                "table": "base_capability",
                "data": {"evil-col; DROP TABLE base_capability": "x"}
            }))
            .unwrap_err();
        assert!(err.to_string().contains("invalid column name"), "{err}");

        let err = ex
            .builtin_db_update(&serde_json::json!({
                "table": "base_capability",
                "data": {"ok_col": "1", "bad col": "2"}
            }))
            .unwrap_err();
        assert!(err.to_string().contains("invalid column name"), "{err}");

        // 注入被拒后表仍完好可查。
        assert!(ex
            .builtin_db_query(&serde_json::json!({"table": "base_capability"}))
            .is_ok());
    }

    /// A6 where 子查询守卫：query/update/delete 的 where 含 "select"
    /// （大小写不敏感）即拒绝。
    #[test]
    fn db_query_update_delete_reject_subquery_in_where() {
        let ex = db_executor();
        let err = ex
            .builtin_db_query(&serde_json::json!({
                "table": "base_capability",
                "where": "id IN (SELECT id FROM model)"
            }))
            .unwrap_err();
        assert!(
            err.to_string().contains("subqueries are not allowed"),
            "{err}"
        );

        let err = ex
            .builtin_db_update(&serde_json::json!({
                "table": "base_capability",
                "data": {"version": "9"},
                "where": "id IN (SeLeCt id FROM model)"
            }))
            .unwrap_err();
        assert!(
            err.to_string().contains("subqueries are not allowed"),
            "{err}"
        );

        let err = ex
            .builtin_db_delete(&serde_json::json!({
                "table": "base_capability",
                "where": "id IN (SELECT api_key FROM model)"
            }))
            .unwrap_err();
        assert!(
            err.to_string().contains("subqueries are not allowed"),
            "{err}"
        );
    }

    /// A6/D3：agent 表移出 db.* 白名单后不可经 db.* 可达。
    #[test]
    fn db_builtins_reject_agent_table() {
        let ex = db_executor();
        let err = ex
            .builtin_db_query(&serde_json::json!({"table": "agent"}))
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");

        let err = ex
            .builtin_db_insert(&serde_json::json!({
                "table": "agent",
                "data": {"id": "x", "name": "X", "mode": "subagent"}
            }))
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");

        let err = ex
            .builtin_db_update(&serde_json::json!({
                "table": "agent",
                "data": {"name": "Y"},
                "where": "id = 'x'"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }
}
