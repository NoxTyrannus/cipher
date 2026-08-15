use crate::common::{AgentError, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

pub(super) fn atomic_write_json<T: Serialize>(
    directory: &Path,
    filename: &str,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AgentError::Parse(format!("serialize thought record: {error}")))?;
    atomic_write_bytes(directory, filename, &bytes)
}

pub(super) fn atomic_write_bytes(directory: &Path, filename: &str, bytes: &[u8]) -> Result<()> {
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

pub(super) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    if !secure_existing_file(path)? {
        return Err(AgentError::NotFound(format!("thought record {:?}", path)));
    }
    let bytes = fs::read(path)
        .map_err(|error| AgentError::Io(format!("read thought record {:?}: {error}", path)))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| AgentError::Parse(format!("parse thought record {:?}: {error}", path)))
}

pub(super) fn ensure_secure_directory(path: &Path) -> Result<()> {
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

pub(super) fn secure_existing_tree(directory: &Path) -> Result<()> {
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

pub(super) fn secure_record_files(record_dir: &Path) -> Result<()> {
    for filename in [
        super::INPUT_FILE,
        super::OUTPUT_FILE,
        super::FAILURE_FILE,
        super::RAW_MODEL_OUTPUT_FILE_NAME,
    ] {
        secure_existing_file(&record_dir.join(filename))?;
    }
    Ok(())
}

pub(super) fn secure_existing_directory(path: &Path) -> Result<bool> {
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

pub(super) fn secure_existing_file(path: &Path) -> Result<bool> {
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

pub(super) fn ensure_secure_file(path: &Path) -> Result<()> {
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
pub(super) fn ensure_secure_file_from_metadata(path: &Path, metadata: fs::Metadata) -> Result<()> {
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
pub(super) fn sync_directory(path: &Path) -> Result<()> {
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
