use crate::common::{AgentError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const WORKSPACE_SCHEMA_VERSION: u32 = 1;

const WORKSPACE_FILE: &str = "workspaces.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRow {
    pub id: String,
    pub name: String,
    pub path: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDocument {
    schema_version: u32,
    workspaces: Vec<WorkspaceRow>,
}

impl Default for WorkspaceDocument {
    fn default() -> Self {
        Self {
            schema_version: WORKSPACE_SCHEMA_VERSION,
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    data_root: PathBuf,
    file_path: PathBuf,
}

impl WorkspaceStore {
    pub fn open(data_root: &Path) -> Result<Self> {
        secure_data_root(data_root)?;
        let store = Self {
            data_root: data_root.to_path_buf(),
            file_path: data_root.join(WORKSPACE_FILE),
        };
        store.load_document()?;
        Ok(store)
    }

    pub fn list(&self) -> Result<Vec<WorkspaceRow>> {
        Ok(self.load_document()?.workspaces)
    }

    pub fn initialize(&self) -> Result<()> {
        if self.file_path.exists() {
            self.load_document()?;
            return Ok(());
        }
        self.persist(&WorkspaceDocument::default())
    }

    pub fn upsert(&self, row: WorkspaceRow) -> Result<()> {
        validate_row(&row)?;
        let mut document = self.load_document()?;
        match document
            .workspaces
            .iter_mut()
            .find(|existing| existing.id == row.id)
        {
            Some(existing) if *existing == row => return Ok(()),
            Some(existing) => *existing = row,
            None => document.workspaces.push(row),
        }
        normalize_and_validate(&mut document.workspaces)?;
        self.persist(&document)
    }

    pub fn set_default(&self, id: &str) -> Result<()> {
        let mut document = self.load_document()?;
        if !document.workspaces.iter().any(|row| row.id == id) {
            return Err(AgentError::NotFound(
                "workspace id is not present in the workspace store".to_string(),
            ));
        }

        let mut changed = false;
        for row in &mut document.workspaces {
            let should_be_default = row.id == id;
            changed |= row.is_default != should_be_default;
            row.is_default = should_be_default;
        }
        if !changed {
            return Ok(());
        }
        normalize_and_validate(&mut document.workspaces)?;
        self.persist(&document)
    }

    pub fn seed_if_empty(&self, row: WorkspaceRow) -> Result<bool> {
        let mut document = self.load_document()?;
        if !document.workspaces.is_empty() {
            return Ok(false);
        }
        validate_row(&row)?;
        document.workspaces.push(row);
        normalize_and_validate(&mut document.workspaces)?;
        self.persist(&document)?;
        Ok(true)
    }

    pub fn import_if_empty(&self, mut rows: Vec<WorkspaceRow>) -> Result<bool> {
        normalize_and_validate(&mut rows)?;
        let current = self.load_document()?;
        if current.workspaces == rows {
            return Ok(false);
        }
        if !current.workspaces.is_empty() {
            return Err(workspace_error(
                "refusing to replace an initialized workspace store",
            ));
        }
        if rows.is_empty() {
            return Ok(false);
        }

        self.persist(&WorkspaceDocument {
            schema_version: WORKSPACE_SCHEMA_VERSION,
            workspaces: rows,
        })?;
        Ok(true)
    }

    fn load_document(&self) -> Result<WorkspaceDocument> {
        secure_data_root(&self.data_root)?;
        match fs::symlink_metadata(&self.file_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(workspace_error("workspace file cannot be a symlink"));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(workspace_error(
                    "workspace storage path is not a regular file",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(WorkspaceDocument::default());
            }
            Err(error) => {
                return Err(workspace_error(format!(
                    "cannot inspect workspace file: {error}"
                )));
            }
        }
        secure_file_without_path(&self.file_path)?;

        let file = File::open(&self.file_path)
            .map_err(|error| workspace_error(format!("cannot open workspace file: {error}")))?;
        let mut document: WorkspaceDocument = serde_json::from_reader(BufReader::new(file))
            .map_err(|error| workspace_error(format!("cannot parse workspace file: {error}")))?;

        let final_metadata = fs::symlink_metadata(&self.file_path).map_err(|error| {
            workspace_error(format!("cannot re-inspect workspace file: {error}"))
        })?;
        if final_metadata.file_type().is_symlink() || !final_metadata.is_file() {
            return Err(workspace_error("workspace file changed type while loading"));
        }
        if document.schema_version != WORKSPACE_SCHEMA_VERSION {
            return Err(workspace_error(
                "unsupported workspace store schema version",
            ));
        }
        normalize_and_validate(&mut document.workspaces)?;
        Ok(document)
    }

    fn persist(&self, document: &WorkspaceDocument) -> Result<()> {
        secure_data_root(&self.data_root)?;
        let mut normalized = document.clone();
        if normalized.schema_version != WORKSPACE_SCHEMA_VERSION {
            return Err(workspace_error(
                "cannot write unsupported workspace schema version",
            ));
        }
        normalize_and_validate(&mut normalized.workspaces)?;
        validate_publish_target(&self.file_path)?;

        let bytes = serde_json::to_vec_pretty(&normalized).map_err(|error| {
            workspace_error(format!("cannot serialize workspace file: {error}"))
        })?;
        let temporary_path = self
            .data_root
            .join(format!(".workspaces.{}.tmp", Uuid::new_v4().simple()));
        let write_result = (|| -> Result<()> {
            let mut file = create_private_file(&temporary_path).map_err(|error| {
                workspace_error(format!("cannot create workspace temporary file: {error}"))
            })?;
            file.write_all(&bytes)
                .and_then(|()| file.write_all(b"\n"))
                .map_err(|error| {
                    workspace_error(format!("cannot write workspace temporary file: {error}"))
                })?;
            file.sync_all().map_err(|error| {
                workspace_error(format!("cannot sync workspace temporary file: {error}"))
            })?;
            drop(file);
            secure_file_without_path(&temporary_path)?;

            validate_publish_target(&self.file_path)?;
            fs::rename(&temporary_path, &self.file_path).map_err(|error| {
                workspace_error(format!("cannot publish workspace file atomically: {error}"))
            })?;
            secure_file_without_path(&self.file_path)?;
            sync_directory(&self.data_root)
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        write_result
    }
}

fn validate_row(row: &WorkspaceRow) -> Result<()> {
    if row.id.trim().is_empty() {
        return Err(workspace_error("workspace id cannot be empty"));
    }
    if row.name.trim().is_empty() {
        return Err(workspace_error("workspace name cannot be empty"));
    }
    if row.path.trim().is_empty() {
        return Err(workspace_error("workspace path cannot be empty"));
    }
    Ok(())
}

fn normalize_and_validate(rows: &mut [WorkspaceRow]) -> Result<()> {
    rows.sort_by(|left, right| left.id.cmp(&right.id));
    let mut ids = HashSet::with_capacity(rows.len());
    let mut paths = HashSet::with_capacity(rows.len());
    let mut default_count = 0_usize;
    for row in rows {
        validate_row(row)?;
        if !ids.insert(row.id.as_str()) {
            return Err(workspace_error("workspace ids must be unique"));
        }
        if !paths.insert(row.path.as_str()) {
            return Err(workspace_error("workspace paths must be unique"));
        }
        default_count += usize::from(row.is_default);
    }
    if default_count > 1 {
        return Err(workspace_error(
            "workspace store cannot contain multiple defaults",
        ));
    }
    Ok(())
}

fn secure_data_root(path: &Path) -> Result<()> {
    crate::data::permissions::ensure_private_directory(path)
        .map_err(|_| workspace_error("cannot create or secure workspace data root"))
}

fn secure_file_without_path(path: &Path) -> Result<()> {
    crate::data::permissions::secure_existing_file(path)
        .map_err(|_| workspace_error("cannot secure workspace file"))
}

fn validate_publish_target(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(workspace_error("workspace file cannot be a symlink"))
        }
        Ok(metadata) if !metadata.is_file() => Err(workspace_error(
            "workspace storage path is not a regular file",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(workspace_error(format!(
            "cannot inspect workspace publish target: {error}"
        ))),
    }
}

fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| workspace_error(format!("cannot sync workspace directory: {error}")))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn workspace_error(message: impl Into<String>) -> AgentError {
    AgentError::Bootstrap(format!("workspace store: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_root() -> (tempfile::TempDir, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        (temporary, root)
    }

    fn row(id: &str, path: &str, is_default: bool) -> WorkspaceRow {
        WorkspaceRow {
            id: id.to_string(),
            name: format!("workspace-{id}"),
            path: path.to_string(),
            is_default,
        }
    }

    #[test]
    fn crud_is_sorted_and_set_default_is_exclusive() {
        let (_temporary, root) = store_root();
        let store = WorkspaceStore::open(&root).unwrap();
        assert!(store.list().unwrap().is_empty());

        store.upsert(row("b", "/project/b", false)).unwrap();
        store.upsert(row("a", "/project/a", false)).unwrap();
        assert_eq!(
            store
                .list()
                .unwrap()
                .iter()
                .map(|workspace| workspace.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );

        let mut updated = row("a", "/project/a", false);
        updated.name = "renamed".to_string();
        store.upsert(updated).unwrap();
        store.set_default("a").unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows[0].name, "renamed");
        assert!(rows[0].is_default);
        assert!(!rows[1].is_default);

        match store.set_default("missing") {
            Err(AgentError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }

        let document: WorkspaceDocument =
            serde_json::from_slice(&fs::read(root.join(WORKSPACE_FILE)).unwrap()).unwrap();
        assert_eq!(document.schema_version, WORKSPACE_SCHEMA_VERSION);
        assert_eq!(document.workspaces[0].id, "a");
        assert_eq!(document.workspaces[1].id, "b");
    }

    #[test]
    fn seed_if_empty_is_idempotent() {
        let (_temporary, root) = store_root();
        let store = WorkspaceStore::open(&root).unwrap();
        assert!(store
            .seed_if_empty(row("first", "/project/first", true))
            .unwrap());
        assert!(!store
            .seed_if_empty(row("second", "/project/second", true))
            .unwrap());

        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "first");
    }

    #[test]
    fn import_if_empty_is_sorted_idempotent_and_non_destructive() {
        let (_temporary, root) = store_root();
        let store = WorkspaceStore::open(&root).unwrap();
        let imported = vec![row("b", "/project/b", false), row("a", "/project/a", true)];
        assert!(store.import_if_empty(imported.clone()).unwrap());

        let mut reversed = imported.clone();
        reversed.reverse();
        assert!(!store.import_if_empty(reversed).unwrap());
        assert!(store
            .import_if_empty(vec![row("other", "/project/other", true)])
            .is_err());
        assert_eq!(store.list().unwrap()[0].id, "a");
    }

    #[test]
    fn duplicate_ids_paths_and_defaults_are_rejected() {
        let (_temporary, root) = store_root();
        let store = WorkspaceStore::open(&root).unwrap();
        assert!(store
            .import_if_empty(vec![
                row("same", "/project/a", false),
                row("same", "/project/b", false),
            ])
            .is_err());
        assert!(store
            .import_if_empty(vec![
                row("a", "/project/same", false),
                row("b", "/project/same", false),
            ])
            .is_err());
        assert!(store
            .import_if_empty(vec![
                row("a", "/project/a", true),
                row("b", "/project/b", true),
            ])
            .is_err());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn invalid_existing_document_is_rejected_on_open() {
        let (_temporary, root) = store_root();
        fs::create_dir(&root).unwrap();
        let invalid = WorkspaceDocument {
            schema_version: WORKSPACE_SCHEMA_VERSION,
            workspaces: vec![row("a", "/project/a", true), row("b", "/project/b", true)],
        };
        fs::write(
            root.join(WORKSPACE_FILE),
            serde_json::to_vec(&invalid).unwrap(),
        )
        .unwrap();
        assert!(WorkspaceStore::open(&root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn open_and_write_enforce_0700_and_0600() {
        use std::os::unix::fs::PermissionsExt;

        let (_temporary, root) = store_root();
        let store = WorkspaceStore::open(&root).unwrap();
        store.upsert(row("a", "/project/a", true)).unwrap();
        let file = root.join(WORKSPACE_FILE);
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            0o600
        );

        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let reopened = WorkspaceStore::open(&root).unwrap();
        assert_eq!(reopened.list().unwrap().len(), 1);
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert!(fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[cfg(unix)]
    #[test]
    fn root_file_symlink_and_special_file_are_rejected() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let temporary = tempfile::tempdir().unwrap();
        let real_root = temporary.path().join("real-root");
        fs::create_dir(&real_root).unwrap();
        let root_link = temporary.path().join("root-link");
        symlink(&real_root, &root_link).unwrap();
        assert!(WorkspaceStore::open(&root_link).is_err());

        let file_root = temporary.path().join("file-root");
        fs::create_dir(&file_root).unwrap();
        let target = temporary.path().join("target.json");
        fs::write(&target, b"{}").unwrap();
        symlink(&target, file_root.join(WORKSPACE_FILE)).unwrap();
        assert!(WorkspaceStore::open(&file_root).is_err());

        let special_root = temporary.path().join("special-root");
        fs::create_dir(&special_root).unwrap();
        let _socket = UnixListener::bind(special_root.join(WORKSPACE_FILE)).unwrap();
        assert!(WorkspaceStore::open(&special_root).is_err());
    }
}
