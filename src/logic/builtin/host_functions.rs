use super::host_context::HostContext;
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

/// 与 WASM 时期完全一致的路径解析：相对路径以首个 root 为基准，
/// 绝对路径也必须 canonicalize 后落在 roots 内。
fn resolve_sandbox_path(path_str: &str, roots: &[PathBuf]) -> std::result::Result<PathBuf, String> {
    let root = roots.first().cloned().unwrap_or_default();
    let raw = Path::new(path_str);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
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
    let is_allowed = roots.iter().any(|r| {
        let canonical_root = r.canonicalize().unwrap_or_else(|_| r.clone());
        resolved.starts_with(&canonical_root)
    });
    if !is_allowed {
        return Err("path not in sandbox roots".to_string());
    }
    Ok(resolved)
}

fn ensure_within(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let ok = roots.iter().any(|root| {
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        path.starts_with(&canonical_root)
    });
    if ok {
        Ok(())
    } else {
        Err("path not in sandbox roots".to_string())
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

const DANGEROUS_CMDS: &[&str] = &[
    "sudo",
    "rm -rf /",
    "rm -rf /*",
    "mkfs",
    ":(){ :|:& };:",
    "chmod -R 777 /",
    "> /dev/sda",
];

fn check_dangerous(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    // dd 只按命令词匹配（首词 == dd 或 dd=...），避免 contains 子串误伤 ip addr 等正常命令。
    if let Some(first) = trimmed.split_whitespace().next() {
        if first == "dd" || first.starts_with("dd=") {
            return true;
        }
    }
    DANGEROUS_CMDS
        .iter()
        .any(|bad| trimmed.starts_with(bad) || trimmed.contains(bad))
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

    let syntax_check = std::process::Command::new("sh")
        .arg("-n")
        .arg("-c")
        .arg(command)
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

    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&workspace_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("host_shell_exec: spawn: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("host_shell_exec: timeout (30s)".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("host_shell_exec: wait: {e}")),
        }
    };

    let stdout = child
        .stdout
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();
    let stderr = child
        .stderr
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);
    Ok(serde_json::json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code,
    }))
}

pub fn host_file_chunk_read(ctx: &HostContext, args: &Value) -> Result<Value, String> {
    let path_str = match required_string(args, "path", "host_file_chunk_read") {
        Ok(v) => v,
        Err(v) => return Ok(v),
    };
    let offset = args.get("offset").and_then(|v| v.as_i64()).unwrap_or(0) as u64;
    let size = args.get("size").and_then(|v| v.as_i64()).unwrap_or(4096) as usize;

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
    let bytes_read = file.read(&mut buf).map_err(|e| format!("read: {e}"))?;
    buf.truncate(bytes_read);
    let is_eof = bytes_read < size;
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
    let runtime = match language {
        "python" | "python3" => "python3",
        "python2" => "python2",
        "sh" | "bash" => "bash",
        "node" | "javascript" => "node",
        "ruby" => "ruby",
        _ => return Err(format!("unsupported language: {language}")),
    };

    let mut cmd = std::process::Command::new(runtime);
    cmd.arg("-c").arg(code);
    cmd.current_dir(ws_root);
    cmd.env_clear();
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    let status = loop {
        if start.elapsed() > timeout {
            let _ = child.kill();
            return Err("code execution timed out (30s)".into());
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };

    let stdout = child
        .stdout
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();
    let stderr = child
        .stderr
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    Ok(serde_json::json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code,
    }))
}

fn execute_rust_code(code: &str, ws_root: &Path) -> Result<Value, String> {
    let tmp_dir = std::env::temp_dir();
    let src_path = tmp_dir.join("host_code_exec.rs");
    let bin_path = tmp_dir.join("host_code_exec_bin");
    std::fs::write(&src_path, code).map_err(|e| format!("write source: {e}"))?;

    let mut cmd = std::process::Command::new("rustc");
    cmd.arg("--edition").arg("2021");
    cmd.arg(&src_path);
    cmd.arg("-o").arg(&bin_path);
    cmd.current_dir(ws_root);
    cmd.env_clear();
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("rustc spawn: {e}"))?;
    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    let status = loop {
        if start.elapsed() > timeout {
            let _ = child.kill();
            return Err("rustc compilation timed out (30s)".into());
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("rustc wait: {e}")),
        }
    };

    let stderr = child
        .stderr
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();

    if !status.success() {
        return Ok(serde_json::json!({
            "stdout": "",
            "stderr": stderr,
            "exit_code": status.code().unwrap_or(-1),
        }));
    }

    let mut run_cmd = std::process::Command::new(&bin_path);
    run_cmd.current_dir(ws_root);
    run_cmd.env_clear();
    run_cmd.stdout(std::process::Stdio::piped());
    run_cmd.stderr(std::process::Stdio::piped());

    let mut run_child = run_cmd.spawn().map_err(|e| format!("run spawn: {e}"))?;
    let start = Instant::now();
    let run_status = loop {
        if start.elapsed() > timeout {
            let _ = run_child.kill();
            return Err("binary execution timed out (30s)".into());
        }
        match run_child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("run wait: {e}")),
        }
    };

    let run_stdout = run_child
        .stdout
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();
    let run_stderr = run_child
        .stderr
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);

    Ok(serde_json::json!({
        "stdout": run_stdout,
        "stderr": run_stderr,
        "exit_code": run_status.code().unwrap_or(-1),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ctx(root: &Path) -> HostContext {
        HostContext::for_workspace(root.to_path_buf())
    }

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
}
