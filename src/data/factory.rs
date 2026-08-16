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
        "path.exists".to_string(),
        "file.glob".to_string(),
        "json.validate".to_string(),
        "file.read".to_string(),
        "file.write".to_string(),
        "file.list".to_string(),
        "file.delete".to_string(),
        "file.move".to_string(),
        "file.chunk_read".to_string(),
        "text.grep".to_string(),
        "code.exec".to_string(),
        "capability.import".to_string(),
    ];
    ids.push(default_shell_capability_id().to_string());
    ids
}
