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
    /// 默认路径行为：仅 workspace_root 作为读写根。
    pub fn for_workspace(workspace_root: PathBuf) -> Self {
        Self::for_workspace_with_roots(workspace_root, Vec::new())
    }

    /// v0.4.8：在 workspace_root 之外追加文件读根（来自 `[fs] read_roots`）。
    /// 读根 = `[workspace_root] + extra`；写根维持 workspace_root 不变。
    /// extra 为空时行为与 `for_workspace` 完全一致（回归）。
    pub fn for_workspace_with_roots(
        workspace_root: PathBuf,
        extra_read_roots: Vec<PathBuf>,
    ) -> Self {
        let mut read_roots = vec![workspace_root.clone()];
        read_roots.extend(extra_read_roots);
        Self {
            permission: PermissionSnapshot {
                file_read_roots: read_roots,
                file_write_roots: vec![workspace_root],
                shell_exec_allowed: true,
            },
            budget: BudgetSnapshot::default(),
        }
    }

    /// v0.4.8：追加额外文件读根（不改写根）。
    /// 供 `CapabilityExecutor::set_extra_read_roots` 在 `set_workspace_root` 之后调用，
    /// 把 `[fs] read_roots` 并入读根列表。extra 为空时无操作（读根保持现状）。
    pub fn add_read_roots(&mut self, extra: &[PathBuf]) {
        self.permission
            .file_read_roots
            .extend(extra.iter().cloned());
    }

    pub fn deny_all() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_workspace_roots_are_only_workspace() {
        let ctx = HostContext::for_workspace(PathBuf::from("/tmp/ws"));
        assert_eq!(
            ctx.permission.file_read_roots,
            vec![PathBuf::from("/tmp/ws")]
        );
        assert_eq!(
            ctx.permission.file_write_roots,
            vec![PathBuf::from("/tmp/ws")]
        );
    }

    #[test]
    fn for_workspace_with_roots_appends_extra_read_roots_but_not_write_roots() {
        let ctx = HostContext::for_workspace_with_roots(
            PathBuf::from("/tmp/ws"),
            vec![PathBuf::from("/tmp/extra"), PathBuf::from("/mnt/shared")],
        );
        assert_eq!(
            ctx.permission.file_read_roots,
            vec![
                PathBuf::from("/tmp/ws"),
                PathBuf::from("/tmp/extra"),
                PathBuf::from("/mnt/shared")
            ]
        );
        // 写根维持 workspace_root 不变。
        assert_eq!(
            ctx.permission.file_write_roots,
            vec![PathBuf::from("/tmp/ws")]
        );
    }

    #[test]
    fn for_workspace_with_empty_extra_matches_for_workspace() {
        let ctx = HostContext::for_workspace_with_roots(PathBuf::from("/tmp/ws"), Vec::new());
        assert_eq!(
            ctx.permission.file_read_roots,
            vec![PathBuf::from("/tmp/ws")]
        );
        assert_eq!(
            ctx.permission.file_write_roots,
            vec![PathBuf::from("/tmp/ws")]
        );
    }
}
