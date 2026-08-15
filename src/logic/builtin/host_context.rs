use std::path::PathBuf;

/// 权限快照：builtin 能力直连 host 时仍保留与 WASM 时期一致的安全边界。
#[derive(Debug, Clone, Default)]
pub struct PermissionSnapshot {
    pub file_read_roots: Vec<PathBuf>,
    pub file_write_roots: Vec<PathBuf>,
    pub shell_exec_allowed: bool,
}

/// 预算快照：builtin 能力直连 host 时仍保留与 WASM 时期一致的资源边界。
#[derive(Debug, Clone)]
pub struct BudgetSnapshot {
    pub max_file_read_bytes: u64,
    pub max_file_write_bytes: u64,
}

impl Default for BudgetSnapshot {
    fn default() -> Self {
        Self {
            max_file_read_bytes: 10_485_760,
            max_file_write_bytes: 1_048_576,
        }
    }
}

impl BudgetSnapshot {
    pub fn unlimited() -> Self {
        Self {
            max_file_read_bytes: u64::MAX,
            max_file_write_bytes: u64::MAX,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HostContext {
    pub permission: PermissionSnapshot,
    pub budget: BudgetSnapshot,
}

impl HostContext {
    pub fn for_workspace(workspace_root: PathBuf) -> Self {
        Self {
            permission: PermissionSnapshot {
                file_read_roots: vec![workspace_root.clone()],
                file_write_roots: vec![workspace_root],
                shell_exec_allowed: true,
            },
            budget: BudgetSnapshot::default(),
        }
    }

    pub fn deny_all() -> Self {
        Self::default()
    }
}
