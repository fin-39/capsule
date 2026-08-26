//! Import portable Windows applications or prepared Wine prefixes into one
//! capsule image. Import only copies data; it never executes source content.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use serde::Serialize;
use uuid::Uuid;

use crate::backend::capabilities::CapabilityReport;
use crate::backend::storage::{ImageCreatePlan, ImageMountPlan, StorageError};
use crate::backend::validate_host_absolute;

#[derive(Clone, Debug)]
pub struct ImportRequest {
    pub id: Uuid,
    pub name: String,
    pub source_prefix: PathBuf,
    pub image_path: PathBuf,
    pub image_size_mib: u64,
    pub runtime_root: PathBuf,
}

/// Create a fresh image locally and copy a prepared Wine prefix into it.
/// Downloaded filesystem images are intentionally not accepted.
pub fn import_prepared_prefix(
    request: &ImportRequest,
    capabilities: &CapabilityReport,
) -> Result<(), ImportError> {
    validate_host_absolute(&request.source_prefix)?;
    validate_host_absolute(&request.image_path)?;
    validate_host_absolute(&request.runtime_root)?;

    let source_metadata = fs::symlink_metadata(&request.source_prefix)
        .map_err(|source| io_error(&request.source_prefix, source))?;
    if !source_metadata.file_type().is_dir() || source_metadata.file_type().is_symlink() {
        return Err(ImportError::SourceNotDirectory(
            request.source_prefix.clone(),
        ));
    }

    let image_parent = request
        .image_path
        .parent()
        .ok_or_else(|| ImportError::MissingImageParent(request.image_path.clone()))?;
    fs::create_dir_all(image_parent).map_err(|source| io_error(image_parent, source))?;
    fs::create_dir_all(&request.runtime_root)
        .map_err(|source| io_error(&request.runtime_root, source))?;
    fs::set_permissions(&request.runtime_root, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(&request.runtime_root, source))?;

    let run_dir = request.runtime_root.join(format!(
        "import-{}-{}",
        request.id.simple(),
        std::process::id()
    ));
    let mount_point = run_dir.join("root");
    fs::create_dir(&run_dir).map_err(|source| io_error(&run_dir, source))?;
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(&run_dir, source))?;

    let create = ImageCreatePlan::new(&request.image_path, request.image_size_mib, capabilities)?;
    if let Err(error) = create.execute() {
        let _ = fs::remove_dir(&run_dir);
        return Err(error.into());
    }

    let mount = ImageMountPlan::new(&request.image_path, &mount_point, capabilities)?;
    if let Err(error) = mount.execute_mount() {
        // A mount helper can fail after partially attaching a filesystem. Only
        // delete the fresh image if the mountpoint itself can be removed as an
        // ordinary empty directory; never recurse through it.
        let mountpoint_detached = match fs::remove_dir(&mount_point) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if mountpoint_detached {
            let _ = fs::remove_dir(&run_dir);
            let _ = fs::remove_file(&request.image_path);
        }
        return Err(error.into());
    }

    let populate_result = populate_image(request, &mount_point);
    let unmount_result = mount.execute_unmount();
    // Never recurse below a mountpoint after a failed unmount: doing so could
    // erase the contents of the still-mounted capsule. Preserve the runtime
    // directory for explicit recovery instead.
    if unmount_result.is_ok() {
        let _ = fs::remove_dir_all(&run_dir);
    }

    match (populate_result, unmount_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => {
            let _ = fs::remove_file(&request.image_path);
            Err(error)
        }
        (_, Err(error)) => Err(ImportError::Storage(error)),
    }
}

fn populate_image(request: &ImportRequest, root: &Path) -> Result<(), ImportError> {
    let prefix = root.join("prefix");
    let metadata = root.join(".capsule");
    for directory in [&prefix, &root.join("home"), &root.join("logs"), &metadata] {
        fs::create_dir(directory).map_err(|source| io_error(directory, source))?;
    }
    copy_directory_contents(&request.source_prefix, &prefix)?;

    let manifest = DescriptiveManifest {
        format_version: 1,
        id: request.id,
        name: &request.name,
    };
    let manifest_path = metadata.join("manifest.json");
    let mut manifest_file =
        File::create(&manifest_path).map_err(|source| io_error(&manifest_path, source))?;
    let json = serde_json::to_vec_pretty(&manifest).map_err(ImportError::Manifest)?;
    manifest_file
        .write_all(&json)
        .and_then(|()| manifest_file.write_all(b"\n"))
        .and_then(|()| manifest_file.sync_all())
        .map_err(|source| io_error(&manifest_path, source))?;
    Ok(())
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<(), ImportError> {
    for entry in fs::read_dir(source).map_err(|error| io_error(source, error))? {
        let entry = entry.map_err(|error| io_error(source, error))?;
        copy_entry(&entry.path(), &destination.join(entry.file_name()), source)?;
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path, source_root: &Path) -> Result<(), ImportError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| io_error(source, error))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        fs::create_dir(destination).map_err(|error| io_error(destination, error))?;
        let mut mode = metadata.permissions().mode();
        mode &= !0o6000;
        fs::set_permissions(destination, fs::Permissions::from_mode(mode))
            .map_err(|error| io_error(destination, error))?;
        copy_directory_contents_from_root(source, destination, source_root)
    } else if file_type.is_file() {
        fs::copy(source, destination).map_err(|error| io_error(destination, error))?;
        let mut mode = metadata.permissions().mode();
        mode &= !0o6000;
        fs::set_permissions(destination, fs::Permissions::from_mode(mode))
            .map_err(|error| io_error(destination, error))
    } else if file_type.is_symlink() {
        let target = fs::read_link(source).map_err(|error| io_error(source, error))?;
        if target.is_absolute() {
            // Wine commonly creates Z: -> /. Host-absolute device mappings do
            // not belong in a portable capsule and are deliberately omitted.
            return Ok(());
        }
        let resolved = source
            .parent()
            .unwrap_or(source_root)
            .join(&target)
            .canonicalize()
            .map_err(|error| io_error(source, error))?;
        let canonical_root = source_root
            .canonicalize()
            .map_err(|error| io_error(source_root, error))?;
        if !resolved.starts_with(&canonical_root) {
            return Err(ImportError::SymlinkEscapesSource(source.to_path_buf()));
        }
        symlink(target, destination).map_err(|error| io_error(destination, error))
    } else {
        Err(ImportError::UnsupportedFileType(source.to_path_buf()))
    }
}

fn copy_directory_contents_from_root(
    source: &Path,
    destination: &Path,
    source_root: &Path,
) -> Result<(), ImportError> {
    for entry in fs::read_dir(source).map_err(|error| io_error(source, error))? {
        let entry = entry.map_err(|error| io_error(source, error))?;
        copy_entry(
            &entry.path(),
            &destination.join(entry.file_name()),
            source_root,
        )?;
    }
    Ok(())
}

#[derive(Serialize)]
struct DescriptiveManifest<'a> {
    format_version: u32,
    id: Uuid,
    name: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    InvalidPath(#[from] crate::backend::PathValidationError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("source must be a real directory, not a symlink: {0:?}")]
    SourceNotDirectory(PathBuf),
    #[error("capsule image has no parent directory: {0:?}")]
    MissingImageParent(PathBuf),
    #[error("source contains a symlink that escapes the imported directory: {0:?}")]
    SymlinkEscapesSource(PathBuf),
    #[error("source contains an unsupported device, socket, or FIFO: {0:?}")]
    UnsupportedFileType(PathBuf),
    #[error("failed to encode descriptive manifest: {0}")]
    Manifest(#[source] serde_json::Error),
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> ImportError {
    ImportError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_copy_preserves_internal_symlinks_and_drops_absolute_ones() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::create_dir(source.join("drive_c")).unwrap();
        File::create(source.join("drive_c/game.exe")).unwrap();
        fs::create_dir(source.join("dosdevices")).unwrap();
        symlink("../drive_c", source.join("dosdevices/c:")).unwrap();
        symlink("/", source.join("dosdevices/z:")).unwrap();

        copy_directory_contents(&source, &destination).unwrap();
        assert_eq!(
            fs::read_link(destination.join("dosdevices/c:")).unwrap(),
            PathBuf::from("../drive_c")
        );
        assert!(!destination.join("dosdevices/z:").exists());
    }
}
