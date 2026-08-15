use super::{BackupFileEntry, VerifiedBackup};
use crate::agent::thought::{ThinkingInput, ThinkingOutput, ThoughtContext};
use crate::common::types::ThoughtId;
use crate::common::{AgentError, Result, UtcTimestamp};
use crate::data::thought_store::ThoughtStore;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const CONVERSATION_MIGRATION_PLAN_SCHEMA_VERSION: u32 = 1;

const CONVERSATIONS_DIR: &str = "conversations";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationMigrationPlan {
    pub schema_version: u32,
    pub source_files: u64,
    pub entries: Vec<ConversationMigrationEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConversationMigrationEntry {
    Import {
        user_path: String,
        assistant_path: String,
        thought_id: ThoughtId,
        occurred_at: UtcTimestamp,
    },
    Quarantine {
        paths: Vec<String>,
        reason: ConversationQuarantineReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationQuarantineReason {
    InvalidUtf8,
    MalformedFrontmatter,
    MissingSessionId,
    MissingTurn,
    MissingRole,
    UnsupportedRole,
    MissingTimestamp,
    InvalidTimestamp,
    IncompletePair,
    DuplicateRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationMigrationReport {
    pub migrated: u64,
    pub quarantined: u64,
    pub source_files: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyRole {
    User,
    Assistant,
}

#[derive(Debug)]
struct ParsedLegacyFile {
    path: String,
    session_id: String,
    turn: u64,
    role: LegacyRole,
    created_at: UtcTimestamp,
    body: String,
}

#[derive(Debug)]
struct PreparedImport {
    thought_id: ThoughtId,
    occurred_at: UtcTimestamp,
    user_body: String,
    assistant_body: String,
}

pub fn plan_conversation_migration(backup: &VerifiedBackup) -> Result<ConversationMigrationPlan> {
    let source_paths = scan_conversation_sources(&backup.backup_dir)?;
    let mut groups = BTreeMap::<(String, u64), Vec<ParsedLegacyFile>>::new();
    let mut quarantined = Vec::new();

    for path in &source_paths {
        let bytes = read_verified_backup_file(backup, path)?;
        match parse_legacy_file(path, &bytes) {
            Ok(parsed) => groups
                .entry((parsed.session_id.clone(), parsed.turn))
                .or_default()
                .push(parsed),
            Err(reason) => quarantined.push(ConversationMigrationEntry::Quarantine {
                paths: vec![path.clone()],
                reason,
            }),
        }
    }

    let mut imports = Vec::new();
    let mut generated_ids = BTreeSet::new();
    for (_, files) in groups {
        let user_count = files
            .iter()
            .filter(|file| file.role == LegacyRole::User)
            .count();
        let assistant_count = files
            .iter()
            .filter(|file| file.role == LegacyRole::Assistant)
            .count();
        if user_count == 1 && assistant_count == 1 && files.len() == 2 {
            let user = files
                .iter()
                .find(|file| file.role == LegacyRole::User)
                .expect("validated user count");
            let assistant = files
                .iter()
                .find(|file| file.role == LegacyRole::Assistant)
                .expect("validated assistant count");
            let thought_id = unique_thought_id(&mut generated_ids);
            imports.push(ConversationMigrationEntry::Import {
                user_path: user.path.clone(),
                assistant_path: assistant.path.clone(),
                thought_id,
                occurred_at: user.created_at.clone(),
            });
        } else {
            let reason = if user_count > 1 || assistant_count > 1 {
                ConversationQuarantineReason::DuplicateRole
            } else {
                ConversationQuarantineReason::IncompletePair
            };
            let mut paths: Vec<_> = files.into_iter().map(|file| file.path).collect();
            paths.sort();
            quarantined.push(ConversationMigrationEntry::Quarantine { paths, reason });
        }
    }

    imports.sort_by(|left, right| import_timestamp(left).cmp(import_timestamp(right)));
    quarantined.sort_by(|left, right| first_entry_path(left).cmp(first_entry_path(right)));
    imports.extend(quarantined);

    let plan = ConversationMigrationPlan {
        schema_version: CONVERSATION_MIGRATION_PLAN_SCHEMA_VERSION,
        source_files: source_paths.len() as u64,
        entries: imports,
    };
    validate_plan(&plan, &source_paths)?;
    Ok(plan)
}

pub fn apply_conversation_migration(
    backup: &VerifiedBackup,
    plan: &ConversationMigrationPlan,
    target_data_root: &Path,
) -> Result<ConversationMigrationReport> {
    let source_paths = scan_conversation_sources(&backup.backup_dir)?;
    let report = validate_plan(plan, &source_paths)?;
    let mut prepared = Vec::new();

    for entry in &plan.entries {
        match entry {
            ConversationMigrationEntry::Import {
                user_path,
                assistant_path,
                thought_id,
                occurred_at,
            } => {
                let user = parse_verified_import_file(backup, user_path, LegacyRole::User)?;
                let assistant =
                    parse_verified_import_file(backup, assistant_path, LegacyRole::Assistant)?;
                if user.session_id != assistant.session_id || user.turn != assistant.turn {
                    return Err(migration_error(
                        "planned user and assistant no longer share legacy identity",
                    ));
                }
                if &user.created_at != occurred_at {
                    return Err(migration_error(
                        "planned Thought timestamp no longer matches the user source",
                    ));
                }
                prepared.push(PreparedImport {
                    thought_id: thought_id.clone(),
                    occurred_at: occurred_at.clone(),
                    user_body: user.body,
                    assistant_body: assistant.body,
                });
            }
            ConversationMigrationEntry::Quarantine { paths, .. } => {
                for path in paths {
                    read_verified_backup_file(backup, path)?;
                }
            }
        }
    }

    let store = ThoughtStore::open(target_data_root)?;
    for item in prepared {
        let mut context = ThoughtContext::new_at(
            item.thought_id,
            item.occurred_at,
            ThinkingInput::User {
                text: item.user_body,
            },
        );
        store.persist_input(&context)?;
        context.set_output(ThinkingOutput::completed(
            None,
            Some(item.assistant_body),
            None,
        ));
        store.persist_output(&context)?;
    }

    Ok(report)
}

fn unique_thought_id(existing: &mut BTreeSet<ThoughtId>) -> ThoughtId {
    loop {
        let candidate = ThoughtId::new();
        if existing.insert(candidate.clone()) {
            return candidate;
        }
    }
}

fn import_timestamp(entry: &ConversationMigrationEntry) -> &UtcTimestamp {
    match entry {
        ConversationMigrationEntry::Import { occurred_at, .. } => occurred_at,
        ConversationMigrationEntry::Quarantine { .. } => {
            unreachable!("timestamp lookup is only used for import entries")
        }
    }
}

fn first_entry_path(entry: &ConversationMigrationEntry) -> &str {
    match entry {
        ConversationMigrationEntry::Import { user_path, .. } => user_path,
        ConversationMigrationEntry::Quarantine { paths, .. } => {
            paths.first().map(String::as_str).unwrap_or("")
        }
    }
}

fn validate_plan(
    plan: &ConversationMigrationPlan,
    source_paths: &[String],
) -> Result<ConversationMigrationReport> {
    if plan.schema_version != CONVERSATION_MIGRATION_PLAN_SCHEMA_VERSION {
        return Err(migration_error(format!(
            "unsupported conversation plan schema version {}",
            plan.schema_version
        )));
    }

    let mut planned_paths = BTreeSet::new();
    let mut thought_ids = BTreeSet::new();
    let mut migrated = 0_u64;
    let mut quarantined = 0_u64;
    for entry in &plan.entries {
        match entry {
            ConversationMigrationEntry::Import {
                user_path,
                assistant_path,
                thought_id,
                ..
            } => {
                validate_source_path(user_path)?;
                validate_source_path(assistant_path)?;
                if user_path == assistant_path {
                    return Err(migration_error(
                        "an import cannot use one source for both roles",
                    ));
                }
                if !thought_ids.insert(thought_id.clone()) {
                    return Err(migration_error("plan contains a duplicate Thought ID"));
                }
                for path in [user_path, assistant_path] {
                    if !planned_paths.insert(path.clone()) {
                        return Err(migration_error("plan contains a duplicate source path"));
                    }
                }
                migrated += 2;
            }
            ConversationMigrationEntry::Quarantine { paths, .. } => {
                if paths.is_empty() {
                    return Err(migration_error("quarantine entry has no source paths"));
                }
                for path in paths {
                    validate_source_path(path)?;
                    if !planned_paths.insert(path.clone()) {
                        return Err(migration_error("plan contains a duplicate source path"));
                    }
                    quarantined += 1;
                }
            }
        }
    }

    let actual_paths: BTreeSet<_> = source_paths.iter().cloned().collect();
    if planned_paths != actual_paths {
        return Err(migration_error(
            "plan source paths do not exactly cover the backup conversations",
        ));
    }
    if plan.source_files != source_paths.len() as u64 || migrated + quarantined != plan.source_files
    {
        return Err(migration_error(
            "conversation migration file counts are not conserved",
        ));
    }

    Ok(ConversationMigrationReport {
        migrated,
        quarantined,
        source_files: plan.source_files,
    })
}

fn scan_conversation_sources(backup_root: &Path) -> Result<Vec<String>> {
    let conversations = backup_root.join(CONVERSATIONS_DIR);
    match fs::symlink_metadata(&conversations) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(migration_error("backup conversations path is a symlink"));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(migration_error(
                "backup conversations path is not a directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(migration_error(format!(
                "cannot inspect backup conversations: {error}"
            )));
        }
    }

    let mut paths = Vec::new();
    collect_markdown_sources(&conversations, backup_root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn collect_markdown_sources(
    directory: &Path,
    backup_root: &Path,
    paths: &mut Vec<String>,
) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(|error| {
        migration_error(format!(
            "cannot list backup conversation directory: {error}"
        ))
    })? {
        let entry = entry.map_err(|error| {
            migration_error(format!("cannot inspect backup conversation entry: {error}"))
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            migration_error(format!(
                "cannot inspect backup conversation source: {error}"
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(migration_error(
                "backup conversations cannot contain symlinks",
            ));
        }
        if metadata.is_dir() {
            collect_markdown_sources(&path, backup_root, paths)?;
        } else if metadata.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("md")
        {
            paths.push(encode_relative_path(&path, backup_root)?);
        } else if !metadata.is_file() {
            return Err(migration_error(
                "backup conversations cannot contain special files",
            ));
        }
    }
    Ok(())
}

fn encode_relative_path(path: &Path, root: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| migration_error("conversation source escaped backup root"))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| migration_error("conversation path is not valid UTF-8"))?;
                if value.is_empty() || value.contains('\\') {
                    return Err(migration_error("conversation path is not portable"));
                }
                parts.push(value);
            }
            _ => {
                return Err(migration_error(
                    "conversation path is not strictly relative",
                ))
            }
        }
    }
    let encoded = parts.join("/");
    validate_source_path(&encoded)?;
    Ok(encoded)
}

fn validate_source_path(path: &str) -> Result<()> {
    let mut parts = path.split('/');
    if parts.next() != Some(CONVERSATIONS_DIR)
        || path.starts_with('/')
        || path.contains('\\')
        || parts.any(|part| part.is_empty() || part == "." || part == "..")
        || !path.ends_with(".md")
    {
        return Err(migration_error("plan contains an unsafe source path"));
    }
    Ok(())
}

fn read_verified_backup_file(backup: &VerifiedBackup, relative_path: &str) -> Result<Vec<u8>> {
    validate_source_path(relative_path)?;
    let manifest_entry = backup
        .manifest
        .files
        .iter()
        .find(|entry| entry.path == relative_path)
        .ok_or_else(|| migration_error("conversation source is absent from backup manifest"))?;
    let path = checked_backup_path(&backup.backup_dir, relative_path)?;
    let bytes = fs::read(&path).map_err(|error| {
        migration_error(format!("cannot read verified conversation source: {error}"))
    })?;
    verify_manifest_hash(manifest_entry, &bytes)?;
    Ok(bytes)
}

fn checked_backup_path(backup_root: &Path, relative_path: &str) -> Result<PathBuf> {
    let mut path = backup_root.to_path_buf();
    let parts: Vec<_> = relative_path.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            migration_error(format!(
                "cannot inspect verified conversation source: {error}"
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(migration_error(
                "verified conversation source path contains a symlink",
            ));
        }
        let is_last = index + 1 == parts.len();
        if (is_last && !metadata.is_file()) || (!is_last && !metadata.is_dir()) {
            return Err(migration_error(
                "verified conversation source changed filesystem type",
            ));
        }
    }
    Ok(path)
}

fn verify_manifest_hash(entry: &BackupFileEntry, bytes: &[u8]) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut actual = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut actual, "{byte:02x}").expect("writing to String cannot fail");
    }
    if entry.bytes != bytes.len() as u64 || entry.sha256 != actual {
        return Err(migration_error(
            "conversation source does not match its verified backup hash",
        ));
    }
    Ok(())
}

fn parse_verified_import_file(
    backup: &VerifiedBackup,
    path: &str,
    expected_role: LegacyRole,
) -> Result<ParsedLegacyFile> {
    let bytes = read_verified_backup_file(backup, path)?;
    let parsed = parse_legacy_file(path, &bytes).map_err(|reason| {
        migration_error(format!("planned import is no longer valid: {reason:?}"))
    })?;
    if parsed.role != expected_role {
        return Err(migration_error(
            "planned conversation source no longer has its expected role",
        ));
    }
    Ok(parsed)
}

fn parse_legacy_file(
    path: &str,
    bytes: &[u8],
) -> std::result::Result<ParsedLegacyFile, ConversationQuarantineReason> {
    let content =
        std::str::from_utf8(bytes).map_err(|_| ConversationQuarantineReason::InvalidUtf8)?;
    let (frontmatter, body) = split_frontmatter(content)?;
    let fields = parse_frontmatter_fields(&frontmatter)?;

    let session_id = required_field(
        &fields,
        "session_id",
        ConversationQuarantineReason::MissingSessionId,
    )?;
    let turn = required_field(&fields, "turn", ConversationQuarantineReason::MissingTurn)?
        .parse::<u64>()
        .map_err(|_| ConversationQuarantineReason::MalformedFrontmatter)?;
    let role = match required_field(&fields, "role", ConversationQuarantineReason::MissingRole)?
        .as_str()
    {
        "user" => LegacyRole::User,
        "assistant" => LegacyRole::Assistant,
        _ => return Err(ConversationQuarantineReason::UnsupportedRole),
    };
    let created_at = normalize_timestamp(&required_field(
        &fields,
        "created_at",
        ConversationQuarantineReason::MissingTimestamp,
    )?)?;

    Ok(ParsedLegacyFile {
        path: path.to_string(),
        session_id,
        turn,
        role,
        created_at,
        body: body.trim().to_string(),
    })
}

fn split_frontmatter(
    content: &str,
) -> std::result::Result<(Vec<&str>, &str), ConversationQuarantineReason> {
    let mut lines = content.split_inclusive('\n');
    let opening = lines
        .next()
        .ok_or(ConversationQuarantineReason::MalformedFrontmatter)?;
    if trim_line_ending(opening) != "---" {
        return Err(ConversationQuarantineReason::MalformedFrontmatter);
    }

    let mut frontmatter = Vec::new();
    let mut body_offset = opening.len();
    for line in lines {
        body_offset += line.len();
        let line_without_ending = trim_line_ending(line);
        if line_without_ending == "---" {
            return Ok((frontmatter, &content[body_offset..]));
        }
        frontmatter.push(line_without_ending);
    }
    Err(ConversationQuarantineReason::MalformedFrontmatter)
}

fn trim_line_ending(line: &str) -> &str {
    line.strip_suffix('\n')
        .unwrap_or(line)
        .strip_suffix('\r')
        .unwrap_or_else(|| line.strip_suffix('\n').unwrap_or(line))
}

fn parse_frontmatter_fields(
    lines: &[&str],
) -> std::result::Result<BTreeMap<String, String>, ConversationQuarantineReason> {
    let mut fields = BTreeMap::new();
    let mut nested_parent: Option<String> = None;
    let mut nested_keys = BTreeSet::new();
    for line in lines {
        if line.is_empty() || line.contains('\t') || line.trim_start().starts_with('#') {
            return Err(ConversationQuarantineReason::MalformedFrontmatter);
        }
        if let Some(nested) = line.strip_prefix("  ") {
            let parent = nested_parent
                .as_ref()
                .ok_or(ConversationQuarantineReason::MalformedFrontmatter)?;
            if nested.starts_with(' ') {
                return Err(ConversationQuarantineReason::MalformedFrontmatter);
            }
            let (key, value) = parse_mapping_line(nested)?;
            if value.is_empty() || !nested_keys.insert((parent.clone(), key)) {
                return Err(ConversationQuarantineReason::MalformedFrontmatter);
            }
            parse_scalar(&value)?;
            continue;
        }
        if line.starts_with(' ') {
            return Err(ConversationQuarantineReason::MalformedFrontmatter);
        }

        let (key, value) = parse_mapping_line(line)?;
        if fields.contains_key(&key) {
            return Err(ConversationQuarantineReason::MalformedFrontmatter);
        }
        if value.is_empty() {
            nested_parent = Some(key.clone());
            fields.insert(key, String::new());
        } else {
            nested_parent = None;
            fields.insert(key, parse_scalar(&value)?);
        }
    }
    Ok(fields)
}

fn parse_mapping_line(
    line: &str,
) -> std::result::Result<(String, String), ConversationQuarantineReason> {
    let (key, value) = line
        .split_once(':')
        .ok_or(ConversationQuarantineReason::MalformedFrontmatter)?;
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(ConversationQuarantineReason::MalformedFrontmatter);
    }
    Ok((key.to_string(), value.trim().to_string()))
}

fn parse_scalar(value: &str) -> std::result::Result<String, ConversationQuarantineReason> {
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value)
            .map_err(|_| ConversationQuarantineReason::MalformedFrontmatter);
    }
    if value.starts_with('\'') {
        if value.len() < 2 || !value.ends_with('\'') {
            return Err(ConversationQuarantineReason::MalformedFrontmatter);
        }
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    if value.is_empty()
        || value.contains(" #")
        || value.starts_with(['[', '{', '&', '*', '!', '|', '>'])
        || matches!(value, "null" | "Null" | "NULL" | "~")
    {
        return Err(ConversationQuarantineReason::MalformedFrontmatter);
    }
    Ok(value.to_string())
}

fn required_field(
    fields: &BTreeMap<String, String>,
    name: &str,
    missing: ConversationQuarantineReason,
) -> std::result::Result<String, ConversationQuarantineReason> {
    fields
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or(missing)
}

fn normalize_timestamp(
    value: &str,
) -> std::result::Result<UtcTimestamp, ConversationQuarantineReason> {
    if let Ok(seconds) = value.parse::<i64>() {
        let datetime = DateTime::<Utc>::from_timestamp(seconds, 0)
            .ok_or(ConversationQuarantineReason::InvalidTimestamp)?;
        return Ok(UtcTimestamp::from_datetime(datetime));
    }
    let datetime = DateTime::parse_from_rfc3339(value)
        .map_err(|_| ConversationQuarantineReason::InvalidTimestamp)?
        .with_timezone(&Utc);
    Ok(UtcTimestamp::from_datetime(datetime))
}

fn migration_error(message: impl Into<String>) -> AgentError {
    AgentError::Parse(format!("conversation migration: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::migration::ensure_verified_backup;

    fn write_legacy_file(
        root: &Path,
        filename: &str,
        session_id: &str,
        turn: u64,
        role: &str,
        created_at: &str,
        body: &str,
    ) {
        let path = root.join(CONVERSATIONS_DIR).join(filename);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "---\nsession_id: \"{session_id}\"\nturn: {turn}\nrole: \"{role}\"\ncreated_at: \"{created_at}\"\n---\n\n{body}\n"
            ),
        )
        .unwrap();
    }

    fn backup_root() -> (tempfile::TempDir, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        fs::create_dir(&root).unwrap();
        (temporary, root)
    }

    #[test]
    fn imports_only_explicit_pair_and_retries_idempotently() {
        let (_temporary, root) = backup_root();
        write_legacy_file(
            &root,
            "turn_001_user.md",
            "legacy-session",
            1,
            "user",
            "1783814400",
            "question",
        );
        write_legacy_file(
            &root,
            "turn_001_assistant.md",
            "legacy-session",
            1,
            "assistant",
            "1783814401",
            "answer",
        );
        let backup = ensure_verified_backup(&root).unwrap();
        let plan = plan_conversation_migration(&backup).unwrap();
        assert_eq!(plan.source_files, 2);
        assert!(matches!(
            plan.entries.as_slice(),
            [ConversationMigrationEntry::Import { .. }]
        ));
        let serialized = serde_json::to_string(&plan).unwrap();
        assert!(!serialized.contains("legacy-session"));
        assert!(!serialized.contains("question"));
        assert!(!serialized.contains("answer"));

        let target = root.join("target");
        let first = apply_conversation_migration(&backup, &plan, &target).unwrap();
        let second = apply_conversation_migration(&backup, &plan, &target).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.migrated, 2);
        assert_eq!(first.quarantined, 0);

        let recovered = ThoughtStore::open(&target).unwrap().recover().unwrap();
        assert_eq!(recovered.groups.len(), 1);
        assert_eq!(recovered.groups[0].contexts.len(), 1);
        let thought = &recovered.groups[0].contexts[0];
        assert_eq!(
            thought.input,
            ThinkingInput::User {
                text: "question".to_string()
            }
        );
        assert_eq!(
            thought.output.as_ref().unwrap().say.as_deref(),
            Some("answer")
        );
        assert!(thought.output.as_ref().unwrap().think.is_none());
        assert_eq!(
            thought.occurred_at.to_string(),
            "2026-07-12T00:00:00.000000000Z"
        );
    }

    #[test]
    fn assistant_only_and_filename_suffix_are_quarantined_without_pairing_guess() {
        let (_temporary, root) = backup_root();
        write_legacy_file(
            &root,
            "20260711_160000_2.md",
            "legacy-session",
            5,
            "assistant",
            "2026-07-11T16:00:00Z",
            "assistant only",
        );
        let backup = ensure_verified_backup(&root).unwrap();
        let plan = plan_conversation_migration(&backup).unwrap();
        assert!(matches!(
            plan.entries.as_slice(),
            [ConversationMigrationEntry::Quarantine {
                reason: ConversationQuarantineReason::IncompletePair,
                ..
            }]
        ));
        let report = apply_conversation_migration(&backup, &plan, &root.join("target")).unwrap();
        assert_eq!(report.migrated, 0);
        assert_eq!(report.quarantined, 1);
    }

    #[test]
    fn duplicate_roles_are_quarantined_and_same_timestamps_remain_an_unordered_group() {
        let (_temporary, root) = backup_root();
        for turn in [1_u64, 2] {
            write_legacy_file(
                &root,
                &format!("u-{turn}.md"),
                "legacy-session",
                turn,
                "user",
                "1783814400",
                &format!("question-{turn}"),
            );
            write_legacy_file(
                &root,
                &format!("a-{turn}.md"),
                "legacy-session",
                turn,
                "assistant",
                "1783814400",
                &format!("answer-{turn}"),
            );
        }
        write_legacy_file(
            &root,
            "duplicate-user.md",
            "legacy-session",
            3,
            "user",
            "1783814400",
            "first",
        );
        write_legacy_file(
            &root,
            "duplicate-user-2.md",
            "legacy-session",
            3,
            "user",
            "1783814400",
            "second",
        );

        let backup = ensure_verified_backup(&root).unwrap();
        let plan = plan_conversation_migration(&backup).unwrap();
        assert_eq!(plan.entries.len(), 3);
        assert!(plan.entries.iter().any(|entry| matches!(
            entry,
            ConversationMigrationEntry::Quarantine {
                reason: ConversationQuarantineReason::DuplicateRole,
                ..
            }
        )));
        let report = apply_conversation_migration(&backup, &plan, &root.join("target")).unwrap();
        assert_eq!(report.migrated, 4);
        assert_eq!(report.quarantined, 2);
        assert_eq!(report.migrated + report.quarantined, report.source_files);

        let timeline = ThoughtStore::open(root.join("target"))
            .unwrap()
            .recover()
            .unwrap();
        assert_eq!(timeline.groups.len(), 1);
        assert_eq!(timeline.groups[0].contexts.len(), 2);
        let inputs: BTreeSet<_> = timeline.groups[0]
            .contexts
            .iter()
            .map(|thought| match &thought.input {
                ThinkingInput::User { text } => text.clone(),
                _ => unreachable!("legacy import only creates user inputs"),
            })
            .collect();
        assert_eq!(
            inputs,
            BTreeSet::from(["question-1".to_string(), "question-2".to_string()])
        );
    }

    #[test]
    fn malformed_and_missing_identity_files_are_quarantined_per_file() {
        let (_temporary, root) = backup_root();
        let conversation_dir = root.join(CONVERSATIONS_DIR);
        fs::create_dir_all(&conversation_dir).unwrap();
        fs::write(conversation_dir.join("bad.md"), "not frontmatter").unwrap();
        fs::write(
            conversation_dir.join("missing-session.md"),
            "---\nturn: 1\nrole: user\ncreated_at: 1783814400\n---\n\nbody\n",
        )
        .unwrap();
        let backup = ensure_verified_backup(&root).unwrap();
        let plan = plan_conversation_migration(&backup).unwrap();
        assert_eq!(plan.source_files, 2);
        assert!(plan
            .entries
            .iter()
            .all(|entry| matches!(entry, ConversationMigrationEntry::Quarantine { .. })));
        let report = apply_conversation_migration(&backup, &plan, &root.join("target")).unwrap();
        assert_eq!(report.migrated, 0);
        assert_eq!(report.quarantined, 2);
    }
}
