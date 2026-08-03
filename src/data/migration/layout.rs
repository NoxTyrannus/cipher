use crate::common::{AgentError, Result};
use crate::data::permissions::{ensure_private_directory, secure_existing_file};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub const CURRENT_DATA_SCHEMA_VERSION: u32 = 2;

const ACTIVE_POINTER_FILE: &str = "active-data.json";
const GENERATIONS_DIR: &str = "generations";
const GENERATION_MANIFEST_FILE: &str = "generation-manifest.json";
const MIGRATIONS_DIR: &str = "migrations";
const MIGRATION_LOCK_FILE: &str = "migration.lock";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPaths {
    root: PathBuf,
    storage_root: PathBuf,
}

impl DataPaths {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }

    pub fn duckdb(&self) -> PathBuf {
        self.storage_root.join("cipher.duckdb")
    }

    pub fn triviumdb_dir(&self) -> PathBuf {
        self.storage_root.join("triviumdb")
    }

    pub fn triviumdb(&self) -> PathBuf {
        self.triviumdb_dir().join("memory.trivium")
    }

    pub fn thoughts_data_root(&self) -> &Path {
        &self.storage_root
    }

    pub fn conversations(&self) -> PathBuf {
        self.storage_root.join("conversations")
    }

    pub fn workspaces(&self) -> PathBuf {
        self.storage_root.join("workspaces.json")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationManifest {
    pub schema_version: u32,
    pub generation_id: String,
    pub activation_content_sha256: String,
    pub source_fingerprint: Option<String>,
    pub migration_plan_sha256: Option<String>,
    pub migration_report_sha256: Option<String>,
}

impl GenerationManifest {
    pub fn fresh(generation_id: impl Into<String>) -> Self {
        Self {
            schema_version: CURRENT_DATA_SCHEMA_VERSION,
            generation_id: generation_id.into(),
            activation_content_sha256: "0".repeat(64),
            source_fingerprint: None,
            migration_plan_sha256: None,
            migration_report_sha256: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveDataPointer {
    schema_version: u32,
    generation: String,
    generation_manifest_sha256: String,
}

pub struct MigrationLock {
    file: File,
}

impl MigrationLock {
    pub fn acquire(data_root: &Path) -> Result<Self> {
        ensure_private_directory(data_root)?;
        let migrations = data_root.join(MIGRATIONS_DIR);
        ensure_private_directory(&migrations)?;
        let lock_path = migrations.join(MIGRATION_LOCK_FILE);
        reject_symlink_if_present(&lock_path, "migration lock")?;

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .map_err(|error| layout_error(format!("cannot open migration lock: {error}")))?;
        secure_existing_file(&lock_path)?;
        file.try_lock_exclusive().map_err(|error| {
            layout_error(format!(
                "another process is preparing the data directory: {error}"
            ))
        })?;
        Ok(Self { file })
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn resolve_active_data(data_root: &Path) -> Result<Option<DataPaths>> {
    ensure_private_directory(data_root)?;
    let pointer_path = data_root.join(ACTIVE_POINTER_FILE);
    match fs::symlink_metadata(&pointer_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(layout_error("active data pointer cannot be a symlink"));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(layout_error("active data pointer is not a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(layout_error(format!(
                "cannot inspect active data pointer: {error}"
            )));
        }
    }
    secure_existing_file(&pointer_path)?;

    let pointer: ActiveDataPointer = read_json(&pointer_path, "active data pointer")?;
    validate_pointer(&pointer)?;
    let storage_root = data_root.join(decode_generation_path(&pointer.generation)?);
    validate_generation_directory(data_root, &storage_root)?;

    let manifest_path = storage_root.join(GENERATION_MANIFEST_FILE);
    secure_existing_file(&manifest_path)?;
    let manifest_bytes = read_regular_file(&manifest_path, "generation manifest")?;
    let actual_hash = sha256_bytes(&manifest_bytes);
    if actual_hash != pointer.generation_manifest_sha256 {
        return Err(layout_error(
            "generation manifest hash does not match pointer",
        ));
    }
    let manifest: GenerationManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| layout_error(format!("cannot parse generation manifest: {error}")))?;
    validate_generation_manifest(&manifest, storage_root.file_name())?;
    Ok(Some(DataPaths {
        root: data_root.to_path_buf(),
        storage_root,
    }))
}

pub fn generation_name(source_fingerprint: Option<&str>) -> Result<String> {
    match source_fingerprint {
        Some(fingerprint) if is_sha256(fingerprint) => Ok(format!("v2-{fingerprint}")),
        Some(_) => Err(layout_error("source fingerprint is not a SHA-256 digest")),
        None => Ok("v2-fresh".to_string()),
    }
}

pub fn create_staging_generation(data_root: &Path, final_name: &str) -> Result<PathBuf> {
    validate_generation_name(final_name)?;
    ensure_private_directory(data_root)?;
    let generations = data_root.join(GENERATIONS_DIR);
    ensure_private_directory(&generations)?;
    let staging = generations.join(format!(".{final_name}.{}.staging", Uuid::new_v4().simple()));
    ensure_private_directory(&staging)?;
    Ok(staging)
}

pub fn publish_generation(
    data_root: &Path,
    staging: &Path,
    final_name: &str,
    manifest: &GenerationManifest,
) -> Result<DataPaths> {
    validate_generation_name(final_name)?;
    if manifest.generation_id != final_name {
        return Err(layout_error(
            "generation manifest ID does not match publication target",
        ));
    }
    validate_generation_manifest(manifest, Some(final_name.as_ref()))?;
    validate_staging_path(data_root, staging)?;

    let mut published_manifest = manifest.clone();
    published_manifest.activation_content_sha256 = hash_generation_content(staging)?;
    let manifest_bytes = serialize_json(&published_manifest, "generation manifest")?;
    atomic_create_file(
        &staging.join(GENERATION_MANIFEST_FILE),
        &manifest_bytes,
        "generation manifest",
    )?;
    sync_tree(staging)?;

    let generations = data_root.join(GENERATIONS_DIR);
    ensure_private_directory(&generations)?;
    let final_path = generations.join(final_name);
    match fs::rename(staging, &final_path) {
        Ok(()) => sync_directory(&generations)?,
        Err(error) if final_path.is_dir() => {
            verify_existing_generation(&final_path, &published_manifest, &manifest_bytes)?;
            fs::remove_dir_all(staging).map_err(|remove_error| {
                layout_error(format!(
                    "cannot remove redundant staging generation after {error}: {remove_error}"
                ))
            })?;
        }
        Err(error) => {
            return Err(layout_error(format!(
                "cannot publish generation directory: {error}"
            )));
        }
    }

    commit_active_pointer(data_root, final_name, &manifest_bytes)?;

    Ok(DataPaths {
        root: data_root.to_path_buf(),
        storage_root: final_path,
    })
}

pub fn activate_existing_generation(
    data_root: &Path,
    final_name: &str,
    expected: &GenerationManifest,
) -> Result<Option<DataPaths>> {
    validate_generation_name(final_name)?;
    let final_path = data_root.join(GENERATIONS_DIR).join(final_name);
    match fs::symlink_metadata(&final_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(layout_error(format!(
                "cannot inspect recoverable generation: {error}"
            )));
        }
    }
    validate_generation_directory(data_root, &final_path)?;
    let manifest_path = final_path.join(GENERATION_MANIFEST_FILE);
    secure_existing_file(&manifest_path)?;
    let manifest_bytes = read_regular_file(&manifest_path, "recoverable generation manifest")?;
    let actual: GenerationManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        layout_error(format!(
            "cannot parse recoverable generation manifest: {error}"
        ))
    })?;
    validate_generation_manifest(&actual, Some(final_name.as_ref()))?;
    if actual.schema_version != expected.schema_version
        || actual.generation_id != expected.generation_id
        || actual.source_fingerprint != expected.source_fingerprint
        || actual.migration_plan_sha256 != expected.migration_plan_sha256
        || actual.migration_report_sha256 != expected.migration_report_sha256
    {
        return Err(layout_error(
            "recoverable generation identity does not match the migration plan",
        ));
    }
    if hash_generation_content(&final_path)? != actual.activation_content_sha256 {
        return Err(layout_error(
            "recoverable generation content does not match its activation digest",
        ));
    }
    commit_active_pointer(data_root, final_name, &manifest_bytes)?;
    Ok(Some(DataPaths {
        root: data_root.to_path_buf(),
        storage_root: final_path,
    }))
}

fn commit_active_pointer(data_root: &Path, final_name: &str, manifest_bytes: &[u8]) -> Result<()> {
    let pointer = ActiveDataPointer {
        schema_version: CURRENT_DATA_SCHEMA_VERSION,
        generation: format!("{GENERATIONS_DIR}/{final_name}"),
        generation_manifest_sha256: sha256_bytes(manifest_bytes),
    };
    let pointer_bytes = serialize_json(&pointer, "active data pointer")?;
    let pointer_path = data_root.join(ACTIVE_POINTER_FILE);
    reject_symlink_if_present(&pointer_path, "active data pointer")?;
    if pointer_path.exists() {
        let existing: ActiveDataPointer = read_json(&pointer_path, "active data pointer")?;
        if existing != pointer {
            return Err(layout_error(
                "active data pointer already selects a different generation",
            ));
        }
    } else {
        atomic_create_file(&pointer_path, &pointer_bytes, "active data pointer")?;
        sync_directory(data_root)?;
    }
    Ok(())
}

fn validate_pointer(pointer: &ActiveDataPointer) -> Result<()> {
    if pointer.schema_version != CURRENT_DATA_SCHEMA_VERSION {
        return Err(layout_error(
            "unsupported active data pointer schema version",
        ));
    }
    decode_generation_path(&pointer.generation)?;
    if !is_sha256(&pointer.generation_manifest_sha256) {
        return Err(layout_error(
            "active data pointer has an invalid manifest digest",
        ));
    }
    Ok(())
}

fn decode_generation_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    let components: Vec<_> = path.components().collect();
    match components.as_slice() {
        [Component::Normal(parent), Component::Normal(name)]
            if parent == &std::ffi::OsStr::new(GENERATIONS_DIR) =>
        {
            let name = name
                .to_str()
                .ok_or_else(|| layout_error("generation name is not valid UTF-8"))?;
            validate_generation_name(name)?;
            Ok(PathBuf::from(GENERATIONS_DIR).join(name))
        }
        _ => Err(layout_error(
            "active generation must be a direct relative child of generations",
        )),
    }
}

fn validate_generation_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(layout_error("generation name contains unsafe characters"));
    }
    Ok(())
}

fn validate_generation_manifest(
    manifest: &GenerationManifest,
    expected_name: Option<&std::ffi::OsStr>,
) -> Result<()> {
    if manifest.schema_version != CURRENT_DATA_SCHEMA_VERSION {
        return Err(layout_error("unsupported generation schema version"));
    }
    validate_generation_name(&manifest.generation_id)?;
    if !is_sha256(&manifest.activation_content_sha256) {
        return Err(layout_error(
            "generation manifest has an invalid content digest",
        ));
    }
    if let Some(expected_name) = expected_name {
        if expected_name != std::ffi::OsStr::new(&manifest.generation_id) {
            return Err(layout_error(
                "generation manifest ID does not match its directory",
            ));
        }
    }
    for digest in [
        manifest.source_fingerprint.as_deref(),
        manifest.migration_plan_sha256.as_deref(),
        manifest.migration_report_sha256.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !is_sha256(digest) {
            return Err(layout_error(
                "generation manifest contains an invalid SHA-256 digest",
            ));
        }
    }
    Ok(())
}

fn validate_generation_directory(data_root: &Path, storage_root: &Path) -> Result<()> {
    let generations = data_root.join(GENERATIONS_DIR);
    reject_directory_symlink(&generations, "generations directory")?;
    reject_directory_symlink(storage_root, "active generation directory")?;
    let canonical_generations = fs::canonicalize(&generations)
        .map_err(|error| layout_error(format!("cannot canonicalize generations: {error}")))?;
    let canonical_storage = fs::canonicalize(storage_root)
        .map_err(|error| layout_error(format!("cannot canonicalize active generation: {error}")))?;
    if canonical_storage.parent() != Some(canonical_generations.as_path()) {
        return Err(layout_error(
            "active generation escaped generations directory",
        ));
    }
    ensure_private_directory(storage_root)
}

fn validate_staging_path(data_root: &Path, staging: &Path) -> Result<()> {
    reject_directory_symlink(staging, "staging generation")?;
    let generations = fs::canonicalize(data_root.join(GENERATIONS_DIR))
        .map_err(|error| layout_error(format!("cannot canonicalize generations: {error}")))?;
    let canonical_staging = fs::canonicalize(staging).map_err(|error| {
        layout_error(format!("cannot canonicalize staging generation: {error}"))
    })?;
    if canonical_staging.parent() != Some(generations.as_path()) {
        return Err(layout_error(
            "staging generation escaped generations directory",
        ));
    }
    let filename = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| layout_error("staging generation has no portable name"))?;
    if !filename.starts_with('.') || !filename.ends_with(".staging") {
        return Err(layout_error("staging generation name is invalid"));
    }
    Ok(())
}

fn verify_existing_generation(
    final_path: &Path,
    expected: &GenerationManifest,
    expected_bytes: &[u8],
) -> Result<()> {
    reject_directory_symlink(final_path, "existing generation")?;
    let path = final_path.join(GENERATION_MANIFEST_FILE);
    let actual_bytes = read_regular_file(&path, "existing generation manifest")?;
    if actual_bytes != expected_bytes {
        return Err(layout_error(
            "existing generation manifest differs from candidate",
        ));
    }
    let actual: GenerationManifest = serde_json::from_slice(&actual_bytes)
        .map_err(|error| layout_error(format!("cannot parse existing generation: {error}")))?;
    if &actual != expected {
        return Err(layout_error("existing generation does not match candidate"));
    }
    if hash_generation_content(final_path)? != actual.activation_content_sha256 {
        return Err(layout_error(
            "existing generation content does not match its manifest",
        ));
    }
    Ok(())
}

fn hash_generation_content(root: &Path) -> Result<String> {
    fn collect(directory: &Path, root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(directory)
            .map_err(|error| layout_error(format!("cannot list generation content: {error}")))?
        {
            let entry = entry.map_err(|error| {
                layout_error(format!("cannot inspect generation content: {error}"))
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                layout_error(format!("cannot stat generation content: {error}"))
            })?;
            if metadata.file_type().is_symlink() {
                return Err(layout_error("generation content contains a symlink"));
            }
            if metadata.is_dir() {
                collect(&path, root, files)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| layout_error("generation file escaped its root"))?;
                if relative != Path::new(GENERATION_MANIFEST_FILE) {
                    files.push(relative.to_path_buf());
                }
            } else {
                return Err(layout_error("generation content contains a special file"));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    collect(root, root, &mut files)?;
    files.sort();
    let mut generation_hasher = Sha256::new();
    generation_hasher.update(b"cipher-generation-content-v1\0");
    for relative in files {
        let relative_text = relative
            .to_str()
            .ok_or_else(|| layout_error("generation path is not valid UTF-8"))?;
        let bytes = read_regular_file(&root.join(&relative), "generation content file")?;
        let file_digest = sha256_bytes(&bytes);
        generation_hasher.update((relative_text.len() as u64).to_be_bytes());
        generation_hasher.update(relative_text.as_bytes());
        generation_hasher.update((bytes.len() as u64).to_be_bytes());
        generation_hasher.update(file_digest.as_bytes());
    }
    let digest = generation_hasher.finalize();
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(output)
}

fn atomic_create_file(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| layout_error(format!("{label} has no parent directory")))?;
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
        let mut file = options
            .open(&temporary)
            .map_err(|error| layout_error(format!("cannot create {label}: {error}")))?;
        file.write_all(bytes)
            .map_err(|error| layout_error(format!("cannot write {label}: {error}")))?;
        file.sync_all()
            .map_err(|error| layout_error(format!("cannot sync {label}: {error}")))?;
        drop(file);
        secure_existing_file(&temporary)?;
        fs::rename(&temporary, path)
            .map_err(|error| layout_error(format!("cannot publish {label}: {error}")))?;
        secure_existing_file(path)?;
        sync_directory(parent)
    })();
    if result.is_err() && temporary.exists() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn serialize_json<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(value)
        .map_err(|error| layout_error(format!("cannot serialize {label}: {error}")))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T> {
    let bytes = read_regular_file(path, label)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| layout_error(format!("cannot parse {label}: {error}")))
}

fn read_regular_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| layout_error(format!("cannot inspect {label}: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(layout_error(format!("{label} is not a regular file")));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|error| layout_error(format!("cannot read {label}: {error}")))?;
    Ok(bytes)
}

fn reject_symlink_if_present(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(layout_error(format!("{label} cannot be a symlink")))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(layout_error(format!("cannot inspect {label}: {error}"))),
    }
}

fn reject_directory_symlink(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| layout_error(format!("cannot inspect {label}: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(layout_error(format!("{label} is not a regular directory")));
    }
    Ok(())
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

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sync_tree(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)
        .map_err(|error| layout_error(format!("cannot list generation tree: {error}")))?
    {
        let entry = entry
            .map_err(|error| layout_error(format!("cannot inspect generation tree: {error}")))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| layout_error(format!("cannot stat generation entry: {error}")))?;
        if metadata.file_type().is_symlink() {
            return Err(layout_error("generation tree contains a symlink"));
        }
        if metadata.is_dir() {
            ensure_private_directory(&path)?;
            sync_tree(&path)?;
        } else if metadata.is_file() {
            secure_existing_file(&path)?;
            sync_generation_file(&path)?;
        } else {
            return Err(layout_error("generation tree contains a special file"));
        }
    }
    sync_directory(directory)
}

fn sync_generation_file(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // Windows: FlushFileBuffers 需要写句柄 — 只读 File::open 的 sync_all 会
        // Access denied (os error 5)。读写打开后 sync, 失败仅告警 (Windows 写缓冲由系统管理)。
        if let Ok(file) = File::options().write(true).open(path) {
            if let Err(e) = file.sync_all() {
                tracing::warn!("sync generation file (windows, non-fatal): {e}");
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| layout_error(format!("cannot sync generation file: {error}")))
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| layout_error(format!("cannot sync directory: {error}")))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn layout_error(message: impl Into<String>) -> AgentError {
    AgentError::Bootstrap(format!("data generation: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_is_single_commit_and_resolves_valid_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        ensure_private_directory(&root).unwrap();
        assert!(resolve_active_data(&root).unwrap().is_none());

        let name = generation_name(None).unwrap();
        let staging = create_staging_generation(&root, &name).unwrap();
        fs::write(staging.join("cipher.duckdb"), b"candidate").unwrap();
        let paths =
            publish_generation(&root, &staging, &name, &GenerationManifest::fresh(&name)).unwrap();

        assert_eq!(paths.duckdb(), paths.storage_root().join("cipher.duckdb"));
        assert!(!staging.exists());
        assert!(root.join(ACTIVE_POINTER_FILE).is_file());
        assert_eq!(resolve_active_data(&root).unwrap().unwrap(), paths);
    }

    #[test]
    fn pointer_rejects_parent_paths_and_manifest_tampering() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        ensure_private_directory(&root).unwrap();

        let unsafe_pointer = ActiveDataPointer {
            schema_version: CURRENT_DATA_SCHEMA_VERSION,
            generation: "generations/../outside".to_string(),
            generation_manifest_sha256: "0".repeat(64),
        };
        fs::write(
            root.join(ACTIVE_POINTER_FILE),
            serde_json::to_vec(&unsafe_pointer).unwrap(),
        )
        .unwrap();
        assert!(resolve_active_data(&root).is_err());

        fs::remove_file(root.join(ACTIVE_POINTER_FILE)).unwrap();
        let name = generation_name(None).unwrap();
        let staging = create_staging_generation(&root, &name).unwrap();
        publish_generation(&root, &staging, &name, &GenerationManifest::fresh(&name)).unwrap();
        let manifest = root
            .join(GENERATIONS_DIR)
            .join(&name)
            .join(GENERATION_MANIFEST_FILE);
        fs::write(manifest, b"{}").unwrap();
        assert!(resolve_active_data(&root).is_err());
    }

    #[test]
    fn migration_lock_is_exclusive_and_reusable() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        let first = MigrationLock::acquire(&root).unwrap();
        assert!(MigrationLock::acquire(&root).is_err());
        drop(first);
        MigrationLock::acquire(&root).unwrap();
    }

    #[test]
    fn recovers_final_generation_when_pointer_publication_was_interrupted() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        ensure_private_directory(&root).unwrap();
        let name = generation_name(None).unwrap();
        let expected = GenerationManifest::fresh(&name);
        let staging = create_staging_generation(&root, &name).unwrap();
        fs::write(staging.join("cipher.duckdb"), b"candidate").unwrap();
        let published = publish_generation(&root, &staging, &name, &expected).unwrap();

        fs::remove_file(root.join(ACTIVE_POINTER_FILE)).unwrap();
        assert!(resolve_active_data(&root).unwrap().is_none());
        let recovered = activate_existing_generation(&root, &name, &expected)
            .unwrap()
            .unwrap();
        assert_eq!(recovered, published);
        assert_eq!(resolve_active_data(&root).unwrap().unwrap(), published);
    }

    #[cfg(unix)]
    #[test]
    fn pointer_and_generation_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        ensure_private_directory(&root).unwrap();
        let name = generation_name(None).unwrap();
        let staging = create_staging_generation(&root, &name).unwrap();
        fs::write(staging.join("secret"), b"secret").unwrap();
        let paths =
            publish_generation(&root, &staging, &name, &GenerationManifest::fresh(&name)).unwrap();

        assert_eq!(
            fs::metadata(root.join(ACTIVE_POINTER_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(paths.storage_root())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(paths.storage_root().join("secret"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
