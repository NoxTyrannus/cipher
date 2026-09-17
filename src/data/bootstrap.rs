use crate::common::AgentError;
use std::path::Path;
use std::time::Duration;

use super::duckdb::{load_all_into_memory, Registry};
use super::migration::{prepare_data_dir, validate_current_duckdb_connection, DataPaths};
use super::permissions::secure_existing_file;
use super::workspace_store::WorkspaceStore;

const DUCKDB_FILE_SUFFIXES: &[&str] = &["", ".wal", ".wal.checkpoint", ".wal.recovery"];

/// C2（v0.5.4）：DuckDB 文件锁冲突时的重试参数（至多 3 次、间隔 1s）。
const LOCK_RETRY_LIMIT: usize = 3;
const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// C1 分类纯函数：duckdb 打开错误串是否为文件锁冲突
///（另一活实例仍持有写锁；文件锁随进程存亡，无残留锁文件）。
pub fn is_lock_conflict_error(message: &str) -> bool {
    message.contains("Could not set lock") || message.contains("Conflicting lock")
}

/// C1 PID 提取纯函数：优先取 `PID <n>`，其次 `/proc/<n>/`；提不出返回 None。
pub fn extract_lock_pid(message: &str) -> Option<u32> {
    fn digits_after(haystack: &str, marker: &str) -> Option<u32> {
        let idx = haystack.find(marker)?;
        let rest = &haystack[idx + marker.len()..];
        let end = rest
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        rest[..end].parse().ok()
    }
    digits_after(message, "PID ").or_else(|| digits_after(message, "/proc/"))
}

pub struct AppState {
    pub duckdb: duckdb::Connection,

    pub registry: Registry,

    pub paths: DataPaths,
}

pub fn bootstrap(data_dir: &Path) -> Result<AppState, AgentError> {
    let paths = prepare_data_dir(data_dir)?;

    let duckdb_path = paths.duckdb();
    secure_duckdb_files(&duckdb_path)?;
    let conn = open_duckdb_with_lock_retry(&duckdb_path)?;
    if let Err(error) = secure_duckdb_files(&duckdb_path) {
        drop(conn);
        return Err(merge_permission_error(
            error,
            secure_duckdb_files(&duckdb_path),
        ));
    }

    // v0.4.4 旧数据目录升级：v2 五表库无 permission_grants 审计表，先幂等补建，
    // 否则下方表集精确校验（TARGET_TABLES 六表）会失败导致启动报错。
    if let Err(error) = super::migration::ensure_permission_grants_table(&conn) {
        drop(conn);
        return Err(merge_permission_error(
            error,
            secure_duckdb_files(&duckdb_path),
        ));
    }
    // v0.4.6 旧数据目录升级：六表库无 web_fetch_audit 审计表，同样先幂等补建
    // （否则表集精确校验——TARGET_TABLES 七表——会失败导致启动报错）。
    if let Err(error) = super::migration::ensure_web_fetch_audit_table(&conn) {
        drop(conn);
        return Err(merge_permission_error(
            error,
            secure_duckdb_files(&duckdb_path),
        ));
    }

    // v0.5.0 旧数据目录补建方法调用审计表。
    if let Err(error) = super::migration::ensure_method_call_audit_table(&conn) {
        drop(conn);
        return Err(merge_permission_error(
            error,
            secure_duckdb_files(&duckdb_path),
        ));
    }

    if let Err(error) = validate_current_duckdb_connection(&conn) {
        drop(conn);
        return Err(merge_permission_error(
            error,
            secure_duckdb_files(&duckdb_path),
        ));
    }

    let workspace_store = WorkspaceStore::open(paths.storage_root())?;
    workspace_store.initialize()?;
    workspace_store.list()?;

    let registry = match load_all_into_memory(&conn) {
        Ok(registry) => registry,
        Err(error) => {
            drop(conn);
            return Err(merge_permission_error(
                error,
                secure_duckdb_files(&duckdb_path),
            ));
        }
    };
    secure_duckdb_files(&duckdb_path)?;

    Ok(AppState {
        duckdb: conn,
        registry,
        paths,
    })
}

fn secure_duckdb_files(database_path: &Path) -> Result<(), AgentError> {
    let path = database_path.to_string_lossy();
    for suffix in DUCKDB_FILE_SUFFIXES {
        secure_existing_file(Path::new(&format!("{path}{suffix}")))?;
    }
    Ok(())
}

/// C1+C2（v0.5.4）：打开活动 DuckDB。锁冲突（另一活实例持有文件锁）先短重试
/// （至多 3 次、间隔 1s）；仍失败转中文可操作提示（含占用 PID，提不出则不带）。
/// 非锁冲突错误走原路径不变（含权限修复合并逻辑）。
fn open_duckdb_with_lock_retry(database_path: &Path) -> Result<duckdb::Connection, AgentError> {
    let mut attempt = 0usize;
    loop {
        match duckdb::Connection::open(database_path) {
            Ok(conn) => return Ok(conn),
            Err(error) => {
                let message = error.to_string();
                if !is_lock_conflict_error(&message) {
                    let open_error = AgentError::Bootstrap(format!(
                        "open DuckDB {:?}: {}",
                        database_path, error
                    ));
                    return Err(merge_permission_error(
                        open_error,
                        secure_duckdb_files(database_path),
                    ));
                }
                if attempt >= LOCK_RETRY_LIMIT {
                    let pid_hint = match extract_lock_pid(&message) {
                        Some(pid) => format!("（占用进程 PID {pid}）"),
                        None => format!("（{message}）"),
                    };
                    tracing::error!(
                        database = %database_path.display(),
                        "bootstrap: DuckDB 锁冲突重试 {} 次仍失败",
                        LOCK_RETRY_LIMIT
                    );
                    return Err(AgentError::Bootstrap(format!(
                        "模型数据库被占用：可能有另一个 cipher 正在运行{pid_hint}。关闭它或等其退出后重试"
                    )));
                }
                attempt += 1;
                tracing::warn!(
                    database = %database_path.display(),
                    attempt,
                    "bootstrap: DuckDB 锁冲突, 短暂等待后重试"
                );
                std::thread::sleep(LOCK_RETRY_INTERVAL);
            }
        }
    }
}

fn merge_permission_error(
    operation_error: AgentError,
    permission_result: Result<(), AgentError>,
) -> AgentError {
    match permission_result {
        Ok(()) => operation_error,
        Err(permission_error) => AgentError::Bootstrap(format!(
            "{operation_error}; additionally failed to secure DuckDB files: {permission_error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn lock_conflict_classifier_matches_duckdb_messages() {
        // C1：duckdb 锁冲突特征串。
        assert!(is_lock_conflict_error(
            "IO Error: Could not set lock on file \"/x/cipher.duckdb\": Conflicting lock is held in /proc/23039 (PID 23039)"
        ));
        assert!(is_lock_conflict_error("Could not set lock on file"));
        assert!(is_lock_conflict_error("Conflicting lock is held"));
        assert!(
            !is_lock_conflict_error("Os { code: 13, kind: PermissionDenied }"),
            "权限错误不是锁冲突"
        );
        assert!(!is_lock_conflict_error("invalid database file"));
    }

    #[test]
    fn lock_pid_extracted_from_pid_marker_first() {
        // C1：优先 `PID ` 后数字。
        assert_eq!(
            extract_lock_pid(
                "Could not set lock: Conflicting lock is held in /proc/23039 (PID 23039)"
            ),
            Some(23039)
        );
    }

    #[test]
    fn lock_pid_extracted_from_proc_path() {
        // C1：无 `PID ` 标记时回退 `/proc/<n>/`。
        assert_eq!(
            extract_lock_pid("Conflicting lock is held in /proc/4242"),
            Some(4242)
        );
    }

    #[test]
    fn lock_pid_extraction_returns_none_when_absent() {
        assert_eq!(extract_lock_pid("Could not set lock on file"), None);
        assert_eq!(extract_lock_pid(""), None);
        // `/proc/` 后无数字 → None。
        assert_eq!(extract_lock_pid("held in /proc/self"), None);
    }

    #[test]
    fn bootstrap_creates_data_dir_and_opens_duckdb() {
        let tmp = env::temp_dir().join(format!("cipher-bootstrap-{}", std::process::id()));

        let data_dir = tmp.join("fresh");
        let _ = std::fs::remove_dir_all(&data_dir);

        let app = bootstrap(&data_dir).expect("bootstrap should succeed");

        assert!(data_dir.exists(), "data_dir should exist after bootstrap");
        assert!(app.paths.duckdb().exists(), "active DuckDB should exist");
        assert_ne!(app.paths.storage_root(), data_dir);

        for table in &[
            "model",
            "agent",
            "base_capability",
            "composite_capability",
            "usage_method",
            "permission_grants",
            "web_fetch_audit",
        ] {
            let mut stmt = app
                .duckdb
                .prepare(&format!("SELECT 1 FROM {} LIMIT 0", table))
                .unwrap_or_else(|e| panic!("table {} missing after bootstrap: {}", table, e));
            let _ = stmt
                .query_map([], |_row| Ok(()))
                .unwrap_or_else(|e| panic!("table {} query failed: {}", table, e));
        }

        drop(app);
        let app2 = bootstrap(&data_dir).expect("second bootstrap should also succeed");

        assert_eq!(
            app2.registry.models.len(),
            0,
            "registry should be empty after bootstrap (prod no seed; models via init_flow)"
        );
        drop(app2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bootstrap_returns_bootstrap_error_when_data_dir_invalid() {
        let invalid = std::path::PathBuf::from("/dev/null/invalid");
        match bootstrap(&invalid) {
            Err(crate::common::AgentError::Bootstrap(msg)) => {
                assert!(msg.contains("private directory"), "got: {msg}");
            }
            Err(other) => panic!("expected Bootstrap error, got: {other:?}"),
            Ok(_) => panic!("expected failure, got Ok"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_repairs_data_directory_and_duckdb_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let data_dir = temporary.path().join("data");
        let app = bootstrap(&data_dir).unwrap();

        let database = app.paths.duckdb();
        assert!(
            database.is_file(),
            "expected {} to exist",
            database.display()
        );
        assert_eq!(
            std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(app);

        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();
        let app = bootstrap(&data_dir).unwrap();

        assert_eq!(
            std::fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            database.is_file(),
            "expected {} to exist",
            database.display()
        );
        assert_eq!(
            std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(app);
    }

    #[cfg(unix)]
    #[test]
    fn secure_duckdb_files_repairs_every_known_existing_artifact() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("cipher.duckdb");
        let base = database.to_string_lossy();
        let artifacts: Vec<_> = DUCKDB_FILE_SUFFIXES
            .iter()
            .map(|suffix| Path::new(&format!("{base}{suffix}")).to_path_buf())
            .collect();

        for artifact in &artifacts {
            std::fs::write(artifact, b"permission fixture").unwrap();
            std::fs::set_permissions(artifact, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                artifact.is_file(),
                "expected {} to exist",
                artifact.display()
            );
        }

        secure_duckdb_files(&database).unwrap();

        for artifact in &artifacts {
            assert!(
                artifact.is_file(),
                "expected {} to exist",
                artifact.display()
            );
            assert_eq!(
                std::fs::metadata(artifact).unwrap().permissions().mode() & 0o777,
                0o600,
                "unexpected mode for {}",
                artifact.display()
            );
        }
    }
}
