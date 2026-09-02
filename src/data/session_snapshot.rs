//! v0.4.9 退出快照：冻结→保存→恢复→降级。
//!
//! 核心诉求（用户原话）：「设计一种快照保存机制，先冻结再存，下次启动直接以快照启动，
//! 设计安全的降级逻辑兜底（不要做的太复杂，能不丢思考引擎对话就可以）」。
//!
//! 本模块只负责快照的**数据格式**与**持久化/恢复/降级**，不含退出触发与恢复注入逻辑
//! （那两处由 `crate::startup::entry` 调用）。设计原则：不丢思考引擎对话——启动后
//! 用户能「看到上次没说完的话」即达标；已完成对话由 conversations/（thought_store）
//! 持久化，本快照不重复存储已完成内容。
//!
//! 降级分层：
//! - 层 1（保存失败）：仅日志 + 继续退出，不阻塞退出；
//! - 层 2（解析失败）：空启动；- 层 3（文件缺失 / schema_version 不兼容）：空启动。

use crate::common::UtcTimestamp;
use crate::common::{AgentError, Result};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 快照 schema 版本。与「保存」侧严格一致；不匹配时 `load_snapshot` 返回 None（空启动）。
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// 快照文件名（位于 `storage_root/snapshots/`）。
pub const SNAPSHOT_FILE_NAME: &str = "last_session.json";

/// 原子写临时文件名（与正式文件同目录，保证 rename 原子性）。
const SNAPSHOT_TMP_FILE_NAME: &str = "last_session.json.tmp";

/// 恢复成功后轮转的目标文件名（保留证据，防陈旧占位失联）。
pub const SNAPSHOT_RESTORED_FILE_NAME: &str = "last_session.json.restored";

/// 退出快照：记录退出瞬间所有「未完成」思考实例已产出的 think/say 片段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub schema_version: u32,
    /// ISO 时间（现有时间工具 `UtcTimestamp::now()`）。
    pub saved_at: String,
    /// 退出时的模式：unni / keep / loop（小写）。
    pub mode: String,
    /// 未完成实例列表（退出时仍在跑、output 尚未终态落盘）。
    pub incomplete: Vec<IncompleteInstance>,
}

/// 单个未完成实例的已产出片段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncompleteInstance {
    pub id: String,
    /// 最小判断：think / say / executing（按已产出文本推断，纯信息性，不驱动任何行为）。
    pub phase: String,
    /// 已产出的 think 片段（TuiMessage::Think.text）。
    pub think_partial: String,
    /// 已产出的 say 片段（TuiMessage::Streaming.content）。
    pub say_partial: String,
}

impl SessionSnapshot {
    /// 构造一个新的 schema=1 快照。
    pub fn new(mode: impl Into<String>, incomplete: Vec<IncompleteInstance>) -> Self {
        Self {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            saved_at: UtcTimestamp::now().to_string(),
            mode: mode.into(),
            incomplete,
        }
    }
}

impl IncompleteInstance {
    pub fn new(
        id: impl Into<String>,
        phase: impl Into<String>,
        think_partial: String,
        say_partial: String,
    ) -> Self {
        Self {
            id: id.into(),
            phase: phase.into(),
            think_partial,
            say_partial,
        }
    }
}

/// 快照完整路径：`storage_root/snapshots/last_session.json`。
pub fn snapshot_path(storage_root: &Path) -> PathBuf {
    storage_root.join("snapshots").join(SNAPSHOT_FILE_NAME)
}

fn snapshots_dir(storage_root: &Path) -> PathBuf {
    storage_root.join("snapshots")
}

/// 保存快照（原子写：临时文件 + rename；目录 0700、文件 0600）。
///
/// 失败仅返回 Err，由调用方「日志 + 继续退出」（降级层 1），不在此阻塞退出。
pub fn save_snapshot(storage_root: &Path, snapshot: &SessionSnapshot) -> Result<()> {
    let dir = snapshots_dir(storage_root);
    ensure_private_directory(&dir)?;

    let tmp = dir.join(SNAPSHOT_TMP_FILE_NAME);
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|e| AgentError::Io(format!("snapshot serialize: {e}")))?;
    std::fs::write(&tmp, json.as_bytes())
        .map_err(|e| AgentError::Io(format!("snapshot write tmp: {e}")))?;
    secure_existing_file(&tmp)?;

    std::fs::rename(&tmp, snapshot_path(storage_root)).map_err(|e| {
        // 清理残留临时文件（rename 失败时尽量不留下 tmp）。
        let _ = std::fs::remove_file(&tmp);
        AgentError::Io(format!("snapshot rename: {e}"))
    })?;
    Ok(())
}

/// 加载快照。任何失败（缺失 / 解析失败 / schema 不兼容）→ `None`（空启动，降级层 2/3）。
pub fn load_snapshot(storage_root: &Path) -> Option<SessionSnapshot> {
    let path = snapshot_path(storage_root);
    let data = std::fs::read(&path).ok()?;
    let snapshot: SessionSnapshot = serde_json::from_slice(&data).ok()?;
    if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return None;
    }
    Some(snapshot)
}

/// 删除/轮转快照（恢复完成后调用，防陈旧）。
///
/// 执行者方案：rename 为 `last_session.json.restored`（保留上次恢复证据），
/// 同时清理可能残留的 tmp 临时文件。rename 失败时回退直接删除。
pub fn clear_snapshot(storage_root: &Path) {
    let dir = snapshots_dir(storage_root);
    let path = snapshot_path(storage_root);
    let restored = dir.join(SNAPSHOT_RESTORED_FILE_NAME);
    if let Err(e) = std::fs::rename(&path, &restored) {
        tracing::warn!(
            "session_snapshot: rotate {} -> {} failed: {e}",
            path.display(),
            restored.display()
        );
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::remove_file(dir.join(SNAPSHOT_TMP_FILE_NAME));
}

/// 快照里是否有「可见」内容（至少一个未完成实例有非空片段或可提示信息）。
/// 供恢复侧判断是否需要注入消息流。
pub fn has_restorable_content(snapshot: &SessionSnapshot) -> bool {
    !snapshot.incomplete.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 每个测试用独立的 tempdir，避免「同一 /tmp 路径多测试并行互相 remove_dir_all」的竞态。
    fn tmp_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir should create");
        // 确保沙箱内可读写（tempdir 已有权限，无需额外处理）。
        dir
    }

    fn sample_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            saved_at: "2026-01-01T00:00:00.000000000Z".to_string(),
            mode: "unni".to_string(),
            incomplete: vec![IncompleteInstance::new(
                "inst-1",
                "say",
                "plan".to_string(),
                "partial reply".to_string(),
            )],
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let root = tmp_root();
        let snap = sample_snapshot();
        save_snapshot(root.path(), &snap).unwrap();
        let loaded = load_snapshot(root.path()).expect("snapshot should load");
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.mode, "unni");
        assert_eq!(loaded.incomplete, snap.incomplete);
        assert_eq!(loaded.incomplete[0].id, "inst-1");
        assert_eq!(loaded.incomplete[0].say_partial, "partial reply");
        assert_eq!(loaded.incomplete[0].think_partial, "plan");
    }

    #[test]
    fn save_twice_keeps_second() {
        let root = tmp_root();
        let first = SessionSnapshot {
            schema_version: 1,
            saved_at: "old".to_string(),
            mode: "keep".to_string(),
            incomplete: vec![],
        };
        save_snapshot(root.path(), &first).unwrap();
        save_snapshot(root.path(), &sample_snapshot()).unwrap();
        let loaded = load_snapshot(root.path()).unwrap();
        assert_eq!(loaded.saved_at, "2026-01-01T00:00:00.000000000Z");
        assert_eq!(loaded.incomplete.len(), 1);
    }

    #[test]
    fn corrupted_json_degrades_to_empty_start() {
        let root = tmp_root();
        let path = snapshot_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not valid json !!").unwrap();
        assert!(load_snapshot(root.path()).is_none(), "损坏 JSON → 空启动");
    }

    #[test]
    fn missing_file_degrades_to_empty_start() {
        let root = tmp_root();
        assert!(load_snapshot(root.path()).is_none(), "文件缺失 → 空启动");
    }

    #[test]
    fn incompatible_schema_degrades_to_empty_start() {
        let root = tmp_root();
        let path = snapshot_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut snap = sample_snapshot();
        snap.schema_version = 99;
        fs::write(&path, serde_json::to_vec(&snap).unwrap()).unwrap();
        assert!(
            load_snapshot(root.path()).is_none(),
            "schema 不兼容 → 空启动"
        );
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let root = tmp_root();
        save_snapshot(root.path(), &sample_snapshot()).unwrap();
        let dir = snapshots_dir(root.path());
        assert!(dir.join(SNAPSHOT_FILE_NAME).exists());
        assert!(
            !dir.join(SNAPSHOT_TMP_FILE_NAME).exists(),
            "原子写后不应残留临时文件"
        );
    }

    #[test]
    fn clear_snapshot_rotates_to_restored() {
        let root = tmp_root();
        save_snapshot(root.path(), &sample_snapshot()).unwrap();
        clear_snapshot(root.path());
        let dir = snapshots_dir(root.path());
        assert!(!dir.join(SNAPSHOT_FILE_NAME).exists(), "原文件应被轮转");
        assert!(
            dir.join(SNAPSHOT_RESTORED_FILE_NAME).exists(),
            "应保留 .restored 证据"
        );
    }

    #[test]
    fn has_restorable_content_requires_incomplete() {
        let empty = SessionSnapshot::new("unni", vec![]);
        assert!(!has_restorable_content(&empty));
        let with = sample_snapshot();
        assert!(has_restorable_content(&with));
    }
}
