//! evidence.check（B4，v0.5.11）：多模式证据检索 builtin 分子。
//!
//! 证据与推断分离：本能力只做「按模式独立检索 + 报告命中片段」，**不做跨模式
//! 综合判断**，也不对问题真伪下结论——结论由调用方（LLM/编排层）基于证据自行做出。
//!
//! 四个模式：
//! - `log`：扫描 `<storage_root>/invocations/` 的不可变事实文件（非 `*.result.json`，
//!   mtime 倒序上限 200 个），按问题关键词子串匹配 `capability_id` / `task_input` /
//!   `arguments_redacted` 字段；有 DuckDB 连接时再查 `method_call_audit`
//!   （method_id / called_by / status / result，即「where called + status」半边）。
//! - `file`：对工作区根（host 读根首选）做浅层遍历（深度≤3、跳过隐藏项与符号链接），
//!   文件名 + 内容子串匹配，命中上限 20。
//! - `web`：输入显式 `url` 时经 `web_fetch` 的共享 client 抓取（仅 https + 域名白名单，
//!   与 web.fetch.public 同策略，拒绝重定向）并按关键词匹配；无 url 明确 miss。
//!   不引入搜索引擎。
//! - `memory`：经 `memory.retrieve`（子串全表扫描）把 question 作为 query，遍历
//!   attention/experience/preference/cognitive 四类。
//!
//! 输出信封：`{"success":true,"results":[{mode,hit,evidence_ref,raw_excerpt,note?}]}`；
//! 输入契约错误（question 缺失 / modes 空 / 未知 mode）返回 `{"success":false,"error"}`
//! 信封（与 ledger.*/memory.* 一致，由能力输出合同整体失败）。

use crate::data::triviumdb::TriviumDb;
use crate::logic::builtin::host_context::HostContext;
use crate::logic::builtin::web_fetch::{is_host_allowed, HttpGetter, ReqwestGetter};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 合法模式集。
const MODES: &[&str] = &["log", "file", "web", "memory"];
/// log 模式扫描的 invocation 事实文件上限（mtime 倒序）。
const MAX_LOG_FILES: usize = 200;
/// log 模式审计表回看行数上限。
const MAX_AUDIT_ROWS: i64 = 50;
/// file 模式命中文件上限。
const MAX_FILE_HITS: usize = 20;
/// file 模式扫描文件数上限（防大仓库扫穿）。
const MAX_FILE_SCAN: usize = 2000;
/// file 模式遍历深度上限（工作区根为 0 层，子项从 1 层计）。
const FILE_WALK_DEPTH: usize = 3;
/// file 模式内容读取的单文件大小上限。
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// 摘要半径（匹配处前后各 N 字符）。
const EXCERPT_RADIUS: usize = 200;
/// 关键词上限。
const MAX_KEYWORDS: usize = 8;

fn fail(msg: impl Into<String>) -> Result<Value, String> {
    Ok(serde_json::json!({"success": false, "error": msg.into()}))
}

/// `evidence.check` 执行入口。
///
/// 各依赖均为可选注入：缺 storage_root / conn / triviumdb 时对应半边跳过并在
/// result 的 note 里如实说明（探针语义：配置缺失不是错误，是无证据）。
pub fn evidence_check(
    storage_root: Option<&Path>,
    conn: Option<&duckdb::Connection>,
    trivium: Option<&TriviumDb>,
    host: &HostContext,
    web_allowed_domains: &[String],
    input: &Value,
) -> Result<Value, String> {
    let question = match input
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(q) => q,
        None => return fail("evidence.check: missing non-empty 'question'"),
    };
    let Some(modes_raw) = input.get("modes").and_then(Value::as_array) else {
        return fail("evidence.check: 'modes' must be a non-empty array of strings");
    };
    let mut modes: Vec<String> = Vec::new();
    for mode in modes_raw {
        let Some(mode) = mode.as_str() else {
            return fail("evidence.check: 'modes' must contain only strings");
        };
        if !MODES.contains(&mode) {
            return fail(format!(
                "evidence.check: unknown mode '{mode}' (valid: {})",
                MODES.join("|")
            ));
        }
        if !modes.iter().any(|m| m == mode) {
            modes.push(mode.to_string());
        }
    }
    if modes.is_empty() {
        return fail("evidence.check: 'modes' must be a non-empty array of strings");
    }

    let needles = match_needles(question);
    let mut results = Vec::with_capacity(modes.len());
    for mode in &modes {
        let result = match mode.as_str() {
            "log" => mode_log(storage_root, conn, &needles),
            "file" => mode_file(host, &needles),
            "web" => mode_web(input, web_allowed_domains, &needles),
            "memory" => mode_memory(trivium, question, &needles),
            other => mode_result(other, false, None, None, Some("unknown mode".into())),
        };
        results.push(result);
    }
    Ok(serde_json::json!({"success": true, "results": results}))
}

/// 检索词：整句小写（CJK 整句即主关键词）+ 分词关键词（去重、字符数≥2、上限 8）。
fn match_needles(question: &str) -> Vec<String> {
    let mut needles: Vec<String> = vec![question.to_lowercase()];
    let mut token = String::new();
    for ch in question.to_lowercase().chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            token.push(ch);
        } else if !token.is_empty() {
            push_keyword(&mut needles, &token);
            token.clear();
        }
    }
    if !token.is_empty() {
        push_keyword(&mut needles, &token);
    }
    needles
}

fn push_keyword(needles: &mut Vec<String>, token: &str) {
    if token.chars().count() < 2 || needles.len() > MAX_KEYWORDS {
        return;
    }
    if !needles.iter().any(|n| n == token) {
        needles.push(token.to_string());
    }
}

/// 大小写不敏感子串查找（ASCII 折叠，字节偏移安全：不改变原文本长度）。
fn find_ci(text: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let hay = text.as_bytes();
    let nee = needle.as_bytes();
    if nee.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - nee.len()).find(|&i| hay[i..i + nee.len()].eq_ignore_ascii_case(nee))
}

/// 匹配处前后各 `radius` 个字符的摘要（按字符收敛，避免切坏多字节字符）。
fn excerpt_around(text: &str, pos: usize, radius: usize) -> String {
    let pos = pos.min(text.len());
    let start = {
        let mut b = pos;
        while b > 0 && !text.is_char_boundary(b) {
            b -= 1;
        }
        let mut s = b;
        let mut count = 0;
        while s > 0 && count < radius {
            s -= 1;
            while s > 0 && !text.is_char_boundary(s) {
                s -= 1;
            }
            count += 1;
        }
        s
    };
    let end = {
        let mut b = pos;
        while b < text.len() && !text.is_char_boundary(b) {
            b += 1;
        }
        text[b..]
            .char_indices()
            .nth(radius)
            .map(|(off, _)| b + off)
            .unwrap_or(text.len())
    };
    let mut out: String = text[start..end].to_string();
    if out.chars().count() > radius * 2 + 16 {
        out = out.chars().take(radius * 2 + 16).collect();
    }
    out
}

fn mode_result(
    mode: &str,
    hit: bool,
    evidence_ref: Option<String>,
    raw_excerpt: Option<String>,
    note: Option<String>,
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("mode".to_string(), Value::String(mode.to_string()));
    obj.insert("hit".to_string(), Value::Bool(hit));
    obj.insert(
        "evidence_ref".to_string(),
        evidence_ref.map(Value::String).unwrap_or(Value::Null),
    );
    obj.insert(
        "raw_excerpt".to_string(),
        raw_excerpt.map(Value::String).unwrap_or(Value::Null),
    );
    if let Some(note) = note {
        obj.insert("note".to_string(), Value::String(note));
    }
    Value::Object(obj)
}

/// log 模式：invocations 事实文件 + method_call_audit 审计两半边。
fn mode_log(
    storage_root: Option<&Path>,
    conn: Option<&duckdb::Connection>,
    needles: &[String],
) -> Value {
    let mut scanned_files = 0usize;
    let mut file_hit: Option<(String, String)> = None;
    if let Some(root) = storage_root {
        let invocations = root.join("invocations");
        if invocations.is_dir() {
            let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&invocations) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.ends_with(".json") || name.ends_with(".result.json") {
                        continue;
                    }
                    let mtime = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    candidates.push((entry.path(), mtime));
                }
            }
            candidates.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
            candidates.truncate(MAX_LOG_FILES);
            for (path, _) in candidates {
                scanned_files += 1;
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(fact) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                for (field, field_text) in fact_field_texts(&fact) {
                    if let Some(pos) = first_match(&field_text, needles) {
                        let invocation_id = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        file_hit = Some((
                            invocation_id,
                            format!(
                                "[{field}] {}",
                                excerpt_around(&field_text, pos, EXCERPT_RADIUS)
                            ),
                        ));
                        break;
                    }
                }
                if file_hit.is_some() {
                    break;
                }
            }
        }
    }

    // 审计半边：有 conn 就查 method_call_audit（where called / status）。
    let mut audit_rows = 0usize;
    let mut audit_hit: Option<(String, String)> = None;
    if let Some(conn) = conn {
        let prepared = conn.prepare(
            "SELECT id, method_id, called_by, status, COALESCE(CAST(result AS VARCHAR), '') \
             FROM method_call_audit ORDER BY called_at DESC LIMIT ?",
        );
        if let Ok(mut stmt) = prepared {
            let rows = stmt.query_map(duckdb::params![MAX_AUDIT_ROWS], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            });
            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    audit_rows += 1;
                    let (id, method_id, called_by, status, result) = row;
                    for (field, field_text) in [
                        ("method_id", method_id),
                        ("called_by", called_by),
                        ("status", status.clone()),
                        ("result", result),
                    ] {
                        if let Some(pos) = first_match(&field_text, needles) {
                            audit_hit = Some((
                                format!("method_call_audit:{id}"),
                                format!(
                                    "[{status}] {field}: {}",
                                    excerpt_around(&field_text, pos, EXCERPT_RADIUS)
                                ),
                            ));
                            break;
                        }
                    }
                    if audit_hit.is_some() {
                        break;
                    }
                }
            }
        }
    }

    if let Some((evidence_ref, excerpt)) = file_hit {
        return mode_result(
            "log",
            true,
            Some(evidence_ref),
            Some(excerpt),
            Some(format!(
                "scanned {scanned_files} invocation files, {audit_rows} audit rows"
            )),
        );
    }
    if let Some((evidence_ref, excerpt)) = audit_hit {
        return mode_result(
            "log",
            true,
            Some(evidence_ref),
            Some(excerpt),
            Some(format!(
                "no invocation file hit; scanned {audit_rows} audit rows"
            )),
        );
    }
    let note = if storage_root.is_some() {
        format!("no match: {scanned_files} invocation files, {audit_rows} audit rows")
    } else {
        format!("storage_root 未配置（invocations 半边跳过）；scanned {audit_rows} audit rows")
    };
    mode_result("log", false, None, None, Some(note))
}

/// invocation 事实文件的候选匹配字段（capability_id / task_input / arguments_redacted）。
fn fact_field_texts(fact: &Value) -> Vec<(&'static str, String)> {
    let mut fields = Vec::new();
    if let Some(id) = fact.get("capability_id").and_then(Value::as_str) {
        fields.push(("capability_id", id.to_string()));
    }
    if let Some(task) = fact.get("task_input").and_then(Value::as_str) {
        fields.push(("task_input", task.to_string()));
    }
    if let Some(args) = fact.get("arguments_redacted") {
        if !args.is_null() {
            fields.push(("arguments_redacted", args.to_string()));
        }
    }
    fields
}

fn first_match(text: &str, needles: &[String]) -> Option<usize> {
    needles.iter().filter_map(|n| find_ci(text, n)).min()
}

/// file 模式：工作区根浅层（深度≤3）文件名 + 内容子串匹配。
fn mode_file(host: &HostContext, needles: &[String]) -> Value {
    let Some(raw_root) = host.permission.file_read_roots.first() else {
        return mode_result(
            "file",
            false,
            None,
            None,
            Some("workspace root 未配置，file 模式无法扫描".to_string()),
        );
    };
    let root = raw_root.canonicalize().unwrap_or_else(|_| raw_root.clone());
    let mut hits: Vec<(String, String)> = Vec::new();
    let mut scanned = 0usize;
    scan_dir(
        &root,
        &root,
        1,
        FILE_WALK_DEPTH,
        needles,
        &mut scanned,
        &mut hits,
    );
    let mut obj = if hits.is_empty() {
        mode_result(
            "file",
            false,
            None,
            None,
            Some(format!(
                "no match: {scanned} files scanned (depth<={FILE_WALK_DEPTH}, hidden entries skipped)"
            )),
        )
    } else {
        let (evidence_ref, excerpt) = &hits[0];
        mode_result(
            "file",
            true,
            Some(evidence_ref.clone()),
            Some(excerpt.clone()),
            Some(format!("{scanned} files scanned; {} hit(s)", hits.len())),
        )
    };
    if let Some(object) = obj.as_object_mut() {
        object.insert(
            "matches".to_string(),
            serde_json::json!(hits.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>()),
        );
    }
    obj
}

fn scan_dir(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    needles: &[String],
    scanned: &mut usize,
    hits: &mut Vec<(String, String)>,
) {
    if depth > max_depth || hits.len() >= MAX_FILE_HITS || *scanned >= MAX_FILE_SCAN {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if hits.len() >= MAX_FILE_HITS || *scanned >= MAX_FILE_SCAN {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            scan_dir(root, &path, depth + 1, max_depth, needles, scanned, hits);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        *scanned += 1;
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if first_match(&rel, needles).is_some() {
            hits.push((rel.clone(), rel.clone()));
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let content = String::from_utf8_lossy(&bytes).into_owned();
        if let Some(pos) = first_match(&content, needles) {
            hits.push((rel, excerpt_around(&content, pos, EXCERPT_RADIUS)));
        }
    }
}

/// web 模式：显式 url 抓取 + 关键词匹配（不引入搜索引擎）。
fn mode_web(input: &Value, web_allowed_domains: &[String], needles: &[String]) -> Value {
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(url) = url else {
        return mode_result(
            "web",
            false,
            None,
            None,
            Some("web 模式需显式 url".to_string()),
        );
    };
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => {
            return mode_result(
                "web",
                false,
                None,
                None,
                Some(format!("invalid url: {error}")),
            )
        }
    };
    if parsed.scheme() != "https" {
        return mode_result(
            "web",
            false,
            None,
            None,
            Some("only https URLs are allowed".to_string()),
        );
    }
    let Some(host) = parsed.host_str() else {
        return mode_result(
            "web",
            false,
            None,
            None,
            Some("url has no host".to_string()),
        );
    };
    if !is_host_allowed(host, web_allowed_domains) {
        return mode_result(
            "web",
            false,
            None,
            None,
            Some(format!("domain '{host}' is not in the whitelist")),
        );
    }
    let getter = match ReqwestGetter::new() {
        Ok(getter) => getter,
        Err(error) => return mode_result("web", false, None, None, Some(error)),
    };
    let outcome = match getter.get(url) {
        Ok(outcome) => outcome,
        Err(error) => {
            return mode_result(
                "web",
                false,
                None,
                None,
                Some(format!("fetch failed: {error}")),
            )
        }
    };
    if (300..400).contains(&outcome.status) {
        return mode_result(
            "web",
            false,
            None,
            None,
            Some(format!(
                "redirects are rejected (status {})",
                outcome.status
            )),
        );
    }
    if !(200..300).contains(&outcome.status) {
        return mode_result(
            "web",
            false,
            None,
            None,
            Some(format!("HTTP {} response", outcome.status)),
        );
    }
    let text = String::from_utf8_lossy(&outcome.body).into_owned();
    match first_match(&text, needles) {
        Some(pos) => mode_result(
            "web",
            true,
            Some(url.to_string()),
            Some(excerpt_around(&text, pos, EXCERPT_RADIUS)),
            Some(format!("fetched {} bytes", outcome.body.len())),
        ),
        None => mode_result(
            "web",
            false,
            None,
            None,
            Some(format!(
                "no keyword match in {} chars",
                text.chars().count()
            )),
        ),
    }
}

/// memory 模式：memory.retrieve 全类型子串扫描（question 原样作 query）。
fn mode_memory(trivium: Option<&TriviumDb>, question: &str, needles: &[String]) -> Value {
    let Some(db) = trivium else {
        return mode_result(
            "memory",
            false,
            None,
            None,
            Some("triviumdb 未配置，memory 模式跳过".to_string()),
        );
    };
    // 检索词优先整句（与 memory.retrieve 的子串语义一致），其次关键词逐个降级。
    let mut queries: Vec<String> = vec![question.to_lowercase()];
    for needle in needles {
        if !queries.iter().any(|q| q == needle) {
            queries.push(needle.clone());
        }
    }
    for memory_type in ["attention", "experience", "preference", "cognitive"] {
        for query in &queries {
            let args = serde_json::json!({"memory_type": memory_type, "query": query, "limit": 20});
            let Ok(out) = super::memory::memory_retrieve(db, &args) else {
                continue;
            };
            let items = out
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(first) = items.first() {
                let id = first.get("id").map(|v| v.to_string()).unwrap_or_default();
                let payload_text = first
                    .get("payload")
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let pos =
                    find_ci(&payload_text, query).or_else(|| first_match(&payload_text, needles));
                let excerpt = match pos {
                    Some(p) => excerpt_around(&payload_text, p, EXCERPT_RADIUS),
                    None => payload_text.chars().take(EXCERPT_RADIUS).collect(),
                };
                return mode_result(
                    "memory",
                    true,
                    Some(format!("memory:{memory_type}:{id}")),
                    Some(excerpt),
                    Some(format!(
                        "{} entries matched in {memory_type} (query: {query})",
                        items.len()
                    )),
                );
            }
        }
    }
    mode_result(
        "memory",
        false,
        None,
        None,
        Some("no memory entry matched (attention/experience/preference/cognitive)".to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::duckdb::schema::create_all_tables;
    use tempfile::tempdir;

    fn run(
        storage_root: Option<&Path>,
        conn: Option<&duckdb::Connection>,
        trivium: Option<&TriviumDb>,
        host: &HostContext,
        input: Value,
    ) -> Value {
        evidence_check(
            storage_root,
            conn,
            trivium,
            host,
            &["example.com".to_string()],
            &input,
        )
        .unwrap()
    }

    fn deny_host() -> HostContext {
        HostContext::deny_all()
    }

    /// 输入契约：question 缺失 / modes 空 / 未知 mode → fail 信封。
    #[test]
    fn envelope_rejects_bad_input() {
        let host = deny_host();
        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"modes": ["log"]}),
        );
        assert_eq!(out["success"], false);
        assert!(out["error"].as_str().unwrap().contains("question"));

        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "q", "modes": []}),
        );
        assert_eq!(out["success"], false);

        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "q", "modes": ["gps"]}),
        );
        assert_eq!(out["success"], false);
        assert!(out["error"].as_str().unwrap().contains("unknown mode"));
    }

    /// modes 去重：重复模式只出一个结果。
    #[test]
    fn modes_are_deduplicated() {
        let host = deny_host();
        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "q", "modes": ["log", "log"]}),
        );
        assert_eq!(out["success"], true);
        assert_eq!(out["results"].as_array().unwrap().len(), 1);
    }

    /// log 模式命中：invocations 事实文件的 capability_id / arguments_redacted 匹配。
    #[test]
    fn log_mode_hits_invocation_fact_file() {
        let dir = tempdir().unwrap();
        let invocations = dir.path().join("invocations");
        std::fs::create_dir_all(&invocations).unwrap();
        std::fs::write(
            invocations.join("inv_abc123.json"),
            serde_json::json!({
                "invocation_id": "inv_abc123",
                "capability_id": "probe.run",
                "arguments_redacted": {"task_input": "运行探针并统计结果"},
                "started_at": "2026-10-18T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        // 干扰项：result 文件不参与匹配。
        std::fs::write(
            invocations.join("inv_abc123.result.json"),
            serde_json::json!({"state": "probe run done"}).to_string(),
        )
        .unwrap();

        let host = deny_host();
        let out = run(
            Some(dir.path()),
            None,
            None,
            &host,
            serde_json::json!({"question": "probe.run 是否执行过", "modes": ["log"]}),
        );
        assert_eq!(out["success"], true);
        let result = &out["results"][0];
        assert_eq!(result["mode"], "log");
        assert_eq!(result["hit"], true);
        assert_eq!(result["evidence_ref"], "inv_abc123");
        assert!(result["raw_excerpt"]
            .as_str()
            .unwrap()
            .contains("probe.run"));

        // 不命中问题：hit=false。
        let out = run(
            Some(dir.path()),
            None,
            None,
            &host,
            serde_json::json!({"question": "完全不相关的问题 xyzzy", "modes": ["log"]}),
        );
        assert_eq!(out["results"][0]["hit"], false);
        assert_eq!(out["results"][0]["evidence_ref"], serde_json::Value::Null);
    }

    /// log 模式审计半边：method_call_audit 的 method_id/status 匹配。
    #[test]
    fn log_mode_hits_method_call_audit() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        create_all_tables(&conn).unwrap();
        conn.execute(
            "INSERT INTO method_call_audit (id, method_id, called_at, called_by, status) \
             VALUES ('mca_1', 'um_capability_self_register', '2026-10-18T00:00:00Z', 'sg-9', 'done')",
            [],
        )
        .unwrap();
        let host = deny_host();
        let out = run(
            None,
            Some(&conn),
            None,
            &host,
            serde_json::json!({"question": "um_capability_self_register 调用状态", "modes": ["log"]}),
        );
        let result = &out["results"][0];
        assert_eq!(result["hit"], true);
        assert_eq!(result["evidence_ref"], "method_call_audit:mca_1");
        let excerpt = result["raw_excerpt"].as_str().unwrap();
        assert!(
            excerpt.contains("done") && excerpt.contains("method_id"),
            "{excerpt}"
        );

        // 空表 + 无 storage_root：hit=false，note 说明跳过。
        let empty = duckdb::Connection::open_in_memory().unwrap();
        create_all_tables(&empty).unwrap();
        let out = run(
            None,
            Some(&empty),
            None,
            &host,
            serde_json::json!({"question": "anything", "modes": ["log"]}),
        );
        assert_eq!(out["results"][0]["hit"], false);
        assert!(out["results"][0]["note"]
            .as_str()
            .unwrap()
            .contains("storage_root"));
    }

    /// file 模式命中/不命中 + 隐藏项跳过 + 深度限制。
    #[test]
    fn file_mode_scans_workspace_shallowly() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("notes.md"),
            "部署手册：先跑 probe.run 再看结果",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        std::fs::write(dir.path().join("sub/deeper/deep.txt"), "部署手册 deep").unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join(".hidden/secret.txt"), "部署手册 hidden").unwrap();

        let host = HostContext::for_workspace(dir.path().to_path_buf());
        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "部署手册 在哪", "modes": ["file"]}),
        );
        let result = &out["results"][0];
        assert_eq!(result["mode"], "file");
        assert_eq!(result["hit"], true);
        assert_eq!(result["evidence_ref"], "notes.md");
        assert!(result["raw_excerpt"]
            .as_str()
            .unwrap()
            .contains("probe.run"));
        let matches = result["matches"].as_array().unwrap();
        assert!(matches.iter().any(|m| m == "notes.md"), "{matches:?}");

        // 不命中。
        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "绝不存在的关键词qqzz", "modes": ["file"]}),
        );
        assert_eq!(out["results"][0]["hit"], false);

        // 无读根：miss + note。
        let host = HostContext::deny_all();
        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "anything", "modes": ["file"]}),
        );
        assert_eq!(out["results"][0]["hit"], false);
        assert!(out["results"][0]["note"].as_str().unwrap().contains("root"));
    }

    /// web 模式离线两分支：无 url 明确 miss；http 拒绝。
    #[test]
    fn web_mode_requires_explicit_https_url() {
        let host = deny_host();
        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "any", "modes": ["web"]}),
        );
        let result = &out["results"][0];
        assert_eq!(result["mode"], "web");
        assert_eq!(result["hit"], false);
        assert_eq!(result["note"].as_str().unwrap(), "web 模式需显式 url");

        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "any", "modes": ["web"], "url": "http://example.com/x"}),
        );
        assert_eq!(out["results"][0]["hit"], false);
        assert!(out["results"][0]["note"]
            .as_str()
            .unwrap()
            .contains("https"));

        // https 但域名不在白名单：拒绝（与 web.fetch.public 同策略）。
        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "any", "modes": ["web"], "url": "https://not-allowed.test/x"}),
        );
        assert_eq!(out["results"][0]["hit"], false);
        assert!(out["results"][0]["note"]
            .as_str()
            .unwrap()
            .contains("whitelist"));
    }

    /// memory 模式命中/不命中（临时 TriviumDb）。
    #[test]
    fn memory_mode_retrieves_matching_entry() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("memory.trivium");
        let mut db = TriviumDb::open(&db_path, 4).unwrap();
        let zero = vec![0.0_f32; 4];
        db.db_mut()
            .insert(
                &zero,
                serde_json::json!({
                    "_memory_type": "experience",
                    "insight": "部署手册位于 docs 目录",
                    "context": "runtime acquired",
                }),
            )
            .unwrap();

        let host = deny_host();
        let out = run(
            None,
            None,
            Some(&db),
            &host,
            serde_json::json!({"question": "部署手册 在哪", "modes": ["memory"]}),
        );
        let result = &out["results"][0];
        assert_eq!(result["mode"], "memory");
        assert_eq!(result["hit"], true);
        let evidence = result["evidence_ref"].as_str().unwrap();
        assert!(evidence.starts_with("memory:experience:"), "{evidence}");
        assert!(result["raw_excerpt"].as_str().unwrap().contains("部署手册"));

        let out = run(
            None,
            None,
            Some(&db),
            &host,
            serde_json::json!({"question": "绝不存在的记忆qqzz", "modes": ["memory"]}),
        );
        assert_eq!(out["results"][0]["hit"], false);

        // triviumdb 未配置：miss + note。
        let out = run(
            None,
            None,
            None,
            &host,
            serde_json::json!({"question": "any", "modes": ["memory"]}),
        );
        assert_eq!(out["results"][0]["hit"], false);
        assert!(out["results"][0]["note"]
            .as_str()
            .unwrap()
            .contains("triviumdb"));
    }

    /// 多模式组合：结果按输入顺序、逐模式独立（不做综合判断）。
    #[test]
    fn multi_mode_results_are_independent() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha needle").unwrap();
        let host = HostContext::for_workspace(dir.path().to_path_buf());
        let out = run(
            Some(dir.path()),
            None,
            None,
            &host,
            serde_json::json!({"question": "needle", "modes": ["file", "web"]}),
        );
        assert_eq!(out["success"], true);
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["mode"], "file");
        assert_eq!(results[0]["hit"], true);
        assert_eq!(results[1]["mode"], "web");
        assert_eq!(results[1]["hit"], false);
        assert!(out.get("verdict").is_none(), "不得输出跨模式综合判断");
    }

    /// 摘要边界：多字节字符不被切坏。
    #[test]
    fn excerpt_keeps_char_boundaries() {
        let text = "abcdefg你好世界hijklmn";
        let out = excerpt_around(text, 7, 3);
        assert!(!out.is_empty());
        // 起止都是合法字符边界（能正常 chars 收集即通过）。
        let _ = out.chars().count();
    }
}
