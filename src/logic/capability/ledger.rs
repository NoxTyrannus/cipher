//! 完成状态台账（B3，v0.5.11）：`deliverable_ledger` 表的 builtin 读写分子。
//!
//! 台账背景（诚实合同）：交付条目必须经 designed→imported→verified 状态机推进，
//! imported/verified 必须携带非空 evidence_refs（invocation_id / 回读行摘要 / 时间戳），
//! 防止「宣称完成但无执行证据」。`ledger.read` / `ledger.write` 是该表的唯一内置
//! 读写通路（db.* 白名单不含本表）；`pending_ledger_lines` 供 ContextAssembler
//! 每轮注入未结条目。风格仿 memory.rs / db.* 分子：校验失败返回
//! `{"success": false, "error"}` 信封，持久层错误走 `Err(String)`。

use duckdb::OptionalExt;
use serde_json::Value;
use uuid::Uuid;

const LEDGER_TABLE: &str = "deliverable_ledger";

pub const DEFAULT_LEDGER_LIMIT: usize = 50;
pub const MAX_LEDGER_LIMIT: usize = 200;

/// 每轮注入上下文的未结条目上限（context_assembler 使用）。
pub const MAX_INJECTION_LINES: usize = 20;

/// 单条 evidence_refs 元素上限（防滥用）。
const MAX_EVIDENCE_REFS: usize = 50;

/// 合法状态（状态机节点）。
pub const VALID_STATES: &[&str] = &["designed", "imported", "verified", "blocked"];

/// imported/verified 缺证据时的引导文案（诚实合同）。
const MISSING_EVIDENCE_HINT: &str = "ledger.write: imported/verified 必须携带非空 evidence_refs \
     （字符串数组，元素如 invocation_id / 回读行摘要 / 时间戳）——先执行并留下证据，\
     再推进状态；仅有计划用 state=designed";

fn fail(msg: impl Into<String>) -> Result<Value, String> {
    Ok(serde_json::json!({"success": false, "error": msg.into()}))
}

fn is_valid_state(state: &str) -> bool {
    VALID_STATES.contains(&state)
}

/// 状态机迁移合法性：designed→imported→verified 前向、designed/imported→blocked、
/// 同态幂等刷新；verified/blocked 为终态（仅允许同态刷新 evidence/note）。
fn transition_allowed(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        ("designed", "imported")
            | ("imported", "verified")
            | ("designed", "blocked")
            | ("imported", "blocked")
    )
}

/// 解析 evidence_refs 入参：缺省空数组；必须为字符串数组（元素非空、去空白）。
fn parse_evidence_refs(args: &Value) -> Result<Vec<String>, String> {
    let Some(value) = args.get("evidence_refs") else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err("ledger.write: 'evidence_refs' must be an array of strings".to_string());
    };
    let mut refs = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(
                "ledger.write: 'evidence_refs' must contain only non-empty strings".to_string(),
            );
        };
        refs.push(text.to_string());
    }
    if refs.len() > MAX_EVIDENCE_REFS {
        return Err(format!(
            "ledger.write: evidence_refs exceeds {MAX_EVIDENCE_REFS} entries"
        ));
    }
    Ok(refs)
}

/// 库内 evidence_refs JSON 文本 → JSON 数组值（损坏回退空数组，读侧不炸）。
fn evidence_refs_value(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| serde_json::json!([]))
}

/// `ledger.read`：{state?, item?, owner_agent?, limit?}（limit 默认 50 上限 200），
/// updated_at 倒序，返回 {rows:[{id,item,state,owner_agent,evidence_refs,note,updated_at}], count}。
/// item 为大小写不敏感子串过滤；state/owner_agent 为精确匹配。
pub fn ledger_read(conn: &duckdb::Connection, args: &Value) -> Result<Value, String> {
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    if let Some(state) = args
        .get("state")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !is_valid_state(state) {
            return fail(format!(
                "ledger.read: invalid state '{state}' (valid: {})",
                VALID_STATES.join("|")
            ));
        }
        clauses.push("state = ?");
        params.push(state.to_string());
    }
    if let Some(item) = args
        .get("item")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        clauses.push("contains(lower(item), lower(?))");
        params.push(item.to_string());
    }
    if let Some(owner) = args
        .get("owner_agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        clauses.push("owner_agent = ?");
        params.push(owner.to_string());
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_LEDGER_LIMIT as u64)
        .clamp(1, MAX_LEDGER_LIMIT as u64) as i64;

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT id, item, state, owner_agent, CAST(evidence_refs AS VARCHAR), note, \
         CAST(updated_at AS VARCHAR) FROM {LEDGER_TABLE}{where_sql} \
         ORDER BY updated_at DESC LIMIT ?"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("ledger.read prepare: {e}"))?;
    let mut param_refs: Vec<&dyn duckdb::ToSql> =
        params.iter().map(|p| p as &dyn duckdb::ToSql).collect();
    param_refs.push(&limit);
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|e| format!("ledger.read: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, item, state, owner_agent, evidence, note, updated_at) =
            row.map_err(|e| format!("ledger.read row: {e}"))?;
        out.push(serde_json::json!({
            "id": id,
            "item": item,
            "state": state,
            "owner_agent": owner_agent,
            "evidence_refs": evidence_refs_value(&evidence),
            "note": note,
            "updated_at": updated_at,
        }));
    }
    let count = out.len();
    Ok(serde_json::json!({"rows": out, "count": count}))
}

/// `ledger.write`：{id?, item, state, owner_agent?, evidence_refs:[...], note?}。
/// id 缺省生成 `dlg_<uuid simple>`；已存在 id 走 UPDATE+迁移校验，不存在走 INSERT；
/// imported/verified 必须携带非空 evidence_refs；同态重复写 = 幂等刷新 evidence/note。
pub fn ledger_write(conn: &duckdb::Connection, args: &Value) -> Result<Value, String> {
    // 输入契约错误一律走 fail 信封（Ok{success:false}），与 memory.* 一致——
    // 调用方（LLM 能力循环）对 Err 与信封的处理路径不同，混用会让拒绝被当成系统故障。
    let Some(item) = args
        .get("item")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return fail("ledger.write: missing non-empty 'item'".to_string());
    };
    let Some(state) = args
        .get("state")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return fail("ledger.write: missing non-empty 'state'".to_string());
    };
    if !is_valid_state(state) {
        return fail(format!(
            "ledger.write: invalid state '{state}' (valid: {})",
            VALID_STATES.join("|")
        ));
    }
    let owner_agent = args
        .get("owner_agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let note = args
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let id = match args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        None => format!("dlg_{}", Uuid::new_v4().simple()),
    };

    // 元素类型错误属于输入契约错误，走 fail 信封（与 memory.* 一致）。
    let evidence = match parse_evidence_refs(args) {
        Ok(ev) => ev,
        Err(msg) => return fail(msg),
    };
    if (state == "imported" || state == "verified") && evidence.is_empty() {
        return fail(MISSING_EVIDENCE_HINT);
    }
    let evidence_json = serde_json::to_string(&evidence)
        .map_err(|e| format!("ledger.write serialize evidence_refs: {e}"))?;

    let current: Option<String> = conn
        .query_row(
            &format!("SELECT state FROM {LEDGER_TABLE} WHERE id = ?"),
            duckdb::params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("ledger.read current state: {e}"))?;

    let action = match &current {
        Some(from) => {
            if !transition_allowed(from, state) {
                return fail(format!(
                    "ledger.write: illegal transition '{from}' -> '{state}' \
                     (allowed: designed→imported→verified forward, \
                      designed/imported→blocked, same-state idempotent refresh; \
                      verified/blocked are terminal)"
                ));
            }
            conn.execute(
                &format!(
                    "UPDATE {LEDGER_TABLE} SET item = ?, state = ?, owner_agent = ?, \
                     evidence_refs = CAST(? AS JSON), note = ?, updated_at = now() WHERE id = ?"
                ),
                duckdb::params![item, state, owner_agent, evidence_json, note, id],
            )
            .map_err(|e| format!("ledger.write update: {e}"))?;
            "updated"
        }
        None => {
            conn.execute(
                &format!(
                    "INSERT INTO {LEDGER_TABLE} \
                     (id, item, state, owner_agent, evidence_refs, note) \
                     VALUES (?, ?, ?, ?, CAST(? AS JSON), ?)"
                ),
                duckdb::params![id, item, state, owner_agent, evidence_json, note],
            )
            .map_err(|e| format!("ledger.write insert: {e}"))?;
            "inserted"
        }
    };

    Ok(serde_json::json!({
        "success": true,
        "id": id,
        "state": state,
        "previous_state": current,
        "action": action,
    }))
}

/// 未结条目（state NOT IN verified/blocked）格式化为紧凑注入行，
/// updated_at 倒序、上限 `limit`。每行：`[state] item — evidence: ref1,ref2 — updated_at`。
/// 供 ContextAssembler 每轮注入（读失败由调用方降级跳过）。
pub fn pending_ledger_lines(
    conn: &duckdb::Connection,
    limit: usize,
) -> Result<Vec<String>, String> {
    let limit = limit.clamp(1, MAX_INJECTION_LINES) as i64;
    let sql = format!(
        "SELECT state, item, CAST(evidence_refs AS VARCHAR), CAST(updated_at AS VARCHAR) \
         FROM {LEDGER_TABLE} WHERE state NOT IN ('verified', 'blocked') \
         ORDER BY updated_at DESC LIMIT ?"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("ledger pending prepare: {e}"))?;
    let rows = stmt
        .query_map(duckdb::params![limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| format!("ledger pending query: {e}"))?;
    let mut lines = Vec::new();
    for row in rows {
        let (state, item, evidence, updated_at) =
            row.map_err(|e| format!("ledger pending row: {e}"))?;
        let refs = evidence_refs_value(&evidence);
        let refs_text = refs
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|joined| !joined.is_empty())
            .unwrap_or_else(|| "-".to_string());
        lines.push(format!(
            "[{state}] {item} — evidence: {refs_text} — {updated_at}"
        ));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::duckdb::schema::create_all_tables;

    fn conn() -> duckdb::Connection {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        create_all_tables(&conn).unwrap();
        conn
    }

    fn state_of(conn: &duckdb::Connection, id: &str) -> String {
        conn.query_row(
            "SELECT state FROM deliverable_ledger WHERE id = ?",
            duckdb::params![id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn evidence_of(conn: &duckdb::Connection, id: &str) -> Value {
        let text: String = conn
            .query_row(
                "SELECT CAST(evidence_refs AS VARCHAR) FROM deliverable_ledger WHERE id = ?",
                duckdb::params![id],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn write_designed_inserts_without_evidence() {
        let conn = conn();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "d1", "item": "实现台账模块", "state": "designed"}),
        )
        .unwrap();
        assert_eq!(out["success"], true);
        assert_eq!(out["action"], "inserted");
        assert_eq!(out["previous_state"], serde_json::Value::Null);
        assert_eq!(state_of(&conn, "d1"), "designed");
    }

    #[test]
    fn write_generates_dlg_prefixed_id() {
        let conn = conn();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"item": "自动 id", "state": "designed"}),
        )
        .unwrap();
        let id = out["id"].as_str().unwrap();
        assert!(id.starts_with("dlg_"), "id 应以 dlg_ 开头: {id}");
        assert!(id.len() > "dlg_".len() + 8);
    }

    #[test]
    fn imported_requires_nonempty_evidence_with_hint() {
        let conn = conn();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "d2", "item": "条目", "state": "designed"}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "d2", "item": "条目", "state": "imported"}),
        )
        .unwrap();
        assert_eq!(out["success"], false, "缺证据的 imported 应拒绝");
        let error = out["error"].as_str().unwrap();
        assert!(
            error.contains("evidence_refs") && error.contains("invocation_id"),
            "错误应附引导文案: {error}"
        );
        assert_eq!(state_of(&conn, "d2"), "designed", "拒绝后状态不变");
    }

    #[test]
    fn imported_with_evidence_then_verified_forward() {
        let conn = conn();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "d3", "item": "条目", "state": "designed"}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "d3", "item": "条目", "state": "imported",
                 "evidence_refs": ["inv_abc123"]}),
        )
        .unwrap();
        assert_eq!(out["success"], true);
        assert_eq!(out["previous_state"], "designed");
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "d3", "item": "条目", "state": "verified",
                 "evidence_refs": ["inv_abc123", "回读行摘要: rows=3"]}),
        )
        .unwrap();
        assert_eq!(out["success"], true);
        assert_eq!(out["previous_state"], "imported");
        assert_eq!(
            evidence_of(&conn, "d3"),
            serde_json::json!(["inv_abc123", "回读行摘要: rows=3"])
        );
    }

    #[test]
    fn illegal_transitions_rejected_full_matrix() {
        let conn = conn();
        // 非法：designed 直跳 verified（跳过 imported）。
        ledger_write(
            &conn,
            &serde_json::json!({"id": "m1", "item": "m", "state": "designed"}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "m1", "item": "m", "state": "verified",
                 "evidence_refs": ["e"]}),
        )
        .unwrap();
        assert_eq!(out["success"], false, "designed→verified 应拒绝");

        // 非法：imported 回退 designed。
        ledger_write(
            &conn,
            &serde_json::json!({"id": "m2", "item": "m", "state": "designed"}),
        )
        .unwrap();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "m2", "item": "m", "state": "imported",
                 "evidence_refs": ["e"]}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "m2", "item": "m", "state": "designed"}),
        )
        .unwrap();
        assert_eq!(out["success"], false, "imported→designed 应拒绝");

        // 终态 verified：仅同态刷新，迁出一律拒绝。
        ledger_write(
            &conn,
            &serde_json::json!({"id": "m3", "item": "m", "state": "designed"}),
        )
        .unwrap();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "m3", "item": "m", "state": "imported",
                 "evidence_refs": ["e"]}),
        )
        .unwrap();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "m3", "item": "m", "state": "verified",
                 "evidence_refs": ["e"]}),
        )
        .unwrap();
        for target in ["designed", "imported", "blocked"] {
            let out = ledger_write(
                &conn,
                &serde_json::json!({"id": "m3", "item": "m", "state": target,
                     "evidence_refs": ["e"]}),
            )
            .unwrap();
            assert_eq!(out["success"], false, "verified→{target} 应拒绝（终态）");
        }

        // 终态 blocked：仅同态刷新。
        ledger_write(
            &conn,
            &serde_json::json!({"id": "m4", "item": "m", "state": "designed"}),
        )
        .unwrap();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "m4", "item": "m", "state": "blocked"}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "m4", "item": "m", "state": "designed"}),
        )
        .unwrap();
        assert_eq!(out["success"], false, "blocked→designed 应拒绝（终态）");
    }

    #[test]
    fn designed_and_imported_can_block() {
        let conn = conn();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "b1", "item": "b", "state": "designed"}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "b1", "item": "b", "state": "blocked",
                 "note": "依赖缺失"}),
        )
        .unwrap();
        assert_eq!(out["success"], true, "designed→blocked 合法");

        ledger_write(
            &conn,
            &serde_json::json!({"id": "b2", "item": "b", "state": "designed"}),
        )
        .unwrap();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "b2", "item": "b", "state": "imported",
                 "evidence_refs": ["e"]}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "b2", "item": "b", "state": "blocked"}),
        )
        .unwrap();
        assert_eq!(out["success"], true, "imported→blocked 合法");
    }

    #[test]
    fn same_state_write_is_idempotent_refresh() {
        let conn = conn();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "r1", "item": "旧描述", "state": "designed"}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "r1", "item": "新描述", "state": "designed",
                 "note": "刷新"}),
        )
        .unwrap();
        assert_eq!(out["success"], true, "同态写应幂等刷新");
        assert_eq!(out["action"], "updated");
        assert_eq!(out["previous_state"], "designed");

        // verified 同态刷新：evidence 更新（仍需非空证据）。
        ledger_write(
            &conn,
            &serde_json::json!({"id": "r2", "item": "v", "state": "designed"}),
        )
        .unwrap();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "r2", "item": "v", "state": "imported",
                 "evidence_refs": ["e1"]}),
        )
        .unwrap();
        ledger_write(
            &conn,
            &serde_json::json!({"id": "r2", "item": "v", "state": "verified",
                 "evidence_refs": ["e1"]}),
        )
        .unwrap();
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "r2", "item": "v", "state": "verified",
                 "evidence_refs": ["e1", "e2"]}),
        )
        .unwrap();
        assert_eq!(out["success"], true, "verified 同态刷新合法");
        assert_eq!(evidence_of(&conn, "r2"), serde_json::json!(["e1", "e2"]));
        // verified 同态刷新也不得退化成空证据。
        let out = ledger_write(
            &conn,
            &serde_json::json!({"id": "r2", "item": "v", "state": "verified"}),
        )
        .unwrap();
        assert_eq!(out["success"], false, "verified 刷新缺证据应拒绝");
    }

    #[test]
    fn invalid_state_and_bad_evidence_rejected() {
        let conn = conn();
        let out = ledger_write(&conn, &serde_json::json!({"item": "x", "state": "done"})).unwrap();
        assert_eq!(out["success"], false, "非法状态应拒绝");

        let out = ledger_write(
            &conn,
            &serde_json::json!({"item": "x", "state": "imported", "evidence_refs": "inv_1"}),
        )
        .unwrap();
        assert_eq!(out["success"], false, "evidence_refs 非数组应拒绝");

        let out = ledger_write(
            &conn,
            &serde_json::json!({"item": "x", "state": "imported", "evidence_refs": [1, 2]}),
        )
        .unwrap();
        assert_eq!(out["success"], false, "evidence_refs 非字符串元素应拒绝");

        let out =
            ledger_write(&conn, &serde_json::json!({"item": "", "state": "designed"})).unwrap();
        assert_eq!(out["success"], false, "空 item 应拒绝");
    }

    #[test]
    fn read_filters_order_and_limit() {
        let conn = conn();
        for (id, item, state, owner) in [
            ("a", "实现解析器", "designed", Some("agent")),
            ("b", "实现解析器 v2", "imported", Some("agent")),
            ("c", "编写文档", "verified", Some("writer")),
            ("d", "修复分页", "blocked", None),
            ("e", "实现路由", "designed", None),
        ] {
            conn.execute(
                "INSERT INTO deliverable_ledger (id, item, state, owner_agent, evidence_refs) \
                 VALUES (?, ?, ?, ?, CAST(? AS JSON))",
                duckdb::params![
                    id,
                    item,
                    state,
                    owner,
                    serde_json::json!(["ref"]).to_string()
                ],
            )
            .unwrap();
        }
        // 固定 updated_at 以验证排序（e 最新 → a 最旧）。
        conn.execute_batch(
            "UPDATE deliverable_ledger SET updated_at = '2026-01-01 10:00:00' WHERE id = 'a'; \
             UPDATE deliverable_ledger SET updated_at = '2026-01-02 10:00:00' WHERE id = 'b'; \
             UPDATE deliverable_ledger SET updated_at = '2026-01-03 10:00:00' WHERE id = 'c'; \
             UPDATE deliverable_ledger SET updated_at = '2026-01-04 10:00:00' WHERE id = 'd'; \
             UPDATE deliverable_ledger SET updated_at = '2026-01-05 10:00:00' WHERE id = 'e';",
        )
        .unwrap();

        // state 过滤。
        let out = ledger_read(&conn, &serde_json::json!({"state": "designed"})).unwrap();
        assert_eq!(out["count"], 2);
        let ids: Vec<&str> = out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["e", "a"], "updated_at 倒序");

        // item 子串（大小写不敏感由调用方输入决定，中文直接匹配）。
        let out = ledger_read(&conn, &serde_json::json!({"item": "实现"})).unwrap();
        assert_eq!(out["count"], 3);

        // owner_agent 过滤。
        let out = ledger_read(&conn, &serde_json::json!({"owner_agent": "agent"})).unwrap();
        assert_eq!(out["count"], 2);

        // 组合过滤 + 行结构。
        let out = ledger_read(
            &conn,
            &serde_json::json!({"state": "imported", "item": "解析器"}),
        )
        .unwrap();
        assert_eq!(out["count"], 1);
        let row = &out["rows"][0];
        assert_eq!(row["id"], "b");
        assert_eq!(row["item"], "实现解析器 v2");
        assert_eq!(row["owner_agent"], "agent");
        assert_eq!(row["evidence_refs"], serde_json::json!(["ref"]));

        // limit。
        let out = ledger_read(&conn, &serde_json::json!({"limit": 2})).unwrap();
        assert_eq!(out["count"], 2);
        let ids: Vec<&str> = out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["e", "d"]);

        // 非法 state 过滤值拒绝。
        let out = ledger_read(&conn, &serde_json::json!({"state": "bogus"})).unwrap();
        assert_eq!(out["success"], false);
    }

    #[test]
    fn pending_lines_skip_terminal_states_and_format() {
        let conn = conn();
        conn.execute_batch(
            "INSERT INTO deliverable_ledger (id, item, state, evidence_refs) VALUES \
             ('p1', '未完成任务', 'designed', '[]'); \
             INSERT INTO deliverable_ledger (id, item, state, evidence_refs) VALUES \
             ('p2', '进行中任务', 'imported', '[\"inv_9\",\"row:3\"]'); \
             INSERT INTO deliverable_ledger (id, item, state, evidence_refs) VALUES \
             ('p3', '已完成任务', 'verified', '[\"inv_8\"]'); \
             INSERT INTO deliverable_ledger (id, item, state, evidence_refs) VALUES \
             ('p4', '受阻任务', 'blocked', '[]');",
        )
        .unwrap();

        let lines = pending_ledger_lines(&conn, 20).unwrap();
        assert_eq!(lines.len(), 2, "verified/blocked 不进注入: {lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("[imported] 进行中任务 — evidence: inv_9,row:3 — ")),
            "行格式应为 [state] item — evidence: refs — updated_at: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("[designed] 未完成任务 — evidence: - — ")),
            "空证据显示 '-' 占位: {lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("已完成任务")));
    }
}
