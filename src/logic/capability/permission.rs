//! 运行时授权能力（v0.4.4）：`permission.grant` / `permission.revoke` 与审计回收。
//!
//! 授权链：`permission.grant/revoke` 授予**执行中台**；执行中台（或获递归授权的
//! subagent）把宽集外能力授予 **subagent 实例**（两级）。授权 = 修改目标实例
//! `capability_allowlist` 本身 → 执行时校验（`definitions_for_agent` /
//! `execute_for_agent` 出口）自动生效，无需新校验通道。
//!
//! 安全底线（用户已确认）：`permission.grant` 本身可被授予（递归授权），全量审计落库
//! `permission_grants`（granter/target/capability/mode/ttl/状态全链），默认 one-shot。
//! v0.5.11 起 grant/revoke/reclaim 中「allowlist 变更 + 审计行」两条写库语句在
//! 同一事务内原子生效，中途失败整体回滚。
//!
//! 本模块只做 DuckDB 持久化（实时校验 target/capability，不依赖内存 registry 快照），
//! 由 `CapabilityExecutor` 分发调用；one-shot 用后回收与 ttl 懒回收的**判定**在
//! `CapabilityService`（读 registry.permission_grants 快照），**写库**经由
//! `CapabilityExecutor::reclaim_permission_grant` 回到本模块的 `reclaim`。

use crate::common::{AgentError, Result};
use chrono::{Duration, SecondsFormat, Utc};
use duckdb::OptionalExt;
use serde_json::Value;
use uuid::Uuid;

pub const CAPABILITY_GRANT: &str = "permission.grant";
pub const CAPABILITY_REVOKE: &str = "permission.revoke";

pub const MODE_ONE_SHOT: &str = "one_shot";
pub const MODE_TTL: &str = "ttl";

pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_USED: &str = "used";
pub const STATUS_EXPIRED: &str = "expired";
pub const STATUS_REVOKED: &str = "revoked";

/// ttl 上限（秒），与 base_capabilities.json 的 schema_in maximum 一致。
pub const MAX_TTL_SECS: i64 = 86400;

/// 入口分发：按 executor 名执行 grant/revoke。
pub fn execute(
    conn: &duckdb::Connection,
    actor_agent: &str,
    builtin_name: &str,
    input: &Value,
) -> Result<Value> {
    match builtin_name {
        CAPABILITY_GRANT => grant(conn, actor_agent, input),
        CAPABILITY_REVOKE => revoke(conn, actor_agent, input),
        other => Err(AgentError::NotFound(format!(
            "permission executor: {other}"
        ))),
    }
}

/// `permission.grant`：把一项 registry 能力授予 subagent 实例（宽集外叠加）。
///
/// 校验（全部实时 DB）：target 必须为 mode='subagent' 且未 tombstoned 的实例；
/// capability_id 必须为 registry 可执行 base **或 composite** contract（v0.5.11 B1：
/// composite 授权后 execute_for_agent 仅校验调用者 allowlist 含 composite id 本身，
/// 节点不查 allowlist——因此 grant 侧放行 composite 与执行侧语义对齐）；
/// mode/ttl_secs 合法；去重（allowlist 已含 → 拒绝，含「活跃授权必然伴随
/// allowlist 含」的重复活跃授权）。
/// 行为：allowlist 追加 + 审计行（status=active）+ 返回授权记录 id。
pub fn grant(conn: &duckdb::Connection, granter_agent: &str, input: &Value) -> Result<Value> {
    let obj = input
        .as_object()
        .ok_or_else(|| AgentError::Parse("permission.grant: arguments must be an object".into()))?;
    let target_agent_id = required_str(obj, "target_agent_id", CAPABILITY_GRANT)?;
    let capability_id = required_str(obj, "capability_id", CAPABILITY_GRANT)?;
    let mode = required_str(obj, "mode", CAPABILITY_GRANT)?;
    let ttl_secs = match obj.get("ttl_secs") {
        Some(Value::Null) | None => None,
        Some(value) => Some(value.as_i64().ok_or_else(|| {
            AgentError::Parse("permission.grant: ttl_secs must be an integer".into())
        })?),
    };
    validate_mode_and_ttl(mode, ttl_secs)?;

    let (target_mode, target_config): (String, Option<String>) = conn
        .query_row(
            "SELECT mode, CAST(config AS VARCHAR) FROM agent WHERE id = ?",
            duckdb::params![target_agent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| {
            AgentError::Bootstrap(format!("permission.grant: read target row: {error}"))
        })?
        .ok_or_else(|| {
            AgentError::NotFound(format!(
                "permission.grant: target subagent '{target_agent_id}' not found"
            ))
        })?;
    if target_mode != "subagent" {
        return Err(AgentError::Parse(format!(
            "permission.grant: target '{target_agent_id}' is not a subagent instance (mode='{target_mode}')"
        )));
    }
    if is_tombstoned(&target_config)? {
        return Err(AgentError::NotFound(format!(
            "permission.grant: target subagent '{target_agent_id}' is tombstoned"
        )));
    }

    // B1c（v0.5.11）：能力存在性二选一——base 或 composite（enabled 且未 tombstone，
    // 与执行侧 validate_base_authority / validate_composite_authority 语义一致）。
    let executable: i64 = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM base_capability \
                      WHERE id = ? AND enabled = true AND tombstoned_at IS NULL) \
                  + (SELECT COUNT(*) FROM composite_capability \
                      WHERE id = ? AND enabled = true AND tombstoned_at IS NULL)",
            duckdb::params![capability_id, capability_id],
            |row| row.get(0),
        )
        .map_err(|error| {
            AgentError::Bootstrap(format!("permission.grant: capability lookup: {error}"))
        })?;
    if executable == 0 {
        return Err(AgentError::NotFound(format!(
            "permission.grant: capability '{capability_id}' is not an executable registry contract (base or composite)"
        )));
    }

    let allowlist = read_allowlist(conn, target_agent_id)?;
    if allowlist.iter().any(|value| value == capability_id) {
        return Err(AgentError::Parse(format!(
            "permission.grant: capability '{capability_id}' is already in allowlist of '{target_agent_id}' (dedup; revoke first to re-grant)"
        )));
    }
    let mut new_allowlist = allowlist;
    new_allowlist.push(capability_id.to_string());

    let now = now_iso();
    let expires_at = ttl_secs.map(|secs| {
        (Utc::now() + Duration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Nanos, true)
    });
    let id = Uuid::new_v4().simple().to_string();

    // v0.5.11 事务化：allowlist 追加与审计行 INSERT 两条写库语句原子生效，中途
    // 失败（Tx 被 Drop）整体回滚，不再留下「allowlist 已改、审计缺失」的不一致。
    // 用 `unchecked_transaction` 是因为本模块只持有 `&Connection`；executor 侧
    // 调用全程持有 db 互斥锁，不存在同连接并发/嵌套开事务。
    let tx = conn.unchecked_transaction().map_err(|error| {
        AgentError::Bootstrap(format!("permission.grant: begin transaction: {error}"))
    })?;
    update_allowlist(&tx, target_agent_id, &new_allowlist)?;
    tx.execute(
        "INSERT INTO permission_grants \
         (id, granted_at, granter_agent, target_agent, capability_id, mode, ttl_secs, expires_at, status) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active')",
        duckdb::params![
            id,
            now,
            granter_agent,
            target_agent_id,
            capability_id,
            mode,
            ttl_secs,
            expires_at,
        ],
    )
    .map_err(|error| AgentError::Bootstrap(format!("permission.grant: insert audit row: {error}")))?;
    tx.commit().map_err(|error| {
        AgentError::Bootstrap(format!("permission.grant: commit transaction: {error}"))
    })?;

    Ok(serde_json::json!({
        "id": id,
        "target_agent_id": target_agent_id,
        "capability_id": capability_id,
        "mode": mode,
        "expires_at": expires_at,
    }))
}

/// `permission.revoke`：从目标 allowlist 移除一项能力，并把活跃授权记录标记 revoked。
///
/// target 必须为 mode='subagent'（tombstoned 实例允许 revoke 以清理授权）；能力不在
/// allowlist / 无活跃记录时幂等成功（removed_from_allowlist/revoked_records 如实返回）。
pub fn revoke(conn: &duckdb::Connection, revoker_agent: &str, input: &Value) -> Result<Value> {
    let obj = input.as_object().ok_or_else(|| {
        AgentError::Parse("permission.revoke: arguments must be an object".into())
    })?;
    let target_agent_id = required_str(obj, "target_agent_id", CAPABILITY_REVOKE)?;
    let capability_id = required_str(obj, "capability_id", CAPABILITY_REVOKE)?;
    let _ = revoker_agent;

    let target_mode: Option<String> = conn
        .query_row(
            "SELECT mode FROM agent WHERE id = ?",
            duckdb::params![target_agent_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            AgentError::Bootstrap(format!("permission.revoke: read target row: {error}"))
        })?;
    match target_mode.as_deref() {
        Some("subagent") => {}
        Some(other) => {
            return Err(AgentError::Parse(format!(
                "permission.revoke: target '{target_agent_id}' is not a subagent instance (mode='{other}')"
            )))
        }
        None => {
            return Err(AgentError::NotFound(format!(
                "permission.revoke: target subagent '{target_agent_id}' not found"
            )))
        }
    }

    let allowlist = read_allowlist(conn, target_agent_id)?;
    let removed = allowlist.iter().any(|value| value == capability_id);

    // v0.5.11 事务化：allowlist 移除与审计行置 revoked 原子生效（同 grant）。
    let tx = conn.unchecked_transaction().map_err(|error| {
        AgentError::Bootstrap(format!("permission.revoke: begin transaction: {error}"))
    })?;
    if removed {
        let new_allowlist: Vec<String> = allowlist
            .into_iter()
            .filter(|value| value != capability_id)
            .collect();
        update_allowlist(&tx, target_agent_id, &new_allowlist)?;
    }

    let revoked_records = tx
        .execute(
            "UPDATE permission_grants SET status = 'revoked', revoked_at = ?, updated_at = now() \
             WHERE target_agent = ? AND capability_id = ? AND status = 'active'",
            duckdb::params![now_iso(), target_agent_id, capability_id],
        )
        .map_err(|error| {
            AgentError::Bootstrap(format!("permission.revoke: audit update: {error}"))
        })?;
    tx.commit().map_err(|error| {
        AgentError::Bootstrap(format!("permission.revoke: commit transaction: {error}"))
    })?;

    Ok(serde_json::json!({
        "target_agent_id": target_agent_id,
        "capability_id": capability_id,
        "removed_from_allowlist": removed,
        "revoked_records": revoked_records,
    }))
}

/// 回收钩子（one-shot 用后 / ttl 过期懒回收，由 `CapabilityService` 判定后调用）：
/// 从目标 allowlist 移除能力 + 审计行置终态（used 回填 used_at；expired 不设 revoked_at）。
pub fn reclaim(
    conn: &duckdb::Connection,
    target_agent: &str,
    capability_id: &str,
    status: &str,
) -> Result<()> {
    let allowlist = read_allowlist(conn, target_agent)?;
    let in_allowlist = allowlist.iter().any(|value| value == capability_id);

    // v0.5.11 事务化：allowlist 移除与审计行置终态原子生效（同 grant）；不支持
    // 的 status 在事务内返回 Err 时同样触发回滚，allowlist 不会被单独改写。
    let tx = conn.unchecked_transaction().map_err(|error| {
        AgentError::Bootstrap(format!("permission reclaim: begin transaction: {error}"))
    })?;
    if in_allowlist {
        let new_allowlist: Vec<String> = allowlist
            .into_iter()
            .filter(|value| value != capability_id)
            .collect();
        update_allowlist(&tx, target_agent, &new_allowlist)?;
    }

    match status {
        STATUS_USED => tx.execute(
            "UPDATE permission_grants SET status = 'used', used_at = ?, updated_at = now() \
                 WHERE target_agent = ? AND capability_id = ? AND status = 'active'",
            duckdb::params![now_iso(), target_agent, capability_id],
        ),
        STATUS_EXPIRED => tx.execute(
            "UPDATE permission_grants SET status = 'expired', updated_at = now() \
                 WHERE target_agent = ? AND capability_id = ? AND status = 'active'",
            duckdb::params![target_agent, capability_id],
        ),
        other => {
            return Err(AgentError::Bootstrap(format!(
                "permission reclaim: unsupported terminal status '{other}'"
            )))
        }
    }
    .map_err(|error| AgentError::Bootstrap(format!("permission reclaim: audit update: {error}")))?;
    tx.commit().map_err(|error| {
        AgentError::Bootstrap(format!("permission reclaim: commit transaction: {error}"))
    })?;
    Ok(())
}

fn validate_mode_and_ttl(mode: &str, ttl_secs: Option<i64>) -> Result<()> {
    match mode {
        MODE_ONE_SHOT => {
            if let Some(secs) = ttl_secs {
                validate_ttl_range(secs)?;
            }
        }
        MODE_TTL => {
            let secs = ttl_secs.ok_or_else(|| {
                AgentError::Parse("permission.grant: ttl mode requires ttl_secs".into())
            })?;
            validate_ttl_range(secs)?;
        }
        other => {
            return Err(AgentError::Parse(format!(
                "permission.grant: invalid mode '{other}' (expected 'one_shot' or 'ttl')"
            )))
        }
    }
    Ok(())
}

fn validate_ttl_range(secs: i64) -> Result<()> {
    if !(1..=MAX_TTL_SECS).contains(&secs) {
        return Err(AgentError::Parse(format!(
            "permission.grant: ttl_secs must be in 1..={MAX_TTL_SECS}, got {secs}"
        )));
    }
    Ok(())
}

fn required_str<'a>(
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
    cap: &str,
) -> Result<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::Parse(format!("{cap}: missing or empty '{key}'")))
}

fn is_tombstoned(config_text: &Option<String>) -> Result<bool> {
    let config: Value = match config_text {
        Some(text) => serde_json::from_str(text)
            .map_err(|error| AgentError::Bootstrap(format!("parse target config: {error}")))?,
        None => Value::Null,
    };
    Ok(config
        .get("subagent")
        .and_then(|block| block.get("tombstoned_at"))
        .and_then(Value::as_str)
        .map(|value| !value.is_empty())
        .unwrap_or(false))
}

fn read_allowlist(conn: &duckdb::Connection, agent_id: &str) -> Result<Vec<String>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT CAST(capability_allowlist AS VARCHAR) FROM agent WHERE id = ?",
            duckdb::params![agent_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            AgentError::Bootstrap(format!("read allowlist for '{agent_id}': {error}"))
        })?;
    match text {
        Some(text) => serde_json::from_str(&text).map_err(|error| {
            AgentError::Bootstrap(format!(
                "parse capability_allowlist for '{agent_id}': {error}"
            ))
        }),
        None => Ok(Vec::new()),
    }
}

fn update_allowlist(conn: &duckdb::Connection, agent_id: &str, allowlist: &[String]) -> Result<()> {
    let json = serde_json::to_string(allowlist).map_err(|error| {
        AgentError::Bootstrap(format!(
            "serialize capability_allowlist for '{agent_id}': {error}"
        ))
    })?;
    conn.execute(
        "UPDATE agent SET capability_allowlist = CAST(? AS JSON), updated_at = now() WHERE id = ?",
        duckdb::params![json, agent_id],
    )
    .map_err(|error| {
        AgentError::Bootstrap(format!("update allowlist for '{agent_id}': {error}"))
    })?;
    Ok(())
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::duckdb::loader::Registry;
    use crate::data::duckdb::schema::create_all_tables;
    use crate::logic::capability::executor::CapabilityExecutor;
    use crate::logic::capability::service::{CapabilityCall, CapabilityService};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn memory_db() -> duckdb::Connection {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        create_all_tables(&conn).unwrap();
        conn
    }

    /// 插入最小 fixture：一项可执行 base capability + 一个 subagent 实例。
    fn seed_fixture(conn: &duckdb::Connection) {
        conn.execute_batch(
            "INSERT INTO base_capability \
             (id, name, type, description, schema_in, schema_out, executor, version, enabled) \
             VALUES \
             ('file.read', 'Read File', 'function', 'read', '{}', '{}', 'builtin:file.read', '1.0.0', true), \
             ('probe.run', 'Probe Run', 'function', 'probe', '{}', '{}', 'builtin:probe.run', '1.0.0', true), \
             ('permission.grant', 'Grant Permission', 'function', 'grant', '{}', '{}', 'builtin:permission.grant', '1.0.0', true), \
             ('permission.revoke', 'Revoke Permission', 'function', 'revoke', '{}', '{}', 'builtin:permission.revoke', '1.0.0', true);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent (id, name, mode, capability_allowlist, config) \
             VALUES ('sg-1', 'S1', 'subagent', CAST(? AS JSON), CAST(? AS JSON))",
            duckdb::params![
                r#"["file.read"]"#,
                r#"{"subagent": {"lifecycle": "idle", "template_id": "t", "lifecycle_kind": "temporary", "startup": "normal", "model_id": "m", "budget": {}}}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent (id, name, mode, capability_allowlist) \
             VALUES ('execution-platform', 'Exec', 'platform', CAST(? AS JSON))",
            duckdb::params![r#"["permission.grant","permission.revoke","subagent.run"]"#],
        )
        .unwrap();
    }

    /// 审计行投影：(id, granter, target, capability, ttl_secs, expires_at, used_at, status)。
    type GrantRecord = (
        String,
        String,
        String,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
        String,
    );

    fn grant_records(conn: &duckdb::Connection) -> Vec<GrantRecord> {
        let mut stmt = conn
            .prepare(
                "SELECT id, granter_agent, target_agent, capability_id, ttl_secs, expires_at, \
                 used_at, status FROM permission_grants ORDER BY granted_at",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    }

    fn allowlist_of(conn: &duckdb::Connection, agent_id: &str) -> Vec<String> {
        read_allowlist(conn, agent_id).unwrap()
    }

    #[test]
    fn grant_appends_allowlist_and_writes_active_audit() {
        let conn = memory_db();
        seed_fixture(&conn);
        let out = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap();
        assert!(out["id"].as_str().unwrap().len() >= 8);
        assert_eq!(out["target_agent_id"], "sg-1");
        assert_eq!(out["capability_id"], "probe.run");
        assert_eq!(out["mode"], "one_shot");

        assert_eq!(allowlist_of(&conn, "sg-1"), vec!["file.read", "probe.run"]);
        let records = grant_records(&conn);
        assert_eq!(records.len(), 1);
        let (id, granter, target, capability, ttl, expires, used_at, status) = &records[0];
        assert_eq!(granter, "execution-platform");
        assert_eq!(target, "sg-1");
        assert_eq!(capability, "probe.run");
        assert_eq!(*ttl, None);
        assert_eq!(*expires, None);
        assert_eq!(*used_at, None);
        assert_eq!(status, "active");
        assert!(!id.is_empty());
    }

    #[test]
    fn grant_ttl_computes_expires_at_and_requires_ttl_secs() {
        let conn = memory_db();
        seed_fixture(&conn);
        let out = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "ttl",
                "ttl_secs": 60,
            }),
        )
        .unwrap();
        let expires = out["expires_at"].as_str().unwrap().to_string();
        assert!(expires.starts_with("20") || expires.starts_with("19"));

        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "ttl",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ttl mode requires ttl_secs"));
    }

    #[test]
    fn grant_rejects_target_not_subagent_or_tombstoned() {
        let conn = memory_db();
        seed_fixture(&conn);
        // platform 行
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "execution-platform",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a subagent instance"));

        // 不存在
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "ghost",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"));

        // tombstoned subagent
        conn.execute(
            "INSERT INTO agent (id, name, mode, capability_allowlist, config) \
             VALUES ('sg-dead', 'Dead', 'subagent', CAST(? AS JSON), CAST(? AS JSON))",
            duckdb::params![
                r#"[]"#,
                r#"{"subagent": {"lifecycle": "tombstoned", "template_id": "t", "lifecycle_kind": "temporary", "startup": "normal", "model_id": "m", "budget": {}, "tombstoned_at": "2026-01-01T00:00:00.000000000Z"}}"#,
            ],
        )
        .unwrap();
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-dead",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("tombstoned"));
    }

    #[test]
    fn grant_rejects_unknown_capability_and_bad_mode_and_ttl() {
        let conn = memory_db();
        seed_fixture(&conn);
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "nope.cap",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("not an executable registry contract"));

        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "forever",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid mode"));

        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "ttl",
                "ttl_secs": 0,
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("1..=86400"));

        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "ttl",
                "ttl_secs": 86401,
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("1..=86400"));
    }

    #[test]
    fn grant_rejects_duplicate_when_already_in_allowlist() {
        let conn = memory_db();
        seed_fixture(&conn);
        // sg-1 allowlist 已含 file.read → 去重拒绝
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "file.read",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("already in allowlist"));
        assert_eq!(grant_records(&conn).len(), 0);

        // 一次 grant 后再 grant 同一能力 → 重复活跃授权拒绝
        grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap();
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("already in allowlist"));
    }

    #[test]
    fn revoke_removes_and_marks_revoked_idempotently() {
        let conn = memory_db();
        seed_fixture(&conn);
        grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "ttl",
                "ttl_secs": 60,
            }),
        )
        .unwrap();

        let out = revoke(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
            }),
        )
        .unwrap();
        assert_eq!(out["removed_from_allowlist"], true);
        assert_eq!(out["revoked_records"], 1);
        assert_eq!(allowlist_of(&conn, "sg-1"), vec!["file.read"]);
        let records = grant_records(&conn);
        assert_eq!(records[0].7, "revoked");
        assert!(records[0].5.is_some());
        assert!(records[0].6.is_none());

        // 幂等：再 revoke 无能力可移、无活跃记录
        let out = revoke(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
            }),
        )
        .unwrap();
        assert_eq!(out["removed_from_allowlist"], false);
        assert_eq!(out["revoked_records"], 0);
    }

    #[test]
    fn revoke_rejects_non_subagent_target() {
        let conn = memory_db();
        seed_fixture(&conn);
        let err = revoke(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "execution-platform",
                "capability_id": "probe.run",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a subagent instance"));
    }

    #[test]
    fn reclaim_one_shot_removes_allowlist_and_marks_used() {
        let conn = memory_db();
        seed_fixture(&conn);
        grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap();
        reclaim(&conn, "sg-1", "probe.run", STATUS_USED).unwrap();
        assert_eq!(allowlist_of(&conn, "sg-1"), vec!["file.read"]);
        let records = grant_records(&conn);
        assert_eq!(records[0].7, "used");
        assert!(records[0].5.is_none());
        assert!(records[0].6.is_some());
    }

    #[test]
    fn reclaim_expired_marks_expired() {
        let conn = memory_db();
        seed_fixture(&conn);
        grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "ttl",
                "ttl_secs": 60,
            }),
        )
        .unwrap();
        reclaim(&conn, "sg-1", "probe.run", STATUS_EXPIRED).unwrap();
        assert_eq!(allowlist_of(&conn, "sg-1"), vec!["file.read"]);
        let records = grant_records(&conn);
        assert_eq!(records[0].7, "expired");
        // expired 不写 used_at（ttl 模式有 expires_at）
        assert!(records[0].5.is_some());
        assert!(records[0].6.is_none());
    }

    #[test]
    fn reclaim_unsupported_status_is_rejected() {
        let conn = memory_db();
        seed_fixture(&conn);
        let err = reclaim(&conn, "sg-1", "probe.run", "unknown").unwrap_err();
        assert!(err.to_string().contains("unsupported terminal status"));
    }

    /// v0.5.11 事务性：grant 的第二条语句（审计 INSERT）失败时，第一条
    /// （allowlist 追加）必须一并回滚。审计行 id 为内部 UUID v4，无法从外部
    /// 预插同 id 制造主键冲突；改为 DROP 审计表使 INSERT 必失败（等价可靠）。
    #[test]
    fn grant_rolls_back_allowlist_when_audit_insert_fails() {
        let conn = memory_db();
        seed_fixture(&conn);
        conn.execute_batch("DROP TABLE permission_grants;").unwrap();
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("insert audit row"), "{err}");
        // 原子性：allowlist 不得留下半提交的追加。
        assert_eq!(allowlist_of(&conn, "sg-1"), vec!["file.read"]);
    }

    /// v0.5.11 事务性：revoke 的第二条语句（审计 UPDATE）失败时，allowlist
    /// 移除必须一并回滚。
    #[test]
    fn revoke_rolls_back_allowlist_when_audit_update_fails() {
        let conn = memory_db();
        seed_fixture(&conn);
        conn.execute_batch("DROP TABLE permission_grants;").unwrap();
        let err = revoke(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "file.read",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("audit update"), "{err}");
        assert_eq!(allowlist_of(&conn, "sg-1"), vec!["file.read"]);
    }

    /// v0.5.11 事务性（成功路径）：grant 成功后 allowlist 追加与审计行同时存在
    /// （与 `grant_appends_allowlist_and_writes_active_audit` 互补，显式断言原子
    /// 提交的两面）。
    #[test]
    fn grant_success_commits_allowlist_and_audit_together() {
        let conn = memory_db();
        seed_fixture(&conn);
        grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap();
        assert!(allowlist_of(&conn, "sg-1").contains(&"probe.run".to_string()));
        assert_eq!(grant_records(&conn).len(), 1);
    }

    #[test]
    fn recursive_grant_chain_writes_full_audit() {
        // 递归授权：execution-platform 把 permission.grant 授给 sg-1（宽集外），
        // sg-1（已有 permission.grant）再把 probe.run 授给 sg-2——审计全链（granter 分别为两者）。
        let conn = memory_db();
        seed_fixture(&conn);
        conn.execute(
            "INSERT INTO agent (id, name, mode, capability_allowlist, config) \
             VALUES ('sg-2', 'S2', 'subagent', CAST(? AS JSON), CAST(? AS JSON))",
            duckdb::params![
                r#"[]"#,
                r#"{"subagent": {"lifecycle": "idle", "template_id": "t", "lifecycle_kind": "temporary", "startup": "normal", "model_id": "m", "budget": {}}}"#,
            ],
        )
        .unwrap();

        // 1) exec 授 permission.grant 给 sg-1（sg-1 的 allowlist 初始不含）
        grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "permission.grant",
                "mode": "one_shot",
            }),
        )
        .unwrap();
        // 2) sg-1 递归再授 probe.run 给 sg-2
        grant(
            &conn,
            "sg-1",
            &json!({
                "target_agent_id": "sg-2",
                "capability_id": "probe.run",
                "mode": "one_shot",
            }),
        )
        .unwrap();

        let records = grant_records(&conn);
        assert_eq!(records.len(), 2);
        let (_, granter1, target1, cap1, ..) = &records[0];
        assert_eq!(granter1, "execution-platform");
        assert_eq!(target1, "sg-1");
        assert_eq!(cap1, "permission.grant");
        let (_, granter2, target2, cap2, ..) = &records[1];
        assert_eq!(granter2, "sg-1");
        assert_eq!(target2, "sg-2");
        assert_eq!(cap2, "probe.run");
    }

    // ---- B1c（v0.5.11）：permission.grant 支持 composite contract ----

    /// composite 契约可授予；disabled / tombstoned / 未知 id 拒绝。
    #[test]
    fn grant_accepts_composite_contract_and_rejects_disabled_or_unknown() {
        let conn = memory_db();
        seed_fixture(&conn);
        conn.execute(
            "INSERT INTO composite_capability \
             (id, name, description, schema_in, schema_out, executor, dag, version, enabled) \
             VALUES ('memory.settle.v1', 'settle', '', CAST(? AS JSON), CAST(? AS JSON), 'dag', CAST(? AS JSON), '1.0.0', true), \
                    ('memory.dead.v1', 'dead', '', CAST(? AS JSON), CAST(? AS JSON), 'dag', CAST(? AS JSON), '1.0.0', false)",
            duckdb::params![
                r#"{"type":"object"}"#,
                r#"{"type":"object"}"#,
                "[]",
                r#"{"type":"object"}"#,
                r#"{"type":"object"}"#,
                "[]",
            ],
        )
        .unwrap();

        let out = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "memory.settle.v1",
                "mode": "one_shot",
            }),
        )
        .unwrap();
        assert_eq!(out["capability_id"], "memory.settle.v1");
        assert_eq!(
            allowlist_of(&conn, "sg-1"),
            vec!["file.read", "memory.settle.v1"]
        );

        // disabled composite 拒绝。
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "memory.dead.v1",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("not an executable registry contract (base or composite)"),
            "{err}"
        );

        // tombstoned composite 拒绝。
        conn.execute(
            "UPDATE composite_capability SET tombstoned_at = now() WHERE id = 'memory.settle.v1'",
            [],
        )
        .unwrap();
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "memory.settle.v1",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("not an executable registry contract (base or composite)"),
            "{err}"
        );

        // 未知 id 拒绝（错误文案更新后仍含既有断言关键字）。
        let err = grant(
            &conn,
            "execution-platform",
            &json!({
                "target_agent_id": "sg-1",
                "capability_id": "nope.composite",
                "mode": "one_shot",
            }),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("not an executable registry contract"));
    }

    // ---- B1a（v0.5.11）：capability.self-register 端到端（任务二核心验收）----

    type SelfRegisterEnv = (Registry, CapabilityExecutor, Arc<Mutex<duckdb::Connection>>);

    /// 工厂种子（capability.import / db.query / ledger.write base 行 +
    /// capability.self-register composite 行）镜像到内存 DuckDB，经
    /// load_all_into_memory 构造真实 registry 快照（装配方式参考 service.rs
    /// composite 执行测试 / permission_env）。
    fn self_register_env(agent_rows: &[(&str, &str, &str, &[&str])]) -> SelfRegisterEnv {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        create_all_tables(&conn).unwrap();

        let base_seed: serde_json::Value =
            serde_json::from_str(crate::data::cognitive_seed::CAPABILITY_SEED_BASE).unwrap();
        for row in base_seed["capabilities"].as_array().unwrap() {
            let id = row["id"].as_str().unwrap_or_default();
            if !["capability.import", "db.query", "ledger.write"].contains(&id) {
                continue;
            }
            conn.execute(
                "INSERT INTO base_capability \
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
                    row["version"].as_str().unwrap_or("1.0.0"),
                    row["enabled"].as_bool().unwrap_or(true),
                    r#"{"partition":"system"}"#,
                ],
            )
            .unwrap();
        }

        let comp_seed: serde_json::Value =
            serde_json::from_str(crate::data::cognitive_seed::CAPABILITY_SEED_COMPOSITE).unwrap();
        let self_register = comp_seed["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "capability.self-register")
            .expect("composite 种子应含 capability.self-register")
            .clone();
        conn.execute(
            "INSERT INTO composite_capability \
             (id, name, description, schema_in, schema_out, executor, dag, version, enabled, metadata) \
             VALUES (?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, CAST(? AS JSON), ?, ?, CAST(? AS JSON))",
            duckdb::params![
                self_register["id"].as_str().unwrap(),
                self_register["name"].as_str().unwrap(),
                self_register["description"].as_str().unwrap_or(""),
                self_register["schema_in"].to_string(),
                self_register["schema_out"].to_string(),
                self_register["executor"].as_str().unwrap_or("dag"),
                self_register["dag"].to_string(),
                self_register["version"].as_str().unwrap_or("1.0.0"),
                self_register["enabled"].as_bool().unwrap_or(true),
                r#"{"partition":"factory"}"#,
            ],
        )
        .unwrap();

        for (id, name, mode, allowlist) in agent_rows {
            let allowlist_json = serde_json::to_string(allowlist).unwrap();
            conn.execute(
                "INSERT INTO agent (id, name, mode, capability_allowlist, is_default) \
                 VALUES (?, ?, ?, CAST(? AS JSON), false)",
                duckdb::params![id, name, mode, allowlist_json],
            )
            .unwrap();
        }

        let db = Arc::new(Mutex::new(conn));
        let mut executor = CapabilityExecutor::new();
        executor.set_duckdb(db.clone());
        let registry = {
            let guard = db.lock().unwrap();
            crate::data::duckdb::loader::load_all_into_memory(&guard).unwrap()
        };
        (registry, executor, db)
    }

    fn self_register_call(arguments: serde_json::Value) -> CapabilityCall {
        CapabilityCall {
            capability_id: "capability.self-register".to_string(),
            capability_name: String::new(),
            arguments,
        }
    }

    fn probe_definition(capability_id: &str) -> serde_json::Value {
        serde_json::json!({
            "base_capabilities": [{
                "id": capability_id,
                "name": "Self Register Probe",
                "description": "self-register 端到端探针",
                "schema_in": {"type": "object"},
                "schema_out": {"type": "object"},
                "executor": "builtin:shell.exec",
                "version": "1.0.0"
            }],
            "capability_id": capability_id,
            "item": format!("注册探针能力 {capability_id} 并回读验证"),
            "owner_agent": "reg-actor"
        })
    }

    /// ① 正常路径：探针 base 能力注册 → verify rows 非空 → gate 通过 →
    /// 台账单行 imported→verified（previous_state 链正确），证据无未解析标记。
    #[test]
    fn self_register_composite_happy_path_lands_verified_ledger() {
        let (registry, executor, db) =
            self_register_env(&[("reg-actor", "Reg", "unni", &["capability.self-register"])]);
        let service = CapabilityService::new(&registry, &executor).unwrap();

        let result = service
            .execute_for_agent(
                "reg-actor",
                &self_register_call(probe_definition("probe.happy.cap")),
            )
            .unwrap();

        // final = 拓扑末节点（ledger_verified）输出：verified 且从 imported 迁移。
        assert_eq!(result.output["final"]["state"], "verified");
        assert_eq!(result.output["final"]["previous_state"], "imported");
        // 五节点依序执行，verify 节点回读到探针行。
        let steps = result.output["steps"].as_array().unwrap();
        let node_ids: Vec<&str> = steps.iter().map(|s| s["node"].as_str().unwrap()).collect();
        assert_eq!(
            node_ids,
            vec![
                "register",
                "verify",
                "gate",
                "ledger_imported",
                "ledger_verified"
            ]
        );
        assert_eq!(steps[1]["output"]["rows"][0]["id"], "probe.happy.cap");

        let conn = db.lock().unwrap();
        // 注册表：探针 base 行 + gate 探针 usage 行落库。
        let (base_n, usage_n): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM base_capability WHERE id = 'probe.happy.cap'), \
                        (SELECT COUNT(*) FROM usage_method WHERE id = 'selfregister.probe.probe.happy.cap')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((base_n, usage_n), (1, 1));
        // 台账：单行 verified、owner 正确、证据非空且无未解析标记。
        let (state, owner, evidence): (String, Option<String>, String) = conn
            .query_row(
                "SELECT state, owner_agent, CAST(evidence_refs AS VARCHAR) FROM deliverable_ledger \
                 WHERE item = '注册探针能力 probe.happy.cap 并回读验证'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "verified");
        assert_eq!(owner.as_deref(), Some("reg-actor"));
        assert!(
            !evidence.contains("${"),
            "证据不得残留未解析标记: {evidence}"
        );
        assert!(
            !evidence.contains("$input."),
            "证据不得残留未解析标记: {evidence}"
        );
        assert!(
            evidence.contains("probe.happy.cap"),
            "证据应含回读行摘要: {evidence}"
        );
        let ledger_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM deliverable_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(ledger_count, 1, "imported→verified 应是同一行推进");
    }

    /// ② 连坐门：capability_id 与真正注册的 id 不一致（空回读）→ gate 探针导入失败
    /// → 整条 composite Err，台账零行、verified 不落账（注册本身如实发生）。
    #[test]
    fn self_register_composite_empty_readback_fails_wholesale() {
        let (registry, executor, db) =
            self_register_env(&[("reg-actor", "Reg", "unni", &["capability.self-register"])]);
        let service = CapabilityService::new(&registry, &executor).unwrap();

        // 定义注册 probe.happy.cap，却用 ghost.cap 回读——空回读。
        let mut arguments = probe_definition("probe.happy.cap");
        arguments["capability_id"] = json!("ghost.cap");
        let error = service
            .execute_for_agent("reg-actor", &self_register_call(arguments))
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("references unknown capability_id"),
            "连坐门应由 gate 探针的存在性校验触发整条失败: {message}"
        );

        let conn = db.lock().unwrap();
        let ledger_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM deliverable_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            ledger_count, 0,
            "空回读不得落任何台账行（verified 不可凭回执落账）"
        );
        // gate 探针行未写入（capability.import 校验先行，原子拒绝）。
        let probe_usage: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_method WHERE id = 'selfregister.probe.ghost.cap'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(probe_usage, 0);
        // register 节点先执行：注册本身如实发生（诚实：已导入未验证）。
        let base_n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM base_capability WHERE id = 'probe.happy.cap'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(base_n, 1);
    }

    /// 任务四端到端：grant composite one-shot → 子代理 allowlist 含该 id →
    /// execute_for_agent 可执行该 composite → 成功后回收（审计 used + allowlist 移除）。
    #[test]
    fn one_shot_composite_grant_executes_and_reclaims() {
        let (_initial_registry, _initial_executor, db) = self_register_env(&[
            ("execution-platform", "Exec", "platform", &[]),
            ("sg-c", "Sub C", "subagent", &[]),
        ]);
        // 未知 composite id 拒绝。
        {
            let conn = db.lock().unwrap();
            let err = grant(
                &conn,
                "execution-platform",
                &json!({
                    "target_agent_id": "sg-c",
                    "capability_id": "nope.composite",
                    "mode": "one_shot",
                }),
            )
            .unwrap_err();
            assert!(err
                .to_string()
                .contains("not an executable registry contract"));
            // 授予 composite：allowlist + active 审计。
            grant(
                &conn,
                "execution-platform",
                &json!({
                    "target_agent_id": "sg-c",
                    "capability_id": "capability.self-register",
                    "mode": "one_shot",
                }),
            )
            .unwrap();
        }
        // 真实 reload 语义：grant 落库后刷新 registry 快照。
        let (registry, executor) = {
            let conn = db.lock().unwrap();
            let registry = crate::data::duckdb::loader::load_all_into_memory(&conn).unwrap();
            let mut executor = CapabilityExecutor::new();
            executor.set_duckdb(db.clone());
            (registry, executor)
        };
        assert!(
            registry
                .agents
                .get("sg-c")
                .unwrap()
                .capability_allowlist
                .iter()
                .any(|id| id == "capability.self-register"),
            "授权后子代理 allowlist 应含 composite id"
        );

        let service = CapabilityService::new(&registry, &executor).unwrap();
        let call = {
            let mut arguments = probe_definition("probe.granted.cap");
            arguments["owner_agent"] = json!("sg-c");
            self_register_call(arguments)
        };
        let result = service.execute_for_agent("sg-c", &call).unwrap();
        assert_eq!(result.output["final"]["state"], "verified");

        // one-shot 回收：审计 used + allowlist 移除（持久层）。
        let conn = db.lock().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM permission_grants \
                 WHERE target_agent = 'sg-c' AND capability_id = 'capability.self-register'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "used");
        assert_eq!(allowlist_of(&conn, "sg-c"), Vec::<String>::new());
    }
}
