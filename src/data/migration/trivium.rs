use super::VerifiedBackup;
use crate::common::{AgentError, Result};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use crate::data::triviumdb::{TriviumDb, DEFAULT_DIM};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const TRIVIUM_DIRECTORY: &str = "triviumdb";
const TRIVIUM_FILE: &str = "memory.trivium";
const ALLOWED_MEMORY_TYPES: &[&str] = &[
    "attention",
    "cognitive",
    "experience",
    "preference",
    "raw_files",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriviumMigrationIssue {
    pub node_id: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriviumMigrationReport {
    pub source_nodes: u64,
    pub migrated_nodes: u64,
    pub quarantined_nodes: u64,
    pub source_edges: u64,
    pub migrated_edges: u64,
    pub quarantined_edges: u64,
    pub issues: Vec<TriviumMigrationIssue>,
}

#[derive(Debug)]
struct CandidateNode {
    id: u64,
    vector: Vec<f32>,
    payload: Value,
    edges: Vec<triviumdb::Edge>,
}

struct ScratchDirectory {
    path: PathBuf,
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn rebuild_trivium_from_backup(
    backup: &VerifiedBackup,
    staging_generation: &Path,
) -> Result<TriviumMigrationReport> {
    if staging_generation.starts_with(&backup.backup_dir) {
        return Err(migration_error(
            "staging generation cannot be inside the immutable backup",
        ));
    }

    let backup_trivium = backup.backup_dir.join(TRIVIUM_DIRECTORY);
    if !contains_trivium_family(&backup_trivium)? {
        return Ok(TriviumMigrationReport::default());
    }

    ensure_private_directory(staging_generation)?;
    let scratch = ScratchDirectory {
        path: staging_generation.join(format!(".trivium-source-{}", Uuid::new_v4())),
    };
    let scratch_trivium = scratch.path.join(TRIVIUM_DIRECTORY);
    copy_directory(&backup_trivium, &scratch_trivium)?;

    let source_path = scratch_trivium.join(TRIVIUM_FILE);
    let source = TriviumDb::open(&source_path, DEFAULT_DIM).map_err(|error| {
        migration_error(format!("cannot open copied TriviumDB source: {error}"))
    })?;

    let mut report = TriviumMigrationReport::default();
    let mut source_ids = source.db().all_node_ids();
    source_ids.sort_unstable();
    report.source_nodes = source_ids.len() as u64;

    let mut candidates = Vec::new();
    for node_id in source_ids {
        let fallback_edges = source.db().get_edges(node_id);
        let Some(node) = source.db().get(node_id) else {
            report.quarantined_nodes += 1;
            report.source_edges += fallback_edges.len() as u64;
            report.quarantined_edges += fallback_edges.len() as u64;
            record_issue(&mut report, node_id, "node payload or vector is unreadable");
            continue;
        };

        report.source_edges += node.edges.len() as u64;
        if let Some(reason) = payload_quarantine_reason(&node.payload) {
            report.quarantined_nodes += 1;
            report.quarantined_edges += node.edges.len() as u64;
            record_issue(&mut report, node_id, reason);
            continue;
        }
        if node.vector.len() != DEFAULT_DIM {
            report.quarantined_nodes += 1;
            report.quarantined_edges += node.edges.len() as u64;
            record_issue(&mut report, node_id, "vector dimension is not 128");
            continue;
        }

        candidates.push(CandidateNode {
            id: node.id,
            vector: node.vector,
            payload: node.payload,
            edges: node.edges,
        });
    }
    drop(source);

    if candidates.is_empty() {
        return Ok(report);
    }

    let valid_ids: BTreeSet<_> = candidates.iter().map(|node| node.id).collect();
    let target_dir = staging_generation.join(TRIVIUM_DIRECTORY);
    if contains_trivium_family(&target_dir)? {
        return Err(migration_error(
            "candidate generation already contains a TriviumDB file family",
        ));
    }
    ensure_private_directory(&target_dir)?;
    let target_path = target_dir.join(TRIVIUM_FILE);
    let mut target = TriviumDb::open(&target_path, DEFAULT_DIM)
        .map_err(|error| migration_error(format!("cannot create candidate TriviumDB: {error}")))?;

    for node in &candidates {
        target
            .db_mut()
            .insert_with_id(node.id, &node.vector, node.payload.clone())
            .map_err(|error| {
                migration_error(format!(
                    "cannot migrate TriviumDB node {}: {error}",
                    node.id
                ))
            })?;
    }

    for node in &candidates {
        for edge in &node.edges {
            if valid_ids.contains(&edge.target_id) {
                target
                    .db_mut()
                    .link(node.id, edge.target_id, &edge.label, edge.weight)
                    .map_err(|error| {
                        migration_error(format!(
                            "cannot migrate edge from node {} to node {}: {error}",
                            node.id, edge.target_id
                        ))
                    })?;
                report.migrated_edges += 1;
            } else {
                report.quarantined_edges += 1;
                record_issue(
                    &mut report,
                    node.id,
                    format!("edge target {} was quarantined or missing", edge.target_id),
                );
            }
        }
    }

    target
        .flush()
        .map_err(|error| migration_error(format!("cannot flush candidate TriviumDB: {error}")))?;
    validate_candidate(target.db(), &valid_ids)?;
    drop(target);

    let reopened = TriviumDb::open(&target_path, DEFAULT_DIM)
        .map_err(|error| migration_error(format!("cannot reopen candidate TriviumDB: {error}")))?;
    validate_candidate(reopened.db(), &valid_ids)?;
    drop(reopened);

    report.migrated_nodes = candidates.len() as u64;
    Ok(report)
}

fn payload_quarantine_reason(payload: &Value) -> Option<&'static str> {
    let object = match payload.as_object() {
        Some(object) => object,
        None => return Some("payload is not a JSON object"),
    };
    let memory_type = match object.get("_memory_type").and_then(Value::as_str) {
        Some(memory_type) => memory_type,
        None => return Some("missing or non-string _memory_type"),
    };
    if !ALLOWED_MEMORY_TYPES.contains(&memory_type) {
        return Some("unknown _memory_type");
    }

    let has_structured_field = object
        .keys()
        .any(|key| key != "_memory_type" && key != "data");
    if !has_structured_field {
        if object.get("data").and_then(Value::as_str).is_some() {
            return Some("legacy Debug-string envelope");
        }
        return Some("payload has no structured fields");
    }

    None
}

fn validate_candidate(
    database: &triviumdb::Database<f32>,
    expected_ids: &BTreeSet<u64>,
) -> Result<()> {
    let actual_ids: BTreeSet<_> = database.all_node_ids().into_iter().collect();
    if database.node_count() != expected_ids.len() || actual_ids != *expected_ids {
        return Err(migration_error(
            "candidate TriviumDB node count or ID set does not match the migration plan",
        ));
    }
    Ok(())
}

fn contains_trivium_family(directory: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(migration_error(format!(
                "cannot inspect TriviumDB directory: {error}"
            )))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(migration_error(
            "TriviumDB source path is not a regular directory",
        ));
    }

    for entry in fs::read_dir(directory)
        .map_err(|error| migration_error(format!("cannot list TriviumDB directory: {error}")))?
    {
        let entry = entry.map_err(|error| {
            migration_error(format!("cannot inspect TriviumDB directory entry: {error}"))
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            migration_error(format!("cannot inspect TriviumDB artifact: {error}"))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(migration_error(
                "TriviumDB directory contains a symbolic link",
            ));
        }
        if metadata.is_file()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(TRIVIUM_FILE)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).map_err(|error| {
        migration_error(format!(
            "cannot inspect backup TriviumDB directory: {error}"
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(migration_error(
            "backup TriviumDB source is not a regular directory",
        ));
    }
    ensure_private_directory(destination)?;

    for entry in fs::read_dir(source).map_err(|error| {
        migration_error(format!("cannot list backup TriviumDB directory: {error}"))
    })? {
        let entry = entry.map_err(|error| {
            migration_error(format!("cannot inspect backup TriviumDB entry: {error}"))
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).map_err(|error| {
            migration_error(format!("cannot inspect backup TriviumDB artifact: {error}"))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(migration_error(
                "backup TriviumDB directory contains a symbolic link",
            ));
        }
        if metadata.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                migration_error(format!("cannot copy backup TriviumDB artifact: {error}"))
            })?;
            secure_existing_file(&destination_path)?;
        } else {
            return Err(migration_error(
                "backup TriviumDB directory contains a special file",
            ));
        }
    }
    Ok(())
}

fn record_issue(report: &mut TriviumMigrationReport, node_id: u64, reason: impl Into<String>) {
    report.issues.push(TriviumMigrationIssue {
        node_id,
        reason: reason.into(),
    });
}

fn migration_error(message: impl Into<String>) -> AgentError {
    AgentError::Bootstrap(format!("TriviumDB migration: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::migration::ensure_verified_backup;
    use std::collections::BTreeMap;

    fn vector(first: f32) -> Vec<f32> {
        let mut vector = vec![0.0; DEFAULT_DIM];
        vector[0] = first;
        vector
    }

    fn create_source_database(data_root: &Path) {
        let source_dir = data_root.join(TRIVIUM_DIRECTORY);
        ensure_private_directory(&source_dir).unwrap();
        let source_path = source_dir.join(TRIVIUM_FILE);
        let mut source = TriviumDb::open(&source_path, DEFAULT_DIM).unwrap();

        source
            .db_mut()
            .insert_with_id(
                10,
                &vector(1.0),
                serde_json::json!({
                    "_memory_type": "experience",
                    "summary": "valid experience",
                    "outcome": "success"
                }),
            )
            .unwrap();
        source
            .db_mut()
            .insert_with_id(
                20,
                &vector(2.0),
                serde_json::json!({
                    "_memory_type": "attention",
                    "focus": "valid attention",
                    "content": "structured detail"
                }),
            )
            .unwrap();
        source
            .db_mut()
            .insert_with_id(
                30,
                &vector(3.0),
                serde_json::json!({
                    "_memory_type": "experience",
                    "data": "SecretFragment { value: do-not-report }"
                }),
            )
            .unwrap();
        source
            .db_mut()
            .insert_with_id(
                40,
                &vector(4.0),
                serde_json::json!({
                    "_memory_type": "unknown",
                    "value": "not supported"
                }),
            )
            .unwrap();
        source
            .db_mut()
            .insert_with_id(50, &vector(5.0), serde_json::json!(["bad", "payload"]))
            .unwrap();

        source.db_mut().link(10, 20, "valid", 0.75).unwrap();
        source
            .db_mut()
            .link(10, 30, "quarantined-target", 0.5)
            .unwrap();
        source
            .db_mut()
            .link(30, 20, "quarantined-source", 0.25)
            .unwrap();
        source.flush().unwrap();
    }

    #[test]
    fn rebuilds_structured_nodes_and_edges_without_touching_backup() {
        let temporary = tempfile::tempdir().unwrap();
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).unwrap();
        create_source_database(&data_root);
        let backup = ensure_verified_backup(&data_root).unwrap();
        let backup_before = snapshot_files(&backup.backup_dir);
        let staging = temporary.path().join("generation-staging");

        let report = rebuild_trivium_from_backup(&backup, &staging).unwrap();

        assert_eq!(report.source_nodes, 5);
        assert_eq!(report.migrated_nodes, 2);
        assert_eq!(report.quarantined_nodes, 3);
        assert_eq!(report.source_edges, 3);
        assert_eq!(report.migrated_edges, 1);
        assert_eq!(report.quarantined_edges, 2);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.node_id == 30 && issue.reason == "legacy Debug-string envelope"));
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.node_id == 40 && issue.reason == "unknown _memory_type"));
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.node_id == 50 && issue.reason == "payload is not a JSON object"));
        let serialized_report = serde_json::to_string(&report).unwrap();
        assert!(!serialized_report.contains("do-not-report"));
        assert_eq!(backup_before, snapshot_files(&backup.backup_dir));

        let candidate_path = staging.join(TRIVIUM_DIRECTORY).join(TRIVIUM_FILE);
        let candidate = TriviumDb::open(&candidate_path, DEFAULT_DIM).unwrap();
        let mut ids = candidate.db().all_node_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![10, 20]);
        let first = candidate.db().get(10).unwrap();
        assert_eq!(first.vector, vector(1.0));
        assert_eq!(first.edges.len(), 1);
        assert_eq!(first.edges[0].target_id, 20);
        assert_eq!(first.edges[0].label, "valid");
        drop(candidate);

        assert!(!fs::read_dir(&staging).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".trivium-source-")
        }));

        #[cfg(unix)]
        assert_private_candidate_files(&candidate_path);
    }

    #[test]
    fn missing_backup_family_returns_empty_report_without_creating_main_file() {
        let temporary = tempfile::tempdir().unwrap();
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).unwrap();
        let backup = ensure_verified_backup(&data_root).unwrap();
        let staging = temporary.path().join("generation-staging");

        let report = rebuild_trivium_from_backup(&backup, &staging).unwrap();

        assert_eq!(report, TriviumMigrationReport::default());
        assert!(!staging.join(TRIVIUM_DIRECTORY).join(TRIVIUM_FILE).exists());
        assert!(!staging.exists());
    }

    #[test]
    fn recognizes_structured_payloads_for_all_five_memory_types() {
        for payload in [
            serde_json::json!({"_memory_type": "attention", "focus": "x"}),
            serde_json::json!({"_memory_type": "cognitive", "insight": "x"}),
            serde_json::json!({"_memory_type": "experience", "summary": "x"}),
            serde_json::json!({"_memory_type": "preference", "key": "x"}),
            serde_json::json!({"_memory_type": "raw_files", "path": "x"}),
        ] {
            assert_eq!(payload_quarantine_reason(&payload), None);
        }
    }

    fn snapshot_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn collect(root: &Path, current: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(current).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }

        let mut files = BTreeMap::new();
        collect(root, root, &mut files);
        files
    }

    #[cfg(unix)]
    fn assert_private_candidate_files(candidate_path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        for suffix in ["", ".lock", ".wal", ".vec", ".flush_ok"] {
            let path = Path::new(&format!("{}{}", candidate_path.display(), suffix)).to_path_buf();
            assert!(path.is_file(), "expected {} to exist", path.display());
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "unexpected mode for {}",
                path.display()
            );
        }
    }
}
