mod backup;
mod conversations;
mod duckdb;
mod layout;
mod prepare;
mod trivium;

pub use backup::{
    ensure_verified_backup, restore_verified_subtree, BackupFileEntry, BackupManifest,
    VerifiedBackup, BACKUP_SCHEMA_VERSION,
};
pub use conversations::{
    apply_conversation_migration, plan_conversation_migration, ConversationMigrationEntry,
    ConversationMigrationPlan, ConversationMigrationReport, ConversationQuarantineReason,
    CONVERSATION_MIGRATION_PLAN_SCHEMA_VERSION,
};
pub use duckdb::{
    build_duckdb_candidate, validate_current_duckdb, validate_current_duckdb_connection,
    DuckdbMigrationIssue, DuckdbMigrationReason, DuckdbMigrationReport, DuckdbValidationReport,
    MemoryTableDisposition, CANDIDATE_DUCKDB_FILE,
};
pub use layout::{
    activate_existing_generation, create_staging_generation, generation_name, publish_generation,
    resolve_active_data, DataPaths, GenerationManifest, MigrationLock, CURRENT_DATA_SCHEMA_VERSION,
};
pub use prepare::{prepare_data_dir, MigrationPlan, MigrationReport, MIGRATION_SCHEMA_VERSION};
pub use trivium::{rebuild_trivium_from_backup, TriviumMigrationIssue, TriviumMigrationReport};
