use crate::common::{AgentError, Result};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use std::path::Path;

pub mod collections;
pub use collections::{
    insert_attention_doc_node, insert_attention_table_node, insert_cognitive_node,
    insert_experience_node, insert_preference_node, insert_raw_file_node,
};

pub const DEFAULT_DIM: usize = 128;

const TRIVIUM_FILE_SUFFIXES: &[&str] = &[
    "",
    ".lock",
    ".wal",
    ".vec",
    ".flush_ok",
    ".quiver",
    ".tmp",
    ".vec.tmp",
    ".flush_ok.tmp",
    ".quiver.tmp",
];

pub struct TriviumDb {
    db: Option<triviumdb::Database<f32>>,
    path: String,
}

impl TriviumDb {
    pub fn open(path: &Path, dim: usize) -> Result<Self> {
        if let Some(parent) = path.parent() {
            ensure_private_directory(parent)?;
        }
        let path_str = path.to_string_lossy().to_string();
        secure_trivium_files(&path_str)?;
        let db = match triviumdb::Database::open(&path_str, dim) {
            Ok(db) => db,
            Err(error) => {
                let open_error =
                    AgentError::Bootstrap(format!("TriviumDB open {}: {}", path.display(), error));
                return Err(merge_permission_error(
                    open_error,
                    secure_trivium_files(&path_str),
                ));
            }
        };
        if let Err(error) = secure_trivium_files(&path_str) {
            drop(db);
            return Err(merge_permission_error(
                error,
                secure_trivium_files(&path_str),
            ));
        }
        Ok(Self {
            db: Some(db),
            path: path_str,
        })
    }

    pub fn db(&self) -> &triviumdb::Database<f32> {
        self.db
            .as_ref()
            .expect("TriviumDB handle is unavailable only during Drop")
    }

    pub(crate) fn db_mut(&mut self) -> &mut triviumdb::Database<f32> {
        self.db
            .as_mut()
            .expect("TriviumDB handle is unavailable only during Drop")
    }

    pub fn flush(&mut self) -> Result<()> {
        let path = self.path.clone();
        let flush_result = self
            .db_mut()
            .flush()
            .map_err(|error| AgentError::Bootstrap(format!("TriviumDB flush {path}: {error}")));
        let permission_result = secure_trivium_files(&path);
        match flush_result {
            Ok(()) => permission_result,
            Err(error) => Err(merge_permission_error(error, permission_result)),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for TriviumDb {
    fn drop(&mut self) {
        if let Some(db) = self.db.take() {
            drop(db);
        }
        if let Err(error) = secure_trivium_files(&self.path) {
            tracing::error!("failed to secure TriviumDB files during close: {error}");
        }
    }
}

fn secure_trivium_files(path: &str) -> Result<()> {
    for suffix in TRIVIUM_FILE_SUFFIXES {
        secure_existing_file(Path::new(&format!("{path}{suffix}")))?;
    }
    Ok(())
}

fn merge_permission_error(
    operation_error: AgentError,
    permission_result: Result<()>,
) -> AgentError {
    match permission_result {
        Ok(()) => operation_error,
        Err(permission_error) => AgentError::Bootstrap(format!(
            "{operation_error}; additionally failed to secure TriviumDB files: {permission_error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triviumdb_open_and_close() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trivium");
        let db = TriviumDb::open(&db_path, 128).unwrap();
        assert!(db.path().contains("test.trivium"));
        assert_eq!(db.db().dim(), 128);
        assert!(
            !db_path.exists(),
            "opening an empty TriviumDB must not pre-create {}",
            db_path.display()
        );
        for suffix in [".lock", ".wal"] {
            let artifact = Path::new(&format!("{}{}", db_path.display(), suffix)).to_path_buf();
            assert!(
                artifact.is_file(),
                "expected {} to exist",
                artifact.display()
            );
        }
    }

    #[test]
    fn triviumdb_insert_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("search.trivium");
        let mut db = TriviumDb::open(&db_path, 4).unwrap();

        let vec = vec![1.0, 0.0, 0.0, 0.0];
        let payload = serde_json::json!({"label": "test", "_memory_type": "experience"});
        let id = db.db_mut().insert(&vec, payload.clone()).unwrap();

        let results = db.db().search(&vec, 5, 0, 0.0).unwrap();
        assert!(!results.is_empty(), "should find at least one result");
        assert_eq!(
            results[0].id, id,
            "first result should be the inserted node"
        );

        let retrieved = db.db().get_payload(id).unwrap();
        assert_eq!(retrieved["label"], "test");
    }

    #[cfg(unix)]
    #[test]
    fn triviumdb_open_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("private.trivium");
        let mut db = TriviumDb::open(&db_path, 4).unwrap();
        db.db_mut()
            .insert(&[1.0, 0.0, 0.0, 0.0], serde_json::json!({"private": true}))
            .unwrap();
        db.flush().unwrap();

        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let artifact_paths: Vec<_> = ["", ".lock", ".wal", ".vec", ".flush_ok"]
            .into_iter()
            .map(|suffix| Path::new(&format!("{}{}", db_path.display(), suffix)).to_path_buf())
            .collect();
        for path in &artifact_paths {
            assert!(path.exists(), "expected {} to exist", path.display());
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600,
                "unexpected mode for {}",
                path.display()
            );
        }
        drop(db);

        for path in &artifact_paths {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let reopened = TriviumDb::open(&db_path, 4).unwrap();
        for path in &artifact_paths {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600,
                "reopen did not repair {}",
                path.display()
            );
        }
        drop(reopened);
    }

    #[cfg(unix)]
    #[test]
    fn secure_trivium_files_repairs_every_known_existing_artifact() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("existing.trivium");
        let base = db_path.to_string_lossy();
        let artifacts: Vec<_> = TRIVIUM_FILE_SUFFIXES
            .iter()
            .map(|suffix| Path::new(&format!("{base}{suffix}")).to_path_buf())
            .collect();

        for artifact in &artifacts {
            std::fs::write(artifact, b"permission fixture").unwrap();
            std::fs::set_permissions(artifact, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                artifact.is_file(),
                "expected {} to exist",
                artifact.display()
            );
        }

        secure_trivium_files(&base).unwrap();

        for artifact in &artifacts {
            assert!(
                artifact.is_file(),
                "expected {} to exist",
                artifact.display()
            );
            assert_eq!(
                std::fs::metadata(artifact).unwrap().permissions().mode() & 0o777,
                0o600,
                "unexpected mode for {}",
                artifact.display()
            );
        }
    }
}
