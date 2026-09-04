//! `web.fetch.public` 能力本体（v0.4.6）：仅 HTTPS GET + 域名白名单 + 拒绝重定向 +
//! 响应体 ≤200KB 硬限 + 10s 超时 + 每次调用全量审计落库 `web_fetch_audit`。
//!
//! 任务书边界（冻结）：
//! - 不做 HTML→Markdown 转换（正文 = `response.text()` 原样截断 50K 字符输出 content）；
//! - 执行与 workspace 无关（正文仅返回内存，不落盘）；
//! - 不 seed 进任何 allowlist（授权仅经 v0.4.5 `permission.grant` 授予）；
//! - 失败语义：结构化错误返回（`{success:false, error:{code,message}}`），不 panic。
//!
//! 测试注记：`HttpGetter` 抽象使取数层可注入——生产用 [`ReqwestGetter`]（reqwest 异步 +
//! 同步桥，executor 在 async 上下文内同步调用），单测用 mock（重定向/超限/成功路径全部
//! 离线确定性可测）。https/白名单判定在取数之前，可直接测完整管线。

use serde_json::{json, Value};
use std::time::Duration;

pub const CAPABILITY_ID: &str = "web.fetch.public";

/// 请求超时（秒）。
pub const TIMEOUT_SECS: u64 = 10;
/// 响应体硬限（字节）。
pub const MAX_BODY_BYTES: usize = 200 * 1024;
/// 输出正文截断（字符）。
pub const MAX_CONTENT_CHARS: usize = 50_000;

/// 一次 GET 的结果（HTTP 状态 + 响应体）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 可注入的 HTTP GET 抽象：生产用 [`ReqwestGetter`]，单测用 mock。
pub trait HttpGetter: Send + Sync {
    fn get(&self, url: &str) -> Result<FetchOutcome, String>;
}

/// 生产实现：reqwest 异步 client（timeout 10s、redirect policy=none）+ 同步桥。
pub struct ReqwestGetter {
    client: reqwest::Client,
}

impl ReqwestGetter {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("build client: {e}"))?;
        Ok(Self { client })
    }
}

impl HttpGetter for ReqwestGetter {
    fn get(&self, url: &str) -> Result<FetchOutcome, String> {
        let client = &self.client;
        let future = async move {
            let response = client.get(url).send().await.map_err(|e| e.to_string())?;
            let status = response.status().as_u16();
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| e.to_string())?;
                body.extend_from_slice(&chunk);
                // 超出硬限立即停止读取（不缓存超限正文）。
                if body.len() > MAX_BODY_BYTES {
                    return Ok(FetchOutcome { status, body });
                }
            }
            Ok(FetchOutcome { status, body })
        };
        drive(future)?
    }
}

/// 同步桥：在 tokio 多线程 runtime 内用 `block_in_place` + 当前 handle 驱动异步请求；
/// 无 runtime（单测等）时用一次性 current_thread runtime。
fn drive<F>(future: F) -> Result<F::Output, String>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            Ok(tokio::task::block_in_place(|| handle.block_on(future)))
        }
        Ok(_) => {
            // current_thread runtime 内：换线程跑一次性 runtime，避免嵌套 block_on 死锁。
            std::thread::scope(|scope| {
                scope
                    .spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| format!("build runtime: {e}"))?;
                        Ok(rt.block_on(future))
                    })
                    .join()
                    .map_err(|_| "runtime thread panicked".to_string())?
            })
        }
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("build runtime: {e}"))?;
            Ok(rt.block_on(future))
        }
    }
}

/// `web.fetch.public` 执行入口（executor 分派调用）。
pub fn execute(
    conn: &duckdb::Connection,
    allowed_domains: &[String],
    actor_id: &str,
    input: &Value,
) -> Result<Value, String> {
    let getter = ReqwestGetter::new()?;
    execute_with_getter(conn, allowed_domains, actor_id, input, &getter)
}

/// 可注入 getter 的执行管线（单测直接调用）。
fn execute_with_getter(
    conn: &duckdb::Connection,
    allowed_domains: &[String],
    actor_id: &str,
    input: &Value,
    getter: &dyn HttpGetter,
) -> Result<Value, String> {
    // 1) input：url 必填字符串。
    let url = match input.get("url").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => {
            return record_and_fail(
                conn,
                actor_id,
                "",
                AuditMetrics::default(),
                "missing_url",
                "missing or empty 'url'",
            )
        }
    };
    // 2) 解析 + 仅 HTTPS。
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => {
            return record_and_fail(
                conn,
                actor_id,
                url,
                AuditMetrics::default(),
                "invalid_url",
                &format!("invalid url: {error}"),
            )
        }
    };
    if parsed.scheme() != "https" {
        return record_and_fail(
            conn,
            actor_id,
            url,
            AuditMetrics::default(),
            "https_required",
            "only https URLs are allowed",
        );
    }
    let Some(host) = parsed.host_str() else {
        return record_and_fail(
            conn,
            actor_id,
            url,
            AuditMetrics::default(),
            "invalid_url",
            "url has no host",
        );
    };
    // 3) 域名白名单（host 精确或子域匹配；端口忽略——仅 https，默认 443）。
    if !is_host_allowed(host, allowed_domains) {
        return record_and_fail(
            conn,
            actor_id,
            url,
            AuditMetrics::default(),
            "domain_not_allowed",
            &format!("domain '{host}' is not in the whitelist"),
        );
    }
    // 4) GET（timeout 10s、redirect policy=none 已在 client 配置）。
    let outcome = match getter.get(url) {
        Ok(outcome) => outcome,
        Err(error) => {
            let code = classify_fetch_error(&error);
            return record_and_fail(conn, actor_id, url, AuditMetrics::default(), code, &error);
        }
    };
    let status = i64::from(outcome.status);
    // 5) 重定向一律拒绝（policy=none 时 3xx 原样返回，不跟随）。
    if (300..400).contains(&outcome.status) {
        return record_and_fail(
            conn,
            actor_id,
            url,
            AuditMetrics {
                http_code: Some(status),
                ..AuditMetrics::default()
            },
            "redirect_rejected",
            &format!("redirects are rejected (status {})", outcome.status),
        );
    }
    // 6) 外部语义：非 2xx 一律视为外部失败（execut 仍为 done）。
    if !(200..300).contains(&outcome.status) {
        return record_and_fail(
            conn,
            actor_id,
            url,
            AuditMetrics {
                http_code: Some(status),
                ..AuditMetrics::default()
            },
            "http_error",
            &format!("HTTP {} response", outcome.status),
        );
    }
    // 7) 响应体 ≤200KB 硬限。
    let bytes = outcome.body.len() as i64;
    if outcome.body.len() > MAX_BODY_BYTES {
        return record_and_fail(
            conn,
            actor_id,
            url,
            AuditMetrics {
                http_code: Some(status),
                bytes: Some(bytes),
                ..AuditMetrics::default()
            },
            "size_limit_exceeded",
            &format!("response body exceeds {} bytes", MAX_BODY_BYTES),
        );
    }
    // 7) 正文提取：UTF-8 解码（lossy），按字符截断 50K 输出 content。
    let text = String::from_utf8_lossy(&outcome.body);
    let extracted_chars = text.chars().count() as i64;
    let content: String = text.chars().take(MAX_CONTENT_CHARS).collect();
    // 8) 审计（成功行：error 为空字符串）。
    write_audit(
        conn,
        actor_id,
        url,
        Some(status),
        Some(bytes),
        Some(extracted_chars),
        "",
    )?;
    Ok(json!({
        "execut": "done",
        "success": true,
        "url": url,
        "http_code": status,
        "bytes": bytes,
        "extracted_chars": extracted_chars,
        "content": content,
    }))
}

/// host 精确匹配白名单条目，或为其子域（`www.kaggle.com` 匹配 `kaggle.com`）。
/// 大小写不敏感；端口由调用方忽略（本能力仅 https，默认 443）。
pub fn is_host_allowed(host: &str, allowed_domains: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allowed_domains.iter().any(|entry| {
        let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
        if entry.is_empty() {
            return false;
        }
        host == entry || host.ends_with(&format!(".{entry}"))
    })
}

fn classify_fetch_error(error: &str) -> &'static str {
    if error.contains("timed out") || error.contains("timeout") {
        "timeout"
    } else {
        "network_error"
    }
}

/// 审计指标（失败路径按实际情况回填，可空）。
#[derive(Debug, Clone, Copy, Default)]
struct AuditMetrics {
    http_code: Option<i64>,
    bytes: Option<i64>,
    extracted_chars: Option<i64>,
}

/// 失败路径：审计行（error=code） + 结构化错误返回。
fn record_and_fail(
    conn: &duckdb::Connection,
    actor_id: &str,
    url: &str,
    metrics: AuditMetrics,
    code: &str,
    message: &str,
) -> Result<Value, String> {
    write_audit(
        conn,
        actor_id,
        url,
        metrics.http_code,
        metrics.bytes,
        metrics.extracted_chars,
        code,
    )?;
    let mut value = fail(code, message);
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "url".to_string(),
            serde_json::Value::String(url.to_string()),
        );
        if let Some(http_code) = metrics.http_code {
            obj.insert(
                "http_code".to_string(),
                serde_json::Value::Number(http_code.into()),
            );
        }
        if let Some(bytes) = metrics.bytes {
            obj.insert("bytes".to_string(), serde_json::Value::Number(bytes.into()));
        }
        if let Some(chars) = metrics.extracted_chars {
            obj.insert(
                "extracted_chars".to_string(),
                serde_json::Value::Number(chars.into()),
            );
        }
    }
    Ok(value)
}

fn fail(code: &str, message: &str) -> Value {
    json!({
        "execut": "done",
        "success": false,
        "error": {"code": code, "message": message},
    })
}

/// 审计落库：每次调用一行（error 为空字符串=成功，非空=结构化错误 code）。
fn write_audit(
    conn: &duckdb::Connection,
    called_by: &str,
    url: &str,
    http_code: Option<i64>,
    bytes: Option<i64>,
    extracted_chars: Option<i64>,
    error: &str,
) -> Result<(), String> {
    use chrono::{SecondsFormat, Utc};
    let id = uuid::Uuid::new_v4().simple().to_string();
    let called_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    conn.execute(
        "INSERT INTO web_fetch_audit \
         (id, called_at, called_by, url, execut, http_code, bytes, extracted_chars, error) \
         VALUES (?, ?, ?, ?, 'done', ?, ?, ?, ?)",
        duckdb::params![
            id,
            called_at,
            called_by,
            url,
            http_code,
            bytes,
            extracted_chars,
            error,
        ],
    )
    .map_err(|e| format!("web_fetch_audit insert: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::duckdb::schema::create_all_tables;

    fn memory_db() -> duckdb::Connection {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        create_all_tables(&conn).unwrap();
        conn
    }

    /// mock getter：预设 status/body（可注入传输错误）。
    struct MockGetter {
        status: u16,
        body: Vec<u8>,
        error: Option<String>,
    }

    impl MockGetter {
        fn ok(status: u16, body: &[u8]) -> Self {
            Self {
                status,
                body: body.to_vec(),
                error: None,
            }
        }
        fn transport_error(message: &str) -> Self {
            Self {
                status: 0,
                body: Vec::new(),
                error: Some(message.to_string()),
            }
        }
    }

    impl HttpGetter for MockGetter {
        fn get(&self, _url: &str) -> Result<FetchOutcome, String> {
            match &self.error {
                Some(error) => Err(error.clone()),
                None => Ok(FetchOutcome {
                    status: self.status,
                    body: self.body.clone(),
                }),
            }
        }
    }

    fn audit_rows(conn: &duckdb::Connection) -> Vec<(String, String, i64, i64, i64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT called_by, url, COALESCE(http_code, -1), COALESCE(bytes, -1), \
                 COALESCE(extracted_chars, -1), COALESCE(error, '') \
                 FROM web_fetch_audit ORDER BY called_at",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    }

    #[test]
    fn missing_url_returns_structured_error_and_audits() {
        let conn = memory_db();
        let out = execute_with_getter(&conn, &[], "sg-1", &json!({}), &MockGetter::ok(200, b"x"))
            .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(out["error"]["code"], "missing_url");
        let rows = audit_rows(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].5, "missing_url");
    }

    #[test]
    fn http_url_rejected_before_fetch() {
        let conn = memory_db();
        let out = execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "http://kaggle.com/page"}),
            &MockGetter::ok(200, b"x"),
        )
        .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(out["error"]["code"], "https_required");
        let rows = audit_rows(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].5, "https_required");
    }

    #[test]
    fn invalid_url_rejected() {
        let conn = memory_db();
        let out = execute_with_getter(
            &conn,
            &[],
            "sg-1",
            &json!({"url": "not a url"}),
            &MockGetter::ok(200, b"x"),
        )
        .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(out["error"]["code"], "invalid_url");
    }

    #[test]
    fn whitelist_rejects_domain_not_allowed() {
        let conn = memory_db();
        let out = execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "https://evil.example/page"}),
            &MockGetter::ok(200, b"x"),
        )
        .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(out["error"]["code"], "domain_not_allowed");
        let rows = audit_rows(&conn);
        assert_eq!(rows[0].5, "domain_not_allowed");
    }

    #[test]
    fn whitelist_empty_rejects_everything() {
        // 缺省空列表 = 拒绝全部（安全默认）。
        let conn = memory_db();
        let out = execute_with_getter(
            &conn,
            &[],
            "sg-1",
            &json!({"url": "https://kaggle.com/page"}),
            &MockGetter::ok(200, b"x"),
        )
        .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(out["error"]["code"], "domain_not_allowed");
    }

    #[test]
    fn host_matching_exact_subdomain_and_negative() {
        let allow: Vec<String> = ["kaggle.com", "www.kaggle.com"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(is_host_allowed("kaggle.com", &allow));
        assert!(is_host_allowed("www.kaggle.com", &allow));
        assert!(is_host_allowed("deep.sub.kaggle.com", &allow), "子域应匹配");
        assert!(!is_host_allowed("example.com", &allow));
        assert!(
            !is_host_allowed("notkaggle.com", &allow),
            "前缀相似域名不得匹配"
        );
        assert!(!is_host_allowed("kaggle.com.evil.example", &allow));
        // 大小写不敏感；尾点归一。
        assert!(is_host_allowed("KAGGLE.COM", &allow));
        assert!(is_host_allowed("kaggle.com.", &allow));
        // 空条目忽略。
        assert!(!is_host_allowed("kaggle.com", &[String::new()]));
    }

    #[test]
    fn success_path_fields_bytes_and_truncation() {
        let conn = memory_db();
        // 正文 60K 个 'a'（> 50K 字符截断线）。
        let body = vec![b'a'; 60_000];
        let out = execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "https://www.kaggle.com/dataset?page=1"}),
            &MockGetter::ok(200, &body),
        )
        .unwrap();
        assert_eq!(out["success"], true);
        assert_eq!(out["url"], "https://www.kaggle.com/dataset?page=1");
        assert_eq!(out["http_code"], 200);
        assert_eq!(out["bytes"], 60_000);
        assert_eq!(
            out["extracted_chars"], 60_000,
            "extracted_chars 为全文 char 数（截断前）"
        );
        assert_eq!(
            out["content"].as_str().unwrap().chars().count(),
            MAX_CONTENT_CHARS,
            "content 截断到 50K 字符"
        );
        let rows = audit_rows(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].5, "", "成功行 error 为空字符串");
        assert_eq!(rows[0].2, 200);
        assert_eq!(rows[0].3, 60_000);
        assert_eq!(rows[0].4, 60_000);
    }

    #[test]
    fn http_404_is_external_failure_but_execution_done() {
        let conn = memory_db();
        let out = execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "https://kaggle.com/missing"}),
            &MockGetter::ok(404, b"<html>not found</html>"),
        )
        .unwrap();
        assert_eq!(out["execut"], "done");
        assert_eq!(out["success"], false);
        assert_eq!(out["http_code"], 404);
        assert_eq!(out["error"]["code"], "http_error");
        let rows = audit_rows(&conn);
        assert_eq!(rows[0].5, "http_error");
    }

    #[test]
    fn redirect_rejected_with_status() {
        let conn = memory_db();
        let out = execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "https://kaggle.com/old"}),
            &MockGetter::ok(301, b""),
        )
        .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(out["error"]["code"], "redirect_rejected");
        let rows = audit_rows(&conn);
        assert_eq!(rows[0].5, "redirect_rejected");
        assert_eq!(rows[0].2, 301, "审计应记录 3xx 状态");
    }

    #[test]
    fn size_limit_exceeded_at_200kb_plus_one() {
        let conn = memory_db();
        let body = vec![b'x'; MAX_BODY_BYTES + 1];
        let out = execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "https://kaggle.com/big"}),
            &MockGetter::ok(200, &body),
        )
        .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(out["error"]["code"], "size_limit_exceeded");
        let rows = audit_rows(&conn);
        assert_eq!(rows[0].5, "size_limit_exceeded");
        assert_eq!(rows[0].3, (MAX_BODY_BYTES + 1) as i64);
    }

    #[test]
    fn transport_timeout_classified_as_timeout() {
        let conn = memory_db();
        let out = execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "https://kaggle.com/slow"}),
            &MockGetter::transport_error("error sending request: operation timed out"),
        )
        .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(out["error"]["code"], "timeout");
    }

    #[test]
    fn audit_rows_written_for_success_and_failure() {
        let conn = memory_db();
        // 成功 1 次 + 失败 2 次（域名拒绝 + 重定向）→ 3 行全量落库。
        execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "https://kaggle.com/ok"}),
            &MockGetter::ok(200, b"hello"),
        )
        .unwrap();
        execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-1",
            &json!({"url": "https://evil.example/x"}),
            &MockGetter::ok(200, b"x"),
        )
        .unwrap();
        execute_with_getter(
            &conn,
            &["kaggle.com".to_string()],
            "sg-2",
            &json!({"url": "https://kaggle.com/redir"}),
            &MockGetter::ok(302, b""),
        )
        .unwrap();
        let rows = audit_rows(&conn);
        assert_eq!(rows.len(), 3, "每次调用全量落库");
        assert_eq!(rows[0].0, "sg-1");
        assert_eq!(rows[1].0, "sg-1");
        assert_eq!(rows[2].0, "sg-2");
        assert_eq!(rows[0].5, "");
        assert_eq!(rows[1].5, "domain_not_allowed");
        assert_eq!(rows[2].5, "redirect_rejected");
    }
}
