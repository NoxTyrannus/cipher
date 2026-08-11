use crate::common::{AgentError, Result};
use crate::data::permissions::secure_existing_file;
use std::fs;
use std::path::Path;

pub const WASM_SHELL_EXEC: &str = include_str!("../../data/wasm/shell_exec.wat");
pub const WASM_FILE_READ: &str = include_str!("../../data/wasm/file_read.wat");
pub const WASM_FILE_WRITE: &str = include_str!("../../data/wasm/file_write.wat");
pub const WASM_FILE_LIST: &str = include_str!("../../data/wasm/file_list.wat");
pub const WASM_FILE_DELETE: &str = include_str!("../../data/wasm/file_delete.wat");
pub const WASM_FILE_MOVE: &str = include_str!("../../data/wasm/file_move.wat");
pub const WASM_FILE_CHUNK_READ: &str = include_str!("../../data/wasm/file_chunk_read.wat");
pub const WASM_TEXT_GREP: &str = include_str!("../../data/wasm/text_grep.wat");
pub const WASM_CODE_EXEC: &str = include_str!("../../data/wasm/code_exec.wat");

const WASM_MODULES: &[(&str, &str)] = &[
    ("shell_exec.wat", WASM_SHELL_EXEC),
    ("file_read.wat", WASM_FILE_READ),
    ("file_write.wat", WASM_FILE_WRITE),
    ("file_list.wat", WASM_FILE_LIST),
    ("file_delete.wat", WASM_FILE_DELETE),
    ("file_move.wat", WASM_FILE_MOVE),
    ("file_chunk_read.wat", WASM_FILE_CHUNK_READ),
    ("text_grep.wat", WASM_TEXT_GREP),
    ("code_exec.wat", WASM_CODE_EXEC),
];

const WASM_MARKER: &str = ".wasm_installed";

pub fn ensure_default_wasm_modules(data_dir: &Path) -> Result<()> {
    let wasm_dir = data_dir.join("wasm");
    let marker = wasm_dir.join(WASM_MARKER);

    if marker.exists() {
        return Ok(());
    }

    for (name, content) in WASM_MODULES {
        let path = wasm_dir.join(name);
        if !path.exists() {
            fs::create_dir_all(&wasm_dir)
                .map_err(|e| AgentError::Io(format!("create wasm dir: {e}")))?;
            fs::write(&path, content)
                .map_err(|e| AgentError::Io(format!("write wasm module {name}: {e}")))?;
            secure_existing_file(&path)?;
        }
    }

    fs::write(&marker, "1").map_err(|e| AgentError::Io(format!("write wasm marker: {e}")))?;
    secure_existing_file(&marker)?;

    tracing::info!("factory: installed {} wasm modules", WASM_MODULES.len());
    Ok(())
}

pub fn default_shell_capability_id() -> &'static str {
    if cfg!(target_os = "windows") {
        "powershell.exec"
    } else {
        "shell.exec"
    }
}

pub fn default_shell_capability_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "Execute PowerShell"
    } else {
        "Execute Shell"
    }
}

pub fn default_shell_capability_ids() -> Vec<String> {
    let mut ids = vec![
        "file.read".to_string(),
        "file.write".to_string(),
        "file.list".to_string(),
        "file.delete".to_string(),
        "file.move".to_string(),
        "text.grep".to_string(),
    ];
    ids.push(default_shell_capability_id().to_string());
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ensure_default_wasm_modules_writes_files() {
        let dir = tempdir().unwrap();
        ensure_default_wasm_modules(dir.path()).unwrap();

        let wasm_dir = dir.path().join("wasm");
        assert!(wasm_dir.join("shell_exec.wat").exists());
        assert!(wasm_dir.join("file_read.wat").exists());
        assert!(wasm_dir.join("file_write.wat").exists());
        assert!(wasm_dir.join("file_list.wat").exists());
        assert!(wasm_dir.join(".wasm_installed").exists());
    }

    #[test]
    fn ensure_default_wasm_modules_idempotent() {
        let dir = tempdir().unwrap();
        ensure_default_wasm_modules(dir.path()).unwrap();
        ensure_default_wasm_modules(dir.path()).unwrap();
        let wasm_dir = dir.path().join("wasm");
        assert!(wasm_dir.join(".wasm_installed").exists());
    }

    #[test]
    fn default_shell_capability_id_matches_platform() {
        let id = default_shell_capability_id();
        if cfg!(target_os = "windows") {
            assert_eq!(id, "powershell.exec");
        } else {
            assert_eq!(id, "shell.exec");
        }
    }

    #[test]
    fn default_shell_capability_ids_includes_shell() {
        let ids = default_shell_capability_ids();
        if cfg!(target_os = "windows") {
            assert!(ids.contains(&"powershell.exec".to_string()));
        } else {
            assert!(ids.contains(&"shell.exec".to_string()));
        }
    }
}
