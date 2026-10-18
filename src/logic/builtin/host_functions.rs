use super::host_context::HostContext;
use crate::common::watchdog;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fail(msg: impl Into<String>) -> Value {
    serde_json::json!({ "success": false, "error": msg.into() })
}

fn fail_str(msg: impl Into<String>) -> Result<Value, String> {
    Ok(fail(msg))
}

fn required_string<'a>(args: &'a Value, field: &str, op: &str) -> Result<&'a str, Value> {
    match args.get(field).and_then(|v| v.as_str()) {
        Some(v) => Ok(v),
        None => Err(fail(format!("{op}: missing '{field}' field"))),
    }
}

fn is_within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| {
        let canonical_root = r.canonicalize().unwrap_or_else(|_| r.clone());
        path.starts_with(&canonical_root)
    })
}

/// 与 WASM 时期一致的路径解析：相对路径以首个 root 为基准，
/// 绝对路径也必须 canonicalize 后落在 roots 内。
/// 完整路径已存在（含符号链接）时必须对解引用后的真实路径做前缀判定，
/// 否则工作区内指向外部的符号链接可以把读写带到沙箱外。
fn resolve_sandbox_path(path_str: &str, roots: &[PathBuf]) -> std::result::Result<PathBuf, String> {
    let root = roots.first().cloned().unwrap_or_default();
    let raw = Path::new(path_str);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    if std::fs::symlink_metadata(&candidate).is_ok() {
        let resolved = candidate
            .canonicalize()
            .map_err(|e| format!("resolve path: {e}"))?;
        if !is_within_roots(&resolved, roots) {
            return Err("path is not within the allowed workspace roots".to_string());
        }
        return Ok(resolved);
    }
    // 末级不存在（写入新文件场景）：父目录 canonicalize 后拼接文件名。
    let parent = candidate
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_canonical = parent
        .canonicalize()
        .map_err(|e| format!("resolve path parent: {e}"))?;
    let file_name = candidate
        .file_name()
        .ok_or_else(|| "resolve path: no file name".to_string())?;
    let resolved = parent_canonical.join(file_name);
    if !is_within_roots(&resolved, roots) {
        return Err("path is not within the allowed workspace roots".to_string());
    }
    Ok(resolved)
}

fn ensure_within(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    if is_within_roots(path, roots) {
        Ok(())
    } else {
        Err("path is not within the allowed workspace roots".to_string())
    }
}

pub fn host_path_exists(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let path_str = match required_string(args, "path", "path.exists") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let canonical = match resolve_sandbox_path(path_str, &ctx.permission.file_read_roots) {
        Ok(path) => path,
        Err(_) => {
            return Ok(serde_json::json!({"exists": false, "is_file": false, "is_dir": false}))
        }
    };
    match std::fs::metadata(&canonical) {
        Ok(meta) => Ok(serde_json::json!({
            "exists": true,
            "is_file": meta.is_file(),
            "is_dir": meta.is_dir(),
        })),
        Err(_) => Ok(serde_json::json!({"exists": false, "is_file": false, "is_dir": false})),
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some('*'), Some('*')) => {
                let mut idx = 0;
                while idx < p.len() && p[idx] == '*' {
                    idx += 1;
                }
                if idx >= p.len() {
                    return true;
                }
                for skip in 0..=t.len() {
                    if inner(&p[idx..], &t[skip..]) {
                        return true;
                    }
                }
                false
            }
            (Some('*'), _) => {
                let mut idx = 0;
                while idx < p.len() && p[idx] == '*' {
                    idx += 1;
                }
                if idx >= p.len() {
                    return true;
                }
                for skip in 0..=t.len() {
                    if skip > 0 && t[skip - 1] == '/' {
                        break;
                    }
                    if inner(&p[idx..], &t[skip..]) {
                        return true;
                    }
                }
                false
            }
            (Some('?'), Some(_)) => inner(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => inner(&p[1..], &t[1..]),
            _ => false,
        }
    }
    inner(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
    )
}

fn collect_workspace_files(root: &Path, limit: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= limit {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= limit {
            return;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        out.push(entry.path());
        if file_type.is_dir() {
            collect_workspace_files(&entry.path(), limit, out);
        }
    }
}

pub fn host_file_glob(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let pattern = match required_string(args, "pattern", "file.glob") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let root_str = args.get("root").and_then(|v| v.as_str()).unwrap_or(".");
    let root = match resolve_sandbox_path(root_str, &ctx.permission.file_read_roots) {
        Ok(root) => root,
        Err(e) => return fail_str(format!("file.glob: {e}")),
    };
    let Ok(meta) = std::fs::metadata(&root) else {
        return Ok(serde_json::json!({"matches": [], "count": 0}));
    };
    if !meta.is_dir() {
        return fail_str("file.glob: root must be a directory");
    }

    let mut all = Vec::new();
    collect_workspace_files(&root, 2000, &mut all);
    let mut matches = Vec::new();
    for path in all {
        let Ok(rel) = path.strip_prefix(&root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let haystack = if pattern.contains('/') {
            rel.clone()
        } else {
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        };
        if glob_match(pattern, &haystack) {
            matches.push(rel);
        }
        if matches.len() >= 1000 {
            break;
        }
    }
    matches.sort();
    let count = matches.len();
    Ok(serde_json::json!({"matches": matches, "count": count}))
}

pub fn host_json_validate(_ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let text = match required_string(args, "text", "json.validate") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let mut errors = Vec::new();
    let parsed: Result<Value, _> = serde_json::from_str(text);
    match parsed {
        Ok(value) => {
            if let Some(schema) = args.get("schema").filter(|v| v.is_object()) {
                if let Ok(validator) = jsonschema::validator_for(schema) {
                    if !validator.is_valid(&value) {
                        errors.extend(
                            validator
                                .iter_errors(&value)
                                .take(10)
                                .map(|e| e.to_string()),
                        );
                    }
                } else {
                    errors.push("invalid json schema".to_string());
                }
            }
            Ok(serde_json::json!({"valid": errors.is_empty(), "errors": errors}))
        }
        Err(e) => Ok(serde_json::json!({"valid": false, "errors": [e.to_string()]})),
    }
}

pub fn host_file_read(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let path_str = match required_string(args, "path", "host_file_read") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let canonical = match resolve_sandbox_path(path_str, &ctx.permission.file_read_roots) {
        Ok(p) => p,
        Err(e) => return fail_str(format!("host_file_read: {e}")),
    };
    ensure_within(&canonical, &ctx.permission.file_read_roots)
        .map_err(|e| format!("host_file_read: {e}"))?;

    let metadata = match std::fs::metadata(&canonical) {
        Ok(metadata) => metadata,
        Err(e) => return fail_str(format!("host_file_read: metadata: {e}")),
    };
    if metadata.len() > ctx.budget.max_file_read_bytes {
        return fail_str(format!(
            "host_file_read: file too large ({} > {} bytes)",
            metadata.len(),
            ctx.budget.max_file_read_bytes
        ));
    }

    let data = match std::fs::read(&canonical) {
        Ok(data) => data,
        Err(e) => return fail_str(format!("host_file_read: read: {e}")),
    };
    let content = match String::from_utf8(data) {
        Ok(content) => content,
        Err(_) => return fail_str("host_file_read: file not utf-8"),
    };
    Ok(serde_json::json!({"content": content, "size": content.len()}))
}

pub fn host_file_write(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let path_str = match required_string(args, "path", "host_file_write") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let content_str = match required_string(args, "content", "host_file_write") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };

    let canonical = match resolve_sandbox_path(path_str, &ctx.permission.file_write_roots) {
        Ok(p) => p,
        Err(e) => return fail_str(format!("host_file_write: {e}")),
    };
    ensure_within(&canonical, &ctx.permission.file_write_roots)
        .map_err(|e| format!("host_file_write: {e}"))?;

    let data = content_str.as_bytes();
    if data.len() as u64 > ctx.budget.max_file_write_bytes {
        return fail_str(format!(
            "host_file_write: data too large ({} > {} bytes)",
            data.len(),
            ctx.budget.max_file_write_bytes
        ));
    }

    let dir = canonical
        .parent()
        .ok_or_else(|| "host_file_write: no parent directory".to_string())?;
    let tmp_path = dir.join(format!(
        ".host_tmp_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    std::fs::write(&tmp_path, data).map_err(|e| format!("host_file_write: tmp write: {e}"))?;
    if let Ok(tmp_file) = std::fs::File::open(&tmp_path) {
        tmp_file.sync_all().ok();
    }
    std::fs::rename(&tmp_path, &canonical).map_err(|e| format!("host_file_write: rename: {e}"))?;
    if let Ok(dir_file) = std::fs::File::open(dir) {
        dir_file.sync_all().ok();
    }
    Ok(serde_json::json!({"success": true}))
}

pub fn host_file_list(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let path_str = match required_string(args, "path", "host_file_list") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let canonical = match resolve_sandbox_path(path_str, &ctx.permission.file_read_roots) {
        Ok(p) => p,
        Err(e) => return fail_str(format!("host_file_list: {e}")),
    };
    ensure_within(&canonical, &ctx.permission.file_read_roots)
        .map_err(|e| format!("host_file_list: {e}"))?;

    let entries: Vec<String> = match std::fs::read_dir(&canonical) {
        Ok(rd) => rd
            .filter_map(|entry| entry.ok().and_then(|e| e.file_name().into_string().ok()))
            .collect(),
        Err(e) => return fail_str(format!("host_file_list: read_dir: {e}")),
    };
    let count = entries.len();
    Ok(serde_json::json!({"success": true, "entries": entries, "count": count}))
}

pub fn host_file_delete(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let path_str = match required_string(args, "path", "host_file_delete") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let canonical = match resolve_sandbox_path(path_str, &ctx.permission.file_write_roots) {
        Ok(p) => p,
        Err(e) => return fail_str(format!("host_file_delete: {e}")),
    };
    ensure_within(&canonical, &ctx.permission.file_write_roots)
        .map_err(|e| format!("host_file_delete: {e}"))?;

    let remove_result = if canonical.is_dir() {
        std::fs::remove_dir_all(&canonical)
    } else {
        std::fs::remove_file(&canonical)
    };
    if let Err(e) = remove_result {
        return fail_str(format!("host_file_delete: remove: {e}"));
    }
    Ok(serde_json::json!({"success": true}))
}

pub fn host_file_move(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let from_str = match required_string(args, "from", "host_file_move") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let to_str = match required_string(args, "to", "host_file_move") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };

    let from_canonical = match resolve_sandbox_path(from_str, &ctx.permission.file_write_roots) {
        Ok(p) => p,
        Err(e) => return fail_str(format!("host_file_move: from: {e}")),
    };
    let to_canonical = match resolve_sandbox_path(to_str, &ctx.permission.file_write_roots) {
        Ok(p) => p,
        Err(e) => return fail_str(format!("host_file_move: to: {e}")),
    };
    ensure_within(&from_canonical, &ctx.permission.file_write_roots)
        .map_err(|e| format!("host_file_move: from: {e}"))?;
    ensure_within(&to_canonical, &ctx.permission.file_write_roots)
        .map_err(|e| format!("host_file_move: to: {e}"))?;

    std::fs::rename(&from_canonical, &to_canonical)
        .map_err(|e| format!("host_file_move: rename: {e}"))?;
    Ok(serde_json::json!({"success": true}))
}

pub fn host_text_grep(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let pattern = match required_string(args, "pattern", "host_text_grep") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let path_str = match required_string(args, "path", "host_text_grep") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let recursive = args
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let canonical = match resolve_sandbox_path(path_str, &ctx.permission.file_read_roots) {
        Ok(p) => p,
        Err(e) => return fail_str(format!("host_text_grep: {e}")),
    };
    ensure_within(&canonical, &ctx.permission.file_read_roots)
        .map_err(|e| format!("host_text_grep: {e}"))?;
    let meta =
        std::fs::metadata(&canonical).map_err(|e| format!("host_text_grep: metadata: {e}"))?;

    let mut matches: Vec<String> = Vec::new();
    if meta.is_file() {
        let content = std::fs::read_to_string(&canonical)
            .map_err(|e| format!("host_text_grep: read: {e}"))?;
        for line in content.lines().filter(|line| line.contains(pattern)) {
            matches.push(line.to_string());
        }
    } else if meta.is_dir() && recursive {
        let mut files = Vec::new();
        collect_workspace_files(&canonical, 2000, &mut files);
        for file in files {
            let Ok(file_meta) = std::fs::metadata(&file) else {
                continue;
            };
            if !file_meta.is_file() {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            for line in content.lines().filter(|line| line.contains(pattern)) {
                let rel = file
                    .strip_prefix(&canonical)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/");
                matches.push(format!("{rel}: {line}"));
            }
            if matches.len() >= 1000 {
                break;
            }
        }
    } else if meta.is_dir() {
        return fail_str(
            "host_text_grep: path is a directory; set recursive=true to search recursively",
        );
    }

    Ok(serde_json::json!({"matches": matches}))
}

/// 首词级黑名单（整词匹配，变形如 grep sudoers 不误伤）。
const DANGEROUS_FIRST_WORDS: &[&str] = &[
    "sudo", "dd", "shutdown", "reboot", "halt", "poweroff", "fdisk", "parted", "wipefs",
];

/// 块设备重定向（去空白小写后的子串，覆盖 > /dev/sda 一类直写盘设备）。
const BLOCK_DEVICE_REDIRECTS: &[&str] = &[">/dev/sd", ">/dev/hd", ">/dev/vd", ">/dev/nvme"];

/// 剥离首尾引号（各一层），用于归一化后的词匹配。
fn strip_quotes(word: &str) -> &str {
    let mut s = word;
    if s.starts_with('"') || s.starts_with('\'') {
        s = &s[1..];
    }
    if s.ends_with('"') || s.ends_with('\'') {
        s = &s[..s.len() - 1];
    }
    s
}

/// 段内词法归一化：连续空白压成单空格分词、剥离引号、转小写。
fn normalized_words(segment: &str) -> Vec<String> {
    segment
        .split_whitespace()
        .map(|w| strip_quotes(w).to_ascii_lowercase())
        .collect()
}

/// 参数中是否含 -r 系递归标志（-r/-rf/-fr/-R/--recursive 等）。
fn has_recursive_flag(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--recursive" || (a.starts_with('-') && a.chars().any(|c| c == 'r')))
}

/// rm 递归删根：目标（引号剥离后）为 /、/* 或 //*。
fn is_recursive_rm_of_root(words: &[String]) -> bool {
    if words.first().map(String::as_str) != Some("rm") || words.len() < 2 {
        return false;
    }
    let args = &words[1..];
    has_recursive_flag(args) && args.iter().any(|a| a == "/" || a == "/*" || a == "//*")
}

/// chmod -R 777 / 一类：递归 + 777 + 目标为根。
fn is_chmod_777_of_root(words: &[String]) -> bool {
    if words.first().map(String::as_str) != Some("chmod") || words.len() < 3 {
        return false;
    }
    let args = &words[1..];
    has_recursive_flag(args)
        && args.iter().any(|a| a == "777")
        && args.iter().any(|a| a == "/" || a.starts_with("/*"))
}

fn check_dangerous(cmd: &str) -> bool {
    // 按 |、;、&、换行分段（覆盖 || 与 && 变体），每段做首词整词匹配，
    // 避免 contains 子串误伤（grep sudoers）与引号/空白变形绕过。
    for segment in cmd.split(['|', ';', '&', '\n', '\r']) {
        let words = normalized_words(segment);
        let Some(first) = words.first() else {
            continue;
        };
        if DANGEROUS_FIRST_WORDS.contains(&first.as_str())
            || first.starts_with("mkfs")
            || first.starts_with("dd=")
        {
            return true;
        }
        if is_recursive_rm_of_root(&words) || is_chmod_777_of_root(&words) {
            return true;
        }
    }
    // fork 炸弹与块设备重定向是跨词的结构性模式：去空白小写后子串匹配。
    let squashed: String = cmd
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    squashed.contains(":(){:|:&};:") || BLOCK_DEVICE_REDIRECTS.iter().any(|p| squashed.contains(p))
}

/// 单路输出捕获上限：读满后继续读但丢弃（防子进程写满管道阻塞）。
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct CapturedOutput {
    data: Vec<u8>,
    truncated: bool,
}

impl CapturedOutput {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

/// 持续读到管道关闭（EOF）。超上限后丢弃后续字节，保证写端永不阻塞。
fn capture_stream<R: Read>(reader: Option<R>) -> CapturedOutput {
    let mut out = CapturedOutput::default();
    let Some(mut reader) = reader else {
        return out;
    };
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.data.len() < MAX_CAPTURE_BYTES {
                    let take = (MAX_CAPTURE_BYTES - out.data.len()).min(n);
                    out.data.extend_from_slice(&buf[..take]);
                    if take < n {
                        out.truncated = true;
                    }
                } else {
                    out.truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    out
}

/// 一次子进程运行的结果；status == None 表示超时被杀（无退出码）。
struct ChildOutcome {
    status: Option<std::process::ExitStatus>,
    stdout: CapturedOutput,
    stderr: CapturedOutput,
}

impl ChildOutcome {
    /// 统一错误契约：超时不再是 Err，而是 success:false + 已读的部分输出，
    /// 与其他失败形态一致。
    fn into_json(self, timeout_error: &str) -> Value {
        let ChildOutcome {
            status,
            stdout,
            stderr,
        } = self;
        match status {
            Some(status) => serde_json::json!({
                "success": status.success(),
                "stdout": stdout.text(),
                "stderr": stderr.text(),
                "exit_code": status.code().unwrap_or(-1),
                "stdout_truncated": stdout.truncated,
                "stderr_truncated": stderr.truncated,
            }),
            None => serde_json::json!({
                "success": false,
                "error": timeout_error,
                "exit_code": null,
                "stdout": stdout.text(),
                "stderr": stderr.text(),
                "stdout_truncated": stdout.truncated,
                "stderr_truncated": stderr.truncated,
            }),
        }
    }
}

/// 带超时与并发输出捕获的子进程运行器：
/// - stdout/stderr 各由一个读线程持续排空管道——否则输出超管道缓冲（~64KB）
///   时子进程阻塞在 write 上永不退出，超时被杀且输出全丢；
/// - 主线程保留 50ms 间隔 try_wait 轮询 + 调用方给定的超时；
/// - 超时：kill_tree 杀整组（组内进程死 → 管道关闭 → 读线程自然结束）+
///   wait 收尸 + reap_child，保留已读的部分输出。
fn run_child_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<ChildOutcome, String> {
    let stdout_reader = child.stdout.take();
    let stderr_reader = child.stderr.take();
    watchdog::register_child(child.id());

    let (stdout, stderr, wait) = std::thread::scope(|scope| {
        let stdout_handle = scope.spawn(move || capture_stream(stdout_reader));
        let stderr_handle = scope.spawn(move || capture_stream(stderr_reader));

        let deadline = Instant::now() + timeout;
        let wait = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(Some(status)),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        watchdog::kill_tree(child.id());
                        let _ = child.wait();
                        break Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => break Err(e),
            }
        };
        watchdog::reap_child(child.id());
        let stdout = stdout_handle.join().unwrap_or_default();
        let stderr = stderr_handle.join().unwrap_or_default();
        (stdout, stderr, wait)
    });

    let status = wait.map_err(|e| format!("wait: {e}"))?;
    Ok(ChildOutcome {
        status,
        stdout,
        stderr,
    })
}

pub fn host_shell_exec(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    if !ctx.permission.shell_exec_allowed {
        return fail_str("host_shell_exec: shell_exec not allowed");
    }
    let command = match required_string(args, "command", "host_shell_exec") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    if check_dangerous(command) {
        return fail_str("host_shell_exec: command is blacklisted");
    }
    let first_word = command.split_whitespace().next().unwrap_or("");
    if !first_word.is_ascii() {
        return fail_str(format!(
            "host_shell_exec: command starts with non-ASCII text (prose, not a command): {}",
            command.chars().take(60).collect::<String>()
        ));
    }

    let mut syntax_cmd = std::process::Command::new("sh");
    syntax_cmd.arg("-n").arg("-c").arg(command);
    watchdog::apply_lifecycle_hooks(&mut syntax_cmd);
    let syntax_check = syntax_cmd
        .output()
        .map_err(|e| format!("host_shell_exec: syntax check spawn: {e}"))?;
    if !syntax_check.status.success() {
        let stderr: String = String::from_utf8_lossy(&syntax_check.stderr)
            .chars()
            .take(160)
            .collect();
        return fail_str(format!("host_shell_exec: shell syntax error: {stderr}"));
    }

    let workspace_root = ctx
        .permission
        .file_write_roots
        .first()
        .cloned()
        .unwrap_or_default();

    let mut shell_cmd = std::process::Command::new("sh");
    shell_cmd
        .arg("-c")
        .arg(command)
        .current_dir(&workspace_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    watchdog::apply_lifecycle_hooks(&mut shell_cmd);
    let child = shell_cmd
        .spawn()
        .map_err(|e| format!("host_shell_exec: spawn: {e}"))?;

    let outcome = run_child_with_timeout(child, Duration::from_secs(30))
        .map_err(|e| format!("host_shell_exec: {e}"))?;
    Ok(outcome.into_json("host_shell_exec: timeout (30s)"))
}

pub fn host_file_chunk_read(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let path_str = match required_string(args, "path", "host_file_chunk_read") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let offset_raw = args.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);
    let size_raw = args.get("size").and_then(|v| v.as_i64()).unwrap_or(4096);
    if offset_raw < 0 {
        return fail_str("host_file_chunk_read: offset must be >= 0");
    }
    if size_raw < 0 {
        return fail_str("host_file_chunk_read: size must be >= 0");
    }
    let offset = offset_raw as u64;
    let size = size_raw as usize;

    let canonical = resolve_sandbox_path(path_str, &ctx.permission.file_read_roots)
        .map_err(|e| format!("host_file_chunk_read: {e}"))?;
    ensure_within(&canonical, &ctx.permission.file_read_roots)
        .map_err(|e| format!("host_file_chunk_read: {e}"))?;

    if size as u64 > ctx.budget.max_file_read_bytes {
        return fail_str(format!(
            "chunk too large ({} > {})",
            size, ctx.budget.max_file_read_bytes
        ));
    }

    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(&canonical).map_err(|e| format!("open: {e}"))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek: {e}"))?;
    let mut buf = vec![0u8; size];
    // 循环读满 size 或读到 EOF（read 返回 0），容忍短读。
    let mut bytes_read = 0usize;
    while bytes_read < size {
        match file.read(&mut buf[bytes_read..]) {
            Ok(0) => break,
            Ok(n) => bytes_read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    buf.truncate(bytes_read);
    // EOF 判定：读满 size 时文件可能恰好在边界结束（长度为 size 整倍数），
    // 用当前位置与文件长度比较，避免 bytes_read < size 的短读推断在整倍数边界漏报。
    let is_eof = match (file.stream_position(), file.metadata()) {
        (Ok(pos), Ok(meta)) => pos >= meta.len(),
        _ => bytes_read < size,
    };
    let content = String::from_utf8(buf).map_err(|_| "content not utf-8".to_string())?;
    Ok(serde_json::json!({
        "content": content,
        "bytes_read": bytes_read,
        "is_eof": is_eof,
    }))
}

const DANGEROUS_CODE_PATTERNS: &[&str] = &[
    "import os; os.system",
    "import subprocess",
    "import socket",
    "import requests",
    "import urllib",
    "import http",
    "import ftplib",
    "__import__('os')",
];

pub fn host_code_exec(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let code = match required_string(args, "code", "host_code_exec") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let language = args
        .get("language")
        .and_then(|v| v.as_str())
        .unwrap_or("python3");

    for bad in DANGEROUS_CODE_PATTERNS {
        if code.contains(bad) {
            return fail_str(format!("code contains dangerous pattern: {bad}"));
        }
    }

    let ws_root = ctx
        .permission
        .file_read_roots
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));

    let result = match language {
        "rust" => execute_rust_code(code, &ws_root),
        _ => execute_interpreted_code(language, code, &ws_root),
    };
    match result {
        Ok(output) => Ok(output),
        Err(e) => fail_str(e),
    }
}

fn execute_interpreted_code(language: &str, code: &str, ws_root: &Path) -> Result<Value, String> {
    // node/ruby 的 -c 是语法检查而非执行，必须用 -e；python/sh 系才是 -c 执行。
    let (runtime, code_flag) = match language {
        "python" | "python3" => ("python3", "-c"),
        "python2" => ("python2", "-c"),
        "sh" | "bash" => ("bash", "-c"),
        "node" | "javascript" => ("node", "-e"),
        "ruby" => ("ruby", "-e"),
        _ => return Err(format!("unsupported language: {language}")),
    };

    let mut cmd = std::process::Command::new(runtime);
    cmd.arg(code_flag).arg(code);
    cmd.current_dir(ws_root);
    // env_clear 后必须继承父进程 PATH：execvp 对清空环境只回落到系统默认路径
    // （/bin:/usr/bin），装在 /usr/local/bin、~/.cargo/bin 的解释器与 rustc 会解析失败
    // （CI runner 实测）。PATH 不承载机密，继承无碍密闭性，与 shell.exec 姿态一致。
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    watchdog::apply_lifecycle_hooks(&mut cmd);

    let child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let outcome = run_child_with_timeout(child, Duration::from_secs(30))?;
    Ok(outcome.into_json("code execution timed out (30s)"))
}

/// 进程内唯一的临时文件名：pid + 递增计数，避免并发执行互相覆盖/误删。
fn unique_rust_paths() -> (PathBuf, PathBuf) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = format!(
        "host_code_exec_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let tmp_dir = std::env::temp_dir();
    (
        tmp_dir.join(format!("{unique}.rs")),
        tmp_dir.join(format!("{unique}_bin")),
    )
}

fn execute_rust_code(code: &str, ws_root: &Path) -> Result<Value, String> {
    let (src_path, bin_path) = unique_rust_paths();
    let result = compile_and_run_rust(code, ws_root, &src_path, &bin_path);
    // 所有路径（含编译失败/超时）统一清理临时文件。
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    result
}

fn compile_and_run_rust(
    code: &str,
    ws_root: &Path,
    src_path: &Path,
    bin_path: &Path,
) -> Result<Value, String> {
    std::fs::write(src_path, code).map_err(|e| format!("write source: {e}"))?;
    let timeout = Duration::from_secs(30);

    let mut cmd = std::process::Command::new("rustc");
    cmd.arg("--edition").arg("2021");
    cmd.arg(src_path);
    cmd.arg("-o").arg(bin_path);
    cmd.current_dir(ws_root);
    // 同解释器路径：rustc 多经 rustup 装在 ~/.cargo/bin，无 PATH 必然解析失败。
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    watchdog::apply_lifecycle_hooks(&mut cmd);

    let child = cmd.spawn().map_err(|e| format!("rustc spawn: {e}"))?;
    let compile_outcome = run_child_with_timeout(child, timeout)?;
    let Some(status) = compile_outcome.status else {
        return Ok(compile_outcome.into_json("rustc compilation timed out (30s)"));
    };
    if !status.success() {
        return Ok(serde_json::json!({
            "success": false,
            "stdout": "",
            "stderr": compile_outcome.stderr.text(),
            "exit_code": status.code().unwrap_or(-1),
            "stdout_truncated": false,
            "stderr_truncated": compile_outcome.stderr.truncated,
        }));
    }

    let mut run_cmd = std::process::Command::new(bin_path);
    run_cmd.current_dir(ws_root);
    run_cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        run_cmd.env("PATH", path);
    }
    run_cmd.stdout(std::process::Stdio::piped());
    run_cmd.stderr(std::process::Stdio::piped());
    watchdog::apply_lifecycle_hooks(&mut run_cmd);

    let run_child = run_cmd.spawn().map_err(|e| format!("run spawn: {e}"))?;
    let run_outcome = run_child_with_timeout(run_child, timeout)?;
    Ok(run_outcome.into_json("binary execution timed out (30s)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ctx(root: &Path) -> HostContext {
        HostContext::for_workspace(root.to_path_buf())
    }

    // 子进程组/看门狗生命周期测试已随实现移至 common::watchdog（v0.5.5）。

    #[test]
    fn file_write_read_roundtrip_within_root() {
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        let w =
            host_file_write(&ctx, &serde_json::json!({"path": "a.txt", "content": "hi"})).unwrap();
        assert_eq!(w["success"], true);
        let r = host_file_read(&ctx, &serde_json::json!({"path": "a.txt"})).unwrap();
        assert_eq!(r["content"], "hi");
    }

    #[test]
    fn file_read_escape_rejected() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ctx(&root);
        let r = host_file_read(&ctx, &serde_json::json!({"path": "../secret"})).unwrap();
        assert_eq!(r["success"], false);
    }

    #[test]
    fn file_read_with_extra_read_roots_allows_append_root() {
        let dir = tempdir().unwrap();
        let ws = dir.path().join("ws");
        let extra = dir.path().join("extra");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::write(extra.join("note.txt"), "from-extra").unwrap();

        // v0.4.8：[fs] read_roots 追加后，file.read 可读追加根内文件。
        let ctx = HostContext::for_workspace_with_roots(ws, vec![extra.clone()]);
        let r = host_file_read(
            &ctx,
            &serde_json::json!({"path": extra.join("note.txt").to_string_lossy()}),
        )
        .unwrap();
        assert_eq!(r["content"], "from-extra", "追加根内文件应可读，got: {r}");
    }

    #[test]
    fn file_read_outside_all_roots_rejected_even_with_extra_roots() {
        let dir = tempdir().unwrap();
        let ws = dir.path().join("ws");
        let extra = dir.path().join("extra");
        let outsider = dir.path().join("outsider");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::create_dir_all(&outsider).unwrap();
        std::fs::write(outsider.join("secret.txt"), "s").unwrap();

        // 配置了追加根仍拒绝所有根之外的越界读。
        let ctx = HostContext::for_workspace_with_roots(ws, vec![extra]);
        let r = host_file_read(
            &ctx,
            &serde_json::json!({"path": outsider.join("secret.txt").to_string_lossy()}),
        )
        .unwrap();
        assert_eq!(r["success"], false, "根外读必须被拒绝，got: {r}");
    }

    #[test]
    fn shell_prose_rejected() {
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        let r = host_shell_exec(&ctx, &serde_json::json!({"command": "读一下文件"})).unwrap();
        assert_eq!(r["success"], false);
    }

    #[test]
    fn shell_simple_command_runs() {
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        let r = host_shell_exec(&ctx, &serde_json::json!({"command": "echo ok"})).unwrap();
        assert_eq!(r["exit_code"], 0);
        assert_eq!(r["stdout"].as_str().unwrap().trim(), "ok");
    }

    #[test]
    fn check_dangerous_rejects_dd_as_first_word() {
        assert!(check_dangerous(
            "dd if=/dev/zero of=/dev/null bs=1M count=1"
        ));
        assert!(check_dangerous("dd of=/tmp/out"));
        assert!(check_dangerous(
            "echo x | sudo dd if=/dev/zero of=/dev/null"
        ));
    }

    #[test]
    fn check_dangerous_allows_ip_addr() {
        assert!(!check_dangerous("ip addr"));
        assert!(!check_dangerous("ip -4 addr show"));
        assert!(!check_dangerous("adduser alice"));
    }

    #[test]
    fn check_dangerous_keeps_sudo_chain_rejection() {
        assert!(check_dangerous("echo x | sudo rm -rf /tmp/x"));
        assert!(check_dangerous("sudo mkfs.ext4 /dev/sdb1"));
    }

    #[test]
    fn check_dangerous_keeps_legacy_dangerous_patterns() {
        assert!(check_dangerous("rm -rf /"));
        assert!(check_dangerous("rm -rf /*"));
        assert!(check_dangerous(":(){ :|:& };:"));
        assert!(check_dangerous("chmod -R 777 /"));
        assert!(check_dangerous("echo x > /dev/sda"));
        assert!(check_dangerous("mkfs.ext4 /dev/sda1"));
    }

    #[test]
    fn check_dangerous_blocks_pipe_segments_and_rm_root_variants() {
        // 段级检测：管道右侧的 dd 也要拦截。
        assert!(check_dangerous("echo x | dd of=/dev/sda"));
        // 空白归一化与引号剥离。
        assert!(check_dangerous("rm  -rf  /"));
        assert!(check_dangerous("rm -rf \"/\""));
        assert!(check_dangerous("rm -rf /*"));
        assert!(check_dangerous("rm -fr /"));
        assert!(check_dangerous("rm -r -f /"));
        // 普通递归删除不受影响。
        assert!(!check_dangerous("rm -rf /tmp/scratch"));
    }

    #[test]
    fn check_dangerous_allows_substring_lookalikes() {
        assert!(!check_dangerous("grep sudoers /etc/passwd"));
        assert!(!check_dangerous("ip addr"));
        assert!(!check_dangerous("ls -la"));
    }

    #[test]
    fn shell_exec_large_output_does_not_deadlock() {
        // 死锁回归：输出超管道缓冲（~64KB）时旧实现读端不排空、子进程阻塞在
        // write 上永不退出，30s 超时被杀且输出全丢。
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        let r = host_shell_exec(&ctx, &serde_json::json!({"command": "seq 1 200000"})).unwrap();
        assert_eq!(r["success"], true, "got: {r}");
        let stdout = r["stdout"].as_str().unwrap();
        assert!(!stdout.is_empty());
        assert!(stdout.starts_with("1\n2\n"), "got: {r}");
        assert_eq!(stdout.len(), 1024 * 1024, "应恰好读满 1 MiB 上限");
        assert_eq!(r["stdout_truncated"], true, "got: {r}");
    }

    #[test]
    fn shell_exec_timeout_returns_failure_json_not_err() {
        // 错误契约统一（Bug 10）：超时返回 Ok + success:false + error 含 timeout，
        // 不再是 Err。
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        let start = Instant::now();
        let r = host_shell_exec(&ctx, &serde_json::json!({"command": "sleep 60"})).unwrap();
        assert!(
            start.elapsed() >= Duration::from_secs(29),
            "应在 30s 超时后被杀"
        );
        assert_eq!(r["success"], false, "got: {r}");
        assert!(r["error"].as_str().unwrap().contains("timeout"), "got: {r}");
        assert!(r["exit_code"].is_null(), "got: {r}");
    }

    #[test]
    fn code_exec_node_ruby_execute_and_print() {
        // A2 回归：node/ruby 的 -c 是语法检查，必须用 -e 才会执行。
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // 运行时缺失则跳过
        }
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        let r = host_code_exec(
            &ctx,
            &serde_json::json!({"code": "console.log('hello-node')", "language": "node"}),
        )
        .unwrap();
        assert_eq!(r["exit_code"], 0, "got: {r}");
        assert!(
            r["stdout"].as_str().unwrap().contains("hello-node"),
            "got: {r}"
        );

        if std::process::Command::new("ruby")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // 运行时缺失则跳过
        }
        let r = host_code_exec(
            &ctx,
            &serde_json::json!({"code": "puts 'hello-ruby'", "language": "ruby"}),
        )
        .unwrap();
        assert_eq!(r["exit_code"], 0, "got: {r}");
        assert!(
            r["stdout"].as_str().unwrap().contains("hello-ruby"),
            "got: {r}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_read_symlink_escape_rejected() {
        // A4 回归：工作区内指向外部的符号链接必须被拒绝。
        let dir = tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "outside").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("link.txt")).unwrap();
        let ctx = ctx(&root);

        let r = host_file_read(&ctx, &serde_json::json!({"path": "link.txt"})).unwrap();
        assert_eq!(r["success"], false, "符号链接指向沙箱外必须拒绝, got: {r}");

        match host_file_chunk_read(
            &ctx,
            &serde_json::json!({"path": "link.txt", "offset": 0, "size": 4}),
        ) {
            Err(_) => {}
            Ok(v) => assert_eq!(
                v["success"], false,
                "chunk_read 经符号链接读外部文件必须拒绝, got: {v}"
            ),
        }
    }

    #[test]
    fn path_exists_reports_type() {
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        host_file_write(&ctx, &serde_json::json!({"path": "x.txt", "content": "x"})).unwrap();
        let r = host_path_exists(&ctx, &serde_json::json!({"path": "x.txt"})).unwrap();
        assert_eq!(r["exists"], true);
        assert_eq!(r["is_file"], true);
        let r = host_path_exists(&ctx, &serde_json::json!({"path": "missing.txt"})).unwrap();
        assert_eq!(r["exists"], false);
    }

    #[test]
    fn file_glob_matches_nested_files() {
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        host_file_write(
            &ctx,
            &serde_json::json!({"path": "a/one.md", "content": "1"}),
        )
        .unwrap();
        host_file_write(
            &ctx,
            &serde_json::json!({"path": "a/two.txt", "content": "2"}),
        )
        .unwrap();
        let r = host_file_glob(&ctx, &serde_json::json!({"pattern": "**/*.md"})).unwrap();
        assert_eq!(r["count"], 1);
        assert!(r["matches"][0].as_str().unwrap().contains("one.md"));
    }

    #[test]
    fn text_grep_recursive_directory() {
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        host_file_write(
            &ctx,
            &serde_json::json!({"path": "logs/a.log", "content": "ERROR one"}),
        )
        .unwrap();
        host_file_write(
            &ctx,
            &serde_json::json!({"path": "logs/b.log", "content": "ok"}),
        )
        .unwrap();
        let r = host_text_grep(
            &ctx,
            &serde_json::json!({"path": "logs", "pattern": "ERROR", "recursive": true}),
        )
        .unwrap();
        assert!(r["matches"][0].as_str().unwrap().contains("a.log"));
    }

    #[test]
    fn json_validate_reports_parse_and_schema_errors() {
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        let r = host_json_validate(&ctx, &serde_json::json!({"text": "{\"a\":1}"})).unwrap();
        assert_eq!(r["valid"], true);
        let r = host_json_validate(&ctx, &serde_json::json!({"text": "{bad"})).unwrap();
        assert_eq!(r["valid"], false);
        let r = host_json_validate(
            &ctx,
            &serde_json::json!({"text": "{\"a\":1}", "schema": {"type":"object","required":["b"]}}),
        )
        .unwrap();
        assert_eq!(r["valid"], false);
    }

    #[test]
    fn chunk_read_reads_prefix() {
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        host_file_write(
            &ctx,
            &serde_json::json!({"path": "c.txt", "content": "abcdef"}),
        )
        .unwrap();
        let r = host_file_chunk_read(
            &ctx,
            &serde_json::json!({"path": "c.txt", "offset": 1, "size": 3}),
        )
        .unwrap();
        assert_eq!(r["content"], "bcd");
        assert_eq!(r["bytes_read"], 3);
    }

    #[test]
    fn chunk_read_rejects_negative_offset_and_size() {
        // A11：负值不再 as u64/as usize 回绕，明确报错。
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        host_file_write(
            &ctx,
            &serde_json::json!({"path": "n.txt", "content": "abcdef"}),
        )
        .unwrap();
        let r = host_file_chunk_read(
            &ctx,
            &serde_json::json!({"path": "n.txt", "offset": -1, "size": 3}),
        )
        .unwrap();
        assert_eq!(r["success"], false, "负 offset 应明确报错, got: {r}");
        let r = host_file_chunk_read(
            &ctx,
            &serde_json::json!({"path": "n.txt", "offset": 0, "size": -2}),
        )
        .unwrap();
        assert_eq!(r["success"], false, "负 size 应明确报错, got: {r}");
    }

    #[test]
    fn chunk_read_exact_multiple_reports_eof_on_final_chunk() {
        // A11：6 字节 = 2 x size(3)，最后一轮读满 size 且恰好到文件末尾，
        // is_eof 必须为 true（旧版 bytes_read < size 推断在此边界漏报）。
        let dir = tempdir().unwrap();
        let ctx = ctx(dir.path());
        host_file_write(
            &ctx,
            &serde_json::json!({"path": "m.txt", "content": "abcdef"}),
        )
        .unwrap();
        let r = host_file_chunk_read(
            &ctx,
            &serde_json::json!({"path": "m.txt", "offset": 0, "size": 3}),
        )
        .unwrap();
        assert_eq!(r["is_eof"], false, "第一块未到 EOF, got: {r}");
        let r = host_file_chunk_read(
            &ctx,
            &serde_json::json!({"path": "m.txt", "offset": 3, "size": 3}),
        )
        .unwrap();
        assert_eq!(r["content"], "def");
        assert_eq!(r["bytes_read"], 3);
        assert_eq!(
            r["is_eof"], true,
            "整倍数边界最后一轮 is_eof 应为 true, got: {r}"
        );
    }
}
