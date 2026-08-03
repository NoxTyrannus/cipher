use super::host_context::HostContext;
use crate::common::AgentError;
use std::io::Read;
use std::time::{Duration, Instant};

fn read_memory(
    caller: &mut wasmtime::Caller<'_, HostContext>,
    ptr: i32,
    len: i32,
) -> Result<Vec<u8>, wasmtime::Error> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("host: missing memory export"))?;
    let mut buf = vec![0u8; len.max(0) as usize];
    memory
        .read(caller, ptr as usize, &mut buf)
        .map_err(|e| wasmtime::Error::msg(format!("host: memory read failed: {e}")))?;
    Ok(buf)
}

fn write_memory(
    caller: &mut wasmtime::Caller<'_, HostContext>,
    ptr: i32,
    data: &[u8],
) -> Result<(), wasmtime::Error> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("host: missing memory export"))?;
    memory
        .write(caller, ptr as usize, data)
        .map_err(|e| wasmtime::Error::msg(format!("host: memory write failed: {e}")))?;
    Ok(())
}

fn write_to_scratch(
    caller: &mut wasmtime::Caller<'_, HostContext>,
    data: &[u8],
) -> Result<(i32, i32), wasmtime::Error> {
    const SCRATCH: i32 = 4096;
    write_memory(caller, SCRATCH, data)?;
    Ok((SCRATCH, data.len() as i32))
}

fn write_result_to_scratch(
    caller: &mut wasmtime::Caller<'_, HostContext>,
    result: &serde_json::Value,
) -> Result<i32, wasmtime::Error> {
    let data =
        serde_json::to_vec(result).map_err(|e| wasmtime::Error::msg(format!("serialize: {e}")))?;
    const SCRATCH: i32 = 4096;
    const SCRATCH_LEN: i32 = 4092;
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("missing memory export"))?;
    memory
        .write(
            &mut *caller,
            SCRATCH_LEN as usize,
            &(data.len() as i32).to_le_bytes(),
        )
        .map_err(|e| wasmtime::Error::msg(format!("scratch len write: {e}")))?;
    memory
        .write(&mut *caller, SCRATCH as usize, &data)
        .map_err(|e| wasmtime::Error::msg(format!("scratch write: {e}")))?;
    Ok(0)
}

fn write_error_to_scratch(
    caller: &mut wasmtime::Caller<'_, HostContext>,
    msg: &str,
) -> Result<i32, wasmtime::Error> {
    let result = serde_json::json!({"success": false, "error": msg});
    write_result_to_scratch(caller, &result)?;
    Ok(1)
}

pub fn host_triviumdb_search(
    mut caller: wasmtime::Caller<'_, HostContext>,
    vec_ptr: i32,
    vec_len: i32,
    k: i32,
    _filter_ptr: i32,
    _filter_len: i32,
) -> Result<(i32, i32), wasmtime::Error> {
    if !caller.data().permission.memory_read {
        return Err(wasmtime::Error::msg(
            "host_triviumdb_search: permission denied (memory_read=false)",
        ));
    }

    let actual_k = (k as u32).min(caller.data().budget.max_k);

    let vec_bytes = read_memory(&mut caller, vec_ptr, vec_len)?;
    if vec_bytes.len() % 4 != 0 {
        return Err(wasmtime::Error::msg(
            "host_triviumdb_search: vec_len must be multiple of 4 (f32)",
        ));
    }
    let vector: Vec<f32> = vec_bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();

    let triviumdb_arc =
        caller.data().triviumdb.clone().ok_or_else(|| {
            wasmtime::Error::msg("host_triviumdb_search: TriviumDB not configured")
        })?;
    let db = triviumdb_arc
        .lock()
        .map_err(|e| wasmtime::Error::msg(format!("host_triviumdb_search: lock: {e}")))?;

    let results = db
        .db()
        .search(&vector, actual_k as usize, 0, 0.0)
        .map_err(|e| wasmtime::Error::msg(format!("host_triviumdb_search: {e}")))?;

    let ids: Vec<i64> = results.iter().map(|r| r.id as i64).collect();
    let ids_json =
        serde_json::to_vec(&ids).map_err(|e| wasmtime::Error::msg(format!("serialize: {e}")))?;
    drop(db);

    write_to_scratch(&mut caller, &ids_json)
}

pub fn host_triviumdb_upsert(
    mut caller: wasmtime::Caller<'_, HostContext>,
    mem_type_ptr: i32,
    mem_type_len: i32,
    payload_ptr: i32,
    payload_len: i32,
    vec_ptr: i32,
    vec_len: i32,
) -> Result<i64, wasmtime::Error> {
    if !caller.data().permission.memory_write {
        return Err(wasmtime::Error::msg(
            "host_triviumdb_upsert: permission denied (memory_write=false)",
        ));
    }

    let mem_type_bytes = read_memory(&mut caller, mem_type_ptr, mem_type_len)?;
    let mem_type = String::from_utf8(mem_type_bytes)
        .map_err(|_| wasmtime::Error::msg("mem_type not utf-8"))?;

    let payload_bytes = read_memory(&mut caller, payload_ptr, payload_len)?;
    let mut payload: serde_json::Value = serde_json::from_slice(&payload_bytes)
        .map_err(|e| wasmtime::Error::msg(format!("invalid JSON payload: {e}")))?;

    if !payload.is_object() {
        return Err(wasmtime::Error::msg("payload must be a JSON object"));
    }
    if let serde_json::Value::Object(ref mut map) = payload {
        map.insert(
            "_memory_type".to_string(),
            serde_json::Value::String(mem_type),
        );
    }

    let vec_bytes = read_memory(&mut caller, vec_ptr, vec_len)?;
    if vec_bytes.len() % 4 != 0 {
        return Err(wasmtime::Error::msg("vec_len must be multiple of 4 (f32)"));
    }
    let vector: Vec<f32> = vec_bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();

    let triviumdb_arc =
        caller.data().triviumdb.clone().ok_or_else(|| {
            wasmtime::Error::msg("host_triviumdb_upsert: TriviumDB not configured")
        })?;
    let mut db = triviumdb_arc
        .lock()
        .map_err(|e| wasmtime::Error::msg(format!("host_triviumdb_upsert: lock: {e}")))?;

    let node_id = db
        .db_mut()
        .insert(&vector, payload)
        .map_err(|e| wasmtime::Error::msg(format!("host_triviumdb_upsert: insert: {e}")))?;

    Ok(node_id as i64)
}

pub fn host_duckdb_query(
    mut caller: wasmtime::Caller<'_, HostContext>,
    sql_ptr: i32,
    sql_len: i32,
) -> Result<(i32, i32), wasmtime::Error> {
    if !caller.data().permission.db_read {
        return Err(wasmtime::Error::msg(
            "host_duckdb_query: permission denied (db_read=false)",
        ));
    }

    let sql_bytes = read_memory(&mut caller, sql_ptr, sql_len)?;
    let mut sql = String::from_utf8(sql_bytes)
        .map_err(|_| wasmtime::Error::msg("host_duckdb_query: SQL not utf-8"))?;

    let upper = sql.trim().to_uppercase();
    if !upper.starts_with("SELECT") {
        return Err(wasmtime::Error::msg(
            "host_duckdb_query: only SELECT is allowed",
        ));
    }
    let disallowed = [
        "DROP", "ALTER", "INSERT", "UPDATE", "DELETE", "CREATE", "TRUNCATE",
    ];
    for kw in &disallowed {
        if upper.contains(kw) {
            return Err(wasmtime::Error::msg(format!(
                "host_duckdb_query: keyword {kw} is not allowed in query"
            )));
        }
    }

    let max_rows = caller.data().budget.max_query_rows;
    sql.push_str(&format!(" LIMIT {max_rows}"));

    let duckdb_arc = caller
        .data()
        .duckdb
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("host_duckdb_query: DuckDB not configured"))?;
    let conn = duckdb_arc
        .lock()
        .map_err(|e| wasmtime::Error::msg(format!("host_duckdb_query: lock: {e}")))?;

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| wasmtime::Error::msg(format!("host_duckdb_query: prepare: {e}")))?;

    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|i| {
            let default = "?".to_string();
            stmt.column_name(i).unwrap_or(&default).to_string()
        })
        .collect();

    let rows = stmt
        .query_map([], |row| {
            let mut map = serde_json::Map::new();
            for (i, name) in column_names.iter().enumerate() {
                let val = row_to_json_value(row, i);
                map.insert(name.clone(), val);
            }
            Ok(serde_json::Value::Object(map))
        })
        .map_err(|e| wasmtime::Error::msg(format!("host_duckdb_query: query: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        let r = row.map_err(|e| wasmtime::Error::msg(format!("host_duckdb_query: row: {e}")))?;
        results.push(r);
    }

    drop(conn);
    let output = serde_json::to_vec(&results)
        .map_err(|e| wasmtime::Error::msg(format!("host_duckdb_query: serialize: {e}")))?;
    write_to_scratch(&mut caller, &output)
}

fn row_to_json_value(row: &duckdb::Row, i: usize) -> serde_json::Value {
    if let Ok(v) = row.get::<_, Option<String>>(i) {
        return match v {
            Some(s) => serde_json::Value::String(s),
            None => serde_json::Value::Null,
        };
    }
    if let Ok(v) = row.get::<_, i64>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.get::<_, f64>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.get::<_, bool>(i) {
        return serde_json::json!(v);
    }
    serde_json::Value::Null
}

pub fn host_duckdb_execute(
    mut caller: wasmtime::Caller<'_, HostContext>,
    sql_ptr: i32,
    sql_len: i32,
) -> Result<i32, wasmtime::Error> {
    if !caller.data().permission.db_write {
        return Err(wasmtime::Error::msg(
            "host_duckdb_execute: permission denied (db_write=false)",
        ));
    }

    let sql_bytes = read_memory(&mut caller, sql_ptr, sql_len)?;
    let sql = String::from_utf8(sql_bytes)
        .map_err(|_| wasmtime::Error::msg("host_duckdb_execute: SQL not utf-8"))?;

    let upper = sql.trim().to_uppercase();
    let allowed = [
        "INSERT",
        "UPDATE",
        "DELETE",
        "CREATE TABLE IF NOT EXISTS",
        "CREATE INDEX IF NOT EXISTS",
    ];
    let is_allowed = allowed.iter().any(|kw| upper.starts_with(kw));
    if !is_allowed {
        return Err(wasmtime::Error::msg(
            "host_duckdb_execute: only INSERT/UPDATE/DELETE/CREATE TABLE IF NOT EXISTS are allowed",
        ));
    }

    let blocked = [
        "DROP",
        "ALTER",
        "TRUNCATE",
        "CREATE DATABASE",
        "ATTACH",
        "DETACH",
    ];
    for kw in &blocked {
        if upper.contains(kw) {
            return Err(wasmtime::Error::msg(format!(
                "host_duckdb_execute: keyword {kw} is blocked"
            )));
        }
    }

    let duckdb_arc = caller
        .data()
        .duckdb
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("host_duckdb_execute: DuckDB not configured"))?;
    let conn = duckdb_arc
        .lock()
        .map_err(|e| wasmtime::Error::msg(format!("host_duckdb_execute: lock: {e}")))?;

    let affected = conn
        .execute(&sql, [])
        .map_err(|e| wasmtime::Error::msg(format!("host_duckdb_execute: {e}")))?;

    Ok(affected as i32)
}

fn resolve_sandbox_path(
    path_str: &str,
    roots: &[std::path::PathBuf],
) -> Result<std::path::PathBuf, wasmtime::Error> {
    let root = roots.first().cloned().unwrap_or_default();
    let raw = std::path::Path::new(path_str);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let parent = candidate
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let parent_canonical = parent
        .canonicalize()
        .map_err(|e| wasmtime::Error::msg(format!("resolve path parent: {e}")))?;
    let file_name = candidate
        .file_name()
        .ok_or_else(|| wasmtime::Error::msg("resolve path: no file name"))?;
    let resolved = parent_canonical.join(file_name);
    let is_allowed = roots.iter().any(|r| {
        let canonical_root = r.canonicalize().unwrap_or_else(|_| r.clone());
        resolved.starts_with(&canonical_root)
    });
    if !is_allowed {
        return Err(wasmtime::Error::msg("path not in sandbox roots"));
    }
    Ok(resolved)
}

pub fn host_file_read(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    let input_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let input: serde_json::Value = match serde_json::from_slice(&input_bytes) {
        Ok(v) => v,
        Err(e) => {
            return write_error_to_scratch(
                &mut caller,
                &format!("host_file_read: invalid JSON: {e}"),
            )
        }
    };
    let Some(path_str) = input.get("path").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_file_read: missing 'path' field");
    };

    let canonical = match resolve_sandbox_path(path_str, &caller.data().permission.file_read_roots)
    {
        Ok(p) => p,
        Err(e) => return write_error_to_scratch(&mut caller, &format!("host_file_read: {e}")),
    };

    let max_bytes = caller.data().budget.max_file_read_bytes;
    let metadata = match std::fs::metadata(&canonical) {
        Ok(m) => m,
        Err(e) => {
            return write_error_to_scratch(&mut caller, &format!("host_file_read: metadata: {e}"))
        }
    };
    if metadata.len() > max_bytes {
        return write_error_to_scratch(
            &mut caller,
            &format!(
                "host_file_read: file too large ({} > {} bytes)",
                metadata.len(),
                max_bytes
            ),
        );
    }

    let data = match std::fs::read(&canonical) {
        Ok(d) => d,
        Err(e) => {
            return write_error_to_scratch(&mut caller, &format!("host_file_read: read: {e}"))
        }
    };
    let content = match String::from_utf8(data) {
        Ok(c) => c,
        Err(_) => return write_error_to_scratch(&mut caller, "host_file_read: file not utf-8"),
    };

    let result = serde_json::json!({"content": content, "size": content.len()});
    write_result_to_scratch(&mut caller, &result)
}

pub fn host_file_write(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    let input_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let input: serde_json::Value = match serde_json::from_slice(&input_bytes) {
        Ok(v) => v,
        Err(e) => {
            return write_error_to_scratch(
                &mut caller,
                &format!("host_file_write: invalid JSON: {e}"),
            )
        }
    };
    let Some(path_str) = input.get("path").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_file_write: missing 'path' field");
    };
    let Some(content_str) = input.get("content").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_file_write: missing 'content' field");
    };

    let canonical = match resolve_sandbox_path(path_str, &caller.data().permission.file_write_roots)
    {
        Ok(p) => p,
        Err(e) => return write_error_to_scratch(&mut caller, &format!("host_file_write: {e}")),
    };

    let data = content_str.as_bytes();
    let max_bytes = caller.data().budget.max_file_write_bytes;
    if data.len() as u64 > max_bytes {
        return write_error_to_scratch(
            &mut caller,
            &format!(
                "host_file_write: data too large ({} > {} bytes)",
                data.len(),
                max_bytes
            ),
        );
    }

    let Some(dir) = canonical.parent() else {
        return write_error_to_scratch(&mut caller, "host_file_write: no parent directory");
    };
    let tmp_path = dir.join(format!(
        ".host_tmp_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    if let Err(e) = std::fs::write(&tmp_path, data) {
        return write_error_to_scratch(&mut caller, &format!("host_file_write: tmp write: {e}"));
    }

    if let Ok(tmp_file) = std::fs::File::open(&tmp_path) {
        tmp_file.sync_all().ok();
    }

    if let Err(e) = std::fs::rename(&tmp_path, &canonical) {
        return write_error_to_scratch(&mut caller, &format!("host_file_write: rename: {e}"));
    }

    if let Ok(dir_file) = std::fs::File::open(dir) {
        dir_file.sync_all().ok();
    }

    let result = serde_json::json!({"success": true});
    write_result_to_scratch(&mut caller, &result)
}

pub fn host_file_list(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    let input_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let input: serde_json::Value = match serde_json::from_slice(&input_bytes) {
        Ok(v) => v,
        Err(e) => {
            return write_error_to_scratch(
                &mut caller,
                &format!("host_file_list: invalid JSON: {e}"),
            )
        }
    };
    let Some(path_str) = input.get("path").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_file_list: missing 'path' field");
    };

    let canonical = match resolve_sandbox_path(path_str, &caller.data().permission.file_read_roots)
    {
        Ok(p) => p,
        Err(e) => return write_error_to_scratch(&mut caller, &format!("host_file_list: {e}")),
    };

    let allowed = caller.data().permission.file_read_roots.clone();
    let is_allowed = allowed.iter().any(|root| canonical.starts_with(root));
    if !is_allowed {
        return write_error_to_scratch(&mut caller, "host_file_list: path not in file_read_roots");
    }

    let entries: Vec<String> = match std::fs::read_dir(&canonical) {
        Ok(rd) => rd
            .filter_map(|entry| entry.ok().and_then(|e| e.file_name().into_string().ok()))
            .collect(),
        Err(e) => {
            return write_error_to_scratch(&mut caller, &format!("host_file_list: read_dir: {e}"))
        }
    };

    let result = serde_json::json!({"entries": entries});
    write_result_to_scratch(&mut caller, &result)
}

pub fn host_file_delete(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    let input_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let input: serde_json::Value = match serde_json::from_slice(&input_bytes) {
        Ok(v) => v,
        Err(e) => {
            return write_error_to_scratch(
                &mut caller,
                &format!("host_file_delete: invalid JSON: {e}"),
            )
        }
    };
    let Some(path_str) = input.get("path").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_file_delete: missing 'path' field");
    };

    let canonical = match resolve_sandbox_path(path_str, &caller.data().permission.file_write_roots)
    {
        Ok(p) => p,
        Err(e) => return write_error_to_scratch(&mut caller, &format!("host_file_delete: {e}")),
    };

    let allowed = caller.data().permission.file_write_roots.clone();
    let is_allowed = allowed.iter().any(|root| canonical.starts_with(root));
    if !is_allowed {
        return write_error_to_scratch(
            &mut caller,
            "host_file_delete: path not in file_write_roots",
        );
    }

    let remove_result = if canonical.is_dir() {
        std::fs::remove_dir_all(&canonical)
    } else {
        std::fs::remove_file(&canonical)
    };
    if let Err(e) = remove_result {
        return write_error_to_scratch(&mut caller, &format!("host_file_delete: remove: {e}"));
    }

    let result = serde_json::json!({"success": true});
    write_result_to_scratch(&mut caller, &result)
}

pub fn host_file_move(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    let input_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let input: serde_json::Value = match serde_json::from_slice(&input_bytes) {
        Ok(v) => v,
        Err(e) => {
            return write_error_to_scratch(
                &mut caller,
                &format!("host_file_move: invalid JSON: {e}"),
            )
        }
    };
    let Some(from_str) = input.get("from").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_file_move: missing 'from' field");
    };
    let Some(to_str) = input.get("to").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_file_move: missing 'to' field");
    };

    let from_canonical =
        match resolve_sandbox_path(from_str, &caller.data().permission.file_write_roots) {
            Ok(p) => p,
            Err(e) => {
                return write_error_to_scratch(&mut caller, &format!("host_file_move: from: {e}"))
            }
        };
    let to_canonical =
        match resolve_sandbox_path(to_str, &caller.data().permission.file_write_roots) {
            Ok(p) => p,
            Err(e) => {
                return write_error_to_scratch(&mut caller, &format!("host_file_move: to: {e}"))
            }
        };

    if let Err(e) = std::fs::rename(&from_canonical, &to_canonical) {
        return write_error_to_scratch(&mut caller, &format!("host_file_move: rename: {e}"));
    }

    let result = serde_json::json!({"success": true});
    write_result_to_scratch(&mut caller, &result)
}

pub fn host_text_grep(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    let input_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let input: serde_json::Value = match serde_json::from_slice(&input_bytes) {
        Ok(v) => v,
        Err(e) => {
            return write_error_to_scratch(
                &mut caller,
                &format!("host_text_grep: invalid JSON: {e}"),
            )
        }
    };
    let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_text_grep: missing 'pattern' field");
    };
    let Some(path_str) = input.get("path").and_then(|v| v.as_str()) else {
        return write_error_to_scratch(&mut caller, "host_text_grep: missing 'path' field");
    };

    let canonical = match resolve_sandbox_path(path_str, &caller.data().permission.file_read_roots)
    {
        Ok(p) => p,
        Err(e) => return write_error_to_scratch(&mut caller, &format!("host_text_grep: {e}")),
    };

    let allowed = caller.data().permission.file_read_roots.clone();
    let is_allowed = allowed.iter().any(|root| canonical.starts_with(root));
    if !is_allowed {
        return write_error_to_scratch(&mut caller, "host_text_grep: path not in file_read_roots");
    }

    let content = match std::fs::read_to_string(&canonical) {
        Ok(c) => c,
        Err(e) => {
            return write_error_to_scratch(&mut caller, &format!("host_text_grep: read: {e}"))
        }
    };

    let matches: Vec<String> = content
        .lines()
        .filter(|line| line.contains(pattern))
        .map(|s| s.to_string())
        .collect();

    let result = serde_json::json!({"matches": matches});
    write_result_to_scratch(&mut caller, &result)
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

pub fn host_shell_exec(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    if !caller.data().permission.shell_exec_allowed {
        return Err(wasmtime::Error::msg(
            "host_shell_exec: shell_exec not allowed",
        ));
    }

    let input_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let input: serde_json::Value = serde_json::from_slice(&input_bytes)
        .map_err(|e| wasmtime::Error::msg(format!("host_shell_exec: invalid JSON: {e}")))?;
    let command = input
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| wasmtime::Error::msg("host_shell_exec: missing 'command' field"))?;

    if check_dangerous(command) {
        return write_error_to_scratch(&mut caller, "host_shell_exec: command is blacklisted");
    }

    let first_word = command.split_whitespace().next().unwrap_or("");
    if !first_word.is_ascii() {
        return write_error_to_scratch(
            &mut caller,
            &format!(
                "host_shell_exec: command starts with non-ASCII text (prose, not a command): {}",
                command.chars().take(60).collect::<String>()
            ),
        );
    }

    let syntax_check = std::process::Command::new("sh")
        .arg("-n")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|e| wasmtime::Error::msg(format!("host_shell_exec: syntax check spawn: {e}")))?;
    if !syntax_check.status.success() {
        let stderr: String = String::from_utf8_lossy(&syntax_check.stderr)
            .chars()
            .take(160)
            .collect();
        return write_error_to_scratch(
            &mut caller,
            &format!("host_shell_exec: shell syntax error: {stderr}"),
        );
    }

    let workspace_root = caller
        .data()
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
        .map_err(|e| wasmtime::Error::msg(format!("host_shell_exec: spawn: {e}")))?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(wasmtime::Error::msg("host_shell_exec: timeout (30s)"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(wasmtime::Error::msg(format!("host_shell_exec: wait: {e}")));
            }
        }
    };

    let stdout = if let Some(mut s) = child.stdout.take() {
        let mut buf = String::new();
        let _ = s.read_to_string(&mut buf);
        buf
    } else {
        String::new()
    };
    let stderr = if let Some(mut s) = child.stderr.take() {
        let mut buf = String::new();
        let _ = s.read_to_string(&mut buf);
        buf
    } else {
        String::new()
    };
    let exit_code = status.code().unwrap_or(-1);

    let result = serde_json::json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code,
    });
    write_result_to_scratch(&mut caller, &result)
}

pub fn host_file_chunk_read(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    let json_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let args: serde_json::Value = serde_json::from_slice(&json_bytes)
        .map_err(|e| wasmtime::Error::msg(format!("parse input: {e}")))?;
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| wasmtime::Error::msg("missing 'path'"))?;
    let offset = args.get("offset").and_then(|v| v.as_i64()).unwrap_or(0) as u64;
    let size = args.get("size").and_then(|v| v.as_i64()).unwrap_or(4096) as usize;

    let path_obj = std::path::Path::new(path);
    let canonical = path_obj
        .canonicalize()
        .map_err(|e| wasmtime::Error::msg(format!("canonicalize: {e}")))?;
    let allowed = caller.data().permission.file_read_roots.clone();
    if !allowed.iter().any(|root| canonical.starts_with(root)) {
        let err = serde_json::json!({"error": "path not in file_read_roots"});
        write_result_to_scratch(&mut caller, &err)?;
        return Ok(-1);
    }

    let max_bytes = caller.data().budget.max_file_read_bytes;
    if size as u64 > max_bytes {
        let err =
            serde_json::json!({"error": format!("chunk too large ({} > {})", size, max_bytes)});
        write_result_to_scratch(&mut caller, &err)?;
        return Ok(-1);
    }

    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(&canonical) {
        Ok(f) => f,
        Err(e) => {
            let err = serde_json::json!({"error": format!("open: {e}")});
            write_result_to_scratch(&mut caller, &err)?;
            return Ok(-1);
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(offset)) {
        let err = serde_json::json!({"error": format!("seek: {e}")});
        write_result_to_scratch(&mut caller, &err)?;
        return Ok(-1);
    }
    let mut buf = vec![0u8; size];
    let bytes_read = match file.read(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            let err = serde_json::json!({"error": format!("read: {e}")});
            write_result_to_scratch(&mut caller, &err)?;
            return Ok(-1);
        }
    };
    buf.truncate(bytes_read);
    let is_eof = bytes_read < size;

    let content = String::from_utf8(buf).map_err(|_| wasmtime::Error::msg("content not utf-8"))?;
    let result = serde_json::json!({
        "content": content,
        "bytes_read": bytes_read,
        "is_eof": is_eof,
    });
    write_result_to_scratch(&mut caller, &result)
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

pub fn host_code_exec(
    mut caller: wasmtime::Caller<'_, HostContext>,
    json_ptr: i32,
    json_len: i32,
) -> Result<i32, wasmtime::Error> {
    let json_bytes = read_memory(&mut caller, json_ptr, json_len)?;
    let args: serde_json::Value = serde_json::from_slice(&json_bytes)
        .map_err(|e| wasmtime::Error::msg(format!("parse input: {e}")))?;
    let code = args
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| wasmtime::Error::msg("missing 'code'"))?;
    let language = args
        .get("language")
        .and_then(|v| v.as_str())
        .unwrap_or("python3");

    for bad in DANGEROUS_CODE_PATTERNS {
        if code.contains(bad) {
            let err =
                serde_json::json!({"error": format!("code contains dangerous pattern: {bad}")});
            write_result_to_scratch(&mut caller, &err)?;
            return Ok(-1);
        }
    }

    let ws_root = caller
        .data()
        .permission
        .file_read_roots
        .first()
        .cloned()
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let result = match language {
        "rust" => execute_rust_code(&mut caller, code, &ws_root),
        _ => execute_interpreted_code(language, code, &ws_root),
    };

    match result {
        Ok(output) => write_result_to_scratch(&mut caller, &output),
        Err(e) => {
            let err = serde_json::json!({"error": format!("{e}")});
            write_result_to_scratch(&mut caller, &err)
        }
    }
}

fn execute_interpreted_code(
    language: &str,
    code: &str,
    ws_root: &std::path::Path,
) -> Result<serde_json::Value, String> {
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

    let timeout = std::time::Duration::from_secs(30);
    let start = std::time::Instant::now();
    let status = loop {
        if start.elapsed() > timeout {
            let _ = child.kill();
            return Err("code execution timed out (30s)".into());
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
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

fn execute_rust_code(
    _caller: &mut wasmtime::Caller<'_, HostContext>,
    code: &str,
    ws_root: &std::path::Path,
) -> Result<serde_json::Value, String> {
    use std::io::Read;
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
    let timeout = std::time::Duration::from_secs(30);
    let start = std::time::Instant::now();
    let status = loop {
        if start.elapsed() > timeout {
            let _ = child.kill();
            return Err("rustc compilation timed out (30s)".into());
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
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
    let start = std::time::Instant::now();
    let run_status = loop {
        if start.elapsed() > timeout {
            let _ = run_child.kill();
            return Err("binary execution timed out (30s)".into());
        }
        match run_child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
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

pub fn register_host_functions(
    linker: &mut wasmtime::Linker<HostContext>,
) -> Result<(), AgentError> {
    linker
        .func_wrap("host", "triviumdb_search", host_triviumdb_search)
        .map_err(|e| AgentError::Script(format!("link triviumdb_search: {e}")))?;
    linker
        .func_wrap("host", "triviumdb_upsert", host_triviumdb_upsert)
        .map_err(|e| AgentError::Script(format!("link triviumdb_upsert: {e}")))?;
    linker
        .func_wrap("host", "duckdb_query", host_duckdb_query)
        .map_err(|e| AgentError::Script(format!("link duckdb_query: {e}")))?;
    linker
        .func_wrap("host", "duckdb_execute", host_duckdb_execute)
        .map_err(|e| AgentError::Script(format!("link duckdb_execute: {e}")))?;

    linker
        .func_wrap("host", "file_read", host_file_read)
        .map_err(|e| AgentError::Script(format!("link file_read: {e}")))?;
    linker
        .func_wrap("host", "file_write", host_file_write)
        .map_err(|e| AgentError::Script(format!("link file_write: {e}")))?;
    linker
        .func_wrap("host", "file_list", host_file_list)
        .map_err(|e| AgentError::Script(format!("link file_list: {e}")))?;
    linker
        .func_wrap("host", "file_delete", host_file_delete)
        .map_err(|e| AgentError::Script(format!("link file_delete: {e}")))?;
    linker
        .func_wrap("host", "file_move", host_file_move)
        .map_err(|e| AgentError::Script(format!("link file_move: {e}")))?;
    linker
        .func_wrap("host", "text_grep", host_text_grep)
        .map_err(|e| AgentError::Script(format!("link text_grep: {e}")))?;
    linker
        .func_wrap("host", "shell_exec", host_shell_exec)
        .map_err(|e| AgentError::Script(format!("link shell_exec: {e}")))?;
    linker
        .func_wrap("host", "file_chunk_read", host_file_chunk_read)
        .map_err(|e| AgentError::Script(format!("link file_chunk_read: {e}")))?;
    linker
        .func_wrap("host", "code_exec", host_code_exec)
        .map_err(|e| AgentError::Script(format!("link code_exec: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::triviumdb::TriviumDb;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    const DIM: usize = 4;

    fn make_host_ctx(
        db_path: &std::path::Path,
        memory_read: bool,
        memory_write: bool,
    ) -> HostContext {
        let triviumdb = Arc::new(Mutex::new(TriviumDb::open(db_path, DIM).unwrap()));
        HostContext {
            duckdb: None,
            triviumdb: Some(triviumdb),
            permission: crate::logic::script::host_context::PermissionSnapshot {
                memory_read,
                memory_write,
                ..Default::default()
            },
            budget: crate::logic::script::host_context::BudgetSnapshot {
                max_k: 10,
                ..Default::default()
            },
        }
    }

    #[test]
    fn search_returns_ids() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("t.trivium");
        {
            let mut db = TriviumDb::open(&db_path, DIM).unwrap();
            let v1 = vec![1.0f32, 0.0, 0.0, 0.0];
            let v2 = vec![0.9f32, 0.1, 0.0, 0.0];
            db.db_mut()
                .insert(
                    &v1,
                    serde_json::json!({"_memory_type": "test", "label": "a"}),
                )
                .unwrap();
            db.db_mut()
                .insert(
                    &v2,
                    serde_json::json!({"_memory_type": "test", "label": "b"}),
                )
                .unwrap();
        }

        let host_ctx = make_host_ctx(&db_path, true, false);

        let wat = r#"
            (module
              (import "host" "triviumdb_search" (func $search
                (param i32 i32 i32 i32 i32) (result i32 i32)))
              (memory (export "memory") 2)
              (func (export "run") (param i32 i32) (result i32)
                (call $search
                  (i32.const 0)
                  (local.get 1)
                  (i32.const 5)
                  (i32.const 0)
                  (i32.const 0))
                (drop) (drop)
                (i32.const 0))
              (func (export "output_len") (result i32) (i32.const 0)))
        "#;

        let query_vec = [1.0f32, 0.0, 0.0, 0.0];
        let qbytes: Vec<u8> = query_vec.iter().flat_map(|f| f.to_le_bytes()).collect();

        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut store = wasmtime::Store::new(&engine, host_ctx);
        store.set_fuel(1_000_000).unwrap();
        let mut linker = wasmtime::Linker::<HostContext>::new(&engine);
        register_host_functions(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        memory.write(&mut store, 0, &qbytes).unwrap();

        let run_func = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .unwrap();
        let result = run_func.call(&mut store, (0, 16));
        assert!(result.is_ok(), "search should succeed: {:?}", result.err());
    }

    #[test]
    fn search_permission_denied() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("t.trivium");
        let host_ctx = make_host_ctx(&db_path, false, false);

        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let wat = r#"
            (module
              (import "host" "triviumdb_search" (func $search
                (param i32 i32 i32 i32 i32) (result i32 i32)))
              (memory (export "memory") 1)
              (func (export "run") (param i32 i32) (result i32)
                (call $search (i32.const 0) (i32.const 16) (i32.const 1) (i32.const 0) (i32.const 0))
                (drop) (drop)
                (i32.const 0))
              (func (export "output_len") (result i32) (i32.const 0)))
        "#;
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut store = wasmtime::Store::new(&engine, host_ctx);
        store.set_fuel(1_000_000).unwrap();
        let mut linker = wasmtime::Linker::<HostContext>::new(&engine);
        register_host_functions(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let run_func = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .unwrap();
        let result = run_func.call(&mut store, (0, 16));
        assert!(result.is_err(), "search should fail when memory_read=false");
    }

    #[test]
    fn search_k_capped_by_budget() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cap.trivium");
        let triviumdb = Arc::new(Mutex::new(TriviumDb::open(&db_path, DIM).unwrap()));
        {
            let mut db = triviumdb.lock().unwrap();
            for i in 0..20 {
                let v = vec![(i as f32) / 20.0, 0.0, 0.0, 0.0];
                db.db_mut()
                    .insert(&v, serde_json::json!({"_memory_type": "test", "idx": i}))
                    .unwrap();
            }
        }

        let host_ctx = HostContext {
            duckdb: None,
            triviumdb: Some(triviumdb),
            permission: crate::logic::script::host_context::PermissionSnapshot {
                memory_read: true,
                ..Default::default()
            },
            budget: crate::logic::script::host_context::BudgetSnapshot {
                max_k: 10,
                ..Default::default()
            },
        };

        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let wat = r#"
            (module
              (import "host" "triviumdb_search" (func $search
                (param i32 i32 i32 i32 i32) (result i32 i32)))
              (memory (export "memory") 2)
              (func (export "run") (param i32 i32) (result i32)
                (call $search
                  (i32.const 0)
                  (local.get 1)
                  (i32.const 999999)
                  (i32.const 0)
                  (i32.const 0))
                (drop) (drop)
                (i32.const 0))
              (func (export "output_len") (result i32) (i32.const 0)))
        "#;
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut store = wasmtime::Store::new(&engine, host_ctx);
        store.set_fuel(1_000_000).unwrap();
        let mut linker = wasmtime::Linker::<HostContext>::new(&engine);
        register_host_functions(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let query_vec = [0.5f32, 0.0, 0.0, 0.0];
        let qbytes: Vec<u8> = query_vec.iter().flat_map(|f| f.to_le_bytes()).collect();
        memory.write(&mut store, 0, &qbytes).unwrap();

        let run_func = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .unwrap();
        let result = run_func.call(&mut store, (0, 16));
        assert!(
            result.is_ok(),
            "k capping should not cause error: {:?}",
            result.err()
        );
    }

    #[test]
    fn upsert_returns_node_id() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("t.trivium");
        let host_ctx = make_host_ctx(&db_path, false, true);

        let payload = serde_json::json!({"label": "test-insert"});
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let payload_len = payload_bytes.len();

        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();

        let wat = format!(
            r#"(module
              (import "host" "triviumdb_upsert" (func $upsert
                (param i32 i32 i32 i32 i32 i32) (result i64)))
              (memory (export "memory") 2)
              (func (export "run") (param i32 i32) (result i32)
                (call $upsert
                  (i32.const 0)
                  (i32.const 8)
                  (i32.const 64)
                  (i32.const {payload_len})
                  (i32.const 128)
                  (i32.const 16))
                (drop)
                (i32.const 0))
              (func (export "output_len") (result i32) (i32.const 0)))
        "#
        );
        let module = wasmtime::Module::new(&engine, &wat).unwrap();
        let mut store = wasmtime::Store::new(&engine, host_ctx);
        store.set_fuel(1_000_000).unwrap();
        let mut linker = wasmtime::Linker::<HostContext>::new(&engine);
        register_host_functions(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();

        memory.write(&mut store, 0, b"test_mem").unwrap();

        memory.write(&mut store, 64, &payload_bytes).unwrap();

        let vec_data = [0.5f32, 0.0, 0.0, 0.0];
        let vec_bytes: Vec<u8> = vec_data.iter().flat_map(|f| f.to_le_bytes()).collect();
        memory.write(&mut store, 128, &vec_bytes).unwrap();

        let run_func = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .unwrap();
        let result = run_func.call(&mut store, (0, 16));
        assert!(result.is_ok(), "upsert should succeed: {:?}", result.err());
    }

    #[test]
    fn upsert_permission_denied() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("t.trivium");
        let host_ctx = make_host_ctx(&db_path, true, false);

        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let wat = r#"
            (module
              (import "host" "triviumdb_upsert" (func $upsert
                (param i32 i32 i32 i32 i32 i32) (result i64)))
              (memory (export "memory") 1)
              (func (export "run") (param i32 i32) (result i32)
                (call $upsert (i32.const 0)(i32.const 1)(i32.const 0)(i32.const 1)(i32.const 0)(i32.const 1))
                (drop)
                (i32.const 0))
              (func (export "output_len") (result i32) (i32.const 0)))
        "#;
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut store = wasmtime::Store::new(&engine, host_ctx);
        store.set_fuel(1_000_000).unwrap();
        let mut linker = wasmtime::Linker::<HostContext>::new(&engine);
        register_host_functions(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let run_func = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .unwrap();
        let result = run_func.call(&mut store, (0, 0));
        assert!(
            result.is_err(),
            "upsert should fail when memory_write=false"
        );
    }

    #[test]
    fn upsert_invalid_json_rejected() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("t.trivium");
        {
            let mut db = TriviumDb::open(&db_path, DIM).unwrap();
            db.db_mut()
                .insert(&[0.0f32; 4], serde_json::json!({"dummy": true}))
                .unwrap();
        }
        let host_ctx = make_host_ctx(&db_path, false, true);

        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let wat = r#"
            (module
              (import "host" "triviumdb_upsert" (func $upsert
                (param i32 i32 i32 i32 i32 i32) (result i64)))
              (memory (export "memory") 2)
              (func (export "run") (param i32 i32) (result i32)
                (call $upsert
                  (i32.const 0)(i32.const 4)     ;; mem_type "test"
                  (i32.const 64)(i32.const 7)    ;; invalid JSON "not-json"
                  (i32.const 128)(i32.const 16)) ;; vec
                (drop)
                (i32.const 0))
              (func (export "output_len") (result i32) (i32.const 0)))
        "#;
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut store = wasmtime::Store::new(&engine, host_ctx);
        store.set_fuel(1_000_000).unwrap();
        let mut linker = wasmtime::Linker::<HostContext>::new(&engine);
        register_host_functions(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        memory.write(&mut store, 0, b"test").unwrap();
        memory.write(&mut store, 64, b"not-json").unwrap();
        memory
            .write(
                &mut store,
                128,
                &[0.5f32.to_le_bytes().to_vec(), vec![0u8; 12]].concat(),
            )
            .unwrap();

        let run_func = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .unwrap();
        let result = run_func.call(&mut store, (0, 0));
        assert!(
            result.is_err(),
            "upsert with invalid JSON should fail: {:?}",
            result
        );
    }
}
