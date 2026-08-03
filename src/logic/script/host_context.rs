use crate::data::triviumdb::TriviumDb;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct PermissionSnapshot {
    pub db_read: bool,
    pub db_write: bool,
    pub memory_read: bool,
    pub memory_write: bool,
    pub file_read_roots: Vec<PathBuf>,
    pub file_write_roots: Vec<PathBuf>,
    pub shell_exec_allowed: bool,
}

impl PermissionSnapshot {
    pub fn deny_all() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone)]
pub struct BudgetSnapshot {
    pub max_k: u32,
    pub max_query_rows: u32,
    pub max_file_read_bytes: u64,
    pub max_file_write_bytes: u64,
}

impl Default for BudgetSnapshot {
    fn default() -> Self {
        Self {
            max_k: 100,
            max_query_rows: 1000,
            max_file_read_bytes: 10_485_760,
            max_file_write_bytes: 1_048_576,
        }
    }
}

impl BudgetSnapshot {
    pub fn unlimited() -> Self {
        Self {
            max_k: u32::MAX,
            max_query_rows: u32::MAX,
            max_file_read_bytes: u64::MAX,
            max_file_write_bytes: u64::MAX,
        }
    }
}

pub struct HostContext {
    pub duckdb: Option<Arc<Mutex<duckdb::Connection>>>,
    pub triviumdb: Option<Arc<Mutex<TriviumDb>>>,
    pub permission: PermissionSnapshot,
    pub budget: BudgetSnapshot,
}

impl HostContext {
    pub fn sandboxed() -> Self {
        Self {
            duckdb: None,
            triviumdb: None,
            permission: PermissionSnapshot::deny_all(),
            budget: BudgetSnapshot::default(),
        }
    }
}
