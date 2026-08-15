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
    Ok(serde_json::json!({"entries": entries}))
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

    let canonical = match resolve_sandbox_path(path_str, &ctx.permission.file_read_roots) {
        Ok(p) => p,
        Err(e) => return fail_str(format!("host_text_grep: {e}")),
    };
    ensure_within(&canonical, &ctx.permission.file_read_roots)
        .map_err(|e| format!("host_text_grep: {e}"))?;

    let content =
        std::fs::read_to_string(&canonical).map_err(|e| format!("host_text_grep: read: {e}"))?;
    let matches: Vec<String> = content
        .lines()
        .filter(|line| line.contains(pattern))
        .map(|s| s.to_string())
        .collect();
    Ok(serde_json::json!({"matches": matches}))
}

const DANGEROUS_CMDS: &[&str] = &[
    "sudo",
    "rm -rf /",
    "rm -rf /*",
    "mkfs",
    "dd",
    ":(){ :|:& };:",
    "chmod -R 777 /",
    "> /dev/sda",
];

fn check_dangerous(cmd: &str) -> bool {
    let trimmed = cmd.trim();
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
