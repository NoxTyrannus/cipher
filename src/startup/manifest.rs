use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const APP_VERSION: &str = "0.3.1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptFileState {
    pub default_sha256: String,
    pub last_auto_version: String,
    pub user_modified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub app_version: String,
    pub created_at: String,
    pub files: HashMap<String, PromptFileState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeChoice {
    Backup,
    Overwrite,
    Cancel,
}

#[derive(Debug, Default)]
pub struct UpgradeReport {
    pub upgraded: Vec<String>,
    pub backed_up: Vec<String>,
    pub overwritten: Vec<String>,
    pub skipped: Vec<String>,
}

pub fn sha256_bytes(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

pub fn compute_file_sha256(path: &Path) -> std::io::Result<String> {
    let data = fs::read(path)?;
    Ok(sha256_bytes(&data))
}

pub fn load(manifest_path: &Path) -> Option<Manifest> {
    let content = fs::read_to_string(manifest_path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn save(manifest: &Manifest, manifest_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(manifest_path, content)
}

pub fn ensure_fresh_install(
    unified_root: &Path,
    defaults: &[(&str, &str)],
) -> std::io::Result<Manifest> {
    let prompts_dir = unified_root.join("prompts");
    fs::create_dir_all(&prompts_dir)?;
    fs::create_dir_all(unified_root.join("backups"))?;
    fs::create_dir_all(unified_root.join("data"))?;

    let mut files = HashMap::new();
    for (name, content) in defaults {
        let path = prompts_dir.join(name);
        fs::write(&path, content)?;
        files.insert(
            format!("prompts/{name}"),
            PromptFileState {
                default_sha256: sha256_bytes(content.as_bytes()),
                last_auto_version: APP_VERSION.to_string(),
                user_modified: false,
            },
        );
    }

    let manifest = Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        app_version: APP_VERSION.to_string(),
        created_at: crate::common::UtcTimestamp::now().to_string(),
        files,
    };
    save(&manifest, &unified_root.join("manifest.json"))?;
    Ok(manifest)
}

pub fn upgrade_prompts(
    unified_root: &Path,
    defaults: &[(&str, &str)],
    mut decide: impl FnMut(&str) -> UpgradeChoice,
) -> std::io::Result<UpgradeReport> {
    let manifest_path = unified_root.join("manifest.json");
    let mut manifest = load(&manifest_path).unwrap_or_else(|| {
        // 没有 manifest 时按全新安装处理（测试版不迁移旧路径）。
        ensure_fresh_install(unified_root, defaults).expect("fresh install failed")
    });

    let prompts_dir = unified_root.join("prompts");
    fs::create_dir_all(&prompts_dir)?;
    let mut report = UpgradeReport::default();
    let timestamp = crate::common::UtcTimestamp::now()
        .to_string()
        .replace(':', "-");

    for (name, content) in defaults {
        let path = prompts_dir.join(name);
        let key = format!("prompts/{name}");
        let default_hash = sha256_bytes(content.as_bytes());
        let state = manifest.files.get(&key);
        let current_hash = if path.exists() {
            Some(compute_file_sha256(&path).unwrap_or_default())
        } else {
            None
        };

        let user_modified = match state {
            Some(s) => {
                s.user_modified || current_hash.as_deref() != Some(s.default_sha256.as_str())
            }
            None => current_hash.is_some(),
        };

        if !user_modified {
            fs::write(&path, content)?;
            manifest.files.insert(
                key.clone(),
                PromptFileState {
                    default_sha256: default_hash.clone(),
                    last_auto_version: APP_VERSION.to_string(),
                    user_modified: false,
                },
            );
            report.upgraded.push(name.to_string());
            continue;
        }

        match decide(name) {
            UpgradeChoice::Backup => {
                let backup_dir = unified_root.join("backups").join(&timestamp);
                fs::create_dir_all(&backup_dir)?;
                if path.exists() {
                    fs::copy(&path, backup_dir.join(name))?;
                }
                fs::write(&path, content)?;
                manifest.files.insert(
                    key.clone(),
                    PromptFileState {
                        default_sha256: default_hash.clone(),
                        last_auto_version: APP_VERSION.to_string(),
                        user_modified: false,
                    },
                );
                report.backed_up.push(name.to_string());
            }
            UpgradeChoice::Overwrite => {
                fs::write(&path, content)?;
                manifest.files.insert(
                    key.clone(),
                    PromptFileState {
                        default_sha256: default_hash.clone(),
                        last_auto_version: APP_VERSION.to_string(),
                        user_modified: false,
                    },
                );
                report.overwritten.push(name.to_string());
            }
            UpgradeChoice::Cancel => {
                manifest.files.insert(
                    key.clone(),
                    PromptFileState {
                        default_sha256: default_hash.clone(),
                        last_auto_version: APP_VERSION.to_string(),
                        user_modified: true,
                    },
                );
                report.skipped.push(name.to_string());
            }
        }
    }

    save(&manifest, &manifest_path)?;
    Ok(report)
}

/// 自监测用：重新扫描磁盘，更新 user_modified；返回是否有变化。
pub fn refresh_user_modified(
    unified_root: &Path,
    defaults: &[(&str, &str)],
) -> std::io::Result<bool> {
    let manifest_path = unified_root.join("manifest.json");
    let mut manifest = match load(&manifest_path) {
        Some(m) => m,
        None => return Ok(false),
    };
    let prompts_dir = unified_root.join("prompts");
    let mut changed = false;
    for (name, _content) in defaults {
        let path = prompts_dir.join(name);
        let current_hash = if path.exists() {
            Some(compute_file_sha256(&path).unwrap_or_default())
        } else {
            None
        };
        let key = format!("prompts/{name}");
        let user_modified = current_hash.as_deref()
            != Some(
                manifest
                    .files
                    .get(&key)
                    .map(|s| s.default_sha256.as_str())
                    .unwrap_or(""),
            );
        if let Some(state) = manifest.files.get_mut(&key) {
            if state.user_modified != user_modified {
                state.user_modified = user_modified;
                changed = true;
            }
        }
    }
    if changed {
        save(&manifest, &manifest_path)?;
    }
    Ok(changed)
}

pub fn unified_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cipher")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_creates_manifest_and_prompts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let defaults = [("system.md", "hello"), ("mode_unni.md", "unni")];
        let m = ensure_fresh_install(root, &defaults).unwrap();
        assert!(root.join("manifest.json").exists());
        assert!(root.join("prompts/system.md").exists());
        assert_eq!(m.files.len(), 2);
        assert!(!m.files["prompts/system.md"].user_modified);
    }

    #[test]
    fn user_modified_file_can_be_backed_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let defaults = [("system.md", "new-default")];
        ensure_fresh_install(root, &defaults).unwrap();
        fs::write(root.join("prompts/system.md"), "user-custom").unwrap();

        let report = upgrade_prompts(root, &defaults, |_| UpgradeChoice::Backup).unwrap();
        assert_eq!(report.backed_up.len(), 1);
        assert_eq!(
            fs::read_to_string(root.join("prompts/system.md")).unwrap(),
            "new-default"
        );
        assert!(root.join("backups").read_dir().unwrap().next().is_some());
    }

    #[test]
    fn user_modified_file_can_be_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let defaults = [("system.md", "new-default")];
        ensure_fresh_install(root, &defaults).unwrap();
        fs::write(root.join("prompts/system.md"), "user-custom").unwrap();

        let report = upgrade_prompts(root, &defaults, |_| UpgradeChoice::Cancel).unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            fs::read_to_string(root.join("prompts/system.md")).unwrap(),
            "user-custom"
        );
    }
}
