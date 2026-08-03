use super::{
    activate_existing_generation, apply_conversation_migration, build_duckdb_candidate,
    create_staging_generation, ensure_verified_backup, generation_name,
    plan_conversation_migration, publish_generation, rebuild_trivium_from_backup,
    resolve_active_data, restore_verified_subtree, validate_current_duckdb,
    ConversationMigrationEntry, ConversationMigrationPlan, ConversationMigrationReport, DataPaths,
    DuckdbMigrationReport, GenerationManifest, MigrationLock, TriviumMigrationReport,
    VerifiedBackup, BACKUP_SCHEMA_VERSION, CURRENT_DATA_SCHEMA_VERSION,
};
use crate::common::{AgentError, Result, UtcTimestamp};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use crate::data::thought_store::ThoughtStore;
use crate::data::triviumdb::{TriviumDb, DEFAULT_DIM};
use crate::data::workspace_store::WorkspaceStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const MIGRATION_SCHEMA_VERSION: u32 = 1;

const PLANS_DIRECTORY: &str = "plans";
const REPORTS_DIRECTORY: &str = "reports";
const MIGRATIONS_DIRECTORY: &str = "migrations";
const DUCKDB_FILE: &str = "cipher.duckdb";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationPlan {
    pub schema_version: u32,
    pub target_data_schema_version: u32,
    pub source_fingerprint: String,
    pub conversations: ConversationMigrationPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationTimeRange {
    pub earliest: UtcTimestamp,
    pub latest: UtcTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuckdbMemorySummary {
    pub table: String,
    pub rows: u64,
    pub quarantined: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuckdbIssueSummary {
    pub table: String,
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuckdbSummary {
    pub fresh: bool,
    pub source_counts: BTreeMap<String, u64>,
    pub target_counts: BTreeMap<String, u64>,
    pub memory: Vec<DuckdbMemorySummary>,
    pub issues: Vec<DuckdbIssueSummary>,
    pub workspace_rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMigrationSummary {
    pub source_rows: u64,
    pub imported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThoughtValidationSummary {
    pub timestamp_groups: u64,
    pub thoughts: u64,
    pub time_range: Option<MigrationTimeRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationReport {
    pub schema_version: u32,
    pub target_data_schema_version: u32,
    pub source_backup_schema_version: u32,
    pub source_fingerprint: String,
    pub migration_plan_sha256: String,
    pub restored_thought_files: u64,
    pub duckdb: DuckdbSummary,
    pub workspaces: WorkspaceMigrationSummary,
    pub conversations: ConversationMigrationReport,
    pub conversation_time_range: Option<MigrationTimeRange>,
    pub thoughts: ThoughtValidationSummary,
    pub trivium: TriviumMigrationReport,
}

struct PersistedDocument<T> {
    value: T,
    sha256: String,
}

struct StagingGuard {
    path: PathBuf,
    keep: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }
}

pub fn prepare_data_dir(data_root: &Path) -> Result<DataPaths> {
    ensure_private_directory(data_root)?;
    if let Some(paths) = resolve_active_data(data_root)? {
        return Ok(paths);
    }

    let _lock = MigrationLock::acquire(data_root)?;
    if let Some(paths) = resolve_active_data(data_root)? {
        return Ok(paths);
    }

    let backup = ensure_verified_backup(data_root)?;
    let final_name = if backup.manifest.files.is_empty() {
        generation_name(None)?
    } else {
        generation_name(Some(&backup.manifest.source_fingerprint))?
    };
    let plan = load_or_create_plan(data_root, &final_name, &backup)?;

    if let Some(report) = load_report(data_root, &final_name)? {
        validate_report_identity(&report.value, &backup, &plan.sha256)?;
        let expected = generation_manifest(
            &final_name,
            &backup.manifest.source_fingerprint,
            &plan.sha256,
            &report.sha256,
        );
        if let Some(paths) = activate_existing_generation(data_root, &final_name, &expected)? {
            return Ok(paths);
        }
    }

    let staging_path = create_staging_generation(data_root, &final_name)?;
    let mut staging = StagingGuard {
        path: staging_path,
        keep: false,
    };

    let restored_thought_files = restore_verified_subtree(&backup, "thoughts", &staging.path)?;
    let duckdb_backup = legacy_duckdb_source(&backup)?;
    let duckdb_report = build_duckdb_candidate(duckdb_backup, &staging.path)?;

    let workspace_rows = duckdb_report.workspace_rows().to_vec();
    let workspace_store = WorkspaceStore::open(&staging.path)?;
    let workspaces_imported = workspace_store.import_if_empty(workspace_rows.clone())?;
    workspace_store.initialize()?;

    let conversation_report =
        apply_conversation_migration(&backup, &plan.value.conversations, &staging.path)?;
    let trivium_report = rebuild_trivium_from_backup(&backup, &staging.path)?;

    let candidate_path = staging.path.join(DUCKDB_FILE);
    validate_current_duckdb(&candidate_path)?;
    let candidate_connection = duckdb::Connection::open(&candidate_path).map_err(|error| {
        migration_error(format!(
            "cannot reopen candidate registry for validation: {error}"
        ))
    })?;
    crate::data::duckdb::load_all_into_memory(&candidate_connection)?;
    drop(candidate_connection);
    let stored_workspaces = workspace_store.list()?;
    if stored_workspaces != workspace_rows {
        return Err(migration_error(
            "workspace candidate differs from the migrated DuckDB rows",
        ));
    }
    let thought_summary = validate_thoughts(&staging.path)?;
    validate_trivium(&staging.path, &trivium_report)?;

    let report_value = MigrationReport {
        schema_version: MIGRATION_SCHEMA_VERSION,
        target_data_schema_version: CURRENT_DATA_SCHEMA_VERSION,
        source_backup_schema_version: BACKUP_SCHEMA_VERSION,
        source_fingerprint: backup.manifest.source_fingerprint.clone(),
        migration_plan_sha256: plan.sha256.clone(),
        restored_thought_files,
        duckdb: summarize_duckdb(&duckdb_report),
        workspaces: WorkspaceMigrationSummary {
            source_rows: workspace_rows.len() as u64,
            imported: workspaces_imported,
        },
        conversations: conversation_report,
        conversation_time_range: conversation_time_range(&plan.value.conversations),
        thoughts: thought_summary,
        trivium: trivium_report,
    };
    let report = persist_report(data_root, &final_name, report_value)?;

    let manifest = generation_manifest(
        &final_name,
        &backup.manifest.source_fingerprint,
        &plan.sha256,
        &report.sha256,
    );
    let paths = publish_generation(data_root, &staging.path, &final_name, &manifest)?;
    staging.keep = true;
    Ok(paths)
}

fn load_or_create_plan(
    data_root: &Path,
    final_name: &str,
    backup: &VerifiedBackup,
) -> Result<PersistedDocument<MigrationPlan>> {
    let directory = metadata_directory(data_root, PLANS_DIRECTORY)?;
    let path = directory.join(format!("{final_name}.json"));
    if let Some(bytes) = read_optional_regular_file(&path, "migration plan")? {
        let value: MigrationPlan = parse_json(&bytes, "migration plan")?;
        validate_plan_identity(&value, backup)?;
        return Ok(PersistedDocument {
            sha256: sha256_bytes(&bytes),
            value,
        });
    }

    let value = MigrationPlan {
        schema_version: MIGRATION_SCHEMA_VERSION,
        target_data_schema_version: CURRENT_DATA_SCHEMA_VERSION,
        source_fingerprint: backup.manifest.source_fingerprint.clone(),
        conversations: plan_conversation_migration(backup)?,
    };
    let bytes = serialize_json(&value, "migration plan")?;
    persist_immutable_file(&path, &bytes, "migration plan")?;
    Ok(PersistedDocument {
        sha256: sha256_bytes(&bytes),
        value,
    })
}

fn load_report(
    data_root: &Path,
    final_name: &str,
) -> Result<Option<PersistedDocument<MigrationReport>>> {
    let directory = metadata_directory(data_root, REPORTS_DIRECTORY)?;
    let path = directory.join(format!("{final_name}.json"));
    let Some(bytes) = read_optional_regular_file(&path, "migration report")? else {
        return Ok(None);
    };
    let value = parse_json(&bytes, "migration report")?;
    Ok(Some(PersistedDocument {
        sha256: sha256_bytes(&bytes),
        value,
    }))
}

fn persist_report(
    data_root: &Path,
    final_name: &str,
    value: MigrationReport,
) -> Result<PersistedDocument<MigrationReport>> {
    let directory = metadata_directory(data_root, REPORTS_DIRECTORY)?;
    let path = directory.join(format!("{final_name}.json"));
    let bytes = serialize_json(&value, "migration report")?;
    persist_immutable_file(&path, &bytes, "migration report")?;
    Ok(PersistedDocument {
        sha256: sha256_bytes(&bytes),
        value,
    })
}

fn metadata_directory(data_root: &Path, child: &str) -> Result<PathBuf> {
    let migrations = data_root.join(MIGRATIONS_DIRECTORY);
    ensure_private_directory(&migrations)?;
    let directory = migrations.join(child);
    ensure_private_directory(&directory)?;
    Ok(directory)
}

fn validate_plan_identity(plan: &MigrationPlan, backup: &VerifiedBackup) -> Result<()> {
    if plan.schema_version != MIGRATION_SCHEMA_VERSION
        || plan.target_data_schema_version != CURRENT_DATA_SCHEMA_VERSION
        || plan.source_fingerprint != backup.manifest.source_fingerprint
    {
        return Err(migration_error(
            "persisted migration plan does not match the verified source",
        ));
    }
    Ok(())
}

fn validate_report_identity(
    report: &MigrationReport,
    backup: &VerifiedBackup,
    plan_sha256: &str,
) -> Result<()> {
    if report.schema_version != MIGRATION_SCHEMA_VERSION
        || report.target_data_schema_version != CURRENT_DATA_SCHEMA_VERSION
        || report.source_backup_schema_version != BACKUP_SCHEMA_VERSION
        || report.source_fingerprint != backup.manifest.source_fingerprint
        || report.migration_plan_sha256 != plan_sha256
    {
        return Err(migration_error(
            "persisted migration report does not match the verified source and plan",
        ));
    }
    Ok(())
}

fn generation_manifest(
    final_name: &str,
    source_fingerprint: &str,
    plan_sha256: &str,
    report_sha256: &str,
) -> GenerationManifest {
    let mut manifest = GenerationManifest::fresh(final_name);
    manifest.source_fingerprint = Some(source_fingerprint.to_string());
    manifest.migration_plan_sha256 = Some(plan_sha256.to_string());
    manifest.migration_report_sha256 = Some(report_sha256.to_string());
    manifest
}

fn legacy_duckdb_source(backup: &VerifiedBackup) -> Result<Option<&VerifiedBackup>> {
    let has_main = backup
        .manifest
        .files
        .iter()
        .any(|entry| entry.path == DUCKDB_FILE);
    let has_family = backup
        .manifest
        .files
        .iter()
        .any(|entry| entry.path.starts_with(DUCKDB_FILE));
    if has_family && !has_main {
        return Err(migration_error(
            "legacy DuckDB artifacts exist without the main database file",
        ));
    }
    Ok(has_main.then_some(backup))
}

fn summarize_duckdb(report: &DuckdbMigrationReport) -> DuckdbSummary {
    DuckdbSummary {
        fresh: report.fresh,
        source_counts: report.source_counts.clone(),
        target_counts: report.target_counts.clone(),
        memory: report
            .memory
            .iter()
            .map(|entry| DuckdbMemorySummary {
                table: entry.table.clone(),
                rows: entry.rows,
                quarantined: entry.quarantined,
            })
            .collect(),
        issues: report
            .issues
            .iter()
            .map(|issue| DuckdbIssueSummary {
                table: issue.table.clone(),
                id: issue.id.clone(),
                reason: serde_json::to_value(issue.reason)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", issue.reason)),
            })
            .collect(),
        workspace_rows: report.workspace_rows().len() as u64,
    }
}

fn conversation_time_range(plan: &ConversationMigrationPlan) -> Option<MigrationTimeRange> {
    let mut timestamps = plan.entries.iter().filter_map(|entry| match entry {
        ConversationMigrationEntry::Import { occurred_at, .. } => Some(occurred_at),
        ConversationMigrationEntry::Quarantine { .. } => None,
    });
    let first = timestamps.next()?.clone();
    let mut earliest = first.clone();
    let mut latest = first;
    for timestamp in timestamps {
        if timestamp < &earliest {
            earliest = timestamp.clone();
        }
        if timestamp > &latest {
            latest = timestamp.clone();
        }
    }
    Some(MigrationTimeRange { earliest, latest })
}

fn validate_thoughts(staging_root: &Path) -> Result<ThoughtValidationSummary> {
    let timeline = ThoughtStore::open(staging_root)?.recover()?;
    let thoughts = timeline
        .groups
        .iter()
        .try_fold(0_u64, |total, group| {
            total.checked_add(group.contexts.len() as u64)
        })
        .ok_or_else(|| migration_error("Thought count overflow"))?;
    let time_range = match (timeline.groups.first(), timeline.groups.last()) {
        (Some(first), Some(last)) => Some(MigrationTimeRange {
            earliest: first.occurred_at.clone(),
            latest: last.occurred_at.clone(),
        }),
        _ => None,
    };
    Ok(ThoughtValidationSummary {
        timestamp_groups: timeline.groups.len() as u64,
        thoughts,
        time_range,
    })
}

fn validate_trivium(staging_root: &Path, report: &TriviumMigrationReport) -> Result<()> {
    if report.source_nodes != report.migrated_nodes + report.quarantined_nodes
        || report.source_edges != report.migrated_edges + report.quarantined_edges
    {
        return Err(migration_error(
            "TriviumDB migration counts are not conserved",
        ));
    }

    let path = staging_root.join("triviumdb").join("memory.trivium");
    let mut database = TriviumDb::open(&path, DEFAULT_DIM)?;
    database.flush()?;
    drop(database);

    let reopened = TriviumDb::open(&path, DEFAULT_DIM)?;
    let node_ids = reopened.db().all_node_ids();
    if node_ids.len() as u64 != report.migrated_nodes {
        return Err(migration_error(
            "reopened TriviumDB node count differs from the migration report",
        ));
    }
    let edge_count = node_ids.iter().try_fold(0_u64, |total, node_id| {
        total.checked_add(reopened.db().get_edges(*node_id).len() as u64)
    });
    if edge_count != Some(report.migrated_edges) {
        return Err(migration_error(
            "reopened TriviumDB edge count differs from the migration report",
        ));
    }
    drop(reopened);
    Ok(())
}

fn persist_immutable_file(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    if let Some(existing) = read_optional_regular_file(path, label)? {
        if existing == bytes {
            return Ok(());
        }
        return Err(migration_error(format!(
            "persisted {label} differs from the validated retry result"
        )));
    }

    let parent = path
        .parent()
        .ok_or_else(|| migration_error(format!("{label} has no parent directory")))?;
    ensure_private_directory(parent)?;
    let temporary = parent.join(format!(".{label}.{}.tmp", Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|error| {
            migration_error(format!("cannot create temporary {label}: {error}"))
        })?;
        file.write_all(bytes)
            .map_err(|error| migration_error(format!("cannot write {label}: {error}")))?;
        file.sync_all()
            .map_err(|error| migration_error(format!("cannot sync {label}: {error}")))?;
        drop(file);
        secure_existing_file(&temporary)?;
        fs::rename(&temporary, path)
            .map_err(|error| migration_error(format!("cannot publish {label}: {error}")))?;
        secure_existing_file(path)?;
        sync_directory(parent)
    })();
    if result.is_err() && temporary.exists() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn read_optional_regular_file(path: &Path, label: &str) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(migration_error(format!("{label} is not a regular file")))
        }
        Ok(_) => {
            secure_existing_file(path)?;
            fs::read(path)
                .map(Some)
                .map_err(|error| migration_error(format!("cannot read {label}: {error}")))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(migration_error(format!("cannot inspect {label}: {error}"))),
    }
}

fn serialize_json<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(value)
        .map_err(|error| migration_error(format!("cannot serialize {label}: {error}")))
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8], label: &str) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| migration_error(format!("cannot parse {label}: {error}")))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| migration_error(format!("cannot sync directory: {error}")))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn migration_error(message: impl Into<String>) -> AgentError {
    AgentError::Bootstrap(format!("data migration: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_prepare_is_exact_five_and_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");

        let first = prepare_data_dir(&root).unwrap();
        assert_eq!(first.root(), root);
        assert_ne!(first.storage_root(), first.root());
        assert_eq!(
            validate_current_duckdb(&first.duckdb())
                .unwrap()
                .table_counts
                .len(),
            5
        );
        assert!(ThoughtStore::open(first.thoughts_data_root())
            .unwrap()
            .recover()
            .unwrap()
            .groups
            .is_empty());
        assert!(WorkspaceStore::open(first.storage_root())
            .unwrap()
            .list()
            .unwrap()
            .is_empty());

        let second = prepare_data_dir(&root).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn malformed_legacy_duckdb_family_never_activates() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        ensure_private_directory(&root).unwrap();
        let legacy = root.join("cipher.duckdb.orphan");
        fs::write(&legacy, b"legacy bytes").unwrap();

        assert!(prepare_data_dir(&root).is_err());
        assert!(resolve_active_data(&root).unwrap().is_none());
        assert_eq!(fs::read(legacy).unwrap(), b"legacy bytes");
        assert!(root.join("migrations").join("backups").is_dir());
    }

    #[test]
    fn migration_plan_is_loaded_without_regenerating_random_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        ensure_private_directory(&root).unwrap();
        let backup = ensure_verified_backup(&root).unwrap();
        let name = generation_name(None).unwrap();

        let first = load_or_create_plan(&root, &name, &backup).unwrap();
        let second = load_or_create_plan(&root, &name, &backup).unwrap();
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(first.value, second.value);
    }
}
