use crate::common::{AgentError, Result};
use std::fs;
use std::path::Path;

pub fn ensure_private_directory(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(AgentError::Bootstrap(
            "private data directory path cannot be empty".to_string(),
        ));
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(AgentError::Bootstrap(format!(
                "private data directory cannot be a symlink: {:?}",
                path
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(AgentError::Bootstrap(format!(
                "private data path is not a directory: {:?}",
                path
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AgentError::Bootstrap(format!(
                "inspect private directory {:?}: {error}",
                path
            )));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).map_err(|error| {
            AgentError::Bootstrap(format!("create private directory {:?}: {error}", path))
        })?;

        let metadata = fs::symlink_metadata(path).map_err(|error| {
            AgentError::Bootstrap(format!("stat private directory {:?}: {error}", path))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AgentError::Bootstrap(format!(
                "private data directory changed type while securing it: {:?}",
                path
            )));
        }
        let canonical = fs::canonicalize(path).map_err(|error| {
            AgentError::Bootstrap(format!(
                "canonicalize private directory {:?}: {error}",
                path
            ))
        })?;
        if canonical.parent().is_none() {
            return Err(AgentError::Bootstrap(format!(
                "refusing to use filesystem root as private data directory: {:?}",
                path
            )));
        }
        if fs::canonicalize(std::env::temp_dir()).ok().as_ref() == Some(&canonical) {
            return Err(AgentError::Bootstrap(format!(
                "refusing to use the shared temporary root as private data directory: {:?}",
                path
            )));
        }
        let mode = metadata.permissions().mode();
        if mode & 0o1000 != 0 && mode & 0o002 != 0 {
            return Err(AgentError::Bootstrap(format!(
                "refusing to chmod shared sticky directory as private data: {:?}",
                path
            )));
        }

        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).map_err(|error| {
            AgentError::Bootstrap(format!("chmod 700 private directory {:?}: {error}", path))
        })?;
    }

    #[cfg(not(unix))]
    fs::create_dir_all(path).map_err(|error| {
        AgentError::Bootstrap(format!("create private directory {:?}: {error}", path))
    })?;

    Ok(())
}

pub fn secure_existing_file(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(AgentError::Bootstrap(format!(
                "private data file cannot be a symlink: {:?}",
                path
            )));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(AgentError::Bootstrap(format!(
                "private data path is not a regular file: {:?}",
                path
            )));
        }
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AgentError::Bootstrap(format!(
                "inspect private file {:?}: {error}",
                path
            )));
        }
    };

    #[cfg(not(unix))]
    let _ = metadata;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions).map_err(|error| {
            AgentError::Bootstrap(format!("chmod 600 private file {:?}: {error}", path))
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn repairs_directory_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        ensure_private_directory(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_directory(&directory).unwrap();

        let file = directory.join("database.bin");
        fs::write(&file, b"database").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        secure_existing_file(&file).unwrap();

        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_filesystem_root_shared_tmp_and_symlinks() {
        use std::os::unix::fs::symlink;

        assert!(ensure_private_directory(Path::new("/")).is_err());
        assert!(ensure_private_directory(Path::new("/tmp")).is_err());

        let temporary = tempfile::tempdir().unwrap();
        let real_directory = temporary.path().join("real");
        fs::create_dir(&real_directory).unwrap();
        let directory_link = temporary.path().join("directory-link");
        symlink(&real_directory, &directory_link).unwrap();
        assert!(ensure_private_directory(&directory_link).is_err());

        let real_file = temporary.path().join("real-file");
        fs::write(&real_file, b"secret").unwrap();
        let file_link = temporary.path().join("file-link");
        symlink(&real_file, &file_link).unwrap();
        assert!(secure_existing_file(&file_link).is_err());
    }
}
