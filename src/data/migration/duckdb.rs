use super::VerifiedBackup;
use crate::common::{AgentError, Result};
use crate::data::workspace_store::WorkspaceRow;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const CANDIDATE_DUCKDB_FILE: &str = "cipher.duckdb";

const DUCKDB_PREFIX: &str = "cipher.duckdb";
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const TARGET_TABLES: &[&str] = &[
    "agent",
    "base_capability",
    "composite_capability",
    "model",
    "usage_method",
];
const LEGACY_TABLES: &[&str] = &[
    "agent",
    "attention_kv",
    "base_capability",
    "cognitive",
    "composite_capability",
    "experience_rag",
    "model",
    "preference_rag",
    "raw_files",
    "usage_method",
    "workspace",
];
const MEMORY_TABLES: &[&str] = &[
    "attention_kv",
    "cognitive",
    "experience_rag",
    "preference_rag",
    "raw_files",
];

const TARGET_SCHEMA: &str = r#"
CREATE TABLE model (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    provider TEXT NOT NULL,
    api_url TEXT NOT NULL,
    api_type TEXT NOT NULL DEFAULT 'OpenAI',
    api_protocol TEXT NOT NULL DEFAULT 'openai-v1',
    api_key TEXT NOT NULL DEFAULT '',
    model_id TEXT NOT NULL,
    config JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE agent (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    mode TEXT NOT NULL,
    prompt TEXT,
    tool_caps JSON NOT NULL DEFAULT '[]',
    config JSON,
    display_name TEXT,
    is_default BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE base_capability (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    schema_in JSON NOT NULL,
    schema_out JSON NOT NULL,
    executor TEXT NOT NULL,
    version TEXT NOT NULL DEFAULT '',
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    tombstoned_at TIMESTAMP,
    metadata JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE composite_capability (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    schema_in JSON,
    schema_out JSON,
    executor TEXT,
    dag JSON NOT NULL,
    version TEXT NOT NULL DEFAULT '',
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    tombstoned_at TIMESTAMP,
    metadata JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE usage_method (
    id TEXT PRIMARY KEY,
    capability_id TEXT NOT NULL,
    name TEXT NOT NULL,
    prompt TEXT NOT NULL,
    examples JSON,
    metadata JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
"#;

const MODEL_COLUMNS: &[&str] = &[
    "api_key",
    "api_protocol",
    "api_type",
    "api_url",
    "config",
    "created_at",
    "id",
    "model_id",
    "name",
    "provider",
    "updated_at",
];
const AGENT_COLUMNS: &[&str] = &[
    "config",
    "created_at",
    "display_name",
    "id",
    "is_default",
    "mode",
    "name",
    "prompt",
    "tool_caps",
    "updated_at",
];
const BASE_COLUMNS: &[&str] = &[
    "created_at",
    "description",
    "enabled",
    "executor",
    "id",
    "metadata",
    "name",
    "schema_in",
    "schema_out",
    "tombstoned_at",
    "type",
    "updated_at",
    "version",
];
const COMPOSITE_COLUMNS: &[&str] = &[
    "created_at",
    "dag",
    "description",
    "enabled",
    "executor",
    "id",
    "metadata",
    "name",
    "schema_in",
    "schema_out",
    "tombstoned_at",
    "updated_at",
    "version",
];
const USAGE_COLUMNS: &[&str] = &[
    "capability_id",
    "created_at",
    "examples",
    "id",
    "metadata",
    "name",
    "prompt",
    "updated_at",
];

type CapabilityIds = BTreeSet<String>;
type CapabilityNameIndex = HashMap<String, Vec<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DuckdbMigrationReason {
    MissingDescription,
    MissingVersion,
    MissingCompositeSchemaIn,
    MissingCompositeSchemaOut,
    MissingCompositeExecutor,
    InvalidToolAuthorization,
    MissingCapabilityId,
    UnknownCapabilityId,
    DuplicateCapabilityId,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DuckdbMigrationIssue {
    pub table: String,
    pub id: String,
    pub reason: DuckdbMigrationReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryTableDisposition {
    pub table: String,
    pub rows: u64,
    pub quarantined: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuckdbValidationReport {
    pub table_counts: BTreeMap<String, u64>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DuckdbMigrationReport {
    pub fresh: bool,
    pub source_counts: BTreeMap<String, u64>,
    pub target_counts: BTreeMap<String, u64>,
    pub memory: Vec<MemoryTableDisposition>,
    pub issues: Vec<DuckdbMigrationIssue>,
    workspace_rows: Vec<WorkspaceRow>,
}

impl DuckdbMigrationReport {
    pub fn workspace_rows(&self) -> &[WorkspaceRow] {
        &self.workspace_rows
    }

    pub fn into_workspace_rows(self) -> Vec<WorkspaceRow> {
        self.workspace_rows
    }
}

impl fmt::Debug for DuckdbMigrationReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DuckdbMigrationReport")
            .field("fresh", &self.fresh)
            .field("source_counts", &self.source_counts)
            .field("target_counts", &self.target_counts)
            .field("memory", &self.memory)
            .field("issues", &self.issues)
            .field("workspace_count", &self.workspace_rows.len())
            .finish()
    }
}

#[derive(Debug)]
struct BaseSourceRow {
    id: String,
    name: String,
    cap_type: String,
    schema_in: String,
    schema_out: String,
    executor: String,
    metadata: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug)]
struct CompositeSourceRow {
    id: String,
    name: String,
    dag: String,
    metadata: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Default)]
struct ContractFields {
    description: String,
    version: String,
    enabled: bool,
    tombstoned_at: Option<String>,
    schema_in: Option<String>,
    schema_out: Option<String>,
    executor: Option<String>,
}

struct ScratchDirectory {
    path: PathBuf,
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct CandidateFileGuard {
    path: PathBuf,
    keep: bool,
}

impl Drop for CandidateFileGuard {
    fn drop(&mut self) {
        if !self.keep {
            remove_file_family(&self.path);
        }
    }
}

pub fn build_duckdb_candidate(
    backup: Option<&VerifiedBackup>,
    staging_root: &Path,
) -> Result<DuckdbMigrationReport> {
    secure_staging_root(staging_root)?;
    reject_existing_candidate_family(staging_root)?;

    let temporary_candidate =
        staging_root.join(format!(".duckdb-candidate-{}.tmp", Uuid::new_v4().simple()));
    let mut candidate_guard = CandidateFileGuard {
        path: temporary_candidate.clone(),
        keep: false,
    };

    let mut report = if let Some(backup) = backup {
        let scratch = copy_backup_duckdb_to_scratch(backup, staging_root)?;
        let source_path = scratch.path.join(CANDIDATE_DUCKDB_FILE);
        let report = build_from_legacy(&source_path, &temporary_candidate)?;
        drop(scratch);
        report
    } else {
        build_fresh(&temporary_candidate)?
    };

    secure_candidate_file(&temporary_candidate)?;
    let validation = validate_current_duckdb(&temporary_candidate)?;
    if validation.table_counts != report.target_counts {
        return Err(migration_error(
            "candidate row counts differ from the migration report",
        ));
    }

    let final_path = staging_root.join(CANDIDATE_DUCKDB_FILE);
    fs::rename(&temporary_candidate, &final_path)
        .map_err(|error| migration_error(format!("cannot publish candidate: {error}")))?;
    candidate_guard.path = final_path.clone();
    secure_candidate_file(&final_path)?;
    sync_directory(staging_root)?;
    report.target_counts = validate_current_duckdb(&final_path)?.table_counts;
    candidate_guard.keep = true;
    Ok(report)
}

pub fn validate_current_duckdb(path: &Path) -> Result<DuckdbValidationReport> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| migration_error(format!("cannot inspect candidate: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(migration_error("candidate is not a regular file"));
    }
    verify_file_mode(&metadata)?;
    let connection = duckdb::Connection::open(path)
        .map_err(|error| migration_error(format!("cannot open candidate: {error}")))?;
    validate_current_duckdb_connection(&connection)
}

pub fn validate_current_duckdb_connection(
    connection: &duckdb::Connection,
) -> Result<DuckdbValidationReport> {
    require_exact_tables(connection, TARGET_TABLES)?;
    require_exact_columns(connection, "model", MODEL_COLUMNS)?;
    require_exact_columns(connection, "agent", AGENT_COLUMNS)?;
    require_exact_columns(connection, "base_capability", BASE_COLUMNS)?;
    require_exact_columns(connection, "composite_capability", COMPOSITE_COLUMNS)?;
    require_exact_columns(connection, "usage_method", USAGE_COLUMNS)?;

    let table_counts = table_counts(connection, TARGET_TABLES)?;
    Ok(DuckdbValidationReport { table_counts })
}

fn build_fresh(candidate_path: &Path) -> Result<DuckdbMigrationReport> {
    let connection = duckdb::Connection::open(candidate_path)
        .map_err(|error| migration_error(format!("cannot create fresh candidate: {error}")))?;
    connection
        .execute_batch(TARGET_SCHEMA)
        .map_err(|error| migration_error(format!("cannot create target schema: {error}")))?;
    let target_counts = validate_current_duckdb_connection(&connection)?.table_counts;
    drop(connection);
    Ok(DuckdbMigrationReport {
        fresh: true,
        source_counts: BTreeMap::new(),
        target_counts,
        memory: Vec::new(),
        issues: Vec::new(),
        workspace_rows: Vec::new(),
    })
}

fn build_from_legacy(source_path: &Path, candidate_path: &Path) -> Result<DuckdbMigrationReport> {
    let source = duckdb::Connection::open(source_path)
        .map_err(|error| migration_error(format!("cannot open legacy scratch copy: {error}")))?;
    validate_legacy_schema(&source)?;
    let source_counts = table_counts(&source, LEGACY_TABLES)?;

    let target = duckdb::Connection::open(candidate_path)
        .map_err(|error| migration_error(format!("cannot create candidate: {error}")))?;
    target
        .execute_batch(TARGET_SCHEMA)
        .map_err(|error| migration_error(format!("cannot create target schema: {error}")))?;

    let mut issues = Vec::new();
    migrate_models(&source, &target)?;
    let (all_capability_ids, executable_capability_ids, capability_names) =
        migrate_capabilities(&source, &target, &mut issues)?;
    migrate_agents(
        &source,
        &target,
        &executable_capability_ids,
        &capability_names,
        &mut issues,
    )?;
    migrate_usage_methods(
        &source,
        &target,
        &all_capability_ids,
        &executable_capability_ids,
        &mut issues,
    )?;
    let workspace_rows = read_workspaces(&source)?;
    let memory = memory_dispositions(&source)?;

    issues.sort();
    let target_counts = validate_current_duckdb_connection(&target)?.table_counts;
    drop(target);
    drop(source);
    Ok(DuckdbMigrationReport {
        fresh: false,
        source_counts,
        target_counts,
        memory,
        issues,
        workspace_rows,
    })
}

fn validate_legacy_schema(connection: &duckdb::Connection) -> Result<()> {
    require_exact_tables(connection, LEGACY_TABLES)?;

    const LEGACY_MODEL_REQUIRED: &[&str] = &[
        "api_key",
        "api_type",
        "api_url",
        "config",
        "created_at",
        "id",
        "model_id",
        "name",
        "provider",
        "updated_at",
    ];
    let required: &[(&str, &[&str])] = &[
        ("model", LEGACY_MODEL_REQUIRED),
        (
            "agent",
            &[
                "config",
                "created_at",
                "display_name",
                "id",
                "is_default",
                "mode",
                "name",
                "prompt",
                "tools",
                "updated_at",
            ],
        ),
        (
            "base_capability",
            &[
                "created_at",
                "executor",
                "id",
                "metadata",
                "name",
                "schema_in",
                "schema_out",
                "type",
                "updated_at",
            ],
        ),
        (
            "composite_capability",
            &["created_at", "dag", "id", "metadata", "name", "updated_at"],
        ),
        (
            "usage_method",
            &[
                "created_at",
                "examples",
                "id",
                "metadata",
                "name",
                "prompt",
                "updated_at",
            ],
        ),
        (
            "workspace",
            &[
                "created_at",
                "id",
                "is_default",
                "name",
                "path",
                "updated_at",
            ],
        ),
        (
            "attention_kv",
            &["key", "session_id", "updated_at", "value"],
        ),
        (
            "cognitive",
            &["created_at", "edges", "id", "label", "nodes"],
        ),
        (
            "experience_rag",
            &[
                "created_at",
                "id",
                "input",
                "output",
                "success",
                "tags",
                "task_type",
            ],
        ),
        (
            "preference_rag",
            &["content", "created_at", "embedding", "id", "metadata"],
        ),
        (
            "raw_files",
            &[
                "created_at",
                "file_path",
                "id",
                "metadata",
                "mime_type",
                "sha256",
                "size_bytes",
            ],
        ),
    ];
    for (table, columns) in required {
        require_columns(connection, table, columns)?;
    }
    Ok(())
}

fn migrate_models(source: &duckdb::Connection, target: &duckdb::Connection) -> Result<()> {
    let mut statement = source
        .prepare(
            "SELECT id, name, provider, api_url, api_type, api_key, model_id, \
             CAST(config AS VARCHAR), CAST(created_at AS VARCHAR), CAST(updated_at AS VARCHAR) \
             FROM model ORDER BY id",
        )
        .map_err(|error| migration_error(format!("cannot read legacy model table: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })
        .map_err(|error| migration_error(format!("cannot query legacy model table: {error}")))?;
    for row in rows {
        let (id, name, provider, api_url, api_type, api_key, model_id, config, created, updated) =
            row.map_err(|error| migration_error(format!("cannot decode model row: {error}")))?;
        target
            .execute(
                "INSERT INTO model \
                 (id, name, provider, api_url, api_type, api_key, model_id, config, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, CAST(? AS JSON), TRY_CAST(? AS TIMESTAMP), TRY_CAST(? AS TIMESTAMP))",
                duckdb::params![
                    id, name, provider, api_url, api_type, api_key, model_id, config, created, updated
                ],
            )
            .map_err(|error| migration_error(format!("cannot migrate model row: {error}")))?;
    }
    Ok(())
}

fn migrate_capabilities(
    source: &duckdb::Connection,
    target: &duckdb::Connection,
    issues: &mut Vec<DuckdbMigrationIssue>,
) -> Result<(CapabilityIds, CapabilityIds, CapabilityNameIndex)> {
    let base_rows = read_base_rows(source)?;
    let composite_rows = read_composite_rows(source)?;
    let mut all_ids = BTreeSet::new();
    let mut executable_ids = BTreeSet::new();
    let mut names = CapabilityNameIndex::new();
    for id in base_rows
        .iter()
        .map(|row| &row.id)
        .chain(composite_rows.iter().map(|row| &row.id))
    {
        if !all_ids.insert(id.clone()) {
            return Err(migration_error(
                "capability ids are not globally unique across legacy tables",
            ));
        }
    }

    for row in base_rows {
        let contract = extract_contract(
            "base_capability",
            &row.id,
            row.metadata.as_deref(),
            false,
            issues,
        );
        if contract.enabled {
            executable_ids.insert(row.id.clone());
            names
                .entry(row.name.clone())
                .or_default()
                .push(row.id.clone());
        }
        target
            .execute(
                "INSERT INTO base_capability \
                 (id, name, type, description, schema_in, schema_out, executor, version, enabled, \
                  tombstoned_at, metadata, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, ?, ?, \
                         TRY_CAST(? AS TIMESTAMP), CAST(? AS JSON), TRY_CAST(? AS TIMESTAMP), \
                         TRY_CAST(? AS TIMESTAMP))",
                duckdb::params![
                    row.id,
                    row.name,
                    row.cap_type,
                    contract.description,
                    row.schema_in,
                    row.schema_out,
                    row.executor,
                    contract.version,
                    contract.enabled,
                    contract.tombstoned_at,
                    row.metadata,
                    row.created_at,
                    row.updated_at
                ],
            )
            .map_err(|error| {
                migration_error(format!("cannot migrate base capability row: {error}"))
            })?;
    }

    for row in composite_rows {
        let contract = extract_contract(
            "composite_capability",
            &row.id,
            row.metadata.as_deref(),
            true,
            issues,
        );
        if contract.enabled {
            executable_ids.insert(row.id.clone());
            names
                .entry(row.name.clone())
                .or_default()
                .push(row.id.clone());
        }
        target
            .execute(
                "INSERT INTO composite_capability \
                 (id, name, description, schema_in, schema_out, executor, dag, version, enabled, \
                  tombstoned_at, metadata, created_at, updated_at) \
                 VALUES (?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, CAST(? AS JSON), ?, ?, \
                         TRY_CAST(? AS TIMESTAMP), CAST(? AS JSON), TRY_CAST(? AS TIMESTAMP), \
                         TRY_CAST(? AS TIMESTAMP))",
                duckdb::params![
                    row.id,
                    row.name,
                    contract.description,
                    contract.schema_in,
                    contract.schema_out,
                    contract.executor,
                    row.dag,
                    contract.version,
                    contract.enabled,
                    contract.tombstoned_at,
                    row.metadata,
                    row.created_at,
                    row.updated_at
                ],
            )
            .map_err(|error| {
                migration_error(format!("cannot migrate composite capability row: {error}"))
            })?;
    }
    for mapped_ids in names.values_mut() {
        mapped_ids.sort();
    }
    Ok((all_ids, executable_ids, names))
}

fn read_base_rows(connection: &duckdb::Connection) -> Result<Vec<BaseSourceRow>> {
    let mut statement = connection
        .prepare(
            "SELECT id, name, type, CAST(schema_in AS VARCHAR), CAST(schema_out AS VARCHAR), \
             executor, CAST(metadata AS VARCHAR), CAST(created_at AS VARCHAR), \
             CAST(updated_at AS VARCHAR) FROM base_capability ORDER BY id",
        )
        .map_err(|error| migration_error(format!("cannot read base capabilities: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok(BaseSourceRow {
                id: row.get(0)?,
                name: row.get(1)?,
                cap_type: row.get(2)?,
                schema_in: row.get(3)?,
                schema_out: row.get(4)?,
                executor: row.get(5)?,
                metadata: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })
        .map_err(|error| migration_error(format!("cannot query base capabilities: {error}")))?;
    rows.map(|row| {
        row.map_err(|error| migration_error(format!("cannot decode base capability: {error}")))
    })
    .collect()
}

fn read_composite_rows(connection: &duckdb::Connection) -> Result<Vec<CompositeSourceRow>> {
    let mut statement = connection
        .prepare(
            "SELECT id, name, CAST(dag AS VARCHAR), CAST(metadata AS VARCHAR), \
             CAST(created_at AS VARCHAR), CAST(updated_at AS VARCHAR) \
             FROM composite_capability ORDER BY id",
        )
        .map_err(|error| migration_error(format!("cannot read composite capabilities: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok(CompositeSourceRow {
                id: row.get(0)?,
                name: row.get(1)?,
                dag: row.get(2)?,
                metadata: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })
        .map_err(|error| {
            migration_error(format!("cannot query composite capabilities: {error}"))
        })?;
    rows.map(|row| {
        row.map_err(|error| migration_error(format!("cannot decode composite capability: {error}")))
    })
    .collect()
}

fn extract_contract(
    table: &str,
    id: &str,
    metadata: Option<&str>,
    composite: bool,
    issues: &mut Vec<DuckdbMigrationIssue>,
) -> ContractFields {
    let parsed = metadata
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let description = parsed
        .get("description")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_default()
        .to_string();
    let version = parsed
        .get("version")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_default()
        .to_string();
    let mut complete = true;
    if description.is_empty() {
        complete = false;
        push_issue(issues, table, id, DuckdbMigrationReason::MissingDescription);
    }
    if version.is_empty() {
        complete = false;
        push_issue(issues, table, id, DuckdbMigrationReason::MissingVersion);
    }

    let mut fields = ContractFields {
        description,
        version,
        enabled: parsed
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tombstoned_at: parsed
            .get("tombstoned_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        ..ContractFields::default()
    };
    if composite {
        fields.schema_in = parsed
            .get("schema_in")
            .filter(|value| value.is_object())
            .and_then(|value| serde_json::to_string(value).ok());
        fields.schema_out = parsed
            .get("schema_out")
            .filter(|value| value.is_object())
            .and_then(|value| serde_json::to_string(value).ok());
        fields.executor = parsed
            .get("executor")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        if fields.schema_in.is_none() {
            complete = false;
            push_issue(
                issues,
                table,
                id,
                DuckdbMigrationReason::MissingCompositeSchemaIn,
            );
        }
        if fields.schema_out.is_none() {
            complete = false;
            push_issue(
                issues,
                table,
                id,
                DuckdbMigrationReason::MissingCompositeSchemaOut,
            );
        }
        if fields.executor.is_none() {
            complete = false;
            push_issue(
                issues,
                table,
                id,
                DuckdbMigrationReason::MissingCompositeExecutor,
            );
        }
    }
    fields.enabled &= complete && fields.tombstoned_at.is_none();
    fields
}

fn migrate_agents(
    source: &duckdb::Connection,
    target: &duckdb::Connection,
    capability_ids: &CapabilityIds,
    capability_names: &CapabilityNameIndex,
    issues: &mut Vec<DuckdbMigrationIssue>,
) -> Result<()> {
    let mut statement = source
        .prepare(
            "SELECT id, name, mode, prompt, CAST(tools AS VARCHAR), CAST(config AS VARCHAR), \
             display_name, is_default, CAST(created_at AS VARCHAR), CAST(updated_at AS VARCHAR) \
             FROM agent ORDER BY id",
        )
        .map_err(|error| migration_error(format!("cannot read legacy agents: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<bool>>(7)?.unwrap_or(false),
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })
        .map_err(|error| migration_error(format!("cannot query legacy agents: {error}")))?;
    for row in rows {
        let (id, name, mode, prompt, tools, config, display_name, is_default, created, updated) =
            row.map_err(|error| migration_error(format!("cannot decode agent row: {error}")))?;
        let tool_caps = migrate_tool_caps(
            &id,
            tools.as_deref(),
            capability_ids,
            capability_names,
            issues,
        );
        target
            .execute(
                "INSERT INTO agent \
                 (id, name, mode, prompt, tool_caps, config, display_name, is_default, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, ?, \
                         TRY_CAST(? AS TIMESTAMP), TRY_CAST(? AS TIMESTAMP))",
                duckdb::params![
                    id,
                    name,
                    mode,
                    prompt,
                    tool_caps,
                    config,
                    display_name,
                    is_default,
                    created,
                    updated
                ],
            )
            .map_err(|error| migration_error(format!("cannot migrate agent row: {error}")))?;
    }
    Ok(())
}

fn migrate_tool_caps(
    agent_id: &str,
    tools: Option<&str>,
    capability_ids: &CapabilityIds,
    capability_names: &CapabilityNameIndex,
    issues: &mut Vec<DuckdbMigrationIssue>,
) -> String {
    let Some(tools) = tools else {
        return "[]".to_string();
    };
    let Ok(Value::Array(values)) = serde_json::from_str::<Value>(tools) else {
        push_issue(
            issues,
            "agent",
            agent_id,
            DuckdbMigrationReason::InvalidToolAuthorization,
        );
        return "[]".to_string();
    };
    let mut resolved = BTreeSet::new();
    for value in values {
        let Some(reference) = value.as_str().filter(|value| !value.trim().is_empty()) else {
            push_issue(
                issues,
                "agent",
                agent_id,
                DuckdbMigrationReason::InvalidToolAuthorization,
            );
            return "[]".to_string();
        };
        if capability_ids.contains(reference) {
            resolved.insert(reference.to_string());
            continue;
        }
        match capability_names.get(reference).map(Vec::as_slice) {
            Some([id]) => {
                resolved.insert(id.clone());
            }
            _ => {
                push_issue(
                    issues,
                    "agent",
                    agent_id,
                    DuckdbMigrationReason::InvalidToolAuthorization,
                );
                return "[]".to_string();
            }
        }
    }
    serde_json::to_string(&resolved.into_iter().collect::<Vec<_>>())
        .expect("serializing capability ids cannot fail")
}

fn migrate_usage_methods(
    source: &duckdb::Connection,
    target: &duckdb::Connection,
    all_capability_ids: &CapabilityIds,
    executable_capability_ids: &CapabilityIds,
    issues: &mut Vec<DuckdbMigrationIssue>,
) -> Result<()> {
    let mut statement = source
        .prepare(
            "SELECT id, name, prompt, CAST(examples AS VARCHAR), CAST(metadata AS VARCHAR), \
             CAST(created_at AS VARCHAR), CAST(updated_at AS VARCHAR) \
             FROM usage_method ORDER BY id",
        )
        .map_err(|error| migration_error(format!("cannot read usage methods: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(|error| migration_error(format!("cannot query usage methods: {error}")))?;
    for row in rows {
        let (id, name, prompt, examples, metadata, created, updated) =
            row.map_err(|error| migration_error(format!("cannot decode usage method: {error}")))?;
        if all_capability_ids.contains(&id) {
            push_issue(
                issues,
                "usage_method",
                &id,
                DuckdbMigrationReason::DuplicateCapabilityId,
            );
            continue;
        }
        let capability_id = metadata
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| {
                value
                    .get("capability_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_string)
            });
        let Some(capability_id) = capability_id else {
            push_issue(
                issues,
                "usage_method",
                &id,
                DuckdbMigrationReason::MissingCapabilityId,
            );
            continue;
        };
        if !executable_capability_ids.contains(&capability_id) {
            push_issue(
                issues,
                "usage_method",
                &id,
                DuckdbMigrationReason::UnknownCapabilityId,
            );
            continue;
        }
        target
            .execute(
                "INSERT INTO usage_method \
                 (id, capability_id, name, prompt, examples, metadata, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, CAST(? AS JSON), CAST(? AS JSON), \
                         TRY_CAST(? AS TIMESTAMP), TRY_CAST(? AS TIMESTAMP))",
                duckdb::params![
                    id,
                    capability_id,
                    name,
                    prompt,
                    examples,
                    metadata,
                    created,
                    updated
                ],
            )
            .map_err(|error| migration_error(format!("cannot migrate usage method: {error}")))?;
    }
    Ok(())
}

fn read_workspaces(connection: &duckdb::Connection) -> Result<Vec<WorkspaceRow>> {
    let mut statement = connection
        .prepare("SELECT id, name, path, is_default FROM workspace ORDER BY id")
        .map_err(|error| migration_error(format!("cannot read workspaces: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok(WorkspaceRow {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                is_default: row.get::<_, Option<bool>>(3)?.unwrap_or(false),
            })
        })
        .map_err(|error| migration_error(format!("cannot query workspaces: {error}")))?;
    let mut workspaces: Vec<WorkspaceRow> = rows
        .map(|row| {
            row.map_err(|error| migration_error(format!("cannot decode workspace: {error}")))
        })
        .collect::<Result<_>>()?;
    workspaces.sort_by(|left, right| left.id.cmp(&right.id));
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut defaults = 0_usize;
    for workspace in &workspaces {
        if workspace.id.trim().is_empty()
            || workspace.name.trim().is_empty()
            || workspace.path.trim().is_empty()
            || !ids.insert(workspace.id.as_str())
            || !paths.insert(workspace.path.as_str())
        {
            return Err(migration_error("legacy workspace rows are not importable"));
        }
        defaults += usize::from(workspace.is_default);
    }
    if defaults > 1 {
        return Err(migration_error(
            "legacy workspace rows contain multiple defaults",
        ));
    }
    Ok(workspaces)
}

fn memory_dispositions(connection: &duckdb::Connection) -> Result<Vec<MemoryTableDisposition>> {
    MEMORY_TABLES
        .iter()
        .map(|table| {
            let rows = table_count(connection, table)?;
            Ok(MemoryTableDisposition {
                table: (*table).to_string(),
                rows,
                quarantined: rows,
            })
        })
        .collect()
}

fn push_issue(
    issues: &mut Vec<DuckdbMigrationIssue>,
    table: &str,
    id: &str,
    reason: DuckdbMigrationReason,
) {
    issues.push(DuckdbMigrationIssue {
        table: table.to_string(),
        id: id.to_string(),
        reason,
    });
}

fn require_exact_tables(connection: &duckdb::Connection, expected: &[&str]) -> Result<()> {
    let mut statement = connection
        .prepare(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'main' AND table_type = 'BASE TABLE' ORDER BY table_name",
        )
        .map_err(|error| migration_error(format!("cannot inspect table set: {error}")))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| migration_error(format!("cannot query table set: {error}")))?;
    let actual: Vec<String> = rows
        .map(|row| {
            row.map_err(|error| migration_error(format!("cannot decode table set: {error}")))
        })
        .collect::<Result<_>>()?;
    let expected: Vec<String> = expected.iter().map(|table| (*table).to_string()).collect();
    if actual != expected {
        return Err(migration_error("DuckDB table set is not recognized"));
    }
    Ok(())
}

fn require_columns(connection: &duckdb::Connection, table: &str, required: &[&str]) -> Result<()> {
    let actual = column_set(connection, table)?;
    if required.iter().any(|column| !actual.contains(*column)) {
        return Err(migration_error(format!(
            "DuckDB table {table} is missing required columns"
        )));
    }
    Ok(())
}

fn require_exact_columns(
    connection: &duckdb::Connection,
    table: &str,
    expected: &[&str],
) -> Result<()> {
    let actual = column_set(connection, table)?;
    let expected: BTreeSet<String> = expected
        .iter()
        .map(|column| (*column).to_string())
        .collect();
    if actual != expected {
        return Err(migration_error(format!(
            "candidate table {table} does not have the exact target columns"
        )));
    }
    Ok(())
}

fn column_set(connection: &duckdb::Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = connection
        .prepare(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'main' AND table_name = ? ORDER BY column_name",
        )
        .map_err(|error| migration_error(format!("cannot inspect table columns: {error}")))?;
    let rows = statement
        .query_map(duckdb::params![table], |row| row.get::<_, String>(0))
        .map_err(|error| migration_error(format!("cannot query table columns: {error}")))?;
    rows.map(|row| {
        row.map_err(|error| migration_error(format!("cannot decode table columns: {error}")))
    })
    .collect()
}

fn table_counts(connection: &duckdb::Connection, tables: &[&str]) -> Result<BTreeMap<String, u64>> {
    tables
        .iter()
        .map(|table| Ok(((*table).to_string(), table_count(connection, table)?)))
        .collect()
}

fn table_count(connection: &duckdb::Connection, table: &str) -> Result<u64> {
    if !LEGACY_TABLES.contains(&table) && !TARGET_TABLES.contains(&table) {
        return Err(migration_error("refusing to count an unknown table"));
    }
    let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
    let count: i64 = connection
        .query_row(&sql, [], |row| row.get(0))
        .map_err(|error| migration_error(format!("cannot count table {table}: {error}")))?;
    u64::try_from(count).map_err(|_| migration_error("table count is negative"))
}

fn copy_backup_duckdb_to_scratch(
    backup: &VerifiedBackup,
    staging_root: &Path,
) -> Result<ScratchDirectory> {
    let scratch_path = staging_root.join(format!(".legacy-duckdb-{}", Uuid::new_v4().simple()));
    fs::create_dir(&scratch_path)
        .map_err(|error| migration_error(format!("cannot create legacy scratch: {error}")))?;
    crate::data::permissions::ensure_private_directory(&scratch_path)
        .map_err(|_| migration_error("cannot secure legacy scratch directory"))?;
    let scratch = ScratchDirectory { path: scratch_path };

    let family: Vec<_> = backup
        .manifest
        .files
        .iter()
        .filter(|entry| !entry.path.contains('/') && entry.path.starts_with(DUCKDB_PREFIX))
        .collect();
    if !family
        .iter()
        .any(|entry| entry.path == CANDIDATE_DUCKDB_FILE)
    {
        return Err(migration_error(
            "verified backup does not contain the DuckDB main file",
        ));
    }

    let actual_family = backup_family_names(&backup.backup_dir)?;
    let manifest_family: BTreeSet<String> = family.iter().map(|entry| entry.path.clone()).collect();
    if actual_family != manifest_family {
        return Err(migration_error(
            "verified backup DuckDB file inventory changed",
        ));
    }

    for entry in family {
        let source = backup.backup_dir.join(&entry.path);
        let destination = scratch.path.join(&entry.path);
        copy_verified_file(&source, &destination, &entry.sha256, entry.bytes)?;
    }
    Ok(scratch)
}

fn backup_family_names(root: &Path) -> Result<BTreeSet<String>> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| migration_error(format!("cannot inspect backup root: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(migration_error("verified backup root changed type"));
    }
    let entries = fs::read_dir(root)
        .map_err(|error| migration_error(format!("cannot list backup root: {error}")))?;
    let mut family = BTreeSet::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| migration_error(format!("cannot inspect backup entry: {error}")))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(DUCKDB_PREFIX) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            migration_error(format!("cannot inspect backup DuckDB file: {error}"))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(migration_error(
                "verified backup DuckDB family contains a non-file",
            ));
        }
        family.insert(name);
    }
    Ok(family)
}

fn copy_verified_file(
    source: &Path,
    destination: &Path,
    expected_hash: &str,
    expected_bytes: u64,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| migration_error(format!("cannot inspect backup file: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(migration_error("backup source is not a regular file"));
    }
    let mut input = File::open(source)
        .map_err(|error| migration_error(format!("cannot open backup file: {error}")))?;
    let mut output = create_private_file(destination)
        .map_err(|error| migration_error(format!("cannot create scratch file: {error}")))?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| migration_error(format!("cannot read backup file: {error}")))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| migration_error(format!("cannot write scratch file: {error}")))?;
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| migration_error("backup file size overflow"))?;
    }
    output
        .sync_all()
        .map_err(|error| migration_error(format!("cannot sync scratch file: {error}")))?;
    drop(output);
    crate::data::permissions::secure_existing_file(destination)
        .map_err(|_| migration_error("cannot secure scratch file"))?;
    let actual_hash = hex_digest(hasher);
    if bytes != expected_bytes || actual_hash != expected_hash {
        return Err(migration_error(
            "backup DuckDB file no longer matches its manifest",
        ));
    }
    Ok(())
}

fn secure_staging_root(path: &Path) -> Result<()> {
    crate::data::permissions::ensure_private_directory(path)
        .map_err(|_| migration_error("cannot create or secure staging root"))
}

fn reject_existing_candidate_family(staging_root: &Path) -> Result<()> {
    let entries = fs::read_dir(staging_root)
        .map_err(|error| migration_error(format!("cannot list staging root: {error}")))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| migration_error(format!("cannot inspect staging entry: {error}")))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(DUCKDB_PREFIX)
        {
            return Err(migration_error(
                "staging root already contains a candidate DuckDB file family",
            ));
        }
    }
    Ok(())
}

fn secure_candidate_file(path: &Path) -> Result<()> {
    crate::data::permissions::secure_existing_file(path)
        .map_err(|_| migration_error("cannot secure candidate DuckDB file"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| migration_error(format!("cannot inspect candidate file: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(migration_error(
            "candidate DuckDB path is not a regular file",
        ));
    }
    verify_file_mode(&metadata)
}

#[cfg(unix)]
fn verify_file_mode(metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o7777 == 0o600 {
        Ok(())
    } else {
        Err(migration_error("candidate DuckDB file is not mode 0600"))
    }
}

#[cfg(not(unix))]
fn verify_file_mode(_metadata: &fs::Metadata) -> Result<()> {
    Ok(())
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

fn remove_file_family(main_path: &Path) {
    let Some(parent) = main_path.parent() else {
        return;
    };
    let Some(prefix) = main_path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| migration_error(format!("cannot sync staging root: {error}")))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn hex_digest(hasher: Sha256) -> String {
    use std::fmt::Write as _;

    let digest = hasher.finalize();
    let mut output = String::with_capacity(64);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn migration_error(message: impl Into<String>) -> AgentError {
    AgentError::Bootstrap(format!("DuckDB migration: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::migration::ensure_verified_backup;

    const LEGACY_SCHEMA: &str = include_str!("fixtures/legacy_duckdb_v1.sql");

    fn roots() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let staging = temporary.path().join("staging");
        fs::create_dir(&source).unwrap();
        (temporary, source, staging)
    }

    fn create_legacy(source_root: &Path) -> duckdb::Connection {
        let connection = duckdb::Connection::open(source_root.join(CANDIDATE_DUCKDB_FILE)).unwrap();
        connection.execute_batch(LEGACY_SCHEMA).unwrap();
        connection
    }

    fn seeded_backup(source_root: &Path) -> VerifiedBackup {
        let connection = create_legacy(source_root);
        connection
            .execute(
                "INSERT INTO model \
                 (id, name, provider, api_url, api_type, api_key, model_id, config) \
                 VALUES ('model-1', 'Model', 'provider', 'https://invalid.example', 'OpenAI', \
                         'sk-secret-never-report', 'remote-model', '{}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO base_capability \
                 (id, name, type, schema_in, schema_out, executor, metadata) VALUES \
                 ('cap.read', 'Reader', 'script', '{}', '{}', 'reader.wasm', \
                  '{\"description\":\"Read data\",\"version\":\"1\",\"enabled\":true}'), \
                 ('cap.disabled', 'Disabled', 'script', '{}', '{}', 'disabled.wasm', '{}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO composite_capability (id, name, dag, metadata) VALUES \
                 ('cap.combo', 'Combo', '{}', \
                  '{\"description\":\"Compose\",\"version\":\"1\",\"enabled\":true,\
                    \"schema_in\":{},\"schema_out\":{},\"executor\":\"combo.wasm\"}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO agent (id, name, mode, tools) VALUES \
                 ('agent-valid', 'Valid', 'unni', '[\"cap.read\",\"Reader\"]'), \
                 ('agent-invalid', 'Invalid', 'unni', '[\"unknown\"]')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO usage_method (id, name, prompt, metadata) VALUES \
                 ('usage-valid', 'Use read', 'read', '{\"capability_id\":\"cap.read\"}'), \
                 ('usage-invalid', 'Unknown', 'unknown', '{\"capability_id\":\"missing\"}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO workspace (id, name, path, is_default) \
                 VALUES ('workspace-1', 'Workspace', '/private/project', true)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO attention_kv (session_id, key, value) VALUES ('legacy', 'focus', '{}')",
                [],
            )
            .unwrap();
        drop(connection);
        ensure_verified_backup(source_root).unwrap()
    }

    #[test]
    fn report_fields_have_stable_serialization() {
        let reasons = [
            DuckdbMigrationReason::MissingDescription,
            DuckdbMigrationReason::MissingVersion,
            DuckdbMigrationReason::MissingCompositeSchemaIn,
            DuckdbMigrationReason::MissingCompositeSchemaOut,
            DuckdbMigrationReason::MissingCompositeExecutor,
            DuckdbMigrationReason::InvalidToolAuthorization,
            DuckdbMigrationReason::MissingCapabilityId,
            DuckdbMigrationReason::UnknownCapabilityId,
            DuckdbMigrationReason::DuplicateCapabilityId,
        ];
        assert_eq!(
            serde_json::to_value(reasons).unwrap(),
            serde_json::json!([
                "missing_description",
                "missing_version",
                "missing_composite_schema_in",
                "missing_composite_schema_out",
                "missing_composite_executor",
                "invalid_tool_authorization",
                "missing_capability_id",
                "unknown_capability_id",
                "duplicate_capability_id"
            ])
        );

        let issue = DuckdbMigrationIssue {
            table: "base_capability".to_string(),
            id: "cap.read".to_string(),
            reason: DuckdbMigrationReason::MissingVersion,
        };
        assert_eq!(
            serde_json::to_value(issue).unwrap(),
            serde_json::json!({
                "table": "base_capability",
                "id": "cap.read",
                "reason": "missing_version"
            })
        );

        let disposition = MemoryTableDisposition {
            table: "attention_kv".to_string(),
            rows: 3,
            quarantined: 3,
        };
        assert_eq!(
            serde_json::to_value(disposition).unwrap(),
            serde_json::json!({
                "table": "attention_kv",
                "rows": 3,
                "quarantined": 3
            })
        );
    }

    #[test]
    fn fresh_candidate_has_exact_five_empty_tables() {
        let (_temporary, _source, staging) = roots();
        let report = build_duckdb_candidate(None, &staging).unwrap();
        assert!(report.fresh);
        assert_eq!(report.target_counts.len(), 5);
        assert!(report.target_counts.values().all(|count| *count == 0));
        let validation = validate_current_duckdb(&staging.join(CANDIDATE_DUCKDB_FILE)).unwrap();
        assert_eq!(validation.table_counts, report.target_counts);

        let connection = duckdb::Connection::open(staging.join(CANDIDATE_DUCKDB_FILE)).unwrap();
        let agent_columns = column_set(&connection, "agent").unwrap();
        assert!(agent_columns.contains("tool_caps"));
        assert!(!agent_columns.contains("tools"));
    }

    #[test]
    fn legacy_rows_migrate_conservatively_and_memory_is_quarantined() {
        let (_temporary, source, staging) = roots();
        let backup = seeded_backup(&source);
        let backup_hash_before = fs::read(backup.backup_dir.join(CANDIDATE_DUCKDB_FILE)).unwrap();
        let report = build_duckdb_candidate(Some(&backup), &staging).unwrap();
        assert!(!report.fresh);
        assert_eq!(report.target_counts["model"], 1);
        assert_eq!(report.target_counts["agent"], 2);
        assert_eq!(report.target_counts["base_capability"], 2);
        assert_eq!(report.target_counts["composite_capability"], 1);
        assert_eq!(report.target_counts["usage_method"], 1);
        assert_eq!(report.workspace_rows().len(), 1);
        assert_eq!(report.memory[0].table, "attention_kv");
        assert_eq!(report.memory[0].quarantined, 1);

        let candidate = duckdb::Connection::open(staging.join(CANDIDATE_DUCKDB_FILE)).unwrap();
        let (description, version, enabled): (String, String, bool) = candidate
            .query_row(
                "SELECT description, version, enabled FROM base_capability WHERE id='cap.disabled'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(description.is_empty());
        assert!(version.is_empty());
        assert!(!enabled);

        let valid_tools: String = candidate
            .query_row(
                "SELECT CAST(tool_caps AS VARCHAR) FROM agent WHERE id='agent-valid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&valid_tools).unwrap(),
            serde_json::json!(["cap.read"])
        );
        let invalid_tools: String = candidate
            .query_row(
                "SELECT CAST(tool_caps AS VARCHAR) FROM agent WHERE id='agent-invalid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&invalid_tools).unwrap(),
            serde_json::json!([])
        );
        drop(candidate);

        assert_eq!(
            fs::read(backup.backup_dir.join(CANDIDATE_DUCKDB_FILE)).unwrap(),
            backup_hash_before
        );
        let debug = format!("{report:?}");
        assert!(!debug.contains("sk-secret-never-report"));
        assert!(!debug.contains("/private/project"));
        assert!(!debug.contains(&staging.to_string_lossy().to_string()));
    }

    #[test]
    fn unknown_legacy_table_set_blocks_candidate() {
        let (_temporary, source, staging) = roots();
        let connection = create_legacy(&source);
        connection
            .execute("CREATE TABLE unexpected(id TEXT)", [])
            .unwrap();
        drop(connection);
        let backup = ensure_verified_backup(&source).unwrap();
        assert!(build_duckdb_candidate(Some(&backup), &staging).is_err());
        assert!(!staging.join(CANDIDATE_DUCKDB_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn candidate_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let (_temporary, _source, staging) = roots();
        build_duckdb_candidate(None, &staging).unwrap();
        assert_eq!(
            fs::metadata(staging.join(CANDIDATE_DUCKDB_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }
}
