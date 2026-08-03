use crate::common::{AgentError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub const BACKUP_SCHEMA_VERSION: u32 = 1;

const DUCKDB_PREFIX: &str = "cipher.duckdb";
const ACTIVE_DIRECTORIES: &[&str] = &["triviumdb", "conversations", "thoughts"];
const MANIFEST_FILE: &str = "manifest.json";
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupFileEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    pub schema_version: u32,
    pub source_fingerprint: String,
    pub files: Vec<BackupFileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBackup {
    pub backup_dir: PathBuf,
    pub manifest: BackupManifest,
    pub reused: bool,
}

#[derive(Debug, Clone)]
struct SnapshotFile {
    source_path: PathBuf,
    relative_path: PathBuf,
    entry: BackupFileEntry,
}

#[derive(Debug, Clone)]
struct SourceSnapshot {
    files: Vec<SnapshotFile>,
    fingerprint: String,
}

impl SourceSnapshot {
    fn manifest(&self) -> BackupManifest {
        BackupManifest {
            schema_version: BACKUP_SCHEMA_VERSION,
            source_fingerprint: self.fingerprint.clone(),
            files: self.files.iter().map(|file| file.entry.clone()).collect(),
        }
    }

    fn same_content_as(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && self
                .files
                .iter()
                .map(|file| &file.entry)
                .eq(other.files.iter().map(|file| &file.entry))
    }
}

struct TemporaryBackup {
    path: PathBuf,
    published: bool,
}

impl Drop for TemporaryBackup {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub fn ensure_verified_backup(data_root: &Path) -> Result<VerifiedBackup> {
    ensure_verified_backup_inner(data_root, || {})
}

fn ensure_verified_backup_inner<F>(
    data_root: &Path,
    before_final_snapshot: F,
) -> Result<VerifiedBackup>
where
    F: FnOnce(),
{
    let initial = snapshot_sources(data_root)?;
    let backup_root = prepare_backup_root(data_root)?;
    let final_path = backup_root.join(format!("v1-to-v2-{}", initial.fingerprint));

    match fs::symlink_metadata(&final_path) {
        Ok(_) => {
            let manifest = verify_backup(&final_path, &initial)?;
            before_final_snapshot();
            let final_source = snapshot_sources(data_root)?;
            require_unchanged_source(&initial, &final_source)?;
            return Ok(VerifiedBackup {
                backup_dir: final_path,
                manifest,
                reused: true,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(backup_error(format!(
                "cannot inspect published backup destination: {error}"
            )));
        }
    }

    let mut temporary = create_temporary_backup(&backup_root)?;
    copy_snapshot(&initial, &temporary.path)?;
    write_manifest(&temporary.path, &initial.manifest())?;
    let manifest = verify_backup(&temporary.path, &initial)?;
    sync_directory_tree(&temporary.path)?;

    before_final_snapshot();
    let final_source = snapshot_sources(data_root)?;
    require_unchanged_source(&initial, &final_source)?;

    match fs::rename(&temporary.path, &final_path) {
        Ok(()) => temporary.published = true,
        Err(error) => {
            if fs::symlink_metadata(&final_path).is_ok() {
                let manifest = verify_backup(&final_path, &initial)?;
                sync_directory(&backup_root)?;
                return Ok(VerifiedBackup {
                    backup_dir: final_path,
                    manifest,
                    reused: true,
                });
            }
            return Err(backup_error(format!(
                "cannot publish verified backup atomically: {error}"
            )));
        }
    }

    sync_directory(&backup_root)?;
    let published_manifest = verify_backup(&final_path, &initial)?;
    debug_assert_eq!(manifest, published_manifest);
    Ok(VerifiedBackup {
        backup_dir: final_path,
        manifest: published_manifest,
        reused: false,
    })
}

fn backup_error(message: impl Into<String>) -> AgentError {
    AgentError::Bootstrap(format!("migration backup: {}", message.into()))
}

fn require_unchanged_source(initial: &SourceSnapshot, final_source: &SourceSnapshot) -> Result<()> {
    if initial.same_content_as(final_source) {
        Ok(())
    } else {
        Err(backup_error(
            "source fingerprint changed while the backup was being created",
        ))
    }
}

fn snapshot_sources(data_root: &Path) -> Result<SourceSnapshot> {
    validate_data_root(data_root)?;

    let mut source_paths = Vec::new();
    let root_entries = fs::read_dir(data_root)
        .map_err(|error| backup_error(format!("cannot list data root: {error}")))?;
    for entry in root_entries {
        let entry = entry
            .map_err(|error| backup_error(format!("cannot inspect a data root entry: {error}")))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(DUCKDB_PREFIX)
        {
            require_source_file(&entry.path(), data_root, &mut source_paths)?;
        }
    }

    for directory_name in ACTIVE_DIRECTORIES {
        let directory = data_root.join(directory_name);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(backup_error(format!(
                    "active source directory is a symlink: {directory_name}"
                )));
            }
            Ok(metadata) if metadata.is_dir() => {
                collect_source_tree(&directory, data_root, &mut source_paths)?;
            }
            Ok(_) => {
                return Err(backup_error(format!(
                    "active source path is not a directory: {directory_name}"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(backup_error(format!(
                    "cannot inspect active source directory {directory_name}: {error}"
                )));
            }
        }
    }

    let mut files = Vec::with_capacity(source_paths.len());
    for source_path in source_paths {
        let relative_path = source_path
            .strip_prefix(data_root)
            .map_err(|_| backup_error("an active source escaped the configured data root"))?
            .to_path_buf();
        let manifest_path = encode_relative_path(&relative_path)?;
        let (sha256, bytes) = hash_regular_file(&source_path, &manifest_path)?;
        files.push(SnapshotFile {
            source_path,
            relative_path,
            entry: BackupFileEntry {
                path: manifest_path,
                sha256,
                bytes,
            },
        });
    }
    files.sort_by(|left, right| left.entry.path.cmp(&right.entry.path));

    for pair in files.windows(2) {
        if pair[0].entry.path == pair[1].entry.path {
            return Err(backup_error("duplicate active source relative path"));
        }
    }

    let fingerprint = source_fingerprint(&files);
    Ok(SourceSnapshot { files, fingerprint })
}

fn validate_data_root(data_root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(data_root)
        .map_err(|error| backup_error(format!("cannot inspect data root: {error}")))?;
    if metadata.file_type().is_symlink() {
        return Err(backup_error("data root cannot be a symlink"));
    }
    if !metadata.is_dir() {
        return Err(backup_error("data root is not a directory"));
    }

    let canonical = fs::canonicalize(data_root)
        .map_err(|error| backup_error(format!("cannot canonicalize data root: {error}")))?;
    if canonical.parent().is_none() {
        return Err(backup_error("filesystem root cannot be used as data root"));
    }
    if fs::canonicalize(std::env::temp_dir()).ok().as_ref() == Some(&canonical) {
        return Err(backup_error(
            "shared temporary root cannot be used as data root",
        ));
    }
    Ok(())
}

fn collect_source_tree(directory: &Path, data_root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let directory_metadata = fs::symlink_metadata(directory).map_err(|error| {
        backup_error(format!(
            "cannot inspect active source directory {}: {error}",
            display_relative(directory, data_root)
        ))
    })?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(backup_error(format!(
            "active source directory changed type: {}",
            display_relative(directory, data_root)
        )));
    }
    let entries = fs::read_dir(directory).map_err(|error| {
        let label = display_relative(directory, data_root);
        backup_error(format!(
            "cannot list active source directory {label}: {error}"
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            backup_error(format!("cannot inspect an active source entry: {error}"))
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            let label = display_relative(&path, data_root);
            backup_error(format!("cannot inspect active source {label}: {error}"))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(backup_error(format!(
                "active source contains a symlink: {}",
                display_relative(&path, data_root)
            )));
        }
        if metadata.is_dir() {
            collect_source_tree(&path, data_root, files)?;
        } else if metadata.is_file() {
            files.push(path);
        } else {
            return Err(backup_error(format!(
                "active source contains a special file: {}",
                display_relative(&path, data_root)
            )));
        }
    }
    Ok(())
}

fn require_source_file(path: &Path, data_root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        backup_error(format!(
            "cannot inspect DuckDB file family entry {}: {error}",
            display_relative(path, data_root)
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(backup_error(format!(
            "DuckDB file family contains a symlink: {}",
            display_relative(path, data_root)
        )));
    }
    if !metadata.is_file() {
        return Err(backup_error(format!(
            "DuckDB file family contains a non-file entry: {}",
            display_relative(path, data_root)
        )));
    }
    files.push(path.to_path_buf());
    Ok(())
}

fn display_relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .and_then(|relative| encode_relative_path(relative).ok())
        .unwrap_or_else(|| "<invalid-relative-path>".to_string())
}

fn encode_relative_path(path: &Path) -> Result<String> {
    let mut encoded = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| backup_error("active source path is not valid UTF-8"))?;
                if value.is_empty() || value.contains('\\') {
                    return Err(backup_error(
                        "active source path cannot be represented portably",
                    ));
                }
                encoded.push(value);
            }
            _ => return Err(backup_error("active source path is not strictly relative")),
        }
    }
    if encoded.is_empty() {
        return Err(backup_error("active source relative path is empty"));
    }
    Ok(encoded.join("/"))
}

fn decode_manifest_path(path: &str) -> Result<PathBuf> {
    validate_manifest_path(path)?;
    let mut decoded = PathBuf::new();
    for component in path.split('/') {
        decoded.push(component);
    }
    Ok(decoded)
}

fn validate_manifest_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(backup_error("manifest contains an unsafe relative path"));
    }
    Ok(())
}

fn hash_regular_file(path: &Path, label: &str) -> Result<(String, u64)> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| backup_error(format!("cannot inspect source file {label}: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(backup_error(format!(
            "source file changed type while hashing: {label}"
        )));
    }

    let mut file = File::open(path)
        .map_err(|error| backup_error(format!("cannot open source file {label}: {error}")))?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| backup_error(format!("cannot read source file {label}: {error}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| backup_error(format!("source file is too large: {label}")))?;
    }
    let final_metadata = fs::symlink_metadata(path)
        .map_err(|error| backup_error(format!("cannot re-inspect source file {label}: {error}")))?;
    if final_metadata.file_type().is_symlink() || !final_metadata.is_file() {
        return Err(backup_error(format!(
            "source file changed type while hashing: {label}"
        )));
    }
    Ok((digest_hex(hasher), bytes))
}

fn source_fingerprint(files: &[SnapshotFile]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cipher-migration-backup-source-v1\0");
    for file in files {
        let path = file.entry.path.as_bytes();
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path);
        hasher.update(file.entry.bytes.to_be_bytes());
        hasher.update(file.entry.sha256.as_bytes());
    }
    digest_hex(hasher)
}

fn digest_hex(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn prepare_backup_root(data_root: &Path) -> Result<PathBuf> {
    let migrations = ensure_private_child_directory(data_root, "migrations")?;
    ensure_private_child_directory(&migrations, "backups")
}

fn ensure_private_child_directory(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(backup_error(format!(
                "backup destination component is a symlink: {name}"
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(backup_error(format!(
                "backup destination component is not a directory: {name}"
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_private_directory(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(backup_error(format!(
                        "cannot create backup directory {name}: {error}"
                    )));
                }
            }
        }
        Err(error) => {
            return Err(backup_error(format!(
                "cannot inspect backup directory {name}: {error}"
            )));
        }
    }
    crate::data::permissions::ensure_private_directory(&path)?;
    Ok(path)
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

fn create_temporary_backup(backup_root: &Path) -> Result<TemporaryBackup> {
    for _ in 0..16 {
        let path = backup_root.join(format!(".backup-tmp-{}", Uuid::new_v4().simple()));
        match create_private_directory(&path) {
            Ok(()) => {
                crate::data::permissions::ensure_private_directory(&path)?;
                return Ok(TemporaryBackup {
                    path,
                    published: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(backup_error(format!(
                    "cannot create temporary backup directory: {error}"
                )));
            }
        }
    }
    Err(backup_error(
        "cannot allocate a unique temporary backup directory",
    ))
}

fn copy_snapshot(snapshot: &SourceSnapshot, destination_root: &Path) -> Result<()> {
    for source in &snapshot.files {
        let destination = destination_root.join(&source.relative_path);
        let parent = destination
            .parent()
            .ok_or_else(|| backup_error("backup destination has no parent"))?;
        crate::data::permissions::ensure_private_directory(parent)?;
        copy_one_file(source, &destination)?;
    }
    Ok(())
}

fn copy_one_file(source: &SnapshotFile, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(&source.source_path).map_err(|error| {
        backup_error(format!(
            "cannot inspect source before copy {}: {error}",
            source.entry.path
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(backup_error(format!(
            "source changed type before copy: {}",
            source.entry.path
        )));
    }

    let mut input = File::open(&source.source_path).map_err(|error| {
        backup_error(format!(
            "cannot open source for copy {}: {error}",
            source.entry.path
        ))
    })?;
    let mut output = create_private_file(destination).map_err(|error| {
        backup_error(format!(
            "cannot create backup file {}: {error}",
            source.entry.path
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer).map_err(|error| {
            backup_error(format!(
                "cannot read source during copy {}: {error}",
                source.entry.path
            ))
        })?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read]).map_err(|error| {
            backup_error(format!(
                "cannot write backup file {}: {error}",
                source.entry.path
            ))
        })?;
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| backup_error("backup file size overflow"))?;
    }
    output.sync_all().map_err(|error| {
        backup_error(format!(
            "cannot sync backup file {}: {error}",
            source.entry.path
        ))
    })?;
    drop(output);
    crate::data::permissions::secure_existing_file(destination)?;

    let final_source_metadata = fs::symlink_metadata(&source.source_path).map_err(|error| {
        backup_error(format!(
            "cannot re-inspect source after copy {}: {error}",
            source.entry.path
        ))
    })?;
    if final_source_metadata.file_type().is_symlink() || !final_source_metadata.is_file() {
        return Err(backup_error(format!(
            "source changed type while copying: {}",
            source.entry.path
        )));
    }

    if bytes != source.entry.bytes || digest_hex(hasher) != source.entry.sha256 {
        return Err(backup_error(format!(
            "source changed while copying: {}",
            source.entry.path
        )));
    }
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

fn write_manifest(backup_root: &Path, manifest: &BackupManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| backup_error(format!("cannot serialize backup manifest: {error}")))?;
    let path = backup_root.join(MANIFEST_FILE);
    let mut file = create_private_file(&path)
        .map_err(|error| backup_error(format!("cannot create backup manifest: {error}")))?;
    file.write_all(&bytes)
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| backup_error(format!("cannot write backup manifest: {error}")))?;
    file.sync_all()
        .map_err(|error| backup_error(format!("cannot sync backup manifest: {error}")))?;
    drop(file);
    crate::data::permissions::secure_existing_file(&path)?;
    Ok(())
}

fn verify_backup(backup_root: &Path, expected: &SourceSnapshot) -> Result<BackupManifest> {
    let root_metadata = fs::symlink_metadata(backup_root)
        .map_err(|error| backup_error(format!("cannot inspect backup directory: {error}")))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(backup_error(
            "published backup path is not a regular directory",
        ));
    }
    verify_mode(&root_metadata, 0o700, "backup directory")?;

    let manifest = read_manifest(&backup_root.join(MANIFEST_FILE))?;
    validate_manifest(&manifest, expected)?;

    let mut actual_files = Vec::new();
    let mut actual_directories = Vec::new();
    collect_backup_inventory(
        backup_root,
        backup_root,
        &mut actual_files,
        &mut actual_directories,
    )?;
    actual_files.sort();
    actual_directories.sort();

    let mut expected_files: Vec<String> = manifest
        .files
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    expected_files.push(MANIFEST_FILE.to_string());
    expected_files.sort();
    if actual_files != expected_files {
        return Err(backup_error(
            "backup file inventory does not match manifest",
        ));
    }

    let expected_directories = expected_directory_inventory(&manifest.files);
    if actual_directories != expected_directories {
        return Err(backup_error(
            "backup directory inventory does not match manifest",
        ));
    }

    for entry in &manifest.files {
        let relative = decode_manifest_path(&entry.path)?;
        let path = backup_root.join(relative);
        let (sha256, bytes) = hash_regular_file(&path, &entry.path)?;
        if sha256 != entry.sha256 || bytes != entry.bytes {
            return Err(backup_error(format!(
                "backup file verification failed: {}",
                entry.path
            )));
        }
    }
    Ok(manifest)
}

fn read_manifest(path: &Path) -> Result<BackupManifest> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| backup_error(format!("cannot inspect backup manifest: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(backup_error("backup manifest is not a regular file"));
    }
    verify_mode(&metadata, 0o600, "backup manifest")?;
    let file = File::open(path)
        .map_err(|error| backup_error(format!("cannot open backup manifest: {error}")))?;
    serde_json::from_reader(BufReader::new(file))
        .map_err(|error| backup_error(format!("cannot parse backup manifest: {error}")))
}

fn validate_manifest(manifest: &BackupManifest, expected: &SourceSnapshot) -> Result<()> {
    if manifest.schema_version != BACKUP_SCHEMA_VERSION {
        return Err(backup_error("unsupported backup manifest schema version"));
    }
    if !is_sha256(&manifest.source_fingerprint)
        || manifest.source_fingerprint != expected.fingerprint
    {
        return Err(backup_error("backup manifest source fingerprint mismatch"));
    }

    for entry in &manifest.files {
        validate_manifest_path(&entry.path)?;
        if !is_sha256(&entry.sha256) {
            return Err(backup_error("backup manifest contains an invalid SHA-256"));
        }
    }
    if !manifest
        .files
        .windows(2)
        .all(|pair| pair[0].path < pair[1].path)
    {
        return Err(backup_error(
            "backup manifest file entries are not uniquely sorted",
        ));
    }

    let expected_entries: Vec<BackupFileEntry> = expected
        .files
        .iter()
        .map(|file| file.entry.clone())
        .collect();
    if manifest.files != expected_entries {
        return Err(backup_error(
            "backup manifest does not match source snapshot",
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn collect_backup_inventory(
    directory: &Path,
    root: &Path,
    files: &mut Vec<String>,
    directories: &mut Vec<String>,
) -> Result<()> {
    let entries = fs::read_dir(directory)
        .map_err(|error| backup_error(format!("cannot list backup directory: {error}")))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| backup_error(format!("cannot inspect backup entry: {error}")))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| backup_error(format!("cannot stat backup entry: {error}")))?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| backup_error("backup entry escaped backup directory"))?;
        let encoded = encode_relative_path(relative)?;
        if metadata.file_type().is_symlink() {
            return Err(backup_error("backup contains a symlink"));
        }
        if metadata.is_dir() {
            verify_mode(&metadata, 0o700, "backup subdirectory")?;
            directories.push(encoded);
            collect_backup_inventory(&path, root, files, directories)?;
        } else if metadata.is_file() {
            verify_mode(&metadata, 0o600, "backup file")?;
            files.push(encoded);
        } else {
            return Err(backup_error("backup contains a special file"));
        }
    }
    Ok(())
}

fn expected_directory_inventory(files: &[BackupFileEntry]) -> Vec<String> {
    let mut directories = BTreeSet::new();
    for file in files {
        let parts: Vec<&str> = file.path.split('/').collect();
        for end in 1..parts.len() {
            directories.insert(parts[..end].join("/"));
        }
    }
    directories.into_iter().collect()
}

#[cfg(unix)]
fn verify_mode(metadata: &fs::Metadata, expected: u32, label: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let actual = metadata.permissions().mode() & 0o7777;
    if actual == expected {
        Ok(())
    } else {
        Err(backup_error(format!(
            "{label} permissions are {actual:04o}, expected {expected:04o}"
        )))
    }
}

#[cfg(not(unix))]
fn verify_mode(_metadata: &fs::Metadata, _expected: u32, _label: &str) -> Result<()> {
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| backup_error(format!("cannot sync backup directory: {error}")))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_directory_tree(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let entries = fs::read_dir(path).map_err(|error| {
            backup_error(format!("cannot list backup tree while syncing: {error}"))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                backup_error(format!("cannot inspect backup tree while syncing: {error}"))
            })?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                backup_error(format!("cannot stat backup tree while syncing: {error}"))
            })?;
            if metadata.file_type().is_symlink() {
                return Err(backup_error("backup tree changed to contain a symlink"));
            }
            if metadata.is_dir() {
                sync_directory_tree(&entry.path())?;
            } else if !metadata.is_file() {
                return Err(backup_error(
                    "backup tree changed to contain a special file",
                ));
            }
        }
        sync_directory(path)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn restore_verified_subtree(
    backup: &VerifiedBackup,
    prefix: &str,
    target_root: &Path,
) -> Result<u64> {
    validate_restore_prefix(prefix)?;
    let prefix_with_separator = format!("{prefix}/");
    let entries: Vec<_> = backup
        .manifest
        .files
        .iter()
        .filter(|entry| entry.path.starts_with(&prefix_with_separator))
        .collect();
    if entries.is_empty() {
        return Ok(0);
    }

    validate_restore_backup_root(&backup.backup_dir)?;
    let mut candidates = Vec::with_capacity(entries.len());
    let mut destinations = BTreeSet::new();
    for entry in entries {
        let relative = decode_manifest_path(&entry.path)?;
        if relative.components().next() != Some(Component::Normal(std::ffi::OsStr::new(prefix))) {
            return Err(backup_error(
                "restore manifest entry escaped the requested subtree",
            ));
        }
        if !destinations.insert(relative.clone()) {
            return Err(backup_error(
                "restore manifest contains a duplicate destination",
            ));
        }

        let source = checked_restore_source_path(&backup.backup_dir, &relative, &entry.path)?;
        let bytes = read_and_verify_restore_source(&source, entry)?;
        let destination = target_root.join(&relative);
        let state = inspect_restore_destination(target_root, &relative, &bytes)?;
        candidates.push(RestoreCandidate {
            destination,
            bytes,
            state,
        });
    }

    ensure_restore_root(target_root)?;
    let mut restored = 0_u64;
    for candidate in candidates {
        let parent = candidate
            .destination
            .parent()
            .ok_or_else(|| backup_error("restore destination has no parent"))?;
        ensure_restore_directory_chain(target_root, parent)?;
        match candidate.state {
            RestoreDestinationState::Identical => {
                crate::data::permissions::secure_existing_file(&candidate.destination)?;
            }
            RestoreDestinationState::Missing => {
                if publish_restored_file(&candidate.destination, &candidate.bytes)? {
                    restored = restored
                        .checked_add(1)
                        .ok_or_else(|| backup_error("restored file count overflow"))?;
                }
            }
        }
    }
    Ok(restored)
}

#[derive(Debug)]
struct RestoreCandidate {
    destination: PathBuf,
    bytes: Vec<u8>,
    state: RestoreDestinationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreDestinationState {
    Missing,
    Identical,
}

struct RestoreTemporaryFile {
    path: PathBuf,
    published: bool,
}

impl Drop for RestoreTemporaryFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn validate_restore_prefix(prefix: &str) -> Result<()> {
    let mut components = Path::new(prefix).components();
    let one_normal_component = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == prefix
    );
    if prefix.is_empty()
        || prefix.contains(['/', '\\'])
        || prefix == "."
        || prefix == ".."
        || !one_normal_component
    {
        return Err(backup_error(
            "restore prefix must be one safe ordinary relative component",
        ));
    }
    Ok(())
}

fn validate_restore_backup_root(backup_root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(backup_root)
        .map_err(|error| backup_error(format!("cannot inspect restore backup root: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(backup_error(
            "restore backup root is not a regular directory",
        ));
    }
    Ok(())
}

fn checked_restore_source_path(
    backup_root: &Path,
    relative: &Path,
    label: &str,
) -> Result<PathBuf> {
    let mut path = backup_root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            return Err(backup_error("restore source path is not strictly relative"));
        };
        path.push(component);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            backup_error(format!("cannot inspect restore source {label}: {error}"))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(backup_error(format!(
                "restore source path contains a symlink: {label}"
            )));
        }
        let is_last = index + 1 == components.len();
        if (is_last && !metadata.is_file()) || (!is_last && !metadata.is_dir()) {
            return Err(backup_error(format!(
                "restore source changed filesystem type: {label}"
            )));
        }
    }
    Ok(path)
}

fn read_and_verify_restore_source(path: &Path, entry: &BackupFileEntry) -> Result<Vec<u8>> {
    let initial = fs::symlink_metadata(path).map_err(|error| {
        backup_error(format!(
            "cannot inspect restore source {}: {error}",
            entry.path
        ))
    })?;
    if initial.file_type().is_symlink() || !initial.is_file() {
        return Err(backup_error(format!(
            "restore source is not a regular file: {}",
            entry.path
        )));
    }
    let bytes = fs::read(path).map_err(|error| {
        backup_error(format!(
            "cannot read restore source {}: {error}",
            entry.path
        ))
    })?;
    let final_metadata = fs::symlink_metadata(path).map_err(|error| {
        backup_error(format!(
            "cannot re-inspect restore source {}: {error}",
            entry.path
        ))
    })?;
    if final_metadata.file_type().is_symlink() || !final_metadata.is_file() {
        return Err(backup_error(format!(
            "restore source changed type while reading: {}",
            entry.path
        )));
    }

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    if bytes.len() as u64 != entry.bytes || digest_hex(hasher) != entry.sha256 {
        return Err(backup_error(format!(
            "restore source hash verification failed: {}",
            entry.path
        )));
    }
    Ok(bytes)
}

fn inspect_restore_destination(
    target_root: &Path,
    relative: &Path,
    source_bytes: &[u8],
) -> Result<RestoreDestinationState> {
    reject_symlinked_target_root(target_root)?;
    let mut path = target_root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            return Err(backup_error(
                "restore destination path is not strictly relative",
            ));
        };
        path.push(component);
        let is_last = index + 1 == components.len();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(backup_error("restore destination contains a symlink"));
            }
            Ok(metadata) if !is_last && !metadata.is_dir() => {
                return Err(backup_error(
                    "restore destination parent is not a directory",
                ));
            }
            Ok(metadata) if is_last && !metadata.is_file() => {
                return Err(backup_error(
                    "restore destination conflicts with a non-file path",
                ));
            }
            Ok(_) if is_last => {
                let existing = fs::read(&path).map_err(|error| {
                    backup_error(format!("cannot read existing restore target: {error}"))
                })?;
                if existing == source_bytes {
                    return Ok(RestoreDestinationState::Identical);
                }
                return Err(backup_error(
                    "restore destination contains different bytes and will not be overwritten",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RestoreDestinationState::Missing);
            }
            Err(error) => {
                return Err(backup_error(format!(
                    "cannot inspect restore destination: {error}"
                )));
            }
        }
    }
    Err(backup_error("restore destination path is empty"))
}

fn reject_symlinked_target_root(target_root: &Path) -> Result<()> {
    match fs::symlink_metadata(target_root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(backup_error("restore target root cannot be a symlink"))
        }
        Ok(metadata) if !metadata.is_dir() => {
            Err(backup_error("restore target root is not a directory"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(backup_error(format!(
            "cannot inspect restore target root: {error}"
        ))),
    }
}

fn ensure_restore_root(target_root: &Path) -> Result<()> {
    reject_symlinked_target_root(target_root)?;
    crate::data::permissions::ensure_private_directory(target_root)?;
    reject_symlinked_target_root(target_root)
}

fn ensure_restore_directory_chain(target_root: &Path, directory: &Path) -> Result<()> {
    let relative = directory
        .strip_prefix(target_root)
        .map_err(|_| backup_error("restore directory escaped target root"))?;
    let mut current = target_root.to_path_buf();
    crate::data::permissions::ensure_private_directory(&current)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(backup_error(
                "restore directory path is not strictly relative",
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(backup_error("restore directory contains a symlink"));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(backup_error("restore directory conflicts with a file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(backup_error(format!(
                    "cannot inspect restore directory: {error}"
                )));
            }
        }
        crate::data::permissions::ensure_private_directory(&current)?;
    }
    Ok(())
}

fn publish_restored_file(destination: &Path, bytes: &[u8]) -> Result<bool> {
    let parent = destination
        .parent()
        .ok_or_else(|| backup_error("restore destination has no parent"))?;
    let mut temporary = None;
    for _ in 0..16 {
        let path = parent.join(format!(".restore-tmp-{}", Uuid::new_v4().simple()));
        match create_private_file(&path) {
            Ok(file) => {
                temporary = Some((
                    RestoreTemporaryFile {
                        path,
                        published: false,
                    },
                    file,
                ));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(backup_error(format!(
                    "cannot create restore temporary file: {error}"
                )));
            }
        }
    }
    let (mut temporary, mut file) =
        temporary.ok_or_else(|| backup_error("cannot allocate a restore temporary file"))?;
    file.write_all(bytes)
        .map_err(|error| backup_error(format!("cannot write restore file: {error}")))?;
    file.sync_all()
        .map_err(|error| backup_error(format!("cannot sync restore file: {error}")))?;
    drop(file);
    crate::data::permissions::secure_existing_file(&temporary.path)?;

    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(backup_error(
                "restore destination changed to a conflicting path",
            ));
        }
        Ok(_) => {
            let existing = fs::read(destination).map_err(|error| {
                backup_error(format!("cannot read concurrent restore target: {error}"))
            })?;
            if existing == bytes {
                crate::data::permissions::secure_existing_file(destination)?;
                return Ok(false);
            }
            return Err(backup_error(
                "restore destination appeared with different bytes and will not be overwritten",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(backup_error(format!(
                "cannot re-inspect restore destination: {error}"
            )));
        }
    }

    fs::rename(&temporary.path, destination)
        .map_err(|error| backup_error(format!("cannot publish restored file: {error}")))?;
    temporary.published = true;
    crate::data::permissions::secure_existing_file(destination)?;
    sync_directory(parent)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_root() -> (tempfile::TempDir, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        fs::create_dir(&root).unwrap();
        (temporary, root)
    }

    fn write_source(root: &Path, relative: &str, content: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn backup_covers_only_active_sources_with_stable_private_manifest() {
        let (_temporary, root) = source_root();
        write_source(&root, "cipher.duckdb", b"duck-main");
        write_source(&root, "cipher.duckdb.wal", b"duck-sidecar");
        write_source(&root, "triviumdb/memory.trivium", b"vector-data");
        write_source(
            &root,
            "conversations/2026/turn.json",
            b"api_key=sk-secret-must-not-leak",
        );
        write_source(&root, "thoughts/2026/input.json", br#"{"input":"hello"}"#);
        write_source(&root, "unrelated.txt", b"not part of migration backup");

        let backup = ensure_verified_backup(&root).unwrap();
        assert!(!backup.reused);
        assert_eq!(backup.manifest.schema_version, BACKUP_SCHEMA_VERSION);
        let mut backup_files: Vec<&str> = backup
            .manifest
            .files
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        backup_files.sort();
        let mut expected = vec![
            "conversations/2026/turn.json",
            "cipher.duckdb",
            "cipher.duckdb.wal",
            "thoughts/2026/input.json",
            "triviumdb/memory.trivium",
        ];
        expected.sort();
        assert_eq!(backup_files, expected);
        assert_eq!(
            backup.backup_dir.file_name().unwrap().to_string_lossy(),
            format!("v1-to-v2-{}", backup.manifest.source_fingerprint)
        );
        assert!(!backup.backup_dir.join("unrelated.txt").exists());

        let manifest_text = fs::read_to_string(backup.backup_dir.join(MANIFEST_FILE)).unwrap();
        assert!(!manifest_text.contains("sk-secret-must-not-leak"));
        assert!(!manifest_text.contains("api_key="));
        assert!(!manifest_text.contains(&root.to_string_lossy().to_string()));
    }

    #[test]
    fn repeated_backup_verifies_and_reuses_content_addressed_directory() {
        let (_temporary, root) = source_root();
        write_source(&root, "thoughts/input.json", b"same source");

        let first = ensure_verified_backup(&root).unwrap();
        let second = ensure_verified_backup(&root).unwrap();
        assert!(!first.reused);
        assert!(second.reused);
        assert_eq!(first.backup_dir, second.backup_dir);
        assert_eq!(first.manifest, second.manifest);

        let published_count = fs::read_dir(root.join("migrations/backups"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("v1-to-v2-"))
            .count();
        assert_eq!(published_count, 1);
    }

    #[test]
    fn tampered_published_backup_is_rejected_not_replaced() {
        let (_temporary, root) = source_root();
        write_source(&root, "thoughts/input.json", b"original source");
        let backup = ensure_verified_backup(&root).unwrap();

        let copied = backup.backup_dir.join("thoughts/input.json");
        fs::write(&copied, b"tampered backup").unwrap();
        #[cfg(unix)]
        crate::data::permissions::secure_existing_file(&copied).unwrap();

        assert!(ensure_verified_backup(&root).is_err());
        assert_eq!(fs::read(copied).unwrap(), b"tampered backup");
    }

    #[test]
    fn source_change_between_copy_and_final_fingerprint_is_rejected() {
        let (_temporary, root) = source_root();
        write_source(&root, "thoughts/input.json", b"before");

        let result = ensure_verified_backup_inner(&root, || {
            write_source(&root, "thoughts/input.json", b"after");
        });
        assert!(result.is_err());
        let backup_root = root.join("migrations/backups");
        assert_eq!(fs::read_dir(backup_root).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn backup_directories_are_0700_and_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;

        let (_temporary, root) = source_root();
        write_source(&root, "conversations/a/b/turn.json", b"turn");
        let backup = ensure_verified_backup(&root).unwrap();

        for directory in [
            root.join("migrations"),
            root.join("migrations/backups"),
            backup.backup_dir.clone(),
            backup.backup_dir.join("conversations"),
            backup.backup_dir.join("conversations/a"),
            backup.backup_dir.join("conversations/a/b"),
        ] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o7777,
                0o700
            );
        }
        for file in [
            backup.backup_dir.join("conversations/a/b/turn.json"),
            backup.backup_dir.join(MANIFEST_FILE),
        ] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o7777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn active_source_symlink_and_special_file_are_rejected() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let (_temporary, symlink_root) = source_root();
        write_source(&symlink_root, "real.json", b"real");
        fs::create_dir(symlink_root.join("thoughts")).unwrap();
        symlink(
            symlink_root.join("real.json"),
            symlink_root.join("thoughts/link.json"),
        )
        .unwrap();
        assert!(ensure_verified_backup(&symlink_root).is_err());

        let (_temporary, special_root) = source_root();
        fs::create_dir(special_root.join("thoughts")).unwrap();
        let _socket = UnixListener::bind(special_root.join("thoughts/socket")).unwrap();
        assert!(ensure_verified_backup(&special_root).is_err());
    }

    #[test]
    fn restores_complete_thoughts_subtree_and_ignores_other_prefixes() {
        let (temporary, root) = source_root();
        write_source(
            &root,
            "thoughts/2026/07/record/input.json",
            br#"{"input":"hello"}"#,
        );
        write_source(
            &root,
            "thoughts/2026/07/record/output.json",
            br#"{"output":"world"}"#,
        );
        write_source(&root, "conversations/legacy.md", b"legacy");
        let backup = ensure_verified_backup(&root).unwrap();
        let target = temporary.path().join("restored");

        assert_eq!(
            restore_verified_subtree(&backup, "thoughts", &target).unwrap(),
            2
        );
        assert_eq!(
            fs::read(target.join("thoughts/2026/07/record/input.json")).unwrap(),
            br#"{"input":"hello"}"#
        );
        assert_eq!(
            fs::read(target.join("thoughts/2026/07/record/output.json")).unwrap(),
            br#"{"output":"world"}"#
        );
        assert!(!target.join("conversations").exists());
        assert_eq!(
            restore_verified_subtree(&backup, "missing", &target).unwrap(),
            0
        );
    }

    #[test]
    fn repeated_restore_is_noop_but_different_target_is_a_conflict() {
        let (temporary, root) = source_root();
        write_source(&root, "thoughts/record/input.json", b"original");
        let backup = ensure_verified_backup(&root).unwrap();
        let target = temporary.path().join("restored");
        let destination = target.join("thoughts/record/input.json");

        assert_eq!(
            restore_verified_subtree(&backup, "thoughts", &target).unwrap(),
            1
        );
        assert_eq!(
            restore_verified_subtree(&backup, "thoughts", &target).unwrap(),
            0
        );

        fs::write(&destination, b"different target").unwrap();
        assert!(restore_verified_subtree(&backup, "thoughts", &target).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"different target");
    }

    #[test]
    fn restore_rejects_tampered_backup_before_writing_target() {
        let (temporary, root) = source_root();
        write_source(&root, "thoughts/record/input.json", b"verified");
        let backup = ensure_verified_backup(&root).unwrap();
        let copied = backup.backup_dir.join("thoughts/record/input.json");
        fs::write(&copied, b"tampered").unwrap();
        #[cfg(unix)]
        crate::data::permissions::secure_existing_file(&copied).unwrap();
        let target = temporary.path().join("restored");

        assert!(restore_verified_subtree(&backup, "thoughts", &target).is_err());
        assert!(!target.join("thoughts/record/input.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn restored_tree_is_private_and_idempotent_restore_repairs_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (temporary, root) = source_root();
        write_source(
            &root,
            "thoughts/2026/07/record/input.json",
            b"private thought",
        );
        let backup = ensure_verified_backup(&root).unwrap();
        let target = temporary.path().join("restored");
        let directories = [
            target.clone(),
            target.join("thoughts"),
            target.join("thoughts/2026"),
            target.join("thoughts/2026/07"),
            target.join("thoughts/2026/07/record"),
        ];
        let destination = target.join("thoughts/2026/07/record/input.json");

        assert_eq!(
            restore_verified_subtree(&backup, "thoughts", &target).unwrap(),
            1
        );
        for directory in &directories {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o7777,
                0o700
            );
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            restore_verified_subtree(&backup, "thoughts", &target).unwrap(),
            0
        );
        for directory in &directories {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o7777,
                0o700
            );
        }
        assert_eq!(
            fs::metadata(destination).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }
}
