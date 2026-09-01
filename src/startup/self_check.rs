use super::config::Config;
#[cfg(test)]
use super::config::{ContextSection, MergeSection, ModeStyles, UiSection, WebSection};
use crate::common::AgentError;
use crate::data::ModelRow;
use crate::mode_runtime::ModeKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn(String),
    Fail(String),
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: &'static str,
    pub status: CheckStatus,
}

pub async fn run_all(
    config: &Config,
    default_model: &ModelRow,
) -> Result<Vec<CheckResult>, AgentError> {
    let mut results = Vec::new();

    results.push(check_config(config));

    results.push(check_data_dir(config));

    results.push(check_duckdb(config).await);

    results.push(check_data_tables(config).await);

    results.push(check_llm_endpoint(default_model).await);

    results.push(check_capability_table(config).await);

    Ok(results)
}

pub fn check_config(config: &Config) -> CheckResult {
    if config.data_dir.as_os_str().is_empty() {
        return CheckResult {
            name: "config",
            status: CheckStatus::Fail("data_dir 为空".to_string()),
        };
    }
    if config.default_mode.parse::<ModeKind>().is_err() {
        return CheckResult {
            name: "config",
            status: CheckStatus::Fail(format!(
                "default_mode 非法: '{}' (合法: unni/keep/loop)",
                config.default_mode
            )),
        };
    }
    CheckResult {
        name: "config",
        status: CheckStatus::Pass,
    }
}

pub fn check_data_dir(config: &Config) -> CheckResult {
    if let Err(error) = crate::data::permissions::ensure_private_directory(&config.data_dir) {
        return CheckResult {
            name: "data_dir",
            status: CheckStatus::Fail(format!("创建或修复私有数据目录失败: {error}")),
        };
    }
    CheckResult {
        name: "data_dir",
        status: CheckStatus::Pass,
    }
}

pub async fn check_duckdb(config: &Config) -> CheckResult {
    match crate::data::bootstrap(&config.data_dir) {
        Ok(_app) => CheckResult {
            name: "duckdb",
            status: CheckStatus::Pass,
        },
        Err(e) => CheckResult {
            name: "duckdb",
            status: CheckStatus::Fail(format!("bootstrap: {}", e)),
        },
    }
}

pub async fn check_data_tables(config: &Config) -> CheckResult {
    match crate::data::bootstrap(&config.data_dir) {
        Ok(app) => verify_tables(&app.duckdb),
        Err(e) => CheckResult {
            name: "data_tables",
            status: CheckStatus::Fail(format!("bootstrap: {}", e)),
        },
    }
}

fn verify_tables(conn: &duckdb::Connection) -> CheckResult {
    const EXPECTED_TABLES: &[&str] = &[
        "model",
        "agent",
        "base_capability",
        "composite_capability",
        "usage_method",
        "permission_grants",
        "web_fetch_audit",
    ];
    let mut stmt = match conn.prepare(
        "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'main' AND table_type = 'BASE TABLE'",
    ) {
        Ok(s) => s,
        Err(e) => {
            return CheckResult {
                name: "data_tables",
                status: CheckStatus::Fail(format!("prepare: {}", e)),
            };
        }
    };
    let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
        Ok(r) => r,
        Err(e) => {
            return CheckResult {
                name: "data_tables",
                status: CheckStatus::Fail(format!("query: {}", e)),
            };
        }
    };
    let present: std::collections::BTreeSet<String> = rows.filter_map(|r| r.ok()).collect();
    let expected: std::collections::BTreeSet<String> = EXPECTED_TABLES
        .iter()
        .map(|table| (*table).to_string())
        .collect();
    let missing: Vec<_> = expected.difference(&present).cloned().collect();
    let extra: Vec<_> = present.difference(&expected).cloned().collect();
    if missing.is_empty() && extra.is_empty() {
        CheckResult {
            name: "data_tables",
            status: CheckStatus::Pass,
        }
    } else {
        let mut problems = Vec::new();
        if !missing.is_empty() {
            problems.push(format!("缺表: {}", missing.join(", ")));
        }
        if !extra.is_empty() {
            problems.push(format!("多余表: {}", extra.join(", ")));
        }
        CheckResult {
            name: "data_tables",
            status: CheckStatus::Fail(problems.join("; ")),
        }
    }
}

pub async fn check_llm_endpoint(model: &ModelRow) -> CheckResult {
    match crate::startup::init_flow::ping_model(model).await {
        Ok(_) => CheckResult {
            name: "llm_endpoint",
            status: CheckStatus::Pass,
        },
        Err(e) => CheckResult {
            name: "llm_endpoint",
            status: CheckStatus::Warn(format!("ping 失败 (offline 可用): {}", e)),
        },
    }
}

pub async fn check_capability_table(config: &Config) -> CheckResult {
    match crate::data::bootstrap(&config.data_dir) {
        Ok(app) => {
            let total = app.registry.models.len()
                + app.registry.agents.len()
                + app.registry.base_capabilities.len()
                + app.registry.composite_capabilities.len()
                + app.registry.usage_methods.len();
            if total == 0 {
                CheckResult {
                    name: "capability_table",
                    status: CheckStatus::Warn("空表, 启动后加载".to_string()),
                }
            } else {
                CheckResult {
                    name: "capability_table",
                    status: CheckStatus::Pass,
                }
            }
        }
        Err(_) => CheckResult {
            name: "capability_table",
            status: CheckStatus::Warn("bootstrap failed, skip".to_string()),
        },
    }
}

pub fn report(results: &[CheckResult]) -> Result<(), AgentError> {
    let mut failed = 0;
    for r in results {
        match &r.status {
            CheckStatus::Pass => tracing::info!(check = r.name, "✓"),
            CheckStatus::Warn(msg) => tracing::warn!(check = r.name, msg = %msg, "⚠"),
            CheckStatus::Fail(msg) => {
                tracing::error!(check = r.name, msg = %msg, "✗");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        return Err(AgentError::StartupFailed(format!(
            "{} 项 check 失败, 阻塞启动",
            failed
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base_config() -> Config {
        Config {
            provider: "openai".into(),
            model_id: "gpt-4".into(),
            api_key: "sk-test".into(),
            data_dir: PathBuf::from("/tmp/__cipher_dummy__"),
            default_mode: "unni".into(),
            mode_styles: ModeStyles::default(),
            default_model: None,
            context: ContextSection::default(),
            ui: UiSection::default(),
            web: WebSection::default(),
            execution: MergeSection::default(),
            insight: MergeSection::default(),
            memory: MergeSection::default(),
        }
    }

    #[test]
    fn check_config_pass_with_valid_data_dir_and_mode() {
        let r = check_config(&base_config());
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn check_config_fail_when_data_dir_empty() {
        let mut c = base_config();
        c.data_dir = PathBuf::new();
        let r = check_config(&c);
        assert!(matches!(r.status, CheckStatus::Fail(_)), "got: {:?}", r);
    }

    #[test]
    fn check_config_fail_when_default_mode_invalid() {
        let mut c = base_config();
        c.default_mode = "bogus-mode".into();
        let r = check_config(&c);
        match &r.status {
            CheckStatus::Fail(msg) => assert!(msg.contains("default_mode"), "got: {msg}"),
            other => panic!("expected Fail, got: {other:?}"),
        }
    }

    #[test]
    fn check_config_pass_when_default_mode_case_insensitive() {
        let mut c = base_config();
        c.default_mode = "UNNI".into();
        assert!(matches!(check_config(&c).status, CheckStatus::Pass));
        c.default_mode = "Keep".into();
        assert!(matches!(check_config(&c).status, CheckStatus::Pass));
    }

    #[test]
    fn check_data_dir_creates_if_missing() {
        let temporary = tempfile::tempdir().unwrap();
        let nested = temporary.path().join("a/b/c");
        let mut c = base_config();
        c.data_dir = nested.clone();
        let r = check_data_dir(&c);
        assert!(matches!(r.status, CheckStatus::Pass));
        assert!(nested.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn check_data_dir_creates_and_repairs_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let data_dir = temporary.path().join("private-data");
        let mut c = base_config();
        c.data_dir = data_dir.clone();

        assert!(matches!(check_data_dir(&c).status, CheckStatus::Pass));
        assert_eq!(
            std::fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(check_data_dir(&c).status, CheckStatus::Pass));
        assert_eq!(
            std::fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_data_dir_rejects_unsafe_directory_targets() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let mut c = base_config();

        c.data_dir = PathBuf::from("/");
        assert!(matches!(check_data_dir(&c).status, CheckStatus::Fail(_)));

        c.data_dir = std::env::temp_dir();
        assert!(matches!(check_data_dir(&c).status, CheckStatus::Fail(_)));

        let temporary = tempfile::tempdir().unwrap();
        let shared = temporary.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        c.data_dir = shared.clone();
        assert!(matches!(check_data_dir(&c).status, CheckStatus::Fail(_)));
        assert_eq!(
            std::fs::metadata(&shared).unwrap().permissions().mode() & 0o7777,
            0o1777
        );

        let regular_file = temporary.path().join("not-a-directory");
        std::fs::write(&regular_file, b"file").unwrap();
        c.data_dir = regular_file;
        assert!(matches!(check_data_dir(&c).status, CheckStatus::Fail(_)));

        let real_directory = temporary.path().join("real-directory");
        std::fs::create_dir(&real_directory).unwrap();
        let directory_link = temporary.path().join("directory-link");
        symlink(&real_directory, &directory_link).unwrap();
        c.data_dir = directory_link;
        assert!(matches!(check_data_dir(&c).status, CheckStatus::Fail(_)));
    }

    #[test]
    fn report_blocks_on_fail() {
        let results = vec![
            CheckResult {
                name: "config",
                status: CheckStatus::Pass,
            },
            CheckResult {
                name: "data_dir",
                status: CheckStatus::Fail("test".to_string()),
            },
        ];
        let r = report(&results);
        assert!(r.is_err());
        match r.unwrap_err() {
            AgentError::StartupFailed(msg) => assert!(msg.contains("1 项"), "got: {msg}"),
            other => panic!("expected StartupFailed, got: {other:?}"),
        }
    }

    #[test]
    fn report_passes_with_only_warns() {
        let results = vec![
            CheckResult {
                name: "config",
                status: CheckStatus::Pass,
            },
            CheckResult {
                name: "llm_endpoint",
                status: CheckStatus::Warn("offline".to_string()),
            },
        ];
        assert!(report(&results).is_ok());
    }

    #[test]
    fn report_counts_multiple_failures() {
        let results = vec![
            CheckResult {
                name: "a",
                status: CheckStatus::Fail("x".into()),
            },
            CheckResult {
                name: "b",
                status: CheckStatus::Fail("y".into()),
            },
        ];
        match report(&results) {
            Err(AgentError::StartupFailed(msg)) => assert!(msg.contains("2 项"), "got: {msg}"),
            other => panic!("expected StartupFailed with count 2, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_data_tables_passes_after_bootstrap() {
        let tmp =
            std::env::temp_dir().join(format!("cipher-dt-pass-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut c = base_config();
        c.data_dir = tmp.clone();
        let r = check_data_tables(&c).await;
        assert_eq!(r.name, "data_tables");
        match r.status {
            CheckStatus::Pass => {}
            other => panic!("expected Pass, got: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn check_data_tables_fails_when_table_missing() {
        let tmp =
            std::env::temp_dir().join(format!("cipher-dt-fail-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&tmp);

        let app = crate::data::bootstrap(&tmp).expect("bootstrap should succeed");

        app.duckdb
            .execute_batch("DROP TABLE usage_method")
            .expect("drop table should succeed");

        let r = verify_tables(&app.duckdb);
        match r.status {
            CheckStatus::Fail(msg) => assert!(msg.contains("usage_method"), "got: {msg}"),
            other => panic!("expected Fail with missing table name, got: {other:?}"),
        }
        drop(app);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn verify_tables_rejects_any_extra_legacy_table() {
        let connection = duckdb::Connection::open_in_memory().unwrap();
        crate::data::duckdb::create_all_tables(&connection).unwrap();
        connection
            .execute_batch("CREATE TABLE workspace(id TEXT)")
            .unwrap();

        match verify_tables(&connection).status {
            CheckStatus::Fail(message) => assert!(message.contains("workspace"), "{message}"),
            other => panic!("expected extra table failure, got {other:?}"),
        }
    }
}
