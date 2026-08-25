//! subagent 最小记忆：单文件追加 + 超窗剪切 + last_output 简报。
//!
//! 布局（storage_root 为 DataPaths::storage_root）：
//! - <storage_root>/subagents/<id>/memory.json
//! - <storage_root>/subagents/<id>/last_output.json
//!
//! 只做追加 + 超窗剪切（窗口 = 模型 context_window * 80%），不做检索/摘要/向量；
//! 每次剪切留 truncation_records。目录 0700 / 文件 0600（复用 data::permissions），
//! 写采用临时文件 + rename 原子替换。

use crate::common::{AgentError, Result, UtcTimestamp};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 记忆窗口占模型上下文窗口的比例（%）。
pub const MEMORY_WINDOW_PCT: usize = 80;

/// 单条记忆条目（任务书 §6.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// UTC 高精度时间戳。
    pub t: String,
    /// 本轮输入。
    pub input: String,
    /// 能力调用事实（"capability_id=... status=..."）。
    pub actions: Vec<String>,
    /// START/OK/FAIL 关键日志证据。
    pub evidence: Vec<String>,
    /// 本轮输出/简报。
    pub output: String,
}

/// 一次超窗剪切的留痕。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TruncationRecord {
    /// 剪切发生的 UTC 时间戳。
    pub truncated_at: String,
    /// 本次剪掉的估算 token 数。
    pub truncated_tokens: usize,
}

/// memory.json 的完整结构。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentMemory {
    pub entries: Vec<MemoryEntry>,
    pub truncation_records: Vec<TruncationRecord>,
}

/// last_output.json 的完整结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastOutput {
    pub subagent_id: String,
    pub t: String,
    /// "completed" / "failed" / "created"（创建占位）。
    pub status: String,
    /// done.summary（completed）或失败事实（failed）。
    pub summary: String,
}

/// 追加记忆的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendOutcome {
    /// 本次剪切掉的总估算 token。
    pub truncated_tokens: usize,
    /// 剪切后保留的条目数。
    pub entries_kept: usize,
}

/// 简单 token 估算：字符数 / 4（与现有 estimate 方式对齐；测试用小窗口驱动剪切）。
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

/// 由模型上下文窗口推导记忆窗口（token）：context_window * 80%。
pub fn memory_window_tokens(context_window: usize) -> usize {
    (context_window * MEMORY_WINDOW_PCT) / 100
}

/// 单条记忆的估算 token 数。
pub fn entry_tokens(entry: &MemoryEntry) -> usize {
    estimate_tokens(&entry.input)
        + estimate_tokens(&entry.output)
        + entry
            .actions
            .iter()
            .map(|a| estimate_tokens(a))
            .sum::<usize>()
        + entry
            .evidence
            .iter()
            .map(|x| estimate_tokens(x))
            .sum::<usize>()
}

/// subagent id 必须为安全路径组件（防目录穿越）。
pub fn validate_subagent_id(subagent_id: &str) -> Result<&str> {
    if subagent_id.is_empty()
        || subagent_id.len() > 128
        || !subagent_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(AgentError::Bootstrap(format!(
            "unsafe subagent id: {subagent_id}"
        )));
    }
    Ok(subagent_id)
}

/// 确保 <storage_root>/subagents/<id> 存在且 0700，返回目录。
pub fn subagent_dir(storage_root: &Path, subagent_id: &str) -> Result<PathBuf> {
    let id = validate_subagent_id(subagent_id)?;
    let dir = storage_root.join("subagents").join(id);
    ensure_private_directory(&dir)?;
    Ok(dir)
}

fn memory_path(storage_root: &Path, subagent_id: &str) -> Result<PathBuf> {
    Ok(subagent_dir(storage_root, subagent_id)?.join("memory.json"))
}

fn last_output_path(storage_root: &Path, subagent_id: &str) -> Result<PathBuf> {
    Ok(subagent_dir(storage_root, subagent_id)?.join("last_output.json"))
}

/// 初始化 subagent 记忆与简报文件（幂等：仅缺省时创建）。
///
/// memory.json 为空结构；last_output.json 写入 created 占位。
pub fn init_subagent_memory(storage_root: &Path, subagent_id: &str) -> Result<()> {
    let id = validate_subagent_id(subagent_id)?;
    let dir = subagent_dir(storage_root, id)?;
    let memory = memory_path(storage_root, id)?;
    if !memory.exists() {
        atomic_write_json(&memory, &SubagentMemory::default())?;
    }
    let last_output = last_output_path(storage_root, id)?;
    if !last_output.exists() {
        atomic_write_json(
            &last_output,
            &LastOutput {
                subagent_id: id.to_string(),
                t: UtcTimestamp::now().to_string(),
                status: "created".to_string(),
                summary: String::new(),
            },
        )?;
    }
    let _ = dir;
    Ok(())
}

/// 读取完整 memory.json（不存在时返回空结构）。
pub fn read_memory(storage_root: &Path, subagent_id: &str) -> Result<SubagentMemory> {
    let path = memory_path(storage_root, subagent_id)?;
    if !path.exists() {
        return Ok(SubagentMemory::default());
    }
    let bytes = std::fs::read(&path)
        .map_err(|e| AgentError::Bootstrap(format!("read subagent memory {:?}: {e}", path)))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| AgentError::Parse(format!("parse subagent memory {:?}: {e}", path)))
}

/// 追加一条记忆，超窗时从最旧条目剪切并留 truncation_record，原子写回。
///
/// 单个超窗条目不丢弃（保留最新一条，避免记忆完全清空）。
pub fn append_entry(
    storage_root: &Path,
    subagent_id: &str,
    entry: MemoryEntry,
    window_tokens: usize,
) -> Result<AppendOutcome> {
    let path = memory_path(storage_root, subagent_id)?;
    let mut memory = if path.exists() {
        read_memory(storage_root, subagent_id)?
    } else {
        SubagentMemory::default()
    };
    memory.entries.push(entry);
    let (truncated, kept) = trim_to_window(&mut memory, window_tokens);
    if truncated > 0 {
        memory.truncation_records.push(TruncationRecord {
            truncated_at: UtcTimestamp::now().to_string(),
            truncated_tokens: truncated,
        });
    }
    atomic_write_json(&path, &memory)?;
    Ok(AppendOutcome {
        truncated_tokens: truncated,
        entries_kept: kept,
    })
}

/// 从最旧条目开始丢弃，直到估算 token 回到窗口内；返回 (剪掉 token, 保留条目数)。
fn trim_to_window(memory: &mut SubagentMemory, window_tokens: usize) -> (usize, usize) {
    let mut truncated = 0usize;
    let mut total: usize = memory.entries.iter().map(entry_tokens).sum();
    while total > window_tokens && memory.entries.len() > 1 {
        let removed = memory.entries.remove(0);
        let removed_tokens = entry_tokens(&removed);
        total = total.saturating_sub(removed_tokens);
        truncated += removed_tokens;
    }
    (truncated, memory.entries.len())
}

/// 写 last_output.json（run 结束写 done.summary；失败写失败事实）。
pub fn write_last_output(
    storage_root: &Path,
    subagent_id: &str,
    status: &str,
    summary: &str,
) -> Result<()> {
    let id = validate_subagent_id(subagent_id)?;
    let path = last_output_path(storage_root, subagent_id)?;
    let output = LastOutput {
        subagent_id: id.to_string(),
        t: UtcTimestamp::now().to_string(),
        status: status.to_string(),
        summary: summary.to_string(),
    };
    atomic_write_json(&path, &output)
}

/// 读取 last_output.json（不存在返回 None）。
pub fn read_last_output(storage_root: &Path, subagent_id: &str) -> Result<Option<LastOutput>> {
    let path = last_output_path(storage_root, subagent_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)
        .map_err(|e| AgentError::Bootstrap(format!("read subagent last_output {:?}: {e}", path)))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| AgentError::Parse(format!("parse subagent last_output {:?}: {e}", path)))
}

/// 有界截断读取 last_output（按 token 预算截断 summary）。
///
/// 读取方（思考引擎/执行中台）使用本 API 感知 subagent 简报，不引入固定字符数。
pub fn read_last_output_truncated(
    storage_root: &Path,
    subagent_id: &str,
    token_budget: usize,
) -> Result<Option<String>> {
    let output = read_last_output(storage_root, subagent_id)?;
    Ok(output.map(|o| truncate_by_tokens(&o.summary, token_budget)))
}

/// 按 token 预算截断文本（超过则截断并追加省略号）。
pub fn truncate_by_tokens(text: &str, token_budget: usize) -> String {
    if estimate_tokens(text) <= token_budget {
        return text.to_string();
    }
    let char_budget = token_budget * 4;
    let truncated: String = text.chars().take(char_budget).collect();
    format!("{truncated}…")
}

/// 原子写 JSON：临时文件（0600）→ fsync → rename → secure_existing_file(0600)。
fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AgentError::Bootstrap(format!("subagent file has no parent: {:?}", path)))?;
    ensure_private_directory(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("subagent"),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| {
            AgentError::Bootstrap(format!("serialize subagent file {:?}: {e}", path))
        })?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|e| {
            AgentError::Bootstrap(format!("create subagent temp file {:?}: {e}", temporary))
        })?;
        file.write_all(&bytes).map_err(|e| {
            AgentError::Bootstrap(format!("write subagent temp file {:?}: {e}", temporary))
        })?;
        file.sync_all().map_err(|e| {
            AgentError::Bootstrap(format!("sync subagent temp file {:?}: {e}", temporary))
        })?;
        drop(file);
        secure_existing_file(&temporary)?;
        std::fs::rename(&temporary, path)
            .map_err(|e| AgentError::Bootstrap(format!("publish subagent file {:?}: {e}", path)))?;
        secure_existing_file(path)
    })();
    if result.is_err() && temporary.exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 权限断言仅 Unix 平台可测（PermissionsExt::mode）；Windows 跳过。
    #[cfg(unix)]
    fn assert_private_file_modes(
        dir: &std::path::Path,
        memory: &std::path::Path,
        last_output: &std::path::Path,
    ) {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(dir.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            memory.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            last_output.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn entry(prefix: &str, size: usize) -> MemoryEntry {
        MemoryEntry {
            t: format!("{prefix}-t"),
            input: prefix.repeat(size),
            actions: vec![format!("capability_id=file.read status=OK")],
            evidence: vec![format!("OK file.read: {prefix}")],
            output: format!("{prefix}-out").repeat(size / 2),
        }
    }

    #[test]
    fn estimate_tokens_is_chars_over_four() {
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("中文测试"), 1);
        assert_eq!(estimate_tokens(&"a".repeat(40)), 10);
    }

    #[test]
    fn memory_window_is_80pct_of_context() {
        assert_eq!(memory_window_tokens(1000), 800);
        assert_eq!(memory_window_tokens(4096), 3276);
    }

    #[test]
    fn init_creates_private_dir_and_files_atomically() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        init_subagent_memory(root, "sg_init").unwrap();

        let dir = root.join("subagents").join("sg_init");
        let memory = dir.join("memory.json");
        let last_output = dir.join("last_output.json");
        #[cfg(unix)]
        assert_private_file_modes(&dir, &memory, &last_output);

        let parsed: SubagentMemory =
            serde_json::from_str(&std::fs::read_to_string(&memory).unwrap()).unwrap();
        assert!(parsed.entries.is_empty());
        let parsed_out: LastOutput =
            serde_json::from_str(&std::fs::read_to_string(&last_output).unwrap()).unwrap();
        assert_eq!(parsed_out.status, "created");

        // 幂等。
        init_subagent_memory(root, "sg_init").unwrap();
    }

    #[test]
    fn append_within_window_keeps_all_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        init_subagent_memory(root, "sg_a").unwrap();

        for i in 0..3 {
            let outcome = append_entry(root, "sg_a", entry(&format!("e{i}"), 8), 10_000).unwrap();
            assert_eq!(outcome.truncated_tokens, 0);
        }
        let memory = read_memory(root, "sg_a").unwrap();
        assert_eq!(memory.entries.len(), 3);
        assert!(memory.truncation_records.is_empty());
    }

    #[test]
    fn append_over_window_trims_oldest_and_records_truncation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        init_subagent_memory(root, "sg_t").unwrap();

        // 小窗口驱动剪切：单条 1 万字符 ≈ 2500+ token，窗口 800 token。
        let mut total_truncated = 0usize;
        for i in 0..5 {
            let outcome = append_entry(root, "sg_t", entry(&format!("e{i}"), 10_000), 800).unwrap();
            if outcome.truncated_tokens > 0 {
                total_truncated += outcome.truncated_tokens;
                assert!(outcome.entries_kept >= 1);
            }
        }

        let memory = read_memory(root, "sg_t").unwrap();
        assert!(
            !memory.truncation_records.is_empty(),
            "剪切必须留下 truncation_records"
        );
        assert!(!memory.entries.is_empty(), "至少保留最新一条");
        // 最新一条始终保留。
        assert_eq!(memory.entries.last().unwrap().input, "e4".repeat(10_000));
        for record in &memory.truncation_records {
            assert!(record.truncated_tokens > 0);
        }
        let _ = total_truncated;
    }

    #[test]
    fn trim_never_empties_memory() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        init_subagent_memory(root, "sg_single").unwrap();
        // 单条远超大窗口：仍保留该条。
        let outcome = append_entry(root, "sg_single", entry("big", 20_000), 100).unwrap();
        assert_eq!(outcome.entries_kept, 1);
        let memory = read_memory(root, "sg_single").unwrap();
        assert_eq!(memory.entries.len(), 1);
        assert_eq!(memory.entries[0].input, "big".repeat(20_000));
    }

    #[test]
    fn last_output_write_read_and_bounded_truncation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        init_subagent_memory(root, "sg_l").unwrap();

        write_last_output(root, "sg_l", "completed", "短摘要").unwrap();
        let output = read_last_output(root, "sg_l").unwrap().unwrap();
        assert_eq!(output.status, "completed");
        assert_eq!(output.summary, "短摘要");

        write_last_output(root, "sg_l", "failed", "failed: attempt timeout").unwrap();
        assert_eq!(
            read_last_output(root, "sg_l").unwrap().unwrap().status,
            "failed"
        );

        // 有界截断读取：小预算截断长摘要。
        let long = "x".repeat(4000);
        write_last_output(root, "sg_l", "completed", &long).unwrap();
        let truncated = read_last_output_truncated(root, "sg_l", 100)
            .unwrap()
            .unwrap();
        assert!(truncated.ends_with('…'));
        assert!(estimate_tokens(&truncated) <= 100 + 1);

        // 未截断读取。
        let full = read_last_output_truncated(root, "sg_l", 10_000)
            .unwrap()
            .unwrap();
        assert!(!full.ends_with('…'));
        assert_eq!(full, long);

        // 未知 subagent 返回 None。
        assert!(read_last_output_truncated(root, "sg_missing", 100)
            .unwrap()
            .is_none());
    }

    #[test]
    fn unsafe_subagent_ids_are_rejected() {
        assert!(validate_subagent_id("../escape").is_err());
        assert!(validate_subagent_id("").is_err());
        assert!(validate_subagent_id("a/b").is_err());
        assert!(validate_subagent_id("sg_ok-1").is_ok());
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_on_success() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        init_subagent_memory(root, "sg_atomic").unwrap();
        append_entry(root, "sg_atomic", entry("a", 8), 10_000).unwrap();
        let dir = root.join("subagents").join("sg_atomic");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains(".tmp")),
            "不应残留临时文件: {names:?}"
        );
    }
}
