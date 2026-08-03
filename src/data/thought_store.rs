use crate::agent::thought::{
    context_from_records, ThinkingFailureInput, ThinkingTerminalState, ThoughtContext,
    ThoughtInputRecord, ThoughtOutputRecord, ThoughtTimeline, ThoughtTimestampGroup,
    RAW_MODEL_OUTPUT_FILE_NAME,
};
use crate::common::{AgentError, Result, UtcTimestamp};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

const THOUGHTS_DIR: &str = "thoughts";
const INPUT_FILE: &str = "input.json";
const OUTPUT_FILE: &str = "output.json";
const FAILURE_FILE: &str = "failure.json";

pub struct ThoughtStore {
    root: PathBuf,
    write_lock: Mutex<()>,
}

impl ThoughtStore {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let root = data_dir.as_ref().join(THOUGHTS_DIR);
        ensure_secure_directory(&root)?;
        Ok(Self {
            root,
            write_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn persist_input(&self, context: &ThoughtContext) -> Result<()> {
        context.validate()?;
        if context.output.is_some() {
            return Err(AgentError::Parse(
                "persist_input only accepts a thought before output exists".to_string(),
            ));
        }

        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| AgentError::Io("thought store write lock was poisoned".to_string()))?;
        let input_record = ThoughtInputRecord::from(context);
        let record_dir = self.record_dir(context);
        self.ensure_record_parent(context.occurred_at.clone())?;
        ensure_secure_directory(&record_dir)?;
        secure_record_files(&record_dir)?;
        atomic_write_json(&record_dir, INPUT_FILE, &input_record)
    }

    pub fn persist_output(&self, context: &ThoughtContext) -> Result<()> {
        let output_record = ThoughtOutputRecord::from_context(context)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| AgentError::Io("thought store write lock was poisoned".to_string()))?;
        let record_dir = self.record_dir(context);
        if !self.secure_existing_record_hierarchy(context)? {
            return Err(AgentError::NotFound(format!(
                "durable thought input for {}",
                context.thought_id
            )));
        }
        let input_path = record_dir.join(INPUT_FILE);
        if !secure_existing_file(&input_path)? {
            return Err(AgentError::NotFound(format!(
                "durable thought input for {}",
                context.thought_id
            )));
        }

        let input: ThoughtInputRecord = read_json(&input_path)?;
        if input.thought_id != context.thought_id
            || input.occurred_at != context.occurred_at
            || input.input != context.input
        {
            return Err(AgentError::Parse(
                "thought output does not match its durable input record".to_string(),
            ));
        }

        atomic_write_json(&record_dir, OUTPUT_FILE, &output_record)
    }

    pub fn persist_failure_input(
        &self,
        context: &ThoughtContext,
        failure: &ThinkingFailureInput,
        raw_model_output: &[u8],
    ) -> Result<()> {
        context.validate()?;
        if !matches!(
            context.output.as_ref().map(|output| &output.terminal_state),
            Some(ThinkingTerminalState::Failed { .. })
        ) {
            return Err(AgentError::Parse(
                "ThinkingFailureInput requires a failed thought output".to_string(),
            ));
        }
        if failure.failed_thought_id != context.thought_id
            || failure.occurred_at != context.occurred_at
        {
            return Err(AgentError::Parse(
                "ThinkingFailureInput does not match its failed thought".to_string(),
            ));
        }
        if !failure.matches_raw_model_output(raw_model_output) {
            return Err(AgentError::Parse(
                "ThinkingFailureInput raw output reference does not match its content".to_string(),
            ));
        }

        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| AgentError::Io("thought store write lock was poisoned".to_string()))?;
        let record_dir = self.record_dir(context);
        if !self.secure_existing_record_hierarchy(context)?
            || !secure_existing_file(&record_dir.join(OUTPUT_FILE))?
        {
            return Err(AgentError::NotFound(format!(
                "durable failed thought output for {}",
                context.thought_id
            )));
        }

        let output: ThoughtOutputRecord = read_json(&record_dir.join(OUTPUT_FILE))?;
        if output.thought_id != context.thought_id
            || output.occurred_at != context.occurred_at
            || !matches!(
                output.output.terminal_state,
                ThinkingTerminalState::Failed { .. }
            )
        {
            return Err(AgentError::Parse(
                "durable thought output is not the expected failed record".to_string(),
            ));
        }

        atomic_write_bytes(&record_dir, RAW_MODEL_OUTPUT_FILE_NAME, raw_model_output)?;
        atomic_write_json(&record_dir, FAILURE_FILE, failure)
    }

    pub fn load_failure_input(
        &self,
        context: &ThoughtContext,
    ) -> Result<Option<ThinkingFailureInput>> {
        let record_dir = self.record_dir(context);
        if !self.secure_existing_record_hierarchy(context)? {
            return Ok(None);
        }
        let path = record_dir.join(FAILURE_FILE);
        if !secure_existing_file(&path)? {
            return Ok(None);
        }
        let failure: ThinkingFailureInput = read_json(&path)?;
        let known_mode = matches!(
            failure.mode_snapshot.to_ascii_lowercase().as_str(),
            "unni" | "keep" | "loop"
        );
        let valid_errors = !failure.validation_errors.is_empty()
            && failure
                .validation_errors
                .iter()
                .all(|error| !error.code.trim().is_empty() && !error.message.trim().is_empty());
        if failure.schema_version != 1
            || !known_mode
            || !valid_errors
            || failure.failed_thought_id != context.thought_id
            || failure.occurred_at != context.occurred_at
        {
            return Err(AgentError::Parse(
                "ThinkingFailureInput is inconsistent with its thought identity".to_string(),
            ));
        }
        let output: ThoughtOutputRecord = read_json(&record_dir.join(OUTPUT_FILE))?;
        if output.thought_id != context.thought_id
            || output.occurred_at != context.occurred_at
            || !matches!(
                output.output.terminal_state,
                ThinkingTerminalState::Failed { .. }
            )
        {
            return Err(AgentError::Parse(
                "ThinkingFailureInput does not reference a durable failed output".to_string(),
            ));
        }
        let raw_path = record_dir.join(&failure.raw_model_output_ref);
        if failure.raw_model_output_ref != RAW_MODEL_OUTPUT_FILE_NAME
            || !secure_existing_file(&raw_path)?
        {
            return Err(AgentError::NotFound(format!(
                "raw model output for failure {}",
                failure.failure_event_id
            )));
        }
        let raw_model_output = fs::read(&raw_path).map_err(|error| {
            AgentError::Io(format!("read raw model output {:?}: {error}", raw_path))
        })?;
        if !failure.matches_raw_model_output(&raw_model_output) {
            return Err(AgentError::Parse(format!(
                "raw model output does not match failure {}",
                failure.failure_event_id
            )));
        }
        Ok(Some(failure))
    }

    pub fn load(&self, context: &ThoughtContext) -> Result<Option<ThoughtContext>> {
        self.load_by_identity(&context.thought_id, &context.occurred_at)
    }

    pub fn load_by_identity(
        &self,
        thought_id: &crate::agent::thought::ThoughtId,
        occurred_at: &UtcTimestamp,
    ) -> Result<Option<ThoughtContext>> {
        let record_dir = self.record_dir_for(thought_id, occurred_at);
        if !self.secure_existing_identity_hierarchy(thought_id, occurred_at)? {
            return Ok(None);
        }

        self.load_from_directory(&record_dir).map(Some)
    }

    pub fn recover(&self) -> Result<ThoughtTimeline> {
        secure_existing_tree(&self.root)?;
        let mut grouped = BTreeMap::<UtcTimestamp, Vec<ThoughtContext>>::new();
        collect_contexts(&self.root, &mut grouped)?;

        Ok(ThoughtTimeline {
            groups: grouped
                .into_iter()
                .map(|(occurred_at, contexts)| ThoughtTimestampGroup {
                    occurred_at,
                    contexts,
                })
                .collect(),
        })
    }

    fn ensure_record_parent(&self, occurred_at: UtcTimestamp) -> Result<()> {
        let (year, month, day) = occurred_at.date_components();
        let year_dir = self.root.join(format!("{year:04}"));
        let month_dir = year_dir.join(format!("{month:02}"));
        let day_dir = month_dir.join(format!("{day:02}"));
        ensure_secure_directory(&self.root)?;
        ensure_secure_directory(&year_dir)?;
        ensure_secure_directory(&month_dir)?;
        ensure_secure_directory(&day_dir)
    }

    fn secure_existing_record_hierarchy(&self, context: &ThoughtContext) -> Result<bool> {
        self.secure_existing_identity_hierarchy(&context.thought_id, &context.occurred_at)
    }

    fn secure_existing_identity_hierarchy(
        &self,
        thought_id: &crate::agent::thought::ThoughtId,
        occurred_at: &UtcTimestamp,
    ) -> Result<bool> {
        ensure_secure_directory(&self.root)?;
        let (year, month, day) = occurred_at.date_components();
        let year_dir = self.root.join(format!("{year:04}"));
        let month_dir = year_dir.join(format!("{month:02}"));
        let day_dir = month_dir.join(format!("{day:02}"));
        let record_dir = day_dir.join(format!("{}_{}", occurred_at.path_component(), thought_id));

        for directory in [&year_dir, &month_dir, &day_dir, &record_dir] {
            if !secure_existing_directory(directory)? {
                return Ok(false);
            }
        }
        secure_record_files(&record_dir)?;
        Ok(true)
    }

    fn record_dir(&self, context: &ThoughtContext) -> PathBuf {
        self.record_dir_for(&context.thought_id, &context.occurred_at)
    }

    fn record_dir_for(
        &self,
        thought_id: &crate::agent::thought::ThoughtId,
        occurred_at: &UtcTimestamp,
    ) -> PathBuf {
        let (year, month, day) = occurred_at.date_components();
        self.root
            .join(format!("{year:04}"))
            .join(format!("{month:02}"))
            .join(format!("{day:02}"))
            .join(format!("{}_{}", occurred_at.path_component(), thought_id))
    }

    fn load_from_directory(&self, record_dir: &Path) -> Result<ThoughtContext> {
        let input: ThoughtInputRecord = read_json(&record_dir.join(INPUT_FILE))?;
        let output_path = record_dir.join(OUTPUT_FILE);
        let output = if output_path.is_file() {
            Some(read_json::<ThoughtOutputRecord>(&output_path)?)
        } else {
            None
        };
        context_from_records(input, output)
    }
}

fn collect_contexts(
    directory: &Path,
    grouped: &mut BTreeMap<UtcTimestamp, Vec<ThoughtContext>>,
) -> Result<()> {
    if !secure_existing_directory(directory)? {
        return Err(AgentError::NotFound(format!(
            "thought directory {:?}",
            directory
        )));
    }
    for entry in fs::read_dir(directory).map_err(|error| {
        AgentError::Io(format!("read thought directory {:?}: {error}", directory))
    })? {
        let entry = entry.map_err(|error| {
            AgentError::Io(format!(
                "read entry in thought directory {:?}: {error}",
                directory
            ))
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| AgentError::Io(format!("inspect thought path {:?}: {error}", path)))?;
        if file_type.is_symlink() {
            return Err(AgentError::Io(format!(
                "thought store rejects symlink {:?}",
                path
            )));
        }
        if !file_type.is_dir() {
            continue;
        }

        let input_path = path.join(INPUT_FILE);
        if input_path.is_file() {
            let input: ThoughtInputRecord = read_json(&input_path)?;
            let output_path = path.join(OUTPUT_FILE);
            let output = if output_path.is_file() {
                Some(read_json::<ThoughtOutputRecord>(&output_path)?)
            } else {
                None
            };
            let context = context_from_records(input, output)?;
            grouped
                .entry(context.occurred_at.clone())
                .or_default()
                .push(context);
        } else {
            collect_contexts(&path, grouped)?;
        }
    }

    Ok(())
}

fn atomic_write_json<T: Serialize>(directory: &Path, filename: &str, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AgentError::Parse(format!("serialize thought record: {error}")))?;
    atomic_write_bytes(directory, filename, &bytes)
}

fn atomic_write_bytes(directory: &Path, filename: &str, bytes: &[u8]) -> Result<()> {
    let final_path = directory.join(filename);
    if secure_existing_file(&final_path)? {
        let existing = fs::read(&final_path).map_err(|error| {
            AgentError::Io(format!(
                "read existing thought record {:?}: {error}",
                final_path
            ))
        })?;
        if existing == bytes {
            return Ok(());
        }
        return Err(AgentError::Io(format!(
            "thought record is immutable and already contains different data: {:?}",
            final_path
        )));
    }

    let temporary_path = directory.join(format!(".{filename}.{}.tmp", Uuid::new_v4()));

    let write_result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary_path).map_err(|error| {
            AgentError::Io(format!(
                "create thought temporary file {:?}: {error}",
                temporary_path
            ))
        })?;
        file.write_all(bytes).map_err(|error| {
            AgentError::Io(format!(
                "write thought temporary file {:?}: {error}",
                temporary_path
            ))
        })?;
        file.sync_all().map_err(|error| {
            AgentError::Io(format!(
                "flush thought temporary file {:?}: {error}",
                temporary_path
            ))
        })?;
        drop(file);
        ensure_secure_file(&temporary_path)?;

        match fs::hard_link(&temporary_path, &final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = fs::read(&final_path).map_err(|read_error| {
                    AgentError::Io(format!(
                        "read concurrently published thought record {:?}: {read_error}",
                        final_path
                    ))
                })?;
                if existing != bytes {
                    return Err(AgentError::Io(format!(
                        "thought record was concurrently published with different data: {:?}",
                        final_path
                    )));
                }
            }
            Err(error) => {
                return Err(AgentError::Io(format!(
                    "publish thought record {:?} to {:?}: {error}",
                    temporary_path, final_path
                )));
            }
        }
        ensure_secure_file(&final_path)?;
        fs::remove_file(&temporary_path).map_err(|error| {
            AgentError::Io(format!(
                "remove published thought temporary file {:?}: {error}",
                temporary_path
            ))
        })?;
        sync_directory(directory)
    })();

    if write_result.is_err() && temporary_path.exists() {
        let _ = fs::remove_file(&temporary_path);
    }

    write_result
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    if !secure_existing_file(path)? {
        return Err(AgentError::NotFound(format!("thought record {:?}", path)));
    }
    let bytes = fs::read(path)
        .map_err(|error| AgentError::Io(format!("read thought record {:?}: {error}", path)))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| AgentError::Parse(format!("parse thought record {:?}: {error}", path)))
}

fn ensure_secure_directory(path: &Path) -> Result<()> {
    if secure_existing_directory(path)? {
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).map_err(|error| {
            AgentError::Io(format!("create thought directory {:?}: {error}", path))
        })?;
    }

    #[cfg(not(unix))]
    fs::create_dir_all(path)
        .map_err(|error| AgentError::Io(format!("create thought directory {:?}: {error}", path)))?;

    if !secure_existing_directory(path)? {
        return Err(AgentError::NotFound(format!(
            "thought directory {:?}",
            path
        )));
    }

    Ok(())
}

fn secure_existing_tree(directory: &Path) -> Result<()> {
    if !secure_existing_directory(directory)? {
        return Err(AgentError::NotFound(format!(
            "thought directory {:?}",
            directory
        )));
    }

    for entry in fs::read_dir(directory).map_err(|error| {
        AgentError::Io(format!("read thought directory {:?}: {error}", directory))
    })? {
        let entry = entry.map_err(|error| {
            AgentError::Io(format!(
                "read entry in thought directory {:?}: {error}",
                directory
            ))
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| AgentError::Io(format!("inspect thought path {:?}: {error}", path)))?;
        if file_type.is_symlink() {
            return Err(AgentError::Io(format!(
                "thought store rejects symlink {:?}",
                path
            )));
        }
        if file_type.is_dir() {
            secure_existing_directory(&path)?;
        } else if file_type.is_file() {
            ensure_secure_file(&path)?;
        } else {
            return Err(AgentError::Io(format!(
                "thought store rejects non-file path {:?}",
                path
            )));
        }
    }

    Ok(())
}

fn secure_record_files(record_dir: &Path) -> Result<()> {
    for filename in [
        INPUT_FILE,
        OUTPUT_FILE,
        FAILURE_FILE,
        RAW_MODEL_OUTPUT_FILE_NAME,
    ] {
        secure_existing_file(&record_dir.join(filename))?;
    }
    Ok(())
}

fn secure_existing_directory(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(AgentError::Io(format!(
                "stat thought directory {:?}: {error}",
                path
            )))
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(AgentError::Io(format!(
            "thought store rejects directory symlink {:?}",
            path
        )));
    }
    if !metadata.is_dir() {
        return Err(AgentError::Io(format!(
            "thought directory path is not a directory: {:?}",
            path
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).map_err(|error| {
            AgentError::Io(format!("chmod 700 thought directory {:?}: {error}", path))
        })?;
    }

    Ok(true)
}

fn secure_existing_file(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(AgentError::Io(format!(
                "stat thought record {:?}: {error}",
                path
            )))
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(AgentError::Io(format!(
            "thought store rejects record symlink {:?}",
            path
        )));
    }
    if !metadata.is_file() {
        return Err(AgentError::Io(format!(
            "thought record path is not a regular file: {:?}",
            path
        )));
    }
    ensure_secure_file_from_metadata(path, metadata)?;
    Ok(true)
}

fn ensure_secure_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AgentError::Io(format!("stat thought record {:?}: {error}", path)))?;
    if metadata.file_type().is_symlink() {
        return Err(AgentError::Io(format!(
            "thought store rejects record symlink {:?}",
            path
        )));
    }
    if !metadata.is_file() {
        return Err(AgentError::Io(format!(
            "thought record path is not a regular file: {:?}",
            path
        )));
    }
    ensure_secure_file_from_metadata(path, metadata)
}

#[allow(unused_variables)]
fn ensure_secure_file_from_metadata(path: &Path, metadata: fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions).map_err(|error| {
            AgentError::Io(format!("chmod 600 thought record {:?}: {error}", path))
        })?;
    }

    Ok(())
}

#[allow(unused_variables)]
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::File;
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                AgentError::Io(format!("flush thought directory {:?}: {error}", path))
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::output::OutputValidationError;
    use crate::agent::thought::{
        DownstreamRequest, ThinkingFailureInput, ThinkingInput, ThinkingOutput,
        ThinkingTerminalState, ThoughtId, ThoughtLifecycleState,
    };

    fn timestamp() -> UtcTimestamp {
        UtcTimestamp::parse("2026-07-15T12:34:56.123456789Z").unwrap()
    }

    fn context(id: &str) -> ThoughtContext {
        ThoughtContext::new_at(
            ThoughtId::parse(id).unwrap(),
            timestamp(),
            ThinkingInput::User {
                text: "persist this input first".to_string(),
            },
        )
    }

    #[cfg(unix)]
    fn record_directories(store: &ThoughtStore, thought: &ThoughtContext) -> Vec<PathBuf> {
        let record_dir = store.record_dir(thought);
        let day_dir = record_dir.parent().unwrap().to_path_buf();
        let month_dir = day_dir.parent().unwrap().to_path_buf();
        let year_dir = month_dir.parent().unwrap().to_path_buf();
        vec![
            store.root().to_path_buf(),
            year_dir,
            month_dir,
            day_dir,
            record_dir,
        ]
    }

    #[cfg(unix)]
    fn set_wide_permissions(store: &ThoughtStore, thought: &ThoughtContext) {
        use std::os::unix::fs::PermissionsExt;

        for directory in record_directories(store, thought) {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let record_dir = store.record_dir(thought);
        for filename in [
            INPUT_FILE,
            OUTPUT_FILE,
            FAILURE_FILE,
            RAW_MODEL_OUTPUT_FILE_NAME,
        ] {
            let path = record_dir.join(filename);
            if path.exists() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
    }

    #[cfg(unix)]
    fn assert_private_permissions(store: &ThoughtStore, thought: &ThoughtContext) {
        use std::os::unix::fs::PermissionsExt;

        for directory in record_directories(store, thought) {
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700,
                "unexpected directory mode for {}",
                directory.display()
            );
        }
        let record_dir = store.record_dir(thought);
        for filename in [
            INPUT_FILE,
            OUTPUT_FILE,
            FAILURE_FILE,
            RAW_MODEL_OUTPUT_FILE_NAME,
        ] {
            let path = record_dir.join(filename);
            if path.exists() {
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600,
                    "unexpected record mode for {}",
                    path.display()
                );
            }
        }
    }

    #[cfg(unix)]
    fn assert_private_ancestor_directories(store: &ThoughtStore, thought: &ThoughtContext) {
        use std::os::unix::fs::PermissionsExt;

        for directory in record_directories(store, thought).into_iter().take(4) {
            if directory.exists() {
                assert_eq!(
                    fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                    0o700,
                    "unexpected directory mode for {}",
                    directory.display()
                );
            }
        }
    }

    #[test]
    fn persists_input_before_output_and_recovers_terminal_output() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ThoughtStore::open(temporary.path()).unwrap();
        let mut thought = context("ca761232-ed42-11ce-bacd-00aa0057b223");

        store.persist_input(&thought).unwrap();
        let input_only = store.load(&thought).unwrap().unwrap();
        assert!(input_only.output.is_none());
        assert_eq!(input_only.lifecycle_state, ThoughtLifecycleState::Thinking);

        thought.set_output(ThinkingOutput::completed(
            Some("working on it".to_string()),
            None,
            Some(DownstreamRequest::Execute {
                intent: "inspect source material".to_string(),
            }),
        ));
        store.persist_output(&thought).unwrap();

        let recovered = store.load(&thought).unwrap().unwrap();
        assert_eq!(recovered.output, thought.output);
        assert_eq!(recovered.lifecycle_state, ThoughtLifecycleState::Execution);
        let record_dir = store.record_dir(&thought);
        assert!(record_dir.join(INPUT_FILE).is_file());
        assert!(record_dir.join(OUTPUT_FILE).is_file());
        assert!(fs::read_dir(record_dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp")));
    }

    #[test]
    fn output_requires_a_durable_input_record() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ThoughtStore::open(temporary.path()).unwrap();
        let mut thought = context("ca761232-ed42-11ce-bacd-00aa0057b223");
        thought.set_output(ThinkingOutput::failed("provider unreachable"));

        assert!(matches!(
            store.persist_output(&thought),
            Err(AgentError::NotFound(_))
        ));
    }

    #[test]
    fn failure_input_requires_and_recovers_a_durable_failed_output() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ThoughtStore::open(temporary.path()).unwrap();
        let mut thought = context("ca761232-ed42-11ce-bacd-00aa0057b223");
        let raw_model_output = r#"{"think":"work","say":"forbidden"}"#;
        let failure = ThinkingFailureInput::new(
            thought.thought_id.clone(),
            thought.occurred_at.clone(),
            "loop",
            raw_model_output,
            vec![OutputValidationError {
                code: "loop_forbids_say".to_string(),
                message: "LOOP forbids say".to_string(),
            }],
        )
        .unwrap();

        store.persist_input(&thought).unwrap();
        assert!(store
            .persist_failure_input(&thought, &failure, raw_model_output.as_bytes())
            .is_err());

        thought.set_output(ThinkingOutput::failed("loop_forbids_say"));
        store.persist_output(&thought).unwrap();
        store
            .persist_failure_input(&thought, &failure, raw_model_output.as_bytes())
            .unwrap();
        store
            .persist_failure_input(&thought, &failure, raw_model_output.as_bytes())
            .unwrap();

        assert_eq!(store.load_failure_input(&thought).unwrap(), Some(failure));
        let record_dir = store.record_dir(&thought);
        assert!(record_dir.join(FAILURE_FILE).is_file());
        assert_eq!(
            fs::read(record_dir.join(RAW_MODEL_OUTPUT_FILE_NAME)).unwrap(),
            raw_model_output.as_bytes()
        );
        let failure_json = fs::read_to_string(record_dir.join(FAILURE_FILE)).unwrap();
        assert!(!failure_json.contains(raw_model_output));
        assert!(!failure_json.contains("\"raw_model_output\":"));
        assert!(failure_json.contains(RAW_MODEL_OUTPUT_FILE_NAME));
        #[cfg(unix)]
        assert_private_permissions(&store, &thought);

        let failure_path = record_dir.join(FAILURE_FILE);
        let original_failure: serde_json::Value = serde_json::from_str(&failure_json).unwrap();
        for (field, replacement) in [
            ("schema_version", serde_json::json!(2)),
            (
                "failed_thought_id",
                serde_json::json!("ca761233-ed42-11ce-bacd-00aa0057b223"),
            ),
            (
                "occurred_at",
                serde_json::json!("2026-07-15T12:34:57.123456789Z"),
            ),
            ("raw_model_output_ref", serde_json::json!("../escape")),
            ("raw_model_output_sha256", serde_json::json!("0".repeat(64))),
            ("raw_model_output_bytes", serde_json::json!(1)),
            ("mode_snapshot", serde_json::json!("unknown")),
            ("validation_errors", serde_json::json!([])),
            (
                "validation_errors",
                serde_json::json!([{"code":"", "message":"missing code"}]),
            ),
        ] {
            let mut corrupted = original_failure.clone();
            corrupted[field] = replacement;
            fs::write(
                &failure_path,
                serde_json::to_vec_pretty(&corrupted).unwrap(),
            )
            .unwrap();
            assert!(
                store.load_failure_input(&thought).is_err(),
                "corrupted {field} must fail closed"
            );
        }
        let mut unknown_field = original_failure.clone();
        unknown_field["raw_model_output"] = serde_json::json!(raw_model_output);
        fs::write(
            &failure_path,
            serde_json::to_vec_pretty(&unknown_field).unwrap(),
        )
        .unwrap();
        assert!(store.load_failure_input(&thought).is_err());
        fs::write(
            &failure_path,
            serde_json::to_vec_pretty(&original_failure).unwrap(),
        )
        .unwrap();

        fs::write(record_dir.join(RAW_MODEL_OUTPUT_FILE_NAME), b"tampered").unwrap();
        assert!(store.load_failure_input(&thought).is_err());
    }

    #[test]
    fn identical_retries_are_idempotent_but_conflicting_output_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ThoughtStore::open(temporary.path()).unwrap();
        let mut thought = context("ca761232-ed42-11ce-bacd-00aa0057b223");

        store.persist_input(&thought).unwrap();
        store.persist_input(&thought).unwrap();
        thought.set_output(ThinkingOutput::completed(
            Some("first immutable answer".to_string()),
            None,
            None,
        ));
        store.persist_output(&thought).unwrap();
        store.persist_output(&thought).unwrap();

        let mut conflicting = context("ca761232-ed42-11ce-bacd-00aa0057b223");
        conflicting.set_output(ThinkingOutput::completed(
            Some("different answer".to_string()),
            None,
            None,
        ));
        assert!(store.persist_output(&conflicting).is_err());
    }

    #[test]
    fn concurrent_store_handles_cannot_overwrite_an_immutable_record() {
        let temporary = tempfile::tempdir().unwrap();
        let first_store = ThoughtStore::open(temporary.path()).unwrap();
        let second_store = ThoughtStore::open(temporary.path()).unwrap();
        let mut first = context("ca761232-ed42-11ce-bacd-00aa0057b223");
        let mut second = context("ca761232-ed42-11ce-bacd-00aa0057b223");
        first_store.persist_input(&first).unwrap();
        first.set_output(ThinkingOutput::completed(
            Some("first candidate".to_string()),
            None,
            None,
        ));
        second.set_output(ThinkingOutput::completed(
            Some("second candidate".to_string()),
            None,
            None,
        ));
        let first_output = first.output.clone();
        let second_output = second.output.clone();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let first_barrier = std::sync::Arc::clone(&barrier);
        let second_barrier = std::sync::Arc::clone(&barrier);

        let first_write = std::thread::spawn(move || {
            first_barrier.wait();
            first_store.persist_output(&first)
        });
        let second_write = std::thread::spawn(move || {
            second_barrier.wait();
            second_store.persist_output(&second)
        });
        let first_result = first_write.join().unwrap();
        let second_result = second_write.join().unwrap();

        assert_ne!(first_result.is_ok(), second_result.is_ok());
        let recovered = ThoughtStore::open(temporary.path())
            .unwrap()
            .recover()
            .unwrap();
        let persisted = &recovered.groups[0].contexts[0].output;
        assert!(persisted == &first_output || persisted == &second_output);
    }

    #[test]
    fn recover_groups_timestamp_collisions_without_secondary_id_sorting() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ThoughtStore::open(temporary.path()).unwrap();
        let mut first = context("ca761233-ed42-11ce-bacd-00aa0057b223");
        let mut second = context("ca761232-ed42-11ce-bacd-00aa0057b223");

        store.persist_input(&first).unwrap();
        store.persist_input(&second).unwrap();
        first.set_output(ThinkingOutput::cancelled(Some(
            "user replaced input".to_string(),
        )));
        second.set_output(ThinkingOutput::failed("model unavailable"));
        store.persist_output(&first).unwrap();
        store.persist_output(&second).unwrap();

        let timeline = store.recover().unwrap();
        assert_eq!(timeline.groups.len(), 1);
        assert_eq!(timeline.groups[0].occurred_at, timestamp());
        assert_eq!(timeline.groups[0].contexts.len(), 2);
        let states: Vec<_> = timeline.groups[0]
            .contexts
            .iter()
            .map(|context| context.output.as_ref().unwrap().terminal_state.clone())
            .collect();
        assert!(states.contains(&ThinkingTerminalState::Cancelled {
            reason: Some("user replaced input".to_string())
        }));
        assert!(states.contains(&ThinkingTerminalState::Failed {
            error: "model unavailable".to_string()
        }));
    }

    #[cfg(unix)]
    #[test]
    fn thought_store_uses_private_directory_and_file_permissions() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ThoughtStore::open(temporary.path()).unwrap();
        let mut thought = context("ca761232-ed42-11ce-bacd-00aa0057b223");
        store.persist_input(&thought).unwrap();
        thought.set_output(ThinkingOutput::completed(
            Some("private answer".to_string()),
            None,
            None,
        ));
        store.persist_output(&thought).unwrap();

        assert_private_permissions(&store, &thought);
    }

    #[cfg(unix)]
    #[test]
    fn open_recover_and_load_repair_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let mut thought = context("ca761232-ed42-11ce-bacd-00aa0057b223");
        thought.set_output(ThinkingOutput::completed(
            Some("repair this record".to_string()),
            None,
            None,
        ));

        let store = ThoughtStore::open(temporary.path()).unwrap();
        let input_only = ThoughtContext::new_at(
            thought.thought_id.clone(),
            thought.occurred_at.clone(),
            thought.input.clone(),
        );
        store.persist_input(&input_only).unwrap();
        store.persist_output(&thought).unwrap();
        set_wide_permissions(&store, &thought);
        drop(store);

        let store = ThoughtStore::open(temporary.path()).unwrap();
        assert_eq!(
            fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
            0o700,
            "open should repair root directory permissions"
        );

        set_wide_permissions(&store, &thought);
        let timeline = store.recover().unwrap();
        assert_eq!(timeline.groups[0].contexts[0].output, thought.output);
        assert_private_ancestor_directories(&store, &thought);

        set_wide_permissions(&store, &thought);
        assert_eq!(
            store.load(&thought).unwrap().unwrap().output,
            thought.output
        );
        assert_private_permissions(&store, &thought);
    }

    #[cfg(unix)]
    #[test]
    fn idempotent_persist_retries_repair_existing_permissions() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ThoughtStore::open(temporary.path()).unwrap();
        let mut thought = context("ca761232-ed42-11ce-bacd-00aa0057b223");

        store.persist_input(&thought).unwrap();
        set_wide_permissions(&store, &thought);
        store.persist_input(&thought).unwrap();
        assert_private_permissions(&store, &thought);

        thought.set_output(ThinkingOutput::completed(
            Some("same immutable answer".to_string()),
            None,
            None,
        ));
        store.persist_output(&thought).unwrap();
        set_wide_permissions(&store, &thought);
        store.persist_output(&thought).unwrap();
        assert_private_permissions(&store, &thought);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_directory_and_record_file_symlinks() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let symlinked_root_data = temporary.path().join("root-link-data");
        let root_target = temporary.path().join("root-target");
        fs::create_dir_all(&symlinked_root_data).unwrap();
        fs::create_dir_all(&root_target).unwrap();
        symlink(&root_target, symlinked_root_data.join(THOUGHTS_DIR)).unwrap();
        assert!(ThoughtStore::open(&symlinked_root_data).is_err());

        let nested_data = temporary.path().join("nested-link-data");
        let nested_store = ThoughtStore::open(&nested_data).unwrap();
        let year_target = temporary.path().join("year-target");
        fs::create_dir_all(&year_target).unwrap();
        symlink(&year_target, nested_store.root().join("2026")).unwrap();
        let thought = context("ca761232-ed42-11ce-bacd-00aa0057b223");
        assert!(nested_store.persist_input(&thought).is_err());

        let record_data = temporary.path().join("record-link-data");
        let record_store = ThoughtStore::open(&record_data).unwrap();
        record_store.persist_input(&thought).unwrap();
        let input_path = record_store.record_dir(&thought).join(INPUT_FILE);
        fs::remove_file(&input_path).unwrap();
        let external_record = temporary.path().join("external-input.json");
        fs::write(&external_record, b"not a thought record").unwrap();
        symlink(&external_record, &input_path).unwrap();
        assert!(record_store.persist_input(&thought).is_err());
    }
}
