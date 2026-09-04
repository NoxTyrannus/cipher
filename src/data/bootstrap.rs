use crate::common::AgentError;
use std::path::Path;

use super::duckdb::{load_all_into_memory, Registry};
use super::migration::{prepare_data_dir, validate_current_duckdb_connection, DataPaths};
use super::permissions::secure_existing_file;
use super::workspace_store::WorkspaceStore;

const DUCKDB_FILE_SUFFIXES: &[&str] = &["", ".wal", ".wal.checkpoint", ".wal.recovery"];

pub struct AppState {
    pub duckdb: duckdb::Connection,

    pub registry: Registry,

    pub paths: DataPaths,
}

pub fn bootstrap(data_dir: &Path) -> Result<AppState, AgentError> {
    let paths = prepare_data_dir(data_dir)?;

    let duckdb_path = paths.duckdb();
    secure_duckdb_files(&duckdb_path)?;
    let conn = match duckdb::Connection::open(&duckdb_path) {
        Ok(conn) => conn,
        Err(error) => {
            let open_error =
                AgentError::Bootstrap(format!("open DuckDB {:?}: {}", duckdb_path, error));
            return Err(merge_permission_error(
                open_error,
                secure_duckdb_files(&duckdb_path),
            ));
        }
    };
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
